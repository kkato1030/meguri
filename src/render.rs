//! Outcome Graph の表示(テキスト / Mermaid / HTML)。導出状態(derive.rs)で色分けする。

use std::collections::HashMap;

use serde_json::json;

use crate::derive::{states, State};
use crate::store::{Outcome, Verify};

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

fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.chars().count() < s.chars().count() {
        format!("{t}…")
    } else {
        t
    }
}

/// 自己完結の HTML グラフビュー。Mermaid で描画し、ノードをクリックすると詳細
/// (statement / description / verify / needs / 状態)が右に出る。ローカルで開く用。
pub fn html(outcomes: &[Outcome]) -> String {
    let st = states(outcomes);

    // Mermaid 本体(ラベルは短縮、全文は詳細パネルで)。click で showNode を呼ぶ。
    let mut mm = String::from("graph TD\n");
    for o in outcomes {
        let label = short(&o.statement, 46).replace('"', "'");
        mm.push_str(&format!("  o{}[\"o{}: {}\"]:::{}\n", o.id, o.id, label, st[&o.id].label()));
    }
    for (from, to) in edges(outcomes) {
        mm.push_str(&format!("  o{from} --> o{to}\n"));
    }
    mm.push_str("  classDef satisfied fill:#bbf7d0,stroke:#16a34a,color:#052e16;\n");
    mm.push_str("  classDef ready fill:#bfdbfe,stroke:#2563eb,color:#0b213f;\n");
    mm.push_str("  classDef blocked fill:#e5e7eb,stroke:#9ca3af,color:#374151;\n");
    for o in outcomes {
        mm.push_str(&format!("  click o{} showNode\n", o.id));
    }

    // ノード詳細データ(JSON)。
    let data: serde_json::Map<String, serde_json::Value> = outcomes
        .iter()
        .map(|o| {
            let (kind, command) = match &o.verify {
                Verify::Command(c) => ("command", Some(c.clone())),
                Verify::Human => ("human", None),
                Verify::Rollup => ("rollup", None),
            };
            let needs: Vec<String> = o.requires.iter().map(|r| format!("o{r}")).collect();
            (
                format!("o{}", o.id),
                json!({
                    "statement": o.statement,
                    "description": o.description,
                    "verify_kind": kind,
                    "command": command,
                    "needs": needs,
                    "state": st[&o.id].label(),
                }),
            )
        })
        .collect();
    let data_json = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());

    let mut h = String::new();
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str("<title>meguri graph</title>\n");
    h.push_str(HTML_STYLE);
    h.push_str("</head>\n<body>\n<div id=\"wrap\">\n<div id=\"graph\"><pre class=\"mermaid\">\n");
    h.push_str(&mm);
    h.push_str("</pre></div>\n<aside id=\"detail\"><p class=\"hint\">Click a node to see its detail.</p></aside>\n</div>\n");
    h.push_str("<script src=\"https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js\"></script>\n");
    h.push_str("<script>\nconst DATA = ");
    h.push_str(&data_json);
    h.push_str(";\n");
    h.push_str(HTML_SCRIPT);
    h.push_str("</script>\n</body>\n</html>\n");
    h
}

const HTML_STYLE: &str = r#"<style>
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.5 system-ui, sans-serif; }
  #wrap { display: flex; height: 100vh; }
  #graph { flex: 1 1 auto; overflow: auto; padding: 16px; }
  #graph .mermaid { min-width: 0; }
  #detail { flex: 0 0 340px; border-left: 1px solid #8884; padding: 16px; overflow: auto; }
  #detail h2 { margin: 0 0 4px; font-size: 18px; }
  #detail .hint { opacity: .6; }
  #detail .state { font-weight: 600; text-transform: uppercase; font-size: 12px; letter-spacing: .04em; }
  #detail .state.ready { color: #2563eb; }
  #detail .state.blocked { color: #6b7280; }
  #detail .state.satisfied { color: #16a34a; }
  #detail .stmt { font-size: 15px; font-weight: 600; margin: 8px 0; }
  #detail .desc { white-space: pre-wrap; }
  #detail pre { background: #8881; padding: 8px; border-radius: 6px; overflow: auto; }
  #detail .needs { opacity: .8; }
  node, .node { cursor: pointer; }
</style>
"#;

const HTML_SCRIPT: &str = r#"function esc(s){return (s==null?'':String(s)).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');}
window.showNode = function(id){
  const d = DATA[id]; if(!d) return;
  let html = '<h2>'+id+'</h2>'
    + '<p class="state '+d.state+'">'+d.state+' · '+d.verify_kind+'</p>'
    + '<p class="stmt">'+esc(d.statement)+'</p>';
  if (d.description) html += '<p class="desc">'+esc(d.description)+'</p>';
  if (d.command) html += '<pre>'+esc(d.command)+'</pre>';
  if (d.needs && d.needs.length) html += '<p class="needs">needs: '+d.needs.map(esc).join(', ')+'</p>';
  document.getElementById('detail').innerHTML = html;
};
mermaid.initialize({ startOnLoad: true, securityLevel: 'loose' });
"#;
