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
use std::time::Duration;

/// 1 Work に許す fix turn の上限。使い切ったら差し戻しをやめ、人間に委ねる。
pub const FIX_TURN_MAX: u32 = 3;

/// o23: 沈黙(result 未出現)への nudge の上限。使い切ったら叩くのをやめる(有界)。
/// これも fix turn と同じく「壊れた自動化がエージェントを叩き続けない」ための箍。
pub const NUDGE_MAX: u32 = 3;

/// 沈黙している Work(まだ result.json を書いていない)に、次の nudge を出すかの判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nudge {
    /// 注入する。`attempt` は今回で何回目か(1..=NUDGE_MAX)。`first` は初回発見での即注入か
    /// (以降は間隔を空けた再注入 = nudge)。
    Send { attempt: u32, first: bool },
    /// まだ間隔が空いていない。次のパスまで待つ(作業中のエージェントを叩き続けない)。
    Hold,
    /// 上限に達した。もう叩かない(有界)。
    Exhausted,
}

/// これまで `sent` 回注入し、最後の注入から `since_last` 経過した沈黙 Work に、次の nudge を
/// 出すか決める。初回発見(`sent == 0`)は間隔ゼロでも即注入し、以降は `interval` 間隔で
/// `NUDGE_MAX` 回まで。ここは pane も時計も持たない**純粋な方針**(main の reconcile が従う)。
pub fn nudge(sent: u32, since_last: Duration, interval: Duration) -> Nudge {
    if sent >= NUDGE_MAX {
        return Nudge::Exhausted;
    }
    let due = sent == 0 || since_last >= interval;
    if !due {
        return Nudge::Hold;
    }
    Nudge::Send { attempt: sent + 1, first: sent == 0 }
}

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

/// o25: pane 死亡の判定結果。running な Work を mux の生死で見張った帰結。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneVerdict {
    /// pane が result を残さず死んだ。Work を failed として表面化する。
    Failed,
    /// まだ failed ではない。pane が生きている(見張り続ける)か、result が既にある
    /// (harvest 対象)——どちらもここでは failed にしない。
    Pending,
}

/// pane の死亡を mux の生死から検知する判定(o25)。`alive` は mux.is_alive、
/// `has_result` は result.json(耐久の完了シグナル)が出ているか。
///
/// 「結果を残さないまま pane が死んだ」= エージェントが落ちた/消えた、を failed とする。
/// result が既にあるなら、pane が死んでいても harvest に回せる(収穫は result.json と git 状態
/// だけを見て live ハンドルを要らない)ので failed にしない —— 死んでも成果と最終画面が残る
/// 「ブロック ≠ 失敗」(§3.5)を、結果を残した死には適用する。
pub fn judge_pane(alive: bool, has_result: bool) -> PaneVerdict {
    if !alive && !has_result {
        PaneVerdict::Failed
    } else {
        PaneVerdict::Pending
    }
}

/// o24: running な Work が期限に達したときの判定。タイムアウトは握りつぶさず表面化するが、
/// pane は殺さない —— 人間が最終画面を覗いて続きを決められる(§3.5「ブロック ≠ 失敗」)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeoutVerdict {
    /// 期限切れ。Work を [timed_out] として表面化する(running から外す)。**pane は残す**。
    TimedOut,
    /// まだ期限内、あるいは result が既に出ている(harvest 対象)——ここでは何もしない。
    Pending,
}

impl TimeoutVerdict {
    /// o24 の核となる不変: タイムアウトの処理は pane を殺さない(どの判定でも false)。
    /// 表面化しても最終画面は残り、人間が覗いて続きを決められる。
    pub const fn kills_pane(&self) -> bool {
        false
    }
}

/// running な Work の経過時間と result 有無から、タイムアウトを表面化するか判定する(o24)。
/// `elapsed` は起動からの経過、`timeout` は許容上限、`has_result` は result.json が出ているか。
/// result が既にあるなら timeout ではなく harvest 対象として拾う(成果を優先、judge_pane と同じ構え)。
pub fn judge_timeout(elapsed: Duration, timeout: Duration, has_result: bool) -> TimeoutVerdict {
    if has_result || elapsed < timeout {
        TimeoutVerdict::Pending
    } else {
        TimeoutVerdict::TimedOut
    }
}

