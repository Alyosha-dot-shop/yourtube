//! Settings from `~/.config/yourtube/config` — plain `key = value` lines, `#` comments.
//! The Preferences window writes this file; editing it by hand works too.
//!
//! ```text
//! # Highest video quality mpv asks for.
//! max_height = 1080
//!
//! # Optional AI help for For You (off by default). Any OpenAI-style chat API:
//! ai = on
//! ai_url = http://127.0.0.1:11434/v1    # Ollama on this computer
//! ai_model = llama3.2
//! ai_api_key =                          # only for hosted services
//!
//! # Optional: take For You from an external recommender service instead (see foryou/remote.rs).
//! recommender = http://127.0.0.1:8890
//! ```

use crate::foryou::ai::AiConfig;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub max_height: u32,
    /// External recommender; when set, For You comes from it instead of the built-in one.
    pub recommender: Option<String>,
    pub ai_enabled: bool,
    pub ai_url: String,
    pub ai_model: String,
    pub ai_api_key: String,
    /// "system", "light" or "dark".
    pub color_scheme: String,
    /// A theme name from ui/theme.rs.
    pub theme: String,
    /// Accent colour as #rrggbb, overriding the theme's.
    pub accent: Option<String>,
    pub corner_radius: u32,
    /// "Small", "Medium" or "Large".
    pub card_size: String,
    pub animations: bool,
    /// The profile used last time.
    pub profile: Option<String>,
    /// Shortcuts you changed: id → key combinations (empty = turned off). See ui/shortcuts.rs.
    pub shortcuts: std::collections::BTreeMap<String, Vec<String>>,

    // Playback.
    /// Starting speed for mpv (1.0 = normal).
    pub speed: f64,
    /// Play the next video of a playlist or queue automatically.
    pub autoplay: bool,
    /// Subtitle languages mpv should prefer, e.g. "en,de" (empty = none unless you turn them on).
    pub subtitles: String,
    /// Proxy for YouTube pages, searches, feeds, thumbnails and the extras (e.g. Tor at
    /// socks5h://127.0.0.1:9050).
    pub proxy: Option<String>,
    /// Also send playback and downloads through `proxy`. Off by default: YouTube refuses
    /// most video streams over Tor ("confirm you're not a bot").
    pub proxy_playback: bool,
    /// For You matches topics with a small local model (downloaded once, ~30 MB).
    pub topic_model: bool,

    // SponsorBlock (sponsor.ajay.app): category → "skip", "ask" or "off".
    pub sponsorblock: bool,
    pub sponsorblock_api: String,
    pub sponsorblock_categories: std::collections::BTreeMap<String, String>,
    /// DeArrow: crowd-sourced, less clickbaity titles and thumbnails.
    pub dearrow: bool,
    /// Return YouTube Dislike counts on the video page.
    pub dislikes: bool,

    // Downloads.
    pub download_dir: Option<String>,
    pub download_height: u32,
    /// Cut SponsorBlock segments out of downloaded files.
    pub download_cut_sponsors: bool,

    // Privacy and distractions.
    pub remember_history: bool,
    pub remember_searches: bool,
    pub hide_views: bool,
    pub hide_live: bool,

    // Web version (`yourtube serve`).
    pub web_address: String,
    /// Password for the web version (empty = none; only sensible on a trusted home network).
    pub web_password: String,
}

/// SponsorBlock categories and what YourTube does with each by default.
pub const SPONSORBLOCK_DEFAULTS: &[(&str, &str, &str)] = &[
    ("sponsor", "Sponsor", "skip"),
    ("selfpromo", "Self-promotion", "skip"),
    ("interaction", "Like and subscribe reminder", "skip"),
    ("intro", "Intro", "ask"),
    ("outro", "Endcards and credits", "ask"),
    ("preview", "Preview or recap", "off"),
    ("filler", "Filler and tangents", "off"),
    ("music_offtopic", "Non-music part of a music video", "skip"),
];

