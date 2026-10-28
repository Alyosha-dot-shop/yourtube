//! Small HTTP pieces with no I/O: routing, escaping, query strings, cookies,
//! Range headers, id checks and the cross-site / Host checks. All unit-tested.

use std::collections::HashMap;

/// Every page and endpoint the server knows. Ids are validated here, so handlers can trust them.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Route {
    ForYou,
    Subscriptions,
    Search,
    Channel(String),
    Library,
    Playlist(String),
    History,
    Watch,
    Settings,
    Login,
    Logout,
    Static(&'static str),
    Thumb(String),
    Stream(String),
    /// `/hls/ID/index.m3u8`: the HLS playlist, rewritten to point here.
    HlsPlaylist(String),
    /// `/hls/ID/N.ts`: one HLS segment.
    HlsSegment(String, usize),
    Info(String),
    Related(String),
    Progress,
    Action,
    Refresh,
    Status,
    Prefs,
    NotFound,
    /// A known path with the wrong method.
    BadMethod,
}

pub const STATIC_FILES: &[&str] = &["style.css", "app.js", "icon.svg", "apple-touch-icon.png", "manifest.webmanifest"];

pub fn route(method: &str, path: &str) -> Route {
    let get = matches!(method, "GET" | "HEAD");
    let post = method == "POST";
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let r = |ok: bool, r: Route| if ok { r } else { Route::BadMethod };
    match seg.as_slice() {
        [""] => r(get, Route::ForYou),
        ["subscriptions"] => r(get, Route::Subscriptions),
        ["search"] => r(get, Route::Search),
        ["library"] => r(get, Route::Library),
        ["history"] => r(get, Route::History),
        ["watch"] => r(get, Route::Watch),
        ["settings"] => r(get, Route::Settings),
        ["login"] if get || post => Route::Login,
        ["logout"] => r(post, Route::Logout),
        ["prefs"] => r(post, Route::Prefs),
        ["channel", id] if valid_channel_id(id) => r(get, Route::Channel((*id).to_owned())),
        ["playlist", id] if valid_playlist_id(id) => r(get, Route::Playlist((*id).to_owned())),
        ["thumb", id] if valid_video_id(id) => r(get, Route::Thumb((*id).to_owned())),
        ["stream", id] if valid_video_id(id) => r(get, Route::Stream((*id).to_owned())),
        ["hls", id, "index.m3u8"] if valid_video_id(id) => r(get, Route::HlsPlaylist((*id).to_owned())),
        ["hls", id, seg] if valid_video_id(id) => match seg.strip_suffix(".ts").filter(|n| n.len() <= 6).and_then(|n| n.parse().ok()) {
            Some(n) => r(get, Route::HlsSegment((*id).to_owned(), n)),
            None => Route::NotFound,
        },
        ["static", name] => match STATIC_FILES.iter().find(|f| *f == name) {
            Some(f) => r(get, Route::Static(f)),
            None => Route::NotFound,
        },
        ["favicon.ico"] => r(get, Route::Static("icon.svg")),
        ["api", "info", id] if valid_video_id(id) => r(get, Route::Info((*id).to_owned())),
        ["api", "related", id] if valid_video_id(id) => r(get, Route::Related((*id).to_owned())),
        ["api", "progress"] => r(post, Route::Progress),
        ["api", "action"] => r(post, Route::Action),
        ["api", "refresh"] => r(post, Route::Refresh),
        ["api", "status"] => r(get, Route::Status),
        _ => Route::NotFound,
    }
}

/// Split a request target into its path and decoded query parameters.
pub fn split_url(url: &str) -> (String, HashMap<String, String>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let path = path.split('#').next().unwrap_or("");
    (path.to_owned(), parse_form(query))
}

/// `a=1&b=x+y` (query strings and form bodies). The first value of a repeated key wins.
pub fn parse_form(s: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.entry(url_decode(k)).or_insert_with(|| url_decode(v));
    }
    m
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Escape text for HTML element content and quoted attribute values.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(id_byte)
}

pub fn valid_channel_id(id: &str) -> bool {
    id.len() == 24 && id.starts_with("UC") && id.bytes().all(id_byte)
}

/// Playlist ids are ours (`yourtube-…`), FreeTube's (`ft-playlist--…`) or YouTube's (`PL…`).
pub fn valid_playlist_id(id: &str) -> bool {
    (1..=100).contains(&id.len()) && id.bytes().all(id_byte)
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Cookies from a `Cookie:` header.
pub fn parse_cookies(header: &str) -> HashMap<String, String> {
    header
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().trim_matches('"').to_owned()))
        .collect()
}

