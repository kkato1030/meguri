//! o22: 検証落ちの差し戻し(fix turn)を**上限付き**で回す方針。
//!
//! 独立検証(verify、o17-o20)が落ちた Work を、meguri はいきなり人間へ投げない。
//! まず「どの検証がなぜ落ちたか」の診断を添えて、同じエージェントに**差し戻す**(fix turn)。
//! エージェントは worktree のまま直して commit し直し、result.json を書き直す(§3.5 の
//! trust-but-verify を回し続ける)。
//!
//! ただし無限には回さない。1 Work あたり `FIX_TURN_MAX` 回まで差し戻し、使い切ってもなお
//! 落ちるなら [rework] にして人間へ委ねる(有界。壊れた自動化がエージェントを叩き続けない)。
//! ここは pane も git も要らない**純粋な方針**だけを持つ。実際の注入・result 掃除・state 遷移は
//! harvest 側(main の finalize_work)が本方針の判定に従って行う。

use crate::verify::Check;

/// 1 Work に許す fix turn の上限。使い切ったら差し戻しをやめ、人間に委ねる。
pub const FIX_TURN_MAX: u32 = 3;

/// 検証落ちを受けての差し戻し判定。`spent`(これまで消費した fix turn 回数)で分岐する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixTurn {
    /// まだ予算内。エージェントに差し戻す。`attempt` は今回で何回目か(1..=FIX_TURN_MAX)。
    /// `instruction` は落ちた検証の診断を載せた注入文。
    Retry { attempt: u32, instruction: String },
    /// 予算切れ。もう差し戻さず [rework] にして人間へ委ねる。
    GiveUp,
}

/// 検証落ち(`checks` に pass=false を含む)を受けて、上限付きの fix turn を出すか決める。
/// `spent` はこの Work が既に消費した fix turn の回数(耐久カウンタ)。
pub fn decide(spent: u32, checks: &[Check]) -> FixTurn {
    if spent >= FIX_TURN_MAX {
        return FixTurn::GiveUp;
    }
    let attempt = spent + 1;
    FixTurn::Retry { attempt, instruction: fix_instruction(attempt, checks) }
}

/// 差し戻しの注入文。落ちた検証子だけを診断として並べ、「何を直せば通るか」を伝える。
fn fix_instruction(attempt: u32, checks: &[Check]) -> String {
    let mut s = format!(
        "Verification failed (fix turn {attempt}/{FIX_TURN_MAX}). \
         Fix the failing checks below in this worktree, then commit and rewrite .meguri/result.json:\n"
    );
    for c in checks.iter().filter(|c| !c.pass) {
        s.push_str(&format!("- {}: {}\n", c.name, c.detail));
    }
    s
}

#[cfg(test)]
fn test_fail_check(name: &'static str) -> Check {
    Check { name, pass: false, detail: format!("{name} did not pass") }
}

/// 検証落ちの差し戻しが「上限付き」であること。予算内は毎回 Retry、上限で GiveUp。
/// (DoD の `cargo test work::fix_turn_bounded` が指す関門。パスが一致するよう
/// nested module に入れず work 直下に置く。)
#[cfg(test)]
#[test]
fn fix_turn_bounded() {
    let checks = vec![test_fail_check("check_command"), test_fail_check("commits_ahead")];

    // 予算内(spent < FIX_TURN_MAX)は毎回 Retry。attempt は spent+1 で単調増加。
    for spent in 0..FIX_TURN_MAX {
        match decide(spent, &checks) {
            FixTurn::Retry { attempt, instruction } => {
                assert_eq!(attempt, spent + 1, "attempt は消費回数+1");
                // 差し戻し文には残り予算と、落ちた検証の診断が載る(何を直すかが伝わる)。
                assert!(instruction.contains(&format!("{attempt}/{FIX_TURN_MAX}")));
                assert!(instruction.contains("check_command"));
                assert!(instruction.contains("commits_ahead"));
            }
            FixTurn::GiveUp => panic!("spent={spent} は予算内なので Retry のはず"),
        }
    }

    // 上限に達したら差し戻さず GiveUp(人間へ委ねる)。上限を超えても同じ。
    assert_eq!(decide(FIX_TURN_MAX, &checks), FixTurn::GiveUp);
    assert_eq!(decide(FIX_TURN_MAX + 7, &checks), FixTurn::GiveUp);
}

/// 差し戻しの診断には pass した検証子は混ぜない(直す対象だけを見せる)。
#[cfg(test)]
#[test]
fn fix_turn_lists_only_failures() {
    let checks = vec![
        Check { name: "clean_tree", pass: true, detail: "clean".into() },
        test_fail_check("check_command"),
    ];
    let FixTurn::Retry { instruction, .. } = decide(0, &checks) else {
        panic!("予算内なので Retry のはず");
    };
    assert!(instruction.contains("check_command"));
    assert!(!instruction.contains("clean_tree"), "pass した検証子は載せない");
}