impl Default for Config {
    fn default() -> Self {
        Config {
            max_height: 1080,
            recommender: None,
            ai_enabled: false,
            ai_url: String::new(),
            ai_model: String::new(),
            ai_api_key: String::new(),
            color_scheme: "system".into(),
            theme: "System".into(),
            accent: None,
            corner_radius: 10,
            card_size: "Medium".into(),
            animations: true,
            profile: None,
            shortcuts: Default::default(),
            speed: 1.0,
            autoplay: true,
            subtitles: String::new(),
            proxy: None,
            proxy_playback: false,
            topic_model: false,
            sponsorblock: true,
            sponsorblock_api: "https://sponsor.ajay.app".into(),
            sponsorblock_categories: SPONSORBLOCK_DEFAULTS.iter().map(|(k, _, v)| (k.to_string(), v.to_string())).collect(),
            dearrow: false,
            dislikes: false,
            download_dir: None,
            download_height: 1080,
            download_cut_sponsors: false,
            remember_history: true,
            remember_searches: true,
            hide_views: false,
            hide_live: false,
            web_address: "127.0.0.1:8723".into(),
            web_password: String::new(),
        }
    }
}

impl Config {
    /// The proxy for mpv, yt-dlp downloads and the web version's video relay, if playback should use it.
    /// With Tor, playback and downloads get circuits of their own, apart from searches and browsing;
    /// within a run, finding a video's stream and fetching it share one exit (YouTube's stream
    /// links only work from the address that asked for them).
    pub fn playback_proxy(&self) -> Option<String> {
        self.proxy.as_deref().filter(|_| self.proxy_playback).map(|p| crate::yt::isolated_proxy(p, "play"))
    }

    /// The AI settings, if AI is on and complete.
    pub fn ai(&self) -> Option<AiConfig> {
        (self.ai_enabled && is_http(&self.ai_url) && !self.ai_model.trim().is_empty()).then(|| AiConfig {
            url: self.ai_url.trim().to_owned(),
            model: self.ai_model.trim().to_owned(),
            api_key: Some(self.ai_api_key.trim().to_owned()).filter(|k| !k.is_empty()),
        })
    }
}

fn on(v: &str) -> bool {
    matches!(v, "on" | "true" | "yes" | "1")
}

fn is_http(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn path() -> PathBuf {
    dirs::config_dir().expect("no config dir").join("yourtube").join("config")
}

pub fn load() -> Config {
    std::fs::read_to_string(path()).map(|s| parse(&s)).unwrap_or_default()
}

pub fn parse(text: &str) -> Config {
    let mut c = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        // Comments after a value need a space before the #, so keys and URLs may contain '#'.
        let v = v.trim().split(" #").next().unwrap_or("").trim();
        match k.trim() {
            "max_height" => c.max_height = v.parse().unwrap_or(c.max_height),
            "recommender" if is_http(v) => c.recommender = Some(v.to_owned()),
            "ai" => c.ai_enabled = matches!(v, "on" | "true" | "yes" | "1"),
            "ai_url" if is_http(v) => c.ai_url = v.to_owned(),
            "ai_model" => c.ai_model = v.to_owned(),
            "ai_api_key" => c.ai_api_key = v.to_owned(),
            "color_scheme" if ["system", "light", "dark"].contains(&v) => c.color_scheme = v.to_owned(),
            "theme" if !v.is_empty() => c.theme = v.to_owned(),
            "accent" if v.len() == 7 && v.starts_with('#') => c.accent = Some(v.to_owned()),
            "corner_radius" => c.corner_radius = v.parse::<u32>().map(|r| r.min(32)).unwrap_or(c.corner_radius),
            "card_size" if ["Small", "Medium", "Large"].contains(&v) => c.card_size = v.to_owned(),
            "animations" => c.animations = !matches!(v, "off" | "false" | "no" | "0"),
            "profile" if !v.is_empty() => c.profile = Some(v.to_owned()),
            "speed" => c.speed = v.parse::<f64>().ok().filter(|s| (0.25..=4.0).contains(s)).unwrap_or(c.speed),
            "autoplay" => c.autoplay = on(v),
            "subtitles" => c.subtitles = v.to_owned(),
            "proxy" if v.contains("://") => c.proxy = Some(v.to_owned()),
            "proxy_playback" => c.proxy_playback = on(v),
            "topic_model" => c.topic_model = on(v),
            "sponsorblock" => c.sponsorblock = on(v),
            "sponsorblock_api" if is_http(v) => c.sponsorblock_api = v.trim_end_matches('/').to_owned(),
            k if k.starts_with("sponsorblock.") && ["skip", "ask", "off"].contains(&v) => {
                c.sponsorblock_categories.insert(k["sponsorblock.".len()..].to_owned(), v.to_owned());
            }
            "dearrow" => c.dearrow = on(v),
            "dislikes" => c.dislikes = on(v),
            "download_dir" if !v.is_empty() => c.download_dir = Some(v.to_owned()),
            "download_height" => c.download_height = v.parse().unwrap_or(c.download_height),
            "download_cut_sponsors" => c.download_cut_sponsors = on(v),
            "remember_history" => c.remember_history = on(v),
            "remember_searches" => c.remember_searches = on(v),
            "hide_views" => c.hide_views = on(v),
            "hide_live" => c.hide_live = on(v),
            "web_address" if v.contains(':') => c.web_address = v.to_owned(),
            "web_password" => c.web_password = v.to_owned(),
            k if k.starts_with("shortcut.") => {
                c.shortcuts.insert(k["shortcut.".len()..].to_owned(), v.split_whitespace().map(str::to_owned).collect());
            }
            _ => {}
        }
    }
    c
}

