//! A minimal standard 5-field cron parser + evaluator (UTC, minute
//! granularity). No chrono dependency — it reuses the repo's Howard-Hinnant
//! civil-date convention (the same algorithm as `store::now` / `store::parse_ts`).
//!
//! Fields, in order: `minute hour day-of-month month day-of-week`. Each field
//! accepts `*`, `a`, `a-b`, `*/n`, `a-b/n`, `a/n` (shorthand for `a-max/n`),
//! and comma lists of those. Day-of-week is `0-6` with Sunday = 0 (`7` is also
//! accepted for Sunday). When *both* day-of-month and day-of-week are
//! restricted (neither is `*`), a day matches if *either* matches — the
//! classic Vixie-cron rule.
//!
//! Time is interpreted as UTC (issue #146: v1 is UTC-only; a per-schedule
//! timezone is deferred). Granularity is one minute, which is finer than the
//! scheduler's poll interval — the due check only asks "did an occurrence fall
//! in this window", so sub-minute precision is irrelevant.

/// A parsed cron expression. Each field is a bitmask where bit `v` set means
/// value `v` is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64, // bits 0..=59
    hours: u64,   // bits 0..=23
    doms: u64,    // bits 1..=31
    months: u64,  // bits 1..=12
    dows: u64,    // bits 0..=6 (Sunday = 0)
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    /// Parse a standard 5-field expression. Returns a human-readable error
    /// string on anything malformed (surfaced by config validation / doctor).
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), got {}",
                fields.len()
            ));
        }
        Ok(Cron {
            minutes: parse_field(fields[0], 0, 59)?,
            hours: parse_field(fields[1], 0, 23)?,
            doms: parse_field(fields[2], 1, 31)?,
            months: parse_field(fields[3], 1, 12)?,
            dows: parse_dow(fields[4])?,
            dom_restricted: fields[2] != "*",
            dow_restricted: fields[4] != "*",
        })
    }

    fn date_matches(&self, c: &Civil) -> bool {
        if self.months & (1u64 << c.month) == 0 {
            return false;
        }
        let dom_ok = self.doms & (1u64 << c.day) != 0;
        let dow_ok = self.dows & (1u64 << c.weekday) != 0;
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom_ok || dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }

    fn time_matches(&self, c: &Civil) -> bool {
        self.hours & (1u64 << c.hour) != 0 && self.minutes & (1u64 << c.minute) != 0
    }

    /// Whether the minute containing `secs` (epoch seconds, UTC) is a firing
    /// time.
    pub fn matches(&self, secs: u64) -> bool {
        let c = civil_from_epoch(secs);
        self.date_matches(&c) && self.time_matches(&c)
    }

    /// The smallest minute-aligned epoch strictly greater than `after` that
    /// fires, or `None` if none occurs within ~5 years (an impossible date
    /// like Feb 30, or simply nothing soon). This is the one primitive the
    /// scheduler needs: an occurrence falls in the window `(lo, now]` iff
    /// `next_after(lo) <= now`.
    pub fn next_after(&self, after: u64) -> Option<u64> {
        // Start at the next whole-minute boundary strictly after `after`.
        let mut t = (after / 60 + 1) * 60;
        // Bound the search so an impossible expression terminates: 5 years of
        // seconds comfortably covers any recurring standard cron (the rarest,
        // Feb 29, is < 4 years apart).
        let limit = t.saturating_add(5 * 366 * 24 * 3600);
        while t <= limit {
            let c = civil_from_epoch(t);
            if !self.date_matches(&c) {
                // Prune a whole day at a time so a rare schedule doesn't cost a
                // minute-by-minute scan over years.
                t = (t / 86_400 + 1) * 86_400;
                continue;
            }
            if self.time_matches(&c) {
                return Some(t);
            }
            t += 60;
        }
        None
    }
}

