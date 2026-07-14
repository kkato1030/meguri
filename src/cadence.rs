//! 時刻駆動の2つの discovery 調速ゲート(issue #148)の純粋ロジック。
//!
//! discovery(`src/tasks.rs`)が claim の前に見る2つのゲート — GitHub-native の
//! 依存チェックと同じ層 — をここに集約する:
//! - **not-before**: 「この日時までは着手しない」。時刻前はサイレントにスキップ
//!   (ラベルもコメントも書かない。ブロックされた依存と同じ流儀)。
//! - **cadence**: 「このラベルの消化は窓あたり N 件まで」。窓内の消化数が上限に
//!   達している間、同ラベルのタスクをスキップする。
//!
//! ここは純粋ロジック(マーカー解析・窓計算・バケツ判定・disposition)だけを持ち、
//! discovery と CLI が同じ実装を共有して表示と挙動がずれないようにする。実際の
//! 消化数え上げは store(`runs.cadence_label`)、config の形は `config.rs` にある。
//! 時刻はすべて UTC(ADR 0011)。テストは clock を注入する。

use crate::config::CadenceRule;
use crate::store::{format_epoch, parse_ts};

/// github issue が解禁時刻を宣言する本文 hidden マーカーの開き。例:
/// `<!-- meguri:not-before 2026-07-20 -->`。cleaner の head-sha マーカーや
/// #146 の schedule マーカーと同じ「本文 hidden コメント」流儀。
const NOT_BEFORE_OPEN: &str = "<!-- meguri:not-before ";

/// not-before 値(マーカー / ローカルフィールド)の解析失敗。呼び出し側は
/// これを受けて fail-closed する(ADR 0011: 解禁日のタイポで早期公開しない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotBeforeParseError {
    /// 解析できなかった生の値(CLI 表示用)。
    pub raw: String,
}

/// 1つの not-before 値を epoch 秒(UTC)へ正規化する。裸の日付 `YYYY-MM-DD`
/// (→ `T00:00:00Z` と解釈)か、完全な RFC3339 `...Z` を受ける。
pub fn parse_not_before_value(raw: &str) -> Result<u64, NotBeforeParseError> {
    let trimmed = raw.trim();
    let candidate = if trimmed.len() == 10 {
        format!("{trimmed}T00:00:00Z")
    } else {
        trimmed.to_string()
    };
    let err = || NotBeforeParseError {
        raw: trimmed.to_string(),
    };
    let ts = parse_ts(&candidate).ok_or_else(err)?;
    // `parse_ts` only checks the shape, not that the month/day are in range —
    // "2026-13-40" would silently roll over to another date. Require the value
    // to round-trip so a typo fails closed instead of shifting the gate.
    if format_epoch(ts) != candidate {
        return Err(err());
    }
    Ok(ts)
}

/// issue 本文から not-before 時刻を取り出す。マーカーが無ければ `Ok(None)`。
/// 複数マーカーは最も遅い(最も制約が強い)ものを採る。解析不能なマーカーが
/// 1つでもあれば `Err` — 呼び出し側が fail-closed するため、どのマーカーを
/// 意図したか判らない以上「まだ通さない」が安全側になる。
pub fn parse_not_before(body: &str) -> Result<Option<u64>, NotBeforeParseError> {
    let mut latest: Option<u64> = None;
    for chunk in body.split(NOT_BEFORE_OPEN).skip(1) {
        let raw = match chunk.split("-->").next() {
            Some(raw) => raw,
            None => continue,
        };
        let ts = parse_not_before_value(raw)?;
        latest = Some(latest.map_or(ts, |cur| cur.max(ts)));
    }
    Ok(latest)
}

/// `not_before` が `now` より未来なら待つべき時刻を、そうでなければ `None`
/// (ゲートは開いている)を返す。
pub fn not_before_wait(not_before: Option<u64>, now: u64) -> Option<u64> {
    match not_before {
        Some(ts) if ts > now => Some(ts),
        _ => None,
    }
}

/// cadence rule の消化上限(窓モードによらず)。config 検証で `max_per_day` 単独
/// か (`per_hours` + `max`) のちょうど一方だけが立つことは保証済み。
pub fn limit(rule: &CadenceRule) -> u32 {
    rule.max_per_day.or(rule.max).unwrap_or(0)
}

/// `now` 時点で rule の消化を数える窓の起点(epoch 秒, UTC)。`max_per_day` は
/// その日の UTC 深夜 `[00:00, now]`、`per_hours = H` はローリング
/// `[now - H*3600, now]`。
pub fn window_start(rule: &CadenceRule, now: u64) -> u64 {
    match rule.per_hours {
        Some(hours) => now.saturating_sub(u64::from(hours) * 3600),
        // max_per_day: unix 時間の暦日は 86400 秒境界に揃う(このコードベースは
        // 一貫して閏秒を無視する)ので、UTC 深夜は now を 86400 で切り捨てたもの。
        None => now - (now % 86_400),
    }
}

/// `max_per_day` 窓が次にリセットする時刻(= 翌 UTC 深夜)。ローリング窓
/// (`per_hours`)は最古の消化 run が窓から抜けた時にリセットするため単純な
/// 時刻では表せず `None`(CLI は「近く」と表示するに留める)。
pub fn resets_at(rule: &CadenceRule, now: u64) -> Option<u64> {
    match rule.per_hours {
        Some(_) => None,
        None => Some(window_start(rule, now) + 86_400),
    }
}

