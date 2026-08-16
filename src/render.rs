//! Outcome Graph の表示(テキスト / Mermaid / HTML)。導出状態(derive.rs)で色分けする。
//!
//! HTML は dagre(層状レイアウトエンジン、min.js を埋め込み = 自己完結)にレイアウトを
//! 任せる。交差最小化とエッジ配線はエンジン任せで、meguri は node/edge を渡すだけ。

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

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// dagre(min.js)を埋め込んだ自己完結 HTML。レイアウトはブラウザ側で dagre に計算させる。
/// ノードクリックで関連チェーン(祖先+子孫)だけにフォーカス、ホバーで一時強調、右に詳細。
pub fn html(outcomes: &[Outcome]) -> String {
    let st = states(outcomes);

    // ノード(位置は JS が dagre の結果で設定)。
    let mut nodes = String::new();
    for o in outcomes {
        nodes.push_str(&format!(
            "<div class=\"node {state}\" id=\"n-o{id}\">\
             <span class=\"nid\">o{id} · {kind}</span><span class=\"nst\">{stmt}</span></div>",
            state = st[&o.id].label(),
            id = o.id,
            kind = o.verify.kind_str(),
            stmt = esc_html(&o.statement),
        ));
    }

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
    let edge_pairs: Vec<[String; 2]> =
        edges(outcomes).iter().map(|(a, b)| [format!("o{a}"), format!("o{b}")]).collect();
    let edges_json = serde_json::to_string(&edge_pairs).unwrap_or_else(|_| "[]".to_string());

    let mut h = String::new();
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str("<title>meguri graph</title>\n");
    h.push_str(HTML_STYLE);
    h.push_str("</head>\n<body>\n<div id=\"wrap\">\n<div id=\"main\">\n");
    h.push_str("<div id=\"legend\"><span><i class=\"dot ready\"></i>ready</span>");
    h.push_str("<span><i class=\"dot blocked\"></i>blocked</span>");
    h.push_str("<span><i class=\"dot satisfied\"></i>satisfied</span> · click a node to focus its chain (click again to reset)</div>\n");
    h.push_str("<div id=\"graph\"><div id=\"canvas\"><svg id=\"edges\"></svg>\n");
    h.push_str(&nodes);
    h.push_str("</div></div>\n</div>\n<aside id=\"detail\"><p class=\"hint\">Click a node to see its detail.</p></aside>\n</div>\n");
    h.push_str("<script>\n");
    h.push_str(DAGRE_JS);
    h.push_str("\n</script>\n<script>\nconst DATA = ");
    h.push_str(&data_json);
    h.push_str(";\nconst EDGES = ");
    h.push_str(&edges_json);
    h.push_str(";\n");
    h.push_str(HTML_SCRIPT);
    h.push_str("</script>\n</body>\n</html>\n");
    h
}

/// dagre のレイアウトエンジン(自己完結にするため埋め込む)。
const DAGRE_JS: &str = include_str!("vendor/dagre.min.js");