/// Parse one field into a bitmask over `min..=max`.
fn parse_field(field: &str, min: u32, max: u32) -> Result<u64, String> {
    if field.is_empty() {
        return Err("empty field".into());
    }
    let mut mask = 0u64;
    for item in field.split(',') {
        mask |= parse_item(item, min, max)?;
    }
    Ok(mask)
}

/// Parse one comma element: `*`, `a`, `a-b`, `*/n`, `a-b/n`, or `a/n`.
fn parse_item(item: &str, min: u32, max: u32) -> Result<u64, String> {
    let (range_part, step) = match item.split_once('/') {
        Some((r, s)) => {
            let step: u32 = s.parse().map_err(|_| format!("invalid step in {item:?}"))?;
            if step == 0 {
                return Err(format!("step cannot be 0 in {item:?}"));
            }
            (r, step)
        }
        None => (item, 1),
    };
    let has_step = step != 1 || item.contains('/');
    let (lo, hi) = if range_part == "*" {
        (min, max)
    } else if let Some((a, b)) = range_part.split_once('-') {
        (
            a.parse::<u32>()
                .map_err(|_| format!("invalid range start in {item:?}"))?,
            b.parse::<u32>()
                .map_err(|_| format!("invalid range end in {item:?}"))?,
        )
    } else {
        let v: u32 = range_part
            .parse()
            .map_err(|_| format!("invalid value {item:?}"))?;
        // `a/n` is Vixie shorthand for `a-max/n`; a bare `a` is just `a`.
        if has_step { (v, max) } else { (v, v) }
    };
    if lo < min || hi > max || lo > hi {
        return Err(format!("value out of range {min}-{max}: {item:?}"));
    }
    let mut mask = 0u64;
    let mut v = lo;
    while v <= hi {
        mask |= 1u64 << v;
        v += step;
    }
    Ok(mask)
}

/// Day-of-week is parsed over `0..=7` and then folded so Sunday (`7`) shares a
/// bit with `0`.
fn parse_dow(field: &str) -> Result<u64, String> {
    let raw = parse_field(field, 0, 7)?;
    let mut mask = raw & 0b0111_1111; // bits 0..=6
    if raw & (1u64 << 7) != 0 {
        mask |= 1u64 << 0; // fold Sunday-as-7 onto Sunday-as-0
    }
    Ok(mask)
}

/// Civil calendar components of an instant, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,   // 1..=12
    pub day: u32,     // 1..=31
    pub hour: u32,    // 0..=23
    pub minute: u32,  // 0..=59
    pub weekday: u32, // 0=Sunday .. 6=Saturday
}

/// Decompose epoch seconds into UTC civil components (Howard-Hinnant
/// days-from-civil, the inverse of `store::parse_ts`).
pub fn civil_from_epoch(secs: u64) -> Civil {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };
    // 1970-01-01 (day 0) was a Thursday; weekday 0 = Sunday.
    let weekday = ((days.rem_euclid(7) + 4) % 7) as u32;
    Civil {
        year,
        month,
        day,
        hour,
        minute,
        weekday,
    }
}

