//! Short human-readable ages, view counts and durations.

use crate::db;

pub fn age(ts: i64) -> String {
    let d = (db::now() - ts).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86400 => (d / 3600, "hour"),
        86400..604800 => (d / 86400, "day"),
        604800..2629800 => (d / 604800, "week"),
        2629800..31557600 => (d / 2629800, "month"),
        _ => (d / 31557600, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub fn views(n: i64) -> String {
    let s = match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}B", n as f64 / 1e9),
    };
    format!("{} views", s.replace(".0", ""))
}

pub fn clock(secs: f64) -> String {
    let s = secs as i64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(clock(65.0), "1:05");
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(views(999), "999 views");
        assert_eq!(views(12_000), "12K views");
        assert_eq!(views(1_250_000), "1.2M views");
        assert_eq!(age(db::now() - 3 * 86400), "3 days ago");
        assert_eq!(age(db::now() - 3600), "1 hour ago");
    }
}
