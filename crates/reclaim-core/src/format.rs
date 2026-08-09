//! Human formatting and parsing of sizes and durations.
//!
//! Sizes use binary units (KiB/MiB/GiB) but the shorter `KB`/`MB`/`GB` spelling,
//! matching what `du -h` and Finder show so the numbers are comparable to what
//! users already see.

use std::time::Duration;

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// Format a byte count for display, e.g. `1.2 GB`.
pub fn bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Fixed-width byte count for aligned table columns, e.g. `  1.2 GB`.
pub fn bytes_padded(n: u64, width: usize) -> String {
    format!("{:>width$}", bytes(n), width = width)
}

/// Parse a human size such as `50MB`, `1.5 GiB`, `1024`, or `2g`.
///
/// Bare numbers are bytes. Both `MB` and `MiB` mean 1024-based, deliberately:
/// a config file saying `min_size = "50MB"` should mean the same thing the
/// display does, and the 4.8% difference is never what the user cares about.
pub fn parse_bytes(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty size".to_string());
    }

    let split = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split);
    let number: f64 = number
        .replace('_', "")
        .parse()
        .map_err(|_| format!("`{input}` is not a valid size"))?;
    if number < 0.0 {
        return Err(format!("`{input}` is negative"));
    }

    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1u64 << 40,
        "p" | "pb" | "pib" => 1u64 << 50,
        other => return Err(format!("unknown size unit `{other}` in `{input}`")),
    };

    Ok((number * multiplier as f64) as u64)
}

/// Parse a duration expressed in days, such as `30d`, `6w`, `3mo`, `1y`, or `45`.
///
/// Bare numbers are days, which is what every threshold in this tool is measured in.
pub fn parse_days(input: &str) -> Result<u32, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty duration".to_string());
    }

    let split = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("`{input}` is not a valid duration"))?;

    // `m` is ambiguous between minutes and months; this tool has no sub-day
    // resolution, so `m` means months and minutes are rejected outright.
    let days = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "d" | "day" | "days" => number,
        "w" | "week" | "weeks" => number * 7.0,
        "m" | "mo" | "month" | "months" => number * 30.0,
        "y" | "year" | "years" => number * 365.0,
        "h" | "hour" | "hours" | "min" | "minute" | "minutes" => {
            return Err(format!(
                "`{input}`: thresholds are measured in days, not hours"
            ))
        }
        other => return Err(format!("unknown duration unit `{other}` in `{input}`")),
    };

    if days < 0.0 {
        return Err(format!("`{input}` is negative"));
    }
    Ok(days.round() as u32)
}

/// Compact duration for progress readouts, e.g. `1.4s`, `2m 05s`.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Truncate a string to `max` display columns, ellipsising the middle.
///
/// Middle-ellipsis rather than tail, because paths are far more identifiable by
/// their last component than their first.
pub fn ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    // Weight the tail at two thirds: `/Users/x/…/app/node_modules` identifies the
    // directory, whereas an even split would leave `/Users/x/…de_modules`.
    let keep = max - 1;
    let head = keep / 3;
    let tail = keep - head;
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_str}…{tail_str}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(0, "0 B")]
    #[case(512, "512 B")]
    #[case(1024, "1.00 KB")]
    #[case(1536, "1.50 KB")]
    #[case(10 * 1024, "10.0 KB")]
    #[case(100 * 1024, "100 KB")]
    #[case(1024 * 1024, "1.00 MB")]
    #[case(3 * 1024 * 1024 * 1024, "3.00 GB")]
    fn bytes_formats_with_shrinking_precision(#[case] input: u64, #[case] expected: &str) {
        assert_eq!(bytes(input), expected);
    }

    #[rstest]
    #[case("1024", 1024)]
    #[case("50MB", 50 * 1024 * 1024)]
    #[case("50 mb", 50 * 1024 * 1024)]
    #[case("1.5GiB", 1_610_612_736)]
    #[case("2g", 2 * 1024 * 1024 * 1024)]
    #[case("100", 100)]
    fn parse_bytes_accepts_common_spellings(#[case] input: &str, #[case] expected: u64) {
        assert_eq!(parse_bytes(input).unwrap(), expected);
    }

    #[rstest]
    #[case("")]
    #[case("abc")]
    #[case("10 furlongs")]
    #[case("-5MB")]
    fn parse_bytes_rejects_junk(#[case] input: &str) {
        assert!(parse_bytes(input).is_err(), "`{input}` should not parse");
    }

    #[rstest]
    #[case("30", 30)]
    #[case("30d", 30)]
    #[case("6w", 42)]
    #[case("3mo", 90)]
    #[case("1y", 365)]
    fn parse_days_accepts_common_spellings(#[case] input: &str, #[case] expected: u32) {
        assert_eq!(parse_days(input).unwrap(), expected);
    }

    #[test]
    fn parse_days_rejects_sub_day_units_rather_than_silently_rounding() {
        let err = parse_days("6h").unwrap_err();
        assert!(err.contains("days"), "error should explain the unit: {err}");
    }

    #[test]
    fn roundtrip_of_parse_and_format_is_stable() {
        for raw in ["1KB", "50MB", "2GB"] {
            let parsed = parse_bytes(raw).unwrap();
            let formatted = bytes(parsed);
            assert_eq!(parse_bytes(&formatted).unwrap(), parsed);
        }
    }

    #[test]
    fn ellipsize_keeps_the_identifying_tail() {
        assert_eq!(ellipsize("short", 10), "short");
        let out = ellipsize("/Users/x/projects/my-app/node_modules", 20);
        assert_eq!(out.chars().count(), 20);
        assert!(
            out.ends_with("node_modules"),
            "tail is the identifying part: {out}"
        );
    }

    #[test]
    fn duration_scales_units() {
        assert_eq!(duration(Duration::from_millis(1400)), "1.4s");
        assert_eq!(duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(duration(Duration::from_secs(3700)), "1h 01m");
    }
}
