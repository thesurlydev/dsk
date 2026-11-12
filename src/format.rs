//! Size, count, and duration formatting helpers.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Compact binary size, e.g. `269.3G`, `953.0M`, `512B`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

/// Thousands-separated count, e.g. `1,176,353`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Percentage of `total`, e.g. `56.5%`.
pub fn percent(part: u64, total: u64) -> String {
    if total == 0 {
        return "0.0%".to_string();
    }
    let value = part as f64 * 100.0 / total as f64;
    if part > 0 && value < 0.05 {
        return "<0.1%".to_string();
    }
    format!("{value:.1}%")
}

/// A count with a singular or plural noun, e.g. `1 dir`, `40 dirs`.
pub fn plural(n: u64, singular: &str, plural: &str) -> String {
    format!("{} {}", count(n), if n == 1 { singular } else { plural })
}

pub fn duration(d: Duration) -> String {
    if d.as_secs() >= 60 {
        format!("{:.1}m", d.as_secs_f64() / 60.0)
    } else if d.as_secs() >= 1 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Seconds since the Unix epoch, negative before it.
pub fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// How long before `now` the Unix time `then` was, in at most four characters:
/// `now`, `5m`, `3h`, `45d`, `14mo`, `3y`. Months and years are 30 and 365
/// days. A time in the future, from a skewed clock, reads as `now`.
pub fn age(now: i64, then: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let secs = now.saturating_sub(then).max(0);
    let days = secs / DAY;
    if secs < MINUTE {
        "now".to_string()
    } else if secs < HOUR {
        format!("{}m", secs / MINUTE)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else if days < 60 {
        format!("{days}d")
    } else if days < 2 * 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// The UTC calendar date of a Unix time, e.g. `2024-03-09`.
pub fn date(secs: i64) -> String {
    // Days since 1970-01-01 to a proleptic Gregorian date, after Howard
    // Hinnant's `civil_from_days`.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Right-align `text` into `width` columns.
pub fn right(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_string()
    } else {
        format!("{}{}", " ".repeat(width - len), text)
    }
}

/// Truncate from the left, keeping the tail of a long path visible.
pub fn truncate_left(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return chars[chars.len() - width..].iter().collect();
    }
    let keep = width - 3;
    let mut out = String::from("...");
    out.extend(chars[chars.len() - keep..].iter());
    out
}

/// Truncate from the right, keeping the start of a long path visible.
pub fn truncate_right(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return chars[..width].iter().collect();
    }
    let mut out: String = chars[..width - 3].iter().collect();
    out.push_str("...");
    out
}

/// A path as the user would type it: relative to the current directory when
/// possible.
pub fn display_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = path.strip_prefix(&cwd)
    {
        if rel.as_os_str().is_empty() {
            return ".".to_string();
        }
        return rel.display().to_string();
    }
    path.display().to_string()
}

/// A path quoted for safe copy-paste into a POSIX shell.
pub fn shell_path(path: &Path) -> String {
    let text = display_path(path);
    let plain = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./~+:@%,".contains(c));
    if plain {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Parse a human size such as `1024`, `512K`, `10MB`, `1.5GiB`.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty size".to_string());
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (num, suffix) = text.split_at(split);
    let value: f64 = num.parse().map_err(|_| format!("invalid size {text:?}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid size {text:?}"));
    }
    let factor = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size suffix {other:?}")),
    };
    Ok((value * factor) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_formats_binary_units() {
        assert_eq!(bytes(0), "0B");
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(1024), "1.0K");
        assert_eq!(bytes(1024 * 1024), "1.0M");
        assert_eq!(bytes(269 * 1024 * 1024 * 1024), "269.0G");
    }

    #[test]
    fn count_groups_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1176353), "1,176,353");
    }

    #[test]
    fn percent_handles_zero_total() {
        assert_eq!(percent(0, 0), "0.0%");
        assert_eq!(percent(50, 100), "50.0%");
        assert_eq!(percent(1, 1_000_000), "<0.1%");
    }

    #[test]
    fn plural_picks_the_right_noun() {
        assert_eq!(plural(1, "dir", "dirs"), "1 dir");
        assert_eq!(plural(2, "dir", "dirs"), "2 dirs");
    }

    #[test]
    fn age_picks_the_coarsest_readable_unit() {
        let now = 1_700_000_000;
        let ago = |secs: i64| age(now, now - secs);
        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now");
        assert_eq!(ago(300), "5m");
        assert_eq!(ago(3 * 3600), "3h");
        assert_eq!(ago(45 * 86_400), "45d");
        assert_eq!(ago(60 * 86_400), "2mo");
        assert_eq!(ago(14 * 30 * 86_400), "14mo");
        assert_eq!(ago(2 * 365 * 86_400), "2y");
        assert_eq!(ago(10 * 365 * 86_400), "10y");
        assert_eq!(ago(-5000), "now", "the future reads as now");
    }

    #[test]
    fn date_is_the_utc_calendar_day() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(86_399), "1970-01-01");
        assert_eq!(date(86_400), "1970-01-02");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_700_000_000), "2023-11-14");
        assert_eq!(date(-86_400), "1969-12-31");
    }

    #[test]
    fn shell_path_quotes_when_needed() {
        assert_eq!(shell_path(Path::new("plain/path")), "plain/path");
        assert_eq!(shell_path(Path::new("has space/x")), "'has space/x'");
        assert_eq!(shell_path(Path::new("it's/x")), "'it'\\''s/x'");
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1.5G").unwrap(), 1610612736);
        assert!(parse_size("nope").is_err());
    }

    #[test]
    fn truncation_keeps_requested_side() {
        assert_eq!(truncate_left("abcdefghij", 6), "...hij");
        assert_eq!(truncate_right("abcdefghij", 6), "abc...");
        assert_eq!(truncate_left("abc", 10), "abc");
    }
}
