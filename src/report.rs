//! Error codes and bug reports.
//!
//! Every problem YourTube shows you has a short code (like `YT-NET-01`), so a
//! report says exactly what went wrong. Press Ctrl+Shift+B (or use the menu)
//! and YourTube takes a screenshot, writes down what happened, and opens a
//! ready-to-send GitHub issue. Nothing private goes in: no history, searches,
//! API keys or passwords — and you see everything before sending.

use std::collections::VecDeque;
use std::sync::Mutex;

pub const REPO: &str = "https://github.com/Alyosha-dot-shop/yourtube";

/// What kind of problem it was. The code is what people quote in issues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// Couldn't reach YouTube (offline, blocked, proxy down).
    Network,
    /// YouTube answered, but its page looked different than expected (YouTube changed something).
    YouTubePage,
    /// mpv didn't start, or stopped unexpectedly.
    Player,
    /// A download failed.
    Download,
    /// Reading or writing your data failed.
    Database,
    /// Import or export failed.
    ImportExport,
    /// The AI service didn't answer or answered nonsense.
    Ai,
    /// The external recommender didn't answer.
    Recommender,
    /// SponsorBlock, DeArrow or Return YouTube Dislike didn't answer.
    #[allow(dead_code)] // these fail quietly today; kept so reports stay stable when they don't
    Extras,
    /// Anything else.
    Other,
}

impl Code {
    pub fn id(self) -> &'static str {
        match self {
            Code::Network => "YT-NET-01",
            Code::YouTubePage => "YT-PAGE-02",
            Code::Player => "YT-MPV-03",
            Code::Download => "YT-DL-04",
            Code::Database => "YT-DB-05",
            Code::ImportExport => "YT-IO-06",
            Code::Ai => "YT-AI-07",
            Code::Recommender => "YT-REC-08",
            Code::Extras => "YT-EXT-09",
            Code::Other => "YT-ERR-99",
        }
    }

    /// Guess the code from an error message, for places that only have text.
    pub fn guess(msg: &str) -> Code {
        let m = msg.to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| m.contains(w));
        if has(&["mpv"]) {
            Code::Player
        } else if has(&["yt-dlp", "download"]) {
            Code::Download
        } else if has(&["ytinitialdata", "no videos on", "parse", "unexpected page"]) {
            Code::YouTubePage
        } else if has(&["dns", "connection", "timed out", "timeout", "network", "resolve", "tls", "proxy", "status code"]) {
            Code::Network
        } else if has(&["sqlite", "database", "disk"]) {
            Code::Database
        } else if has(&["import", "export", "takeout", "newpipe"]) {
            Code::ImportExport
        } else if has(&["ai ", "model", "chat/completions"]) {
            Code::Ai
        } else if has(&["recommender"]) {
            Code::Recommender
        } else {
            Code::Other
        }
    }
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub at: i64,
    pub code: Code,
    pub message: String,
}

static RECENT: Mutex<VecDeque<Problem>> = Mutex::new(VecDeque::new());
const KEEP: usize = 40;

/// Remember a problem for the next bug report (last 40 are kept, only in memory).
pub fn record(code: Code, message: &str) {
    let mut r = RECENT.lock().unwrap();
    if r.len() == KEEP {
        r.pop_front();
    }
    r.push_back(Problem { at: crate::db::now(), code, message: scrub(message) });
}

pub fn recent() -> Vec<Problem> {
    RECENT.lock().unwrap().iter().cloned().collect()
}

/// Remove things that shouldn't leave your computer: your home folder, the secrets in your
/// settings, keys and tokens, logins inside addresses, and YouTube's per-session video links.
pub fn scrub(s: &str) -> String {
    let c = crate::config::load();
    let secrets: Vec<String> = [Some(c.ai_api_key.clone()), Some(c.web_password.clone()), c.proxy.as_deref().and_then(url_login)]
        .into_iter()
        .flatten()
        .filter(|s| s.len() >= 4)
        .collect();
    scrub_with(s, &secrets)
}

/// The `user:pass` in `scheme://user:pass@host`.
fn url_login(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let at = rest.find('@')?;
    (!rest[..at].contains('/')).then(|| rest[..at].to_owned())
}

