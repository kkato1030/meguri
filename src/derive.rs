//! 導出(§5)。satisfied / ready / blocked は**保存せず、事実から毎回導く**。
//!
//! - satisfied:
//!   - human   … human 充足表明(sticky)
//!   - command … p1 では実行系(マージ)が無いので常に未充足(p2/p3 で担当 Work の
//!               マージから満たされるようになる)
//!   - rollup  … 子(requires)が全て satisfied(子が無ければ未充足)
//! - ready   = 未充足 かつ requires が全て satisfied(→ ここに Work を起こせる)
//! - blocked = 未充足 かつ 未充足の requires がある

use std::collections::HashMap;

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
pub fn satisfied(id: i64, idx: &HashMap<i64, &Outcome>, memo: &mut HashMap<i64, bool>) -> bool {
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
        Verify::Human => o.human_satisfied,
        Verify::Command(_) => false, // p1: 実行系が無い
        Verify::Rollup => {
            !o.requires.is_empty() && o.requires.iter().all(|&r| satisfied(r, idx, memo))
        }
    };
    memo.insert(id, result);
    result
}

/// 全 Outcome の状態を一括導出。
pub fn states(outcomes: &[Outcome]) -> HashMap<i64, State> {
    let idx = index(outcomes);
    let mut memo = HashMap::new();
    let mut out = HashMap::new();
    for o in outcomes {
        let s = if satisfied(o.id, &idx, &mut memo) {
            State::Satisfied
        } else if o.requires.iter().all(|&r| satisfied(r, &idx, &mut memo)) {
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
            verify,
            human_satisfied: human,
            requires: requires.to_vec(),
        }
    }

    #[test]
    fn human_satisfied_is_sticky_fact() {
        let g = vec![o(1, Verify::Human, true, &[]), o(2, Verify::Human, false, &[])];
        let st = states(&g);
        assert_eq!(st[&1], State::Satisfied);
        assert_eq!(st[&2], State::Ready); // 未充足だが前提が無い → ready
    }

    #[test]
    fn command_is_unsatisfiable_in_p1_but_ready_when_prereqs_met() {
        // o1(human, satisfied) ← o2(command)。o2 は前提充足で ready だが、
        // command は p1 では自ら satisfied にならない。
        let g = vec![o(1, Verify::Human, true, &[]), o(2, Verify::Command("t".into()), false, &[1])];
        let st = states(&g);
        assert_eq!(st[&1], State::Satisfied);
        assert_eq!(st[&2], State::Ready);
    }

    #[test]
    fn blocked_until_prereq_satisfied() {
        let g = vec![o(1, Verify::Human, false, &[]), o(2, Verify::Human, false, &[1])];
        let st = states(&g);
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
        assert_eq!(states(&unmet)[&3], State::Blocked);

        let met = vec![
            o(1, Verify::Human, true, &[]),
            o(2, Verify::Human, true, &[]),
            o(3, Verify::Rollup, false, &[1, 2]),
        ];
        assert_eq!(states(&met)[&3], State::Satisfied);
    }

    #[test]
    fn rollup_with_no_children_is_not_satisfied() {
        // 子の無い rollup は満たしようがない(退化した設定)。満たされないことだけを保証する
        // (前提が空なので導出は Ready になるが、そこは意味を持たせない)。
        let g = vec![o(1, Verify::Rollup, false, &[])];
        assert_ne!(states(&g)[&1], State::Satisfied);
    }
}
