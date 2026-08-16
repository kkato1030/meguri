//! 導出(§5)。satisfied / ready / blocked は**保存せず、事実から毎回導く**。
//!
//! - satisfied:
//!   - human   … human 充足表明(sticky)、または accept された Work がある(o21+ ローカル Human Gate)
//!   - command … accept された担当 Work があれば満たされる(v0.2: ローカル accept。GitHub 化は v0.3)
//!   - rollup  … 子(requires)が全て satisfied(子が無ければ未充足)
//! - ready   = 未充足 かつ requires が全て satisfied(→ ここに Work を起こせる)
//! - blocked = 未充足 かつ 未充足の requires がある
//!
//! `accepted` = 「accept された Work を持つ Outcome の id 集合」。**保存する事実は Work の
//! accepted 状態**であり、satisfied はそこから毎回導く(§5 の導出方針は維持)。

use std::collections::{HashMap, HashSet};

use crate::store::{Outcome, Verify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Satisfied,
    Ready,
    Blocked,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Satisfied => "satisfied",
            State::Ready => "ready",
            State::Blocked => "blocked",
        }
    }
}

/// id → Outcome の索引。
pub fn index(outcomes: &[Outcome]) -> HashMap<i64, &Outcome> {
    outcomes.iter().map(|o| (o.id, o)).collect()
}

/// 1 つの Outcome が充足しているか(メモ化しつつ再帰。グラフは DAG なので停止する)。
/// `accepted` = accept 済み Work を持つ Outcome の id 集合。
pub fn satisfied(
    id: i64,
    idx: &HashMap<i64, &Outcome>,
    accepted: &HashSet<i64>,
    memo: &mut HashMap<i64, bool>,
) -> bool {
    if let Some(&v) = memo.get(&id) {
        return v;
    }
    // サイクルは store 側で禁止しているが、保険で先に false を置いて自己再帰を止める。
    memo.insert(id, false);
    let o = match idx.get(&id) {
        Some(o) => o,
        None => return false,
    };
    let result = match &o.verify {
        // human は表明 sticky、または accept された Work があれば満たされる。
        Verify::Human => o.human_satisfied || accepted.contains(&id),
        // command は担当 Work が verified→accept されて初めて満たされる(ローカル Human Gate)。
        Verify::Command(_) => accepted.contains(&id),
        Verify::Rollup => {
            !o.requires.is_empty() && o.requires.iter().all(|&r| satisfied(r, idx, accepted, memo))
        }
    };
    memo.insert(id, result);
    result
}

/// 全 Outcome の状態を一括導出。`accepted` = accept 済み Work を持つ Outcome の id 集合。
pub fn states(outcomes: &[Outcome], accepted: &HashSet<i64>) -> HashMap<i64, State> {
    let idx = index(outcomes);
    let mut memo = HashMap::new();
    let mut out = HashMap::new();
    for o in outcomes {
        let s = if satisfied(o.id, &idx, accepted, &mut memo) {
            State::Satisfied
        } else if o.requires.iter().all(|&r| satisfied(r, &idx, accepted, &mut memo)) {
            State::Ready
        } else {
            State::Blocked
        };
        out.insert(o.id, s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Verify;

    fn o(id: i64, verify: Verify, human: bool, requires: &[i64]) -> Outcome {
        Outcome {
            id,
            intent_id: 1,
            statement: format!("o{id}"),
            description: String::new(),
            verify,
            human_satisfied: human,
            requires: requires.to_vec(),
        }
    }

    fn none() -> HashSet<i64> {
        HashSet::new()
    }

    #[test]
    fn human_satisfied_is_sticky_fact() {
        let g = vec![o(1, Verify::Human, true, &[]), o(2, Verify::Human, false, &[])];
        let st = states(&g, &none());
        assert_eq!(st[&1], State::Satisfied);
        assert_eq!(st[&2], State::Ready); // 未充足だが前提が無い → ready
    }

    #[test]
    fn command_needs_accepted_work_but_ready_when_prereqs_met() {
        // o1(human, satisfied) ← o2(command)。o2 は前提充足で ready だが、
        // command は accept された Work が無ければ自ら satisfied にならない。
        let g = vec![o(1, Verify::Human, true, &[]), o(2, Verify::Command("t".into()), false, &[1])];
        let st = states(&g, &none());
        assert_eq!(st[&1], State::Satisfied);
        assert_eq!(st[&2], State::Ready);
    }

    #[test]
    fn accepted_work_satisfies_command_and_unlocks_next() {
        // o1(command) ← o2(command)。o1 を accept すると o1=satisfied, o2=ready。
        let g = vec![
            o(1, Verify::Command("t".into()), false, &[]),
            o(2, Verify::Command("t".into()), false, &[1]),
        ];
        // accept 前: o1 ready(前提なし)/ o2 blocked(o1 未充足)。
        let before = states(&g, &none());
        assert_eq!(before[&1], State::Ready);
        assert_eq!(before[&2], State::Blocked);
        // o1 を accept:
        let accepted: HashSet<i64> = [1].into_iter().collect();
        let after = states(&g, &accepted);
        assert_eq!(after[&1], State::Satisfied);
        assert_eq!(after[&2], State::Ready);
    }

    #[test]
    fn blocked_until_prereq_satisfied() {
        let g = vec![o(1, Verify::Human, false, &[]), o(2, Verify::Human, false, &[1])];
        let st = states(&g, &none());
        assert_eq!(st[&2], State::Blocked); // o1 が未充足
    }

    #[test]
    fn rollup_follows_children() {
        // o3(rollup)← o1,o2。両方 satisfied のときだけ o3 が satisfied。
        let unmet = vec![
            o(1, Verify::Human, true, &[]),
            o(2, Verify::Human, false, &[]),
            o(3, Verify::Rollup, false, &[1, 2]),
        ];
        assert_eq!(states(&unmet, &none())[&3], State::Blocked);

        let met = vec![
            o(1, Verify::Human, true, &[]),
            o(2, Verify::Human, true, &[]),
            o(3, Verify::Rollup, false, &[1, 2]),
        ];
        assert_eq!(states(&met, &none())[&3], State::Satisfied);
    }

    #[test]
    fn rollup_with_no_children_is_not_satisfied() {
        // 子の無い rollup は満たしようがない(退化した設定)。満たされないことだけを保証する
        // (前提が空なので導出は Ready になるが、そこは意味を持たせない)。
        let g = vec![o(1, Verify::Rollup, false, &[])];
        assert_ne!(states(&g, &none())[&1], State::Satisfied);
    }
}