fn scrub_with(s: &str, secrets: &[String]) -> String {
    let mut out = s.to_owned();
    for secret in secrets {
        out = out.replace(secret.as_str(), "…");
    }
    if let Some(home) = dirs::home_dir().and_then(|h| h.to_str().map(str::to_owned)) {
        if !home.is_empty() {
            out = out.replace(&home, "~");
        }
    }
    // Words after a marker, up to the next space/&/quote: tokens, keys, signatures.
    let cut = |out: &mut String, marker: &str, keep_marker: bool, is_end: fn(char) -> bool| {
        let mut from = 0;
        while let Some(i) = out[from..].find(marker) {
            let start = from + i + if keep_marker { marker.len() } else { 0 };
            let end = out[from + i + marker.len()..].find(is_end).map_or(out.len(), |e| from + i + marker.len() + e);
            let with = if keep_marker { "…".to_owned() } else { format!("{}…", &marker[..marker.len().min(3)]) };
            out.replace_range(start..end, &with);
            from = start + with.len();
        }
    };
    let word_end: fn(char) -> bool = |c| c == '&' || c == '"' || c == '\'' || c.is_whitespace();
    for key in ["key=", "token=", "password=", "secret=", "auth=", "sig=", "Bearer "] {
        cut(&mut out, key, true, word_end);
    }
    cut(&mut out, "sk-", false, |c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    // A login inside an address: scheme://user:pass@host.
    let mut from = 0;
    while let Some(i) = out[from..].find("://") {
        let start = from + i + 3;
        let host_end = out[start..].find(|c: char| c == '/' || c.is_whitespace()).map_or(out.len(), |e| start + e);
        match out[start..host_end].rfind('@') {
            Some(at) => {
                out.replace_range(start..start + at, "…");
                from = start + "…".len() + 1;
            }
            None => from = start,
        }
    }
    // YouTube's video links name the session and the address that asked for them.
    let mut from = 0;
    while let Some(i) = out[from..].find(".googlevideo.com/") {
        let begin = out[..from + i].rfind(|c: char| c.is_whitespace() || c == '"' || c == '(').map_or(0, |b| b + 1);
        let end = out[from + i..].find(|c: char| c.is_whitespace() || c == '"' || c == ')').map_or(out.len(), |e| from + i + e);
        out.replace_range(begin..end, "[YouTube video link]");
        from = begin + "[YouTube video link]".len();
    }
    out
}

fn version_of(cmd: &str, arg: &str) -> String {
    std::process::Command::new(cmd)
        .arg(arg)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(str::to_owned))
        .unwrap_or_else(|| "not found".into())
}

fn os_name() -> String {
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_owned())));
    format!("{} ({} {})", pretty.unwrap_or_else(|| std::env::consts::OS.to_owned()), std::env::consts::OS, std::env::consts::ARCH)
}