/// A `Set-Cookie` value. Every cookie we set is HttpOnly and SameSite=Strict.
pub fn set_cookie(name: &str, value: &str, max_age: Option<u64>) -> String {
    let age = match max_age {
        Some(s) => format!("; Max-Age={s}"),
        None => String::new(),
    };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict{age}")
}

pub fn clear_cookie(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Cookie values must be plain tokens; profile names can hold spaces and accents.
pub fn cookie_encode(s: &str) -> String {
    url_encode(s)
}

/// Compare secrets without stopping at the first difference.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// A single byte range from a `Range:` header. Multiple ranges aren't supported (the whole file is sent).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ByteRange {
    /// `bytes=start-` or `bytes=start-end` (end inclusive).
    From(u64, Option<u64>),
    /// `bytes=-n`: the last n bytes.
    Suffix(u64),
}

pub fn parse_range(h: &str) -> Option<ByteRange> {
    let spec = h.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        return (n > 0).then_some(ByteRange::Suffix(n));
    }
    let start: u64 = a.parse().ok()?;
    if b.is_empty() {
        return Some(ByteRange::From(start, None));
    }
    let end: u64 = b.parse().ok()?;
    (end >= start).then_some(ByteRange::From(start, Some(end)))
}

/// The Range to ask the upstream server for: the client's, but at most `cap` bytes, so one
/// response never ties a thread (or YouTube's throttling) to a whole film. Browsers ask for
/// the rest with the next request, as HTTP allows.
pub fn upstream_range(r: Option<ByteRange>, cap: u64) -> Option<String> {
    match r? {
        ByteRange::From(start, end) => {
            let last = start.saturating_add(cap - 1);
            let end = end.map_or(last, |e| e.min(last));
            Some(format!("bytes={start}-{end}"))
        }
        ByteRange::Suffix(n) => Some(format!("bytes=-{n}")),
    }
}

/// `bytes 0-99/1234` → (0, 99, Some(1234)); `bytes */1234` → None.
pub fn parse_content_range(h: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = h.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (a, b) = span.split_once('-')?;
    let total = if total.trim() == "*" { None } else { Some(total.trim().parse().ok()?) };
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total))
}

/// Was this POST sent by our own pages? Modern browsers send Sec-Fetch-Site; older ones Origin.
/// A request with neither (curl, very old browsers) is refused too.
pub fn same_origin(sec_fetch_site: Option<&str>, origin: Option<&str>, host: Option<&str>) -> bool {
    if let Some(s) = sec_fetch_site {
        return matches!(s, "same-origin" | "none");
    }
    match (origin, host) {
        (Some(o), Some(h)) => {
            let o = o.trim();
            let o = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")).unwrap_or("");
            !o.is_empty() && o.eq_ignore_ascii_case(h.trim())
        }
        _ => false,
    }
}

/// Another site embedding or fetching one of our URLs: an <img>, <video> or fetch() on some web page
/// pointed at this server. Following a link here (a top-level page load) is still allowed.
pub fn foreign_subrequest(site: Option<&str>, mode: Option<&str>, dest: Option<&str>) -> bool {
    matches!(site, Some("cross-site" | "same-site")) && !(mode == Some("navigate") && matches!(dest, None | Some("document")))
}

/// May this GET write (search history, clicks)? Only when it came from our own pages, a bookmark or
/// the address bar. Browsers without Sec-Fetch-Site (curl, very old ones) are trusted as before.
pub fn own_request(site: Option<&str>) -> bool {
    matches!(site, None | Some("same-origin" | "none"))
}

/// Without a password, only accept Host names a DNS-rebinding page can't use: IP addresses,
/// localhost, single-label names and home-network suffixes. Public domains are refused.
pub fn safe_host(host: &str) -> bool {
    let h = host.trim();
    // Drop the port ("[::1]:8723", "pi.local:8723").
    let name = if let Some(rest) = h.strip_prefix('[') {
        return rest.split(']').next().is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok());
    } else {
        h.rsplit_once(':').map_or(h, |(n, port)| if port.bytes().all(|b| b.is_ascii_digit()) { n } else { h })
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if name.parse::<std::net::IpAddr>().is_ok() || name == "localhost" || !name.contains('.') {
        return true;
    }
    [".local", ".lan", ".home", ".internal", ".home.arpa", ".localhost", ".ts.net", ".intranet", ".private"]
        .iter()
        .any(|s| name.ends_with(s))
}

