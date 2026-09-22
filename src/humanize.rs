//! Human-readable ages for millisecond timestamps (Phase 4: execution memory).

/// Milliseconds since the Unix epoch, wall-clock now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// "3 days ago", "2 weeks ago", "8 months ago" — relative to `now_ms()`.
/// `then_ms <= 0` or in the future returns `None` rather than a nonsensical
/// label — silence over a confidently wrong guess.
pub fn age_label(then_ms: i64) -> Option<String> {
    age_label_at(then_ms, now_ms())
}

fn age_label_at(then_ms: i64, now_ms: i64) -> Option<String> {
    if then_ms <= 0 || then_ms > now_ms {
        return None;
    }
    let delta_ms = now_ms - then_ms;
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    Some(if delta_ms < MINUTE {
        "just now".to_string()
    } else if delta_ms < HOUR {
        plural(delta_ms / MINUTE, "minute")
    } else if delta_ms < DAY {
        plural(delta_ms / HOUR, "hour")
    } else if delta_ms < 2 * DAY {
        "yesterday".to_string()
    } else if delta_ms < WEEK {
        plural(delta_ms / DAY, "day")
    } else if delta_ms < MONTH {
        plural(delta_ms / WEEK, "week")
    } else if delta_ms < YEAR {
        plural(delta_ms / MONTH, "month")
    } else {
        plural(delta_ms / YEAR, "year")
    })
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const NOW: i64 = 2_000_000_000_000;

    #[test]
    fn zero_or_negative_is_never_recorded_not_a_label() {
        assert_eq!(age_label_at(0, NOW), None);
        assert_eq!(age_label_at(-5, NOW), None);
    }

    #[test]
    fn future_timestamp_is_silence_not_a_negative_age() {
        assert_eq!(age_label_at(NOW + 1_000_000, NOW), None);
    }

    #[test]
    fn just_now_and_minutes() {
        assert_eq!(age_label_at(NOW - 1_000, NOW), Some("just now".into()));
        assert_eq!(
            age_label_at(NOW - 5 * MINUTE, NOW),
            Some("5 minutes ago".into())
        );
        assert_eq!(age_label_at(NOW - MINUTE, NOW), Some("1 minute ago".into()));
    }

    #[test]
    fn hours_yesterday_and_days() {
        assert_eq!(
            age_label_at(NOW - 3 * HOUR, NOW),
            Some("3 hours ago".into())
        );
        assert_eq!(
            age_label_at(NOW - DAY - HOUR, NOW),
            Some("yesterday".into())
        );
        assert_eq!(age_label_at(NOW - 5 * DAY, NOW), Some("5 days ago".into()));
    }

    #[test]
    fn weeks_months_years() {
        assert_eq!(
            age_label_at(NOW - 14 * DAY, NOW),
            Some("2 weeks ago".into())
        );
        assert_eq!(
            age_label_at(NOW - 90 * DAY, NOW),
            Some("3 months ago".into())
        );
        assert_eq!(
            age_label_at(NOW - 400 * DAY, NOW),
            Some("1 year ago".into())
        );
    }

    #[test]
    fn real_now_ms_is_plausibly_current() {
        let ms = now_ms();
        assert!(
            ms > 1_700_000_000_000,
            "now_ms() looks like seconds, not milliseconds: {ms}"
        );
    }
}