/// `YYYY-MM-DD` (UTC) for the `{{date}}` title template variable.
pub fn date_utc(secs: u64) -> String {
    let c = civil_from_epoch(secs);
    format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::parse_ts;

    fn ts(s: &str) -> u64 {
        parse_ts(s).unwrap_or_else(|| panic!("bad ts {s}"))
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(Cron::parse("* * * *").is_err());
        assert!(Cron::parse("* * * * * *").is_err());
        assert!(Cron::parse("").is_err());
    }

    #[test]
    fn rejects_out_of_range_and_bad_step() {
        assert!(Cron::parse("60 * * * *").is_err()); // minute max 59
        assert!(Cron::parse("* 24 * * *").is_err()); // hour max 23
        assert!(Cron::parse("* * 0 * *").is_err()); // dom min 1
        assert!(Cron::parse("* * * 13 *").is_err()); // month max 12
        assert!(Cron::parse("*/0 * * * *").is_err()); // step 0
        assert!(Cron::parse("5-1 * * * *").is_err()); // inverted range
    }

    #[test]
    fn civil_anchors() {
        // Day 0 is Thursday (weekday 4).
        assert_eq!(civil_from_epoch(0).weekday, 4);
        // 2000-01-01 was a Saturday (weekday 6).
        assert_eq!(civil_from_epoch(ts("2000-01-01T00:00:00Z")).weekday, 6);
        let c = civil_from_epoch(ts("2026-07-13T09:34:00Z"));
        assert_eq!(
            (c.year, c.month, c.day, c.hour, c.minute),
            (2026, 7, 13, 9, 34)
        );
    }

    #[test]
    fn daily_at_nine() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        assert!(cron.matches(ts("2026-07-13T09:00:00Z")));
        assert!(!cron.matches(ts("2026-07-13T09:01:00Z")));
        assert!(!cron.matches(ts("2026-07-13T10:00:00Z")));
        // next occurrence after 09:05 is tomorrow 09:00.
        assert_eq!(
            cron.next_after(ts("2026-07-13T09:05:00Z")),
            Some(ts("2026-07-14T09:00:00Z"))
        );
        // next occurrence after 08:00 is today 09:00.
        assert_eq!(
            cron.next_after(ts("2026-07-13T08:00:00Z")),
            Some(ts("2026-07-13T09:00:00Z"))
        );
    }

    #[test]
    fn every_fifteen_minutes() {
        let cron = Cron::parse("*/15 * * * *").unwrap();
        for m in ["00", "15", "30", "45"] {
            assert!(cron.matches(ts(&format!("2026-07-13T10:{m}:00Z"))));
        }
        assert!(!cron.matches(ts("2026-07-13T10:07:00Z")));
        assert_eq!(
            cron.next_after(ts("2026-07-13T10:16:00Z")),
            Some(ts("2026-07-13T10:30:00Z"))
        );
    }

    #[test]
    fn lists_and_ranges() {
        let cron = Cron::parse("0 9,17 * * 1-5").unwrap();
        // Monday 2026-07-13 at 09:00 and 17:00.
        assert!(cron.matches(ts("2026-07-13T09:00:00Z")));
        assert!(cron.matches(ts("2026-07-13T17:00:00Z")));
        assert!(!cron.matches(ts("2026-07-13T12:00:00Z")));
        // Saturday 2026-07-18 is outside 1-5 (Mon-Fri).
        assert!(!cron.matches(ts("2026-07-18T09:00:00Z")));
    }

    #[test]
    fn dow_zero_and_seven_are_sunday() {
        let sun0 = Cron::parse("0 0 * * 0").unwrap();
        let sun7 = Cron::parse("0 0 * * 7").unwrap();
        // 2026-07-19 is a Sunday.
        assert!(sun0.matches(ts("2026-07-19T00:00:00Z")));
        assert!(sun7.matches(ts("2026-07-19T00:00:00Z")));
        assert!(!sun0.matches(ts("2026-07-13T00:00:00Z"))); // Monday
    }

    #[test]
    fn dom_and_dow_both_restricted_is_or() {
        // Vixie rule: "1st of the month OR any Monday" at 00:00.
        let cron = Cron::parse("0 0 1 * 1").unwrap();
        assert!(cron.matches(ts("2026-07-01T00:00:00Z"))); // the 1st (a Wednesday)
        assert!(cron.matches(ts("2026-07-13T00:00:00Z"))); // a Monday
        assert!(!cron.matches(ts("2026-07-14T00:00:00Z"))); // Tue, not the 1st
    }

    #[test]
    fn monthly_next_after_spans_month() {
        let cron = Cron::parse("0 0 1 * *").unwrap();
        assert_eq!(
            cron.next_after(ts("2026-07-13T00:00:00Z")),
            Some(ts("2026-08-01T00:00:00Z"))
        );
    }

    #[test]
    fn date_template() {
        assert_eq!(date_utc(ts("2026-07-13T09:00:00Z")), "2026-07-13");
    }
}