/// タイムアウトで表面化する Work の state 名(o24)。pane を残したまま running から外す。
pub const TIMED_OUT_STATE: &str = "timed_out";

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

/// pane の死亡を mux の生死から検知し、Work を failed として表面化する(o25)。
/// (DoD の `cargo test work::pane_death` が指す関門。パスが一致するよう work 直下に置く。)
#[cfg(test)]
#[test]
fn pane_death() {
    // pane が死に、result も無い = 結果を残さず落ちた → failed として表面化する。
    assert_eq!(judge_pane(false, false), PaneVerdict::Failed);

    // pane が生きている間は failed ではない(まだ回している / 収穫待ち)。
    assert_eq!(judge_pane(true, false), PaneVerdict::Pending);
    assert_eq!(judge_pane(true, true), PaneVerdict::Pending);

    // pane が死んでいても result があれば harvest に回せる。死亡だけで failed にしない
    //(result は耐久で live ハンドルを要らない = 「ブロック ≠ 失敗」、§3.5)。
    assert_eq!(judge_pane(false, true), PaneVerdict::Pending);
}

/// タイムアウトは表面化するが pane は残す(o24)。
/// (DoD の `cargo test work::timeout_keeps_pane` が指す関門。パスが一致するよう
/// nested module に入れず work 直下に置く。)
#[cfg(test)]
#[test]
fn timeout_keeps_pane() {
    let timeout = Duration::from_secs(60);

    // 期限内はまだ何もしない(pane は生きたまま回している)。
    assert_eq!(
        judge_timeout(timeout - Duration::from_secs(1), timeout, false),
        TimeoutVerdict::Pending
    );
    assert_eq!(judge_timeout(Duration::ZERO, timeout, false), TimeoutVerdict::Pending);

    // 期限に達したら握りつぶさず timeout を表面化する。上限を超えても同じ。
    assert_eq!(judge_timeout(timeout, timeout, false), TimeoutVerdict::TimedOut);
    assert_eq!(judge_timeout(timeout * 3, timeout, false), TimeoutVerdict::TimedOut);

    // ただし result が既にあれば timeout ではなく harvest 対象(成果を優先、死んでも収穫する)。
    assert_eq!(judge_timeout(timeout * 3, timeout, true), TimeoutVerdict::Pending);

    // o24 の核: どの判定でも pane は殺さない。タイムアウトの処理は最終画面を残す。
    assert!(!TimeoutVerdict::TimedOut.kills_pane(), "timeout は pane を残す");
    assert!(!TimeoutVerdict::Pending.kills_pane());
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

/// エージェントの沈黙が、上限付きの nudge を発生させること(o23 の関門)。
/// 初回発見は即注入、以降は間隔を空けて上限まで、上限を超えたら黙る(有界)。
/// (DoD の `cargo test work::nudge_bounded` が指す。パスが一致するよう work 直下に置く。)
#[cfg(test)]
#[test]
fn nudge_bounded() {
    let interval = Duration::from_secs(20);

    // 初回発見(sent==0)は間隔ゼロでも即注入。first=true(injected)。
    assert_eq!(nudge(0, Duration::ZERO, interval), Nudge::Send { attempt: 1, first: true });

    // 以降は間隔が空くたびに 1 回だけ、上限まで再注入(nudge)。
    for sent in 1..NUDGE_MAX {
        // 間隔未満なら待つ(沈黙していても叩き続けない)。
        assert_eq!(nudge(sent, interval - Duration::from_secs(1), interval), Nudge::Hold);
        // 間隔を満たせば注入。attempt は sent+1 で単調増加、first は初回だけ。
        assert_eq!(
            nudge(sent, interval, interval),
            Nudge::Send { attempt: sent + 1, first: false },
            "sent={sent} は間隔を満たせば再注入"
        );
    }

    // 上限に達したら、いくら沈黙が続いても叩かない(有界)。上限を超えても同じ。
    assert_eq!(nudge(NUDGE_MAX, interval * 100, interval), Nudge::Exhausted);
    assert_eq!(nudge(NUDGE_MAX + 5, interval * 100, interval), Nudge::Exhausted);
}