pub fn render(c: &Config) -> String {
    let mut s = String::from("# YourTube settings. Written by Preferences; hand edits are kept only for these keys.\n");
    s += &format!("max_height = {}\n", c.max_height);
    s += &format!(
        "\n# AI help for For You (sends recent searches and watched titles to ai_url).\nai = {}\n",
        if c.ai_enabled { "on" } else { "off" }
    );
    s += &format!("ai_url = {}\nai_model = {}\nai_api_key = {}\n", c.ai_url, c.ai_model, c.ai_api_key);
    s += &format!(
        "\n# Appearance. Your own CSS goes in style.css next to this file.\ncolor_scheme = {}\ntheme = {}\naccent = {}\ncorner_radius = {}\ncard_size = {}\nanimations = {}\n",
        c.color_scheme,
        c.theme,
        c.accent.as_deref().unwrap_or(""),
        c.corner_radius,
        c.card_size,
        if c.animations { "on" } else { "off" }
    );
    let yn = |b: bool| if b { "on" } else { "off" };
    s += &format!(
        "\n# Playback\nspeed = {}\nautoplay = {}\nsubtitles = {}\nproxy = {}\nproxy_playback = {}\n",
        c.speed,
        yn(c.autoplay),
        c.subtitles,
        c.proxy.as_deref().unwrap_or(""),
        yn(c.proxy_playback)
    );
    s += &format!(
        "\n# SponsorBlock: skip, ask or off per category\nsponsorblock = {}\nsponsorblock_api = {}\n",
        yn(c.sponsorblock),
        c.sponsorblock_api
    );
    for (k, v) in &c.sponsorblock_categories {
        s += &format!("sponsorblock.{k} = {v}\n");
    }
    s += &format!("dearrow = {}\ndislikes = {}\n", yn(c.dearrow), yn(c.dislikes));
    s += &format!(
        "\n# Downloads\ndownload_dir = {}\ndownload_height = {}\ndownload_cut_sponsors = {}\n",
        c.download_dir.as_deref().unwrap_or(""),
        c.download_height,
        yn(c.download_cut_sponsors)
    );
    s += &format!(
        "\n# Privacy and distractions\nremember_history = {}\nremember_searches = {}\nhide_views = {}\nhide_live = {}\n",
        yn(c.remember_history),
        yn(c.remember_searches),
        yn(c.hide_views),
        yn(c.hide_live)
    );
    s += &format!("\n# Web version (yourtube serve)\nweb_address = {}\nweb_password = {}\n", c.web_address, c.web_password);
    if !c.shortcuts.is_empty() {
        s += "\n# Shortcuts you changed (Preferences → Keyboard). Empty means off.\n";
        for (id, keys) in &c.shortcuts {
            s += &format!("shortcut.{id} = {}\n", keys.join(" "));
        }
    }
    if let Some(p) = &c.profile {
        s += &format!("\nprofile = {p}\n");
    }
    if let Some(r) = &c.recommender {
        s += &format!("\n# For You from an external recommender instead of the built-in one.\nrecommender = {r}\n");
    }
    s += &format!("\n# For You matches topics with a small local model (downloaded once, ~30 MB).\ntopic_model = {}\n", yn(c.topic_model));
    s
}

/// Is this a stored password hash (`$argon2id$…`) rather than the password itself?
pub fn is_password_hash(s: &str) -> bool {
    s.starts_with("$argon2")
}

