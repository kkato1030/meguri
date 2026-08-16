//! Outcome Graph の表示(テキスト / Mermaid)。導出状態(derive.rs)で色分けする。

use std::collections::HashMap;

use crate::derive::{states, State};
use crate::store::Outcome;

/// requires 辺は「requires_id → outcome_id」(前提が先)で描く。
fn edges(outcomes: &[Outcome]) -> Vec<(i64, i64)> {
    let mut e = Vec::new();
    for o in outcomes {
        for &req in &o.requires {
            e.push((req, o.id));
        }
    }
    e
}

/// 人が読むテキスト表示。状態ごとにまとめる。
pub fn text(outcomes: &[Outcome]) -> String {
    let st = states(outcomes);
    let mut out = String::new();
    for group in [State::Satisfied, State::Ready, State::Blocked] {
        let mut any = false;
        for o in outcomes.iter().filter(|o| st[&o.id] == group) {
            if !any {
                out.push_str(&format!("{}:\n", group.label()));
                any = true;
            }
            let reqs = if o.requires.is_empty() {
                String::new()
            } else {
                let unmet: Vec<String> = o
                    .requires
                    .iter()
                    .filter(|r| st[r] != State::Satisfied)
                    .map(|r| format!("o{r}"))
                    .collect();
                if unmet.is_empty() {
                    "  (all prerequisites satisfied)".to_string()
                } else {
                    format!("  (unmet prerequisites: {})", unmet.join(", "))
                }
            };
            out.push_str(&format!(
                "  o{:<3} [{}] {}{}\n",
                o.id,
                o.verify.kind_str(),
                o.statement,
                reqs
            ));
        }
    }
    if out.is_empty() {
        out.push_str("(no outcomes)\n");
    }
    out
}

/// Mermaid(`graph TD`)。ノードを状態で classDef 着色。
pub fn mermaid(outcomes: &[Outcome]) -> String {
    let st: HashMap<i64, State> = states(outcomes);
    let mut out = String::from("graph TD\n");
    for o in outcomes {
        // ラベルの " は Mermaid を壊すので除去。
        let label = o.statement.replace('"', "'");
        out.push_str(&format!("  o{}[\"o{}: {}\"]:::{}\n", o.id, o.id, label, st[&o.id].label()));
    }
    for (from, to) in edges(outcomes) {
        out.push_str(&format!("  o{from} --> o{to}\n"));
    }
    out.push_str("  classDef satisfied fill:#bbf7d0,stroke:#16a34a,color:#052e16;\n");
    out.push_str("  classDef ready fill:#bfdbfe,stroke:#2563eb,color:#0b213f;\n");
    out.push_str("  classDef blocked fill:#e5e7eb,stroke:#9ca3af,color:#374151;\n");
    out
}