const HTML_STYLE: &str = r#"<style>
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.5 system-ui, -apple-system, sans-serif; }
  #wrap { display: flex; height: 100vh; }
  #main { flex: 1 1 auto; display: flex; flex-direction: column; min-width: 0; }
  #legend { padding: 8px 16px; border-bottom: 1px solid #8883; font-size: 12px; }
  #legend span { margin-right: 14px; }
  .dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 5px; vertical-align: middle; }
  .dot.ready { background: #2563eb; } .dot.blocked { background: #9ca3af; } .dot.satisfied { background: #16a34a; }
  #graph { position: relative; flex: 1 1 auto; overflow: auto; }
  #canvas { position: relative; }
  .node { position: absolute; width: 230px; height: 72px; padding: 8px 10px; border-radius: 10px;
          border: 1px solid #8884; cursor: pointer; background: var(--bg, #fff); box-shadow: 0 1px 2px #0001; overflow: hidden; }
  .node:hover, .node.sel { box-shadow: 0 2px 10px #0002; outline: 2px solid #f59e0b; outline-offset: 1px; }
  .node .nid { font-weight: 700; font-size: 11px; opacity: .65; }
  .node .nst { display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden;
               margin-top: 2px; font-size: 13px; line-height: 1.35; }
  .node.ready { border-color: #2563eb; --bg: #eff6ff; }
  .node.blocked { border-color: #cbd5e1; }
  .node.satisfied { border-color: #16a34a; --bg: #f0fdf4; }
  @media (prefers-color-scheme: dark) {
    .node { --bg: #1f2937; color: #e5e7eb; }
    .node.ready { --bg: #1e293b; } .node.satisfied { --bg: #14261c; }
    .node.blocked { border-color: #475569; }
  }
  #edges { position: absolute; top: 0; left: 0; pointer-events: none; }
  #detail { flex: 0 0 320px; border-left: 1px solid #8884; padding: 16px; overflow: auto; }
  #detail h2 { margin: 0 0 4px; font-size: 18px; }
  #detail .hint { opacity: .6; }
  #detail .state { font-weight: 600; text-transform: uppercase; font-size: 12px; letter-spacing: .04em; }
  #detail .state.ready { color: #2563eb; } #detail .state.blocked { color: #6b7280; } #detail .state.satisfied { color: #16a34a; }
  #detail .stmt { font-size: 15px; font-weight: 600; margin: 8px 0; }
  #detail .desc { white-space: pre-wrap; }
  #detail pre { background: #8881; padding: 8px; border-radius: 6px; overflow: auto; white-space: pre-wrap; }
  #detail .needs { opacity: .8; }
</style>
"#;

const HTML_SCRIPT: &str = r#"function esc(s){return (s==null?'':String(s)).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');}

const NODES = Object.keys(DATA);
const W = 230, H = 72;
const predMap = {}, succMap = {};
for (const [a,b] of EDGES){ (succMap[a]=succMap[a]||[]).push(b); (predMap[b]=predMap[b]||[]).push(a); }
function related(id){
  const R = new Set([id]);
  for (const m of [predMap, succMap]){ const st=[id]; while(st.length){ const x=st.pop(); for(const y of (m[x]||[])) if(!R.has(y)){R.add(y);st.push(y);} } }
  return R;
}

let g = null, HL = null, PIN = null;

window.showNode = function(id){
  const d = DATA[id]; if(!d) return;
  let h = '<h2>'+id+'</h2><p class="state '+d.state+'">'+d.state+' · '+d.verify_kind+'</p>'
        + '<p class="stmt">'+esc(d.statement)+'</p>';
  if (d.description) h += '<p class="desc">'+esc(d.description)+'</p>';
  if (d.command) h += '<pre>'+esc(d.command)+'</pre>';
  if (d.needs && d.needs.length) h += '<p class="needs">needs: '+d.needs.map(esc).join(', ')+'</p>';
  document.getElementById('detail').innerHTML = h;
};

function pathFor(pts){
  if (!pts || !pts.length) return '';
  let d = 'M'+pts[0].x+','+pts[0].y;
  for (let i=1;i<pts.length-1;i++){ const xc=(pts[i].x+pts[i+1].x)/2, yc=(pts[i].y+pts[i+1].y)/2; d += ' Q'+pts[i].x+','+pts[i].y+' '+xc+','+yc; }
  const last = pts[pts.length-1];
  d += ' L'+last.x+','+last.y;
  return d;
}

function drawEdges(){
  const svg = document.getElementById('edges');
  if (!g){ svg.innerHTML=''; return; }
  const A = HL || PIN;
  let s = '';
  for (const [a,b] of EDGES){
    const e = g.edge(a,b); if(!e) continue;
    const inc = A && (a===A || b===A);
    const op = A ? (inc?0.95:0.08) : 0.45;
    const col = inc ? '#f59e0b' : '#94a3b8';
    const w = inc ? 2 : 1.3;
    s += '<path fill="none" stroke="'+col+'" stroke-width="'+w+'" stroke-opacity="'+op+'" d="'+pathFor(e.points)+'"/>';
  }
  svg.innerHTML = s;
}

// dagre にレイアウトさせる。visible が Set ならその節点だけで組み直す(フォーカス)。
function layout(visible){
  g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir:'LR', nodesep:22, ranksep:72, marginx:24, marginy:24 });
  g.setDefaultEdgeLabel(()=>({}));
  const show = id => !visible || visible.has(id);
  for (const id of NODES){
    const el = document.getElementById('n-'+id);
    if (show(id)){ el.style.display=''; g.setNode(id, {width:W, height:H}); }
    else { el.style.display='none'; }
  }
  for (const [a,b] of EDGES){ if (show(a) && show(b)) g.setEdge(a,b); }
  dagre.layout(g);
  const gi = g.graph();
  const canvas = document.getElementById('canvas'), svg = document.getElementById('edges');
  canvas.style.width = gi.width+'px'; canvas.style.height = gi.height+'px';
  svg.setAttribute('width', gi.width); svg.setAttribute('height', gi.height);
  for (const id of NODES){
    if (!show(id)) continue;
    const n = g.node(id), el = document.getElementById('n-'+id);
    el.style.left = (n.x - W/2)+'px'; el.style.top = (n.y - H/2)+'px';
  }
  drawEdges();
}

function setPin(id){
  PIN = (PIN===id ? null : id);
  for (const el of document.querySelectorAll('.node.sel')) el.classList.remove('sel');
  if (PIN){ const n=document.getElementById('n-'+PIN); if(n) n.classList.add('sel'); }
  layout(PIN ? related(PIN) : null);
}

function wire(){
  for (const n of document.querySelectorAll('.node')){
    const id = n.id.slice(2);
    n.addEventListener('mouseenter', ()=>{ HL=id; drawEdges(); });
    n.addEventListener('mouseleave', ()=>{ HL=null; drawEdges(); });
    n.addEventListener('click', ()=>{ showNode(id); setPin(id); });
  }
}
window.addEventListener('load', ()=>{ wire(); layout(null); });
"#;