/// An argon2id hash of `pw` with a random salt, as stored in the config file.
pub fn hash_password(pw: &str) -> Option<String> {
    use argon2::password_hash::PasswordHasher;
    argon2::Argon2::default().hash_password(pw.as_bytes()).ok().map(|h| h.to_string())
}

/// Does `given` match the stored web password (a hash, or a password typed into the file by hand)?
pub fn password_matches(stored: &str, given: &str) -> bool {
    use argon2::password_hash::{PasswordVerifier, phc::PasswordHash};
    if is_password_hash(stored) {
        return PasswordHash::new(stored).is_ok_and(|h| argon2::Argon2::default().verify_password(given.as_bytes(), &h).is_ok());
    }
    // Compare in constant time, without leaking the length either.
    let (a, b) = (sha2_of(stored), sha2_of(given));
    a.iter().zip(b.iter()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0 && !stored.is_empty()
}

fn sha2_of(s: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).into()
}

/// Write the config file, readable only by you (it may hold an API key). A web password typed
/// in is stored as a hash.
pub fn save(c: &Config) -> std::io::Result<()> {
    use std::io::Write;
    let mut c = c.clone();
    if !c.web_password.is_empty() && !is_password_hash(&c.web_password) {
        if let Some(h) = hash_password(&c.web_password) {
            c.web_password = h;
        }
    }
    let c = &c;
    let p = path();
    std::fs::create_dir_all(p.parent().expect("config dir"))?;
    let tmp = p.with_extension("tmp");
    // A leftover from a crash would keep whatever permissions it had: start fresh.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        // Only you can read it: it may hold an API key or the web password.
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(render(c).as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_passwords_are_hashed_and_checked() {
        let h = hash_password("correct horse").unwrap();
        assert!(is_password_hash(&h) && h.starts_with("$argon2id$"), "{h}");
        assert_ne!(h, hash_password("correct horse").unwrap(), "salted");
        assert!(password_matches(&h, "correct horse"));
        assert!(!password_matches(&h, "correct horsE") && !password_matches(&h, ""));
        // Typed into the file by hand, before it was hashed.
        assert!(password_matches("pw", "pw") && !password_matches("pw", "pw2") && !password_matches("", ""));
    }

    #[test]
    fn parses_and_round_trips() {
        let c = parse(
            "# hi\nrecommender = http://127.0.0.1:8890 # Argus\nmax_height=720\nbogus\nai = on\nai_url = http://127.0.0.1:11434/v1\nai_model = llama3.2\nai_api_key = sk-a#b\n",
        );
        assert_eq!(c.recommender.as_deref(), Some("http://127.0.0.1:8890"));
        assert_eq!(c.max_height, 720);
        assert_eq!(c.ai_api_key, "sk-a#b");
        assert_eq!(c.ai().unwrap().model, "llama3.2");
        assert_eq!(parse(&render(&c)), c);
        let mut d = Config {
            theme: "Midnight".into(),
            accent: Some("#ff8800".into()),
            corner_radius: 4,
            card_size: "Large".into(),
            animations: false,
            color_scheme: "dark".into(),
            profile: Some("Kid".into()),
            ..Config::default()
        };
        assert_eq!(parse(&render(&d)), d);
        d.accent = None;
        d.shortcuts.insert("search".into(), vec!["<Ctrl>slash".into(), "<Ctrl>f".into()]);
        d.shortcuts.insert("video.more".into(), Vec::new());
        assert_eq!(parse(&render(&d)), d);
        assert_eq!(parse("recommender = file:///etc/passwd").recommender, None);
        assert!(parse("ai = on\nai_model = x").ai().is_none(), "no URL, no AI");
        assert!(parse("ai = off\nai_url = http://x/v1\nai_model = x").ai().is_none());
        let mut e = Config {
            speed: 1.5,
            autoplay: false,
            subtitles: "en,de".into(),
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            proxy_playback: true,
            topic_model: true,
            dearrow: true,
            download_dir: Some("/tmp/v".into()),
            hide_views: true,
            web_address: "0.0.0.0:8723".into(),
            web_password: "pw".into(),
            ..Config::default()
        };
        e.sponsorblock_categories.insert("intro".into(), "skip".into());
        assert_eq!(parse(&render(&e)), e);
    }
}