/// A safe place to send someone after logging in: a path on this server.
pub fn safe_next(next: &str) -> String {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') && !next.contains("://") {
        next.to_owned()
    } else {
        "/".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing() {
        assert_eq!(route("GET", "/"), Route::ForYou);
        assert_eq!(route("HEAD", "/"), Route::ForYou);
        assert_eq!(route("POST", "/"), Route::BadMethod);
        assert_eq!(route("GET", "/subscriptions"), Route::Subscriptions);
        assert_eq!(route("GET", "/watch"), Route::Watch);
        assert_eq!(route("GET", "/channel/UCX6OQ3DkcsbYNE6H8uQQuVA"), Route::Channel("UCX6OQ3DkcsbYNE6H8uQQuVA".into()));
        assert_eq!(route("GET", "/channel/UC../../etc/passwd"), Route::NotFound);
        assert_eq!(route("GET", "/channel/xx"), Route::NotFound);
        assert_eq!(route("GET", "/stream/jNQXAC9IVRw"), Route::Stream("jNQXAC9IVRw".into()));
        assert_eq!(route("HEAD", "/stream/jNQXAC9IVRw"), Route::Stream("jNQXAC9IVRw".into()));
        assert_eq!(route("GET", "/stream/jNQXAC9IVR"), Route::NotFound);
        assert_eq!(route("GET", "/thumb/jNQXAC9IVRw"), Route::Thumb("jNQXAC9IVRw".into()));
        assert_eq!(route("GET", "/thumb/..%2F..%2Fx"), Route::NotFound);
        assert_eq!(
            route("GET", "/playlist/ft-playlist--7d58ac8ea3a047dfb80e825d7414cc02"),
            Route::Playlist("ft-playlist--7d58ac8ea3a047dfb80e825d7414cc02".into())
        );
        assert_eq!(route("GET", "/playlist/a.b"), Route::NotFound);
        assert_eq!(route("GET", "/static/app.js"), Route::Static("app.js"));
        assert_eq!(route("GET", "/static/../mod.rs"), Route::NotFound);
        assert_eq!(route("GET", "/static/secret"), Route::NotFound);
        assert_eq!(route("POST", "/api/progress"), Route::Progress);
        assert_eq!(route("GET", "/api/progress"), Route::BadMethod);
        assert_eq!(route("POST", "/api/action"), Route::Action);
        assert_eq!(route("GET", "/api/info/jNQXAC9IVRw"), Route::Info("jNQXAC9IVRw".into()));
        assert_eq!(route("GET", "/login"), Route::Login);
        assert_eq!(route("POST", "/login"), Route::Login);
        assert_eq!(route("GET", "/logout"), Route::BadMethod);
        assert_eq!(route("GET", "/nope"), Route::NotFound);
        assert_eq!(route("DELETE", "/library"), Route::BadMethod);
    }

    #[test]
    fn urls_and_forms() {
        let (p, q) = split_url("/search?q=e30+turbo+%26+co&x=%E2%9C%93&q=second");
        assert_eq!(p, "/search");
        assert_eq!(q["q"], "e30 turbo & co");
        assert_eq!(q["x"], "✓");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz%4"), "%zz%4");
        assert_eq!(url_decode("a%2"), "a%2");
        assert_eq!(url_decode("%41"), "A");
        assert_eq!(url_encode("Kid's TV"), "Kid%27s%20TV");
        assert_eq!(url_decode(&url_encode("Zoë / ☃ + x")), "Zoë / ☃ + x");
        assert_eq!(split_url("/").1.len(), 0);
    }

    #[test]
    fn escaping() {
        assert_eq!(esc(r#"<script>alert("x")</script> & 'y'"#), "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;");
        assert_eq!(esc("plain ünïcode"), "plain ünïcode");
    }

    #[test]
    fn ids() {
        assert!(valid_video_id("jNQXAC9IVRw"));
        assert!(valid_video_id("a-_0123456Z"));
        assert!(!valid_video_id("jNQXAC9IVR"));
        assert!(!valid_video_id("jNQXAC9IVRw1"));
        assert!(!valid_video_id("jNQXAC9IV/w"));
        assert!(!valid_video_id("jNQXAC9IV w"));
        assert!(!valid_video_id("jNQXAC9IVé"));
        assert!(valid_channel_id("UCX6OQ3DkcsbYNE6H8uQQuVA"));
        assert!(!valid_channel_id("XCX6OQ3DkcsbYNE6H8uQQuVA"));
        assert!(!valid_channel_id("UCX6OQ3DkcsbYNE6H8uQQuV"));
        assert!(valid_playlist_id("yourtube-1792662034-1234"));
        assert!(!valid_playlist_id(""));
        assert!(!valid_playlist_id("../x"));
        assert!(!valid_playlist_id(&"a".repeat(101)));
    }

    #[test]
    fn cookies() {
        let c = parse_cookies("yt_session=abc123; yt_profile=Kid%27s; theme=\"dark\"; junk");
        assert_eq!(c["yt_session"], "abc123");
        assert_eq!(url_decode(&c["yt_profile"]), "Kid's");
        assert_eq!(c["theme"], "dark");
        assert!(!c.contains_key("junk"));
        assert_eq!(set_cookie("yt_session", "t", Some(60)), "yt_session=t; Path=/; HttpOnly; SameSite=Strict; Max-Age=60");
        assert!(clear_cookie("x").ends_with("Max-Age=0"));
        assert!(!cookie_encode("A b;c=d,\"").contains([' ', ';', '=', ',', '"']));
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"hunter2", b"hunter"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
        // Lengths that differ by a multiple of 256 still differ.
        assert!(!ct_eq(&[0u8; 256], b""));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-"), Some(ByteRange::From(0, None)));
        assert_eq!(parse_range("bytes=0-1"), Some(ByteRange::From(0, Some(1))));
        assert_eq!(parse_range(" bytes=100-199 "), Some(ByteRange::From(100, Some(199))));
        assert_eq!(parse_range("bytes=-500"), Some(ByteRange::Suffix(500)));
        assert_eq!(parse_range("bytes=-0"), None);
        assert_eq!(parse_range("bytes=200-100"), None);
        assert_eq!(parse_range("bytes=0-1,5-6"), None);
        assert_eq!(parse_range("items=0-1"), None);
        assert_eq!(parse_range("bytes=x-"), None);
        assert_eq!(parse_range("bytes=-"), None);

        const CAP: u64 = 8 * 1024 * 1024;
        assert_eq!(upstream_range(None, CAP), None);
        assert_eq!(upstream_range(parse_range("bytes=0-"), CAP).unwrap(), "bytes=0-8388607");
        assert_eq!(upstream_range(parse_range("bytes=0-1"), CAP).unwrap(), "bytes=0-1");
        assert_eq!(upstream_range(parse_range("bytes=1000-"), CAP).unwrap(), format!("bytes=1000-{}", 1000 + CAP - 1));
        assert_eq!(upstream_range(parse_range("bytes=10-99999999"), CAP).unwrap(), format!("bytes=10-{}", 10 + CAP - 1));
        assert_eq!(upstream_range(parse_range("bytes=-500"), CAP).unwrap(), "bytes=-500");
        assert_eq!(upstream_range(Some(ByteRange::From(u64::MAX - 1, None)), CAP).unwrap(), format!("bytes={}-{}", u64::MAX - 1, u64::MAX));

        assert_eq!(parse_content_range("bytes 0-1/791367"), Some((0, 1, Some(791367))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range("bytes */791367"), None);
        assert_eq!(parse_content_range("garbage"), None);
    }

    #[test]
    fn cross_site_and_hosts() {
        assert!(same_origin(Some("same-origin"), None, None));
        assert!(same_origin(Some("none"), None, None));
        assert!(!same_origin(Some("cross-site"), Some("http://pi:8723"), Some("pi:8723")));
        assert!(!same_origin(Some("same-site"), None, None));
        assert!(same_origin(None, Some("http://192.168.1.5:8723"), Some("192.168.1.5:8723")));
        assert!(!same_origin(None, Some("http://evil.example"), Some("192.168.1.5:8723")));
        assert!(!same_origin(None, Some("null"), Some("192.168.1.5:8723")));
        assert!(!same_origin(None, None, Some("192.168.1.5:8723")));
        // <img src="http://pi:8723/search?q=…"> on another site: refused. A link from another site: allowed.
        assert!(foreign_subrequest(Some("cross-site"), Some("no-cors"), Some("image")));
        assert!(foreign_subrequest(Some("same-site"), Some("cors"), Some("empty")));
        assert!(!foreign_subrequest(Some("cross-site"), Some("navigate"), Some("document")));
        assert!(!foreign_subrequest(Some("same-origin"), Some("no-cors"), Some("video")));
        assert!(!foreign_subrequest(None, None, None));
        assert!(own_request(Some("same-origin")) && own_request(Some("none")) && own_request(None));
        assert!(!own_request(Some("cross-site")) && !own_request(Some("same-site")));

        for ok in [
            "127.0.0.1:8723",
            "192.168.1.5",
            "[::1]:8723",
            "localhost:8723",
            "raspberrypi:8723",
            "pi.local",
            "argus.tail1234.ts.net",
            "nas.home.arpa:80",
            "box.lan.",
        ] {
            assert!(safe_host(ok), "{ok}");
        }
        for bad in ["evil.example.com", "rebind.attacker.net:8723", "", "[nonsense]", "127.0.0.1.nip.io"] {
            assert!(!safe_host(bad), "{bad}");
        }
        assert_eq!(safe_next("/watch?v=x"), "/watch?v=x");
        assert_eq!(safe_next("//evil.com"), "/");
        assert_eq!(safe_next("https://evil.com"), "/");
        assert_eq!(safe_next("/\\evil.com"), "/");
    }
}
