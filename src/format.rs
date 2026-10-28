//! Size, count, and duration formatting helpers.

use std::path::Path;
use std::time::Duration;

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