/// issue のラベル集合が該当する cadence rule。`Ok(None)` = 該当なし、
/// `Ok(Some(label))` = ちょうど1つ、`Err(labels)` = 2つ以上該当(fail-closed:
/// 単一の `runs.cadence_label` では2バケツを数えられないため)。
pub fn cadence_bucket(
    issue_labels: &[String],
    rules: &[CadenceRule],
) -> Result<Option<String>, Vec<String>> {
    let matched: Vec<String> = rules
        .iter()
        .filter(|r| issue_labels.iter().any(|l| l == &r.label))
        .map(|r| r.label.clone())
        .collect();
    match matched.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => Err(matched),
    }
}

/// discovery が1タスクに下す判定。CLI(`meguri tasks`)はこれをそのまま理由
/// 表示に使い、discovery の挙動と表示がずれないようにする。判定は
/// [`LabelTaskSource::evaluate`](crate::tasks) が discovery と共有する単一の
/// ゲート実装で行うため、ここは表示用の enum だけを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// どのゲートにも掛からず、消化してよい。
    Ready,
    /// not-before 未通過。`until` まで待つ。
    WaitingNotBefore { until: u64 },
    /// not-before マーカー/フィールドが解析不能(fail-closed で止まっている)。
    UnparsableNotBefore { raw: String },
    /// 未解決の依存(ブロッカー)で止まっている。
    Blocked,
    /// 2つ以上の cadence rule に一致(fail-closed)。
    ConflictingCadenceLabels { labels: Vec<String> },
    /// cadence の窓が埋まっている(過去の消化 + 同一 discovery pass での予約を
    /// 含めた実効消化数が上限に達している)。
    WaitingCadence {
        label: String,
        consumed: u32,
        max: u32,
        resets_at: Option<u64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day_rule(label: &str, n: u32) -> CadenceRule {
        CadenceRule {
            label: label.into(),
            max_per_day: Some(n),
            per_hours: None,
            max: None,
        }
    }

    fn hours_rule(label: &str, hours: u32, n: u32) -> CadenceRule {
        CadenceRule {
            label: label.into(),
            max_per_day: None,
            per_hours: Some(hours),
            max: Some(n),
        }
    }

    fn ts(s: &str) -> u64 {
        parse_ts(s).unwrap_or_else(|| panic!("bad ts {s}"))
    }

    #[test]
    fn not_before_accepts_date_and_rfc3339() {
        assert_eq!(
            parse_not_before_value("2026-07-20").unwrap(),
            ts("2026-07-20T00:00:00Z")
        );
        assert_eq!(
            parse_not_before_value("2026-07-20T09:30:00Z").unwrap(),
            ts("2026-07-20T09:30:00Z")
        );
        assert_eq!(
            parse_not_before_value("  2026-07-20 ").unwrap(),
            ts("2026-07-20T00:00:00Z")
        );
        assert!(parse_not_before_value("nope").is_err());
        assert!(parse_not_before_value("2026-13-40").is_err());
    }

    #[test]
    fn parse_not_before_extracts_marker_and_takes_latest() {
        assert_eq!(parse_not_before("no marker here").unwrap(), None);
        assert_eq!(
            parse_not_before("body\n<!-- meguri:not-before 2026-07-20 -->\nmore").unwrap(),
            Some(ts("2026-07-20T00:00:00Z"))
        );
        // 複数マーカーは最も遅いものを採る。
        assert_eq!(
            parse_not_before(
                "<!-- meguri:not-before 2026-07-20 -->\n<!-- meguri:not-before 2026-08-01 -->"
            )
            .unwrap(),
            Some(ts("2026-08-01T00:00:00Z"))
        );
        // 1つでも壊れていれば fail-closed(Err)。
        assert!(parse_not_before("<!-- meguri:not-before oops -->").is_err());
    }

    #[test]
    fn not_before_wait_gates_only_the_future() {
        let now = ts("2026-07-20T00:00:00Z");
        assert_eq!(not_before_wait(None, now), None);
        assert_eq!(not_before_wait(Some(now - 1), now), None);
        assert_eq!(not_before_wait(Some(now), now), None); // 同時刻は通す
        assert_eq!(not_before_wait(Some(now + 1), now), Some(now + 1));
    }

    #[test]
    fn window_start_day_is_utc_midnight() {
        let noon = ts("2026-07-20T12:34:56Z");
        assert_eq!(
            window_start(&day_rule("sns", 1), noon),
            ts("2026-07-20T00:00:00Z")
        );
        assert_eq!(
            resets_at(&day_rule("sns", 1), noon),
            Some(ts("2026-07-21T00:00:00Z"))
        );
    }

    #[test]
    fn window_start_rolling_subtracts_hours() {
        let now = ts("2026-07-20T12:00:00Z");
        assert_eq!(
            window_start(&hours_rule("nl", 24, 1), now),
            ts("2026-07-19T12:00:00Z")
        );
        assert_eq!(resets_at(&hours_rule("nl", 24, 1), now), None);
    }

    #[test]
    fn limit_reads_whichever_mode_is_set() {
        assert_eq!(limit(&day_rule("sns", 3)), 3);
        assert_eq!(limit(&hours_rule("nl", 168, 1)), 1);
    }

    #[test]
    fn cadence_bucket_matches_zero_one_or_many() {
        let rules = vec![day_rule("sns", 1), hours_rule("nl", 168, 1)];
        assert_eq!(cadence_bucket(&["other".into()], &rules).unwrap(), None);
        assert_eq!(
            cadence_bucket(&["sns".into(), "other".into()], &rules).unwrap(),
            Some("sns".to_string())
        );
        let err = cadence_bucket(&["sns".into(), "nl".into()], &rules).unwrap_err();
        assert_eq!(err.len(), 2);
        assert!(err.contains(&"sns".to_string()) && err.contains(&"nl".to_string()));
    }
}