/// Everything a bug report contains, as Markdown. `what` is the person's own words.
pub fn build(what: &str, page: &str, toolkit: &str, screenshot: Option<&str>) -> (String, String) {
    let problems = recent();
    let code = problems.last().map_or("YT-ERR-99", |p| p.code.id());
    let title = match problems.last() {
        Some(p) => format!("[{}] {}", p.code.id(), p.message.chars().take(70).collect::<String>()),
        None => format!("[{code}] "),
    };
    let mut b = String::new();
    b += "### What happened\n\n";
    b += if what.trim().is_empty() { "_(please describe what you did and what you expected)_" } else { what.trim() };
    b += "\n\n### Recent problems\n\n";
    if problems.is_empty() {
        b += "None recorded.\n";
    }
    for p in problems.iter().rev().take(10) {
        b += &format!("- `{}` {} — {}\n", p.code.id(), crate::ui::fmt::age(p.at), p.message);
    }
    b += "\n### Where\n\n";
    b += &format!(
        "- Page: {page}\n- YourTube {} ({})\n- {}\n- {toolkit}\n",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) { "debug build" } else { "release build" },
        os_name()
    );
    b += &format!("- mpv: {}\n- yt-dlp: {}\n", version_of("mpv", "--version"), version_of("yt-dlp", "--version"));
    let c = crate::config::load();
    b += &format!(
        "- Settings: For You {}, AI {}, SponsorBlock {}, proxy {}, incognito {}\n",
        if c.recommender.is_some() { "external" } else { "built in" },
        if c.ai().is_some() {
            if c.ai_url.contains("127.0.0.1") || c.ai_url.contains("localhost") { "local" } else { "cloud" }
        } else {
            "off"
        },
        if c.sponsorblock { "on" } else { "off" },
        if c.proxy.is_some() { "on" } else { "off" },
        if crate::db::incognito() { "on" } else { "off" },
    );
    if let Some(shot) = screenshot {
        b += &format!("\n### Screenshot\n\n_Saved at `{}` — drag it into this issue._\n", scrub(shot));
    }
    let log = dirs::state_dir()
        .map(|d| d.join("yourtube").join("player.log"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        // "<time> <video id> exited …": the id says what you watched, so it's left out.
        .map(|s| {
            let last: Vec<&str> = s.lines().rev().take(5).collect();
            last.into_iter().rev().map(|l| l.splitn(3, ' ').nth(2).unwrap_or("")).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default();
    if !log.is_empty() {
        b += &format!("\n### Last mpv exits\n\n```\n{}\n```\n", scrub(&log));
    }
    (title, b)
}

/// A "new issue" link with title and body filled in (GitHub limits URL length, so the body is cut if needed).
pub fn issue_url(title: &str, body: &str) -> String {
    let mut body = body.to_owned();
    while enc(&body).len() > 6500 {
        let cut = body.char_indices().nth(body.chars().count() * 9 / 10).map_or(0, |(i, _)| i);
        body.truncate(cut);
        body += "\n…(cut; the full report is on your clipboard)";
    }
    format!("{REPO}/issues/new?title={}&body={}&labels=bug", enc(title), enc(&body))
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_scrubbing() {
        assert_eq!(Code::guess("could not start mpv: No such file").id(), "YT-MPV-03");
        assert_eq!(Code::guess("Connection refused (os error 111)").id(), "YT-NET-01");
        assert_eq!(Code::guess("no ytInitialData on the page").id(), "YT-PAGE-02");
        let s = scrub("GET https://x/api?key=SECRET123&v=1 and token=abc");
        assert!(!s.contains("SECRET123") && !s.contains("abc"), "{s}");
        if let Some(h) = dirs::home_dir() {
            assert_eq!(scrub(&format!("{}/x", h.display())), "~/x");
        }
    }

    #[test]
    fn scrubs_keys_logins_secrets_and_video_links() {
        let s = scrub_with(
            "Authorization: Bearer abc.def-123 key sk-proj-AbC_123xyz proxy socks5h://me:hunter2@127.0.0.1:9050 \
             url https://rr3---sn-abc.googlevideo.com/videoplayback?expire=1&ip=203.0.113.9 my-own-secret-value done",
            &["my-own-secret-value".into()],
        );
        for gone in ["abc.def-123", "AbC_123xyz", "hunter2", "me:", "203.0.113.9", "sn-abc", "my-own-secret-value"] {
            assert!(!s.contains(gone), "{gone} survived: {s}");
        }
        assert!(
            s.contains("Bearer …") && s.contains("sk-…") && s.contains("socks5h://…@127.0.0.1:9050") && s.contains("[YouTube video link]"),
            "{s}"
        );
        assert!(s.ends_with("… done"), "{s}");
        assert_eq!(url_login("socks5h://u:p@host:1"), Some("u:p".into()));
        assert_eq!(url_login("http://host/a@b"), None);
    }

    #[test]
    fn report_has_no_secrets_and_url_is_bounded() {
        record(Code::Network, "Connection refused");
        let (title, body) = build("It froze", "For You", "GTK 4", None);
        assert!(title.starts_with("[YT-NET-01]"), "{title}");
        assert!(body.contains("It froze") && body.contains("YT-NET-01"));
        let url = issue_url(&title, &"x".repeat(20_000));
        assert!(url.len() < 7000 && url.starts_with(REPO));
    }
}
