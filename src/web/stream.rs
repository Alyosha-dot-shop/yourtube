//! Playback in the browser. YouTube's direct stream URLs only work from the IP that asked for
//! them, so the server resolves a progressive MP4 (audio and video in one file) with yt-dlp
//! and relays it, forwarding the browser's Range requests so seeking works.
//!
//! That MP4 is 360p for most videos now. Browsers that play HLS (Safari, so every iPhone) get
//! HLS instead: YouTube's Safari client offers video and audio together in HLS up to 1080p.
//! Those URLs are tied to the server's IP too, so the server relays the playlist, with each
//! segment's address rewritten to point here, and every segment. HLS requests carry a key
//! from the watch page instead of relying on the login cookie, which a phone's built-in player
//! doesn't always send.

use super::http::{parse_content_range, parse_range, upstream_range};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a resolved URL is reused (YouTube's expire after about six hours).
const FRESH_FOR: Duration = Duration::from_secs(4 * 3600);
/// Don't retry a failed lookup sooner than this.
const FAILED_FOR: Duration = Duration::from_secs(20);
const MAX_CACHED: usize = 300;
/// How long a watch page's HLS key works.
const HLS_KEY_FOR: Duration = Duration::from_secs(12 * 3600);
/// The most one response relays; the browser asks for the next part itself.
pub const CHUNK: u64 = 8 * 1024 * 1024;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// What yt-dlp told us about a video.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub url: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub duration: Option<f64>,
    pub views: Option<i64>,
    pub timestamp: Option<i64>,
    pub description: String,
    pub height: Option<i64>,
}

enum Slot {
    Pending,
    Ready(Arc<Resolved>, Instant),
    Failed(String, Instant),
}

pub struct Resolver {
    slots: Mutex<HashMap<String, Slot>>,
    cv: Condvar,
    max_height: u32,
    proxy: Option<String>,
    agent: ureq::Agent,
    /// Resolved HLS playlists by video id.
    hls: Mutex<HashMap<String, (Arc<Hls>, Instant)>>,
    /// Keys handed out by watch pages: key -> (video id, when).
    hls_keys: Mutex<HashMap<String, (String, Instant)>>,
}

/// One video's HLS playlist: its lines, with the segment addresses taken out, and those addresses.
#[derive(Debug, PartialEq)]
pub struct Hls {
    parts: Vec<Part>,
    pub segments: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Part {
    /// A line kept as it is.
    Line(String),
    /// A segment's own line: becomes `N.ts?k=KEY`.
    Segment(usize),
    /// A tag with URI="…" (EXT-X-MAP, EXT-X-KEY): the URI becomes `N.ts?k=KEY`.
    Attr(String, usize, String),
}

impl Resolver {
    pub fn new(max_height: u32, proxy: Option<String>) -> Self {
        let mut cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(20)))
            // A paused player stops reading; give up on the upstream after a while (the browser re-asks).
            .timeout_recv_body(Some(Duration::from_secs(600)))
            .user_agent(UA);
        if let Some(p) = proxy.as_deref().and_then(|p| ureq::Proxy::new(p).ok()) {
            cfg = cfg.proxy(Some(p));
        }
        Resolver {
            slots: Mutex::new(HashMap::new()),
            cv: Condvar::new(),
            max_height,
            proxy,
            agent: cfg.build().new_agent(),
            hls: Mutex::new(HashMap::new()),
            hls_keys: Mutex::new(HashMap::new()),
        }
    }

    /// Start resolving in the background (the watch page does this so the video starts sooner).
    pub fn prefetch(self: &Arc<Self>, id: &str) {
        let (me, id) = (self.clone(), id.to_owned());
        std::thread::spawn(move || {
            let _ = me.get(&id);
        });
    }

    /// The cached result, if a lookup already finished.
    pub fn peek(&self, id: &str) -> Option<Arc<Resolved>> {
        match self.slots.lock().unwrap().get(id) {
            Some(Slot::Ready(r, at)) if at.elapsed() < FRESH_FOR => Some(r.clone()),
            _ => None,
        }
    }

    /// Resolve a video, waiting for a lookup another request already started.
    pub fn get(&self, id: &str) -> Result<Arc<Resolved>> {
        let mut slots = self.slots.lock().unwrap();
        loop {
            match slots.get(id) {
                Some(Slot::Ready(r, at)) if at.elapsed() < FRESH_FOR => return Ok(r.clone()),
                Some(Slot::Failed(e, at)) if at.elapsed() < FAILED_FOR => return Err(anyhow!("{e}")),
                Some(Slot::Pending) => slots = self.cv.wait(slots).unwrap(),
                _ => break,
            }
        }
        slots.insert(id.to_owned(), Slot::Pending);
        drop(slots);
        let r = self.run_ytdlp(id);
        let mut slots = self.slots.lock().unwrap();
        if slots.len() > MAX_CACHED {
            slots.retain(|_, s| match s {
                Slot::Ready(_, at) => at.elapsed() < FRESH_FOR,
                Slot::Failed(..) => false,
                Slot::Pending => true,
            });
        }
        let out = match r {
            Ok(r) => {
                let r = Arc::new(r);
                slots.insert(id.to_owned(), Slot::Ready(r.clone(), Instant::now()));
                Ok(r)
            }
            Err(e) => {
                slots.insert(id.to_owned(), Slot::Failed(e.to_string(), Instant::now()));
                Err(e)
            }
        };
        self.cv.notify_all();
        out
    }

    /// Forget a URL that stopped working (expired, or YouTube refused it).
    pub fn forget(&self, id: &str) {
        let mut slots = self.slots.lock().unwrap();
        if matches!(slots.get(id), Some(Slot::Ready(..))) {
            slots.remove(id);
        }
    }

    /// The format to ask for: progressive MP4 up to `max_height` (YouTube only offers 360p
    /// for most videos now, 720p for some), then whatever single file has both tracks.
    pub fn format(&self) -> String {
        let h = self.max_height.clamp(144, 720);
        format!(
            "best[height<={h}][ext=mp4][acodec!=none][vcodec!=none][protocol=https]/18/best[acodec!=none][vcodec!=none][protocol=https]"
        )
    }

    /// A key for this video's HLS addresses, handed out by a watch page.
    pub fn hls_key(&self, id: &str) -> String {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("random bytes");
        let key: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let mut keys = self.hls_keys.lock().unwrap();
        if keys.len() > 2000 {
            keys.retain(|_, (_, at)| at.elapsed() < HLS_KEY_FOR);
        }
        keys.insert(key.clone(), (id.to_owned(), Instant::now()));
        key
    }

    /// Is this a key a watch page gave out for this video, and still fresh?
    pub fn hls_key_ok(&self, key: &str, id: &str) -> bool {
        !key.is_empty() && self.hls_keys.lock().unwrap().get(key).is_some_and(|(v, at)| v == id && at.elapsed() < HLS_KEY_FOR)
    }

    /// HLS with video and audio together, up to `max_height` (YouTube offers it up to 1080p).
    pub fn hls_format(&self) -> String {
        let h = self.max_height.clamp(144, 1080);
        format!("best[protocol^=m3u8][height<={h}][vcodec!=none][acodec!=none]")
    }

    /// The playlist for a video, looked up with yt-dlp (Safari client) and fetched, then cached.
    pub fn hls(&self, id: &str) -> Result<Arc<Hls>> {
        if let Some((h, at)) = self.hls.lock().unwrap().get(id)
            && at.elapsed() < FRESH_FOR
        {
            return Ok(h.clone());
        }
        let mut cmd = crate::proc::ytdlp();
        cmd.args(["--no-playlist", "--no-warnings", "--socket-timeout", "15", "--extractor-args", "youtube:player_client=web_safari"])
            .args(["-f", &self.hls_format(), "-O", "urls"]);
        if let Some(p) = &self.proxy {
            cmd.args(["--proxy", p]);
        }
        let out = cmd.arg("--").arg(id).stdin(Stdio::null()).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { anyhow!("yt-dlp isn't installed on the server") } else { e.into() }
        })?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let url = stdout.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_owned();
        if !out.status.success() || !url.starts_with("https://") {
            return Err(anyhow!("no HLS stream for this video"));
        }
        let resp = self.agent.get(&url).call()?;
        if resp.status().as_u16() != 200 {
            return Err(anyhow!("YouTube answered {} for the playlist", resp.status().as_u16()));
        }
        let text = resp.into_body().with_config().limit(4 << 20).read_to_string()?;
        let h = Arc::new(parse_hls(&text, &url)?);
        let mut cache = self.hls.lock().unwrap();
        if cache.len() > MAX_CACHED {
            cache.retain(|_, (_, at)| at.elapsed() < FRESH_FOR);
        }
        cache.insert(id.to_owned(), (h.clone(), Instant::now()));
        Ok(h)
    }

    fn forget_hls(&self, id: &str) {
        self.hls.lock().unwrap().remove(id);
    }

    fn run_ytdlp(&self, id: &str) -> Result<Resolved> {
        let mut cmd = crate::proc::ytdlp();
        // YouTube's default clients now offer only separate audio and video (or HLS); the mobile
        // web client still has the single-file 360p MP4 (itag 18) a <video> element can play.
        cmd.args(["--no-playlist", "--no-warnings", "--socket-timeout", "15", "--extractor-args", "youtube:player_client=default,mweb"])
            .args(["-f", &self.format()])
            .args(["-O", "%(.{title,channel,uploader,channel_id,duration,view_count,timestamp,description,height})j", "-O", "urls"]);
        if let Some(p) = &self.proxy {
            cmd.args(["--proxy", p]);
        }
        let out = cmd.arg("--").arg(id).stdin(Stdio::null()).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { anyhow!("yt-dlp isn't installed on the server") } else { e.into() }
        })?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err.lines().rev().find(|l| l.contains("ERROR")).unwrap_or("yt-dlp failed").trim();
            return Err(anyhow!("{}", line.trim_start_matches("ERROR:").trim()));
        }
        parse_output(&String::from_utf8_lossy(&out.stdout))
    }
}

/// yt-dlp's output: one JSON line of details, then the stream URL.
pub fn parse_output(s: &str) -> Result<Resolved> {
    let mut lines = s.lines().filter(|l| !l.trim().is_empty());
    let info: Value = serde_json::from_str(lines.next().ok_or_else(|| anyhow!("yt-dlp printed nothing"))?)?;
    let url = lines.next().ok_or_else(|| anyhow!("no playable format"))?.trim().to_owned();
    if !url.starts_with("https://") || lines.next().is_some() {
        // Two URLs mean separate audio and video, which a <video> element can't play from one file.
        return Err(anyhow!("no single-file format for this video"));
    }
    let s = |k: &str| info[k].as_str().unwrap_or_default().to_owned();
    Ok(Resolved {
        url,
        title: s("title"),
        channel: Some(s("channel")).filter(|c| !c.is_empty()).unwrap_or_else(|| s("uploader")),
        channel_id: s("channel_id"),
        duration: info["duration"].as_f64(),
        views: info["view_count"].as_i64(),
        timestamp: info["timestamp"].as_i64(),
        description: s("description"),
        height: info["height"].as_i64(),
    })
}

/// Read a media playlist, keeping each segment's absolute address and leaving a numbered slot
/// where it was. A master playlist (a list of other playlists) isn't expected and is refused.
pub fn parse_hls(text: &str, base: &str) -> Result<Hls> {
    if !text.starts_with("#EXTM3U") {
        return Err(anyhow!("not an HLS playlist"));
    }
    if text.contains("#EXT-X-STREAM-INF") {
        return Err(anyhow!("got a master playlist, not a media playlist"));
    }
    let absolute = |u: &str| -> String {
        if u.starts_with("https://") || u.starts_with("http://") {
            u.to_owned()
        } else if let Some(rest) = u.strip_prefix('/') {
            let origin = base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
            format!("{origin}/{rest}")
        } else {
            format!("{}/{u}", base.rsplit_once('/').map_or(base, |(dir, _)| dir))
        }
    };
    let (mut parts, mut segments) = (Vec::new(), Vec::new());
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() {
            continue;
        }
        if let Some(tag) = line.strip_prefix('#') {
            match tag.find("URI=\"") {
                Some(i) => {
                    let start = i + 1 + 5;
                    let Some(len) = line[start..].find('"') else { return Err(anyhow!("bad URI in playlist")) };
                    segments.push(absolute(&line[start..start + len]));
                    parts.push(Part::Attr(line[..start].to_owned(), segments.len() - 1, line[start + len..].to_owned()));
                }
                None => parts.push(Part::Line(line.to_owned())),
            }
        } else {
            segments.push(absolute(line));
            parts.push(Part::Segment(segments.len() - 1));
        }
    }
    if segments.is_empty() {
        return Err(anyhow!("the playlist has no segments"));
    }
    Ok(Hls { parts, segments })
}

impl Hls {
    /// The playlist as the browser gets it: segments are `N.ts?k=KEY`, relative to
    /// `/hls/ID/index.m3u8`, so they resolve to `/hls/ID/N.ts`.
    pub fn render(&self, key: &str) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Line(l) => out.push_str(l),
                Part::Segment(n) => out.push_str(&format!("{n}.ts?k={key}")),
                Part::Attr(a, n, b) => out.push_str(&format!("{a}{n}.ts?k={key}{b}")),
            }
            out.push('\n');
        }
        out
    }
}

/// Relay one HLS segment. Expired addresses (YouTube's last about six hours) are looked up again once.
pub fn relay_segment(res: &Resolver, id: &str, n: usize, range: Option<&str>) -> Result<Relay> {
    for attempt in 0..2 {
        let h = res.hls(id)?;
        let url = h.segments.get(n).ok_or_else(|| anyhow!("no such segment"))?;
        let mut req = res.agent.get(url).header("Accept-Encoding", "identity");
        if let Some(r) = range.filter(|r| parse_range(r).is_some()) {
            req = req.header("Range", r);
        }
        let resp = req.call()?;
        let status = resp.status().as_u16();
        if matches!(status, 403 | 404 | 410) && attempt == 0 {
            res.forget_hls(id);
            continue;
        }
        if !matches!(status, 200 | 206) {
            return Err(anyhow!("YouTube answered {status}"));
        }
        let hdr = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut headers = vec![
            ("Content-Type", hdr("content-type").filter(|t| t.starts_with("video/")).unwrap_or_else(|| "video/mp2t".into())),
            ("Cache-Control", "private, max-age=3600".into()),
        ];
        if let Some(cr) = hdr("content-range") {
            headers.push(("Content-Range", cr));
        }
        let length = hdr("content-length").and_then(|l| l.parse::<usize>().ok());
        return Ok(Relay { status, headers, length, body: Box::new(resp.into_body().into_reader()) });
    }
    Err(anyhow!("YouTube refused the segment"))
}

/// What to send back to the browser.
pub struct Relay {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub length: Option<usize>,
    pub body: Box<dyn std::io::Read + Send>,
}

/// Relay (part of) a video. `range` is the browser's Range header.
pub fn relay(res: &Resolver, id: &str, range: Option<&str>) -> Result<Relay> {
    let client = range.and_then(parse_range);
    // Without a Range the browser wants the whole file (200); ask upstream for all of it as a
    // range anyway, so its answer carries the total length.
    let up_range = upstream_range(client, CHUNK).unwrap_or_else(|| "bytes=0-".into());
    for attempt in 0..2 {
        let r = res.get(id)?;
        let mut req = res.agent.get(&r.url).header("Accept-Encoding", "identity");
        req = req.header("Range", &up_range);
        let resp = req.call()?;
        let status = resp.status().as_u16();
        if matches!(status, 403 | 404 | 410) && attempt == 0 {
            // The URL expired or was refused: look it up again once.
            res.forget(id);
            continue;
        }
        let hdr = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let content_range = hdr("content-range");
        let mut length = hdr("content-length").and_then(|l| l.parse::<usize>().ok());
        if length.is_none() {
            length = content_range.as_deref().and_then(parse_content_range).map(|(a, b, _)| (b - a + 1) as usize);
        }
        let mut headers = vec![
            (
                "Content-Type",
                hdr("content-type").filter(|t| t.starts_with("video/") || t.starts_with("audio/")).unwrap_or_else(|| "video/mp4".into()),
            ),
            ("Accept-Ranges", "bytes".into()),
            ("Cache-Control", "private, max-age=3600".into()),
        ];
        let status = match status {
            206 if client.is_none() => {
                if let Some((_, _, Some(total))) = content_range.as_deref().and_then(parse_content_range) {
                    length = Some(total as usize);
                }
                200
            }
            206 => {
                if let Some(cr) = content_range {
                    headers.push(("Content-Range", cr));
                }
                206
            }
            200 => 200,
            416 => {
                if let Some(cr) = content_range {
                    headers.push(("Content-Range", cr));
                }
                return Ok(Relay { status: 416, headers, length: Some(0), body: Box::new(std::io::empty()) });
            }
            s => return Err(anyhow!("YouTube answered {s}")),
        };
        // A client that asked for a range but got the whole file (200) gets the whole file.
        return Ok(Relay { status, headers, length, body: Box::new(resp.into_body().into_reader()) });
    }
    Err(anyhow!("YouTube refused the stream"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ytdlp_output() {
        let out = "{\"title\": \"Me at the zoo\", \"channel\": \"jawed\", \"channel_id\": \"UC4QobU6STFB0P71PMvOGN5A\", \"duration\": 19, \"view_count\": 300, \"timestamp\": 1114256080, \"description\": \"x\", \"height\": 240}\nhttps://rr1---sn.googlevideo.com/videoplayback?expire=1&itag=18\n";
        let r = parse_output(out).unwrap();
        assert_eq!(r.title, "Me at the zoo");
        assert_eq!(r.channel_id, "UC4QobU6STFB0P71PMvOGN5A");
        assert_eq!(r.duration, Some(19.0));
        assert!(r.url.contains("itag=18"));
        assert!(parse_output("{}\nhttps://a\nhttps://b\n").is_err(), "split audio/video can't play");
        assert!(parse_output("{}\nfile:///etc/passwd\n").is_err());
        assert!(parse_output("").is_err());
    }

    const PLAYLIST: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:7\n\
#EXTINF:5.24,\nhttps://rr2---sn-x.googlevideo.com/videoplayback/id/1/itag/96/sq/0/file/seg.ts\n\
#EXTINF:5.0,\nhttps://rr2---sn-x.googlevideo.com/videoplayback/id/1/itag/96/sq/1/file/seg.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn hls_playlist_is_rewritten_to_point_here() {
        let h = parse_hls(PLAYLIST, "https://manifest.googlevideo.com/api/manifest/hls_playlist/x/index.m3u8").unwrap();
        assert_eq!(h.segments.len(), 2);
        assert!(h.segments[1].contains("/sq/1/"));
        let out = h.render("abc");
        assert!(out.starts_with("#EXTM3U\n#EXT-X-VERSION:3\n"));
        assert!(out.contains("#EXTINF:5.24,\n0.ts?k=abc\n#EXTINF:5.0,\n1.ts?k=abc\n#EXT-X-ENDLIST\n"));
        assert!(!out.contains("googlevideo"));
    }

    #[test]
    fn hls_map_uris_and_relative_segments() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nseg/1.m4s\n#EXTINF:4,\n/abs/2.m4s\n";
        let h = parse_hls(text, "https://example.com/a/b/index.m3u8").unwrap();
        assert_eq!(h.segments, ["https://example.com/a/b/init.mp4", "https://example.com/a/b/seg/1.m4s", "https://example.com/abs/2.m4s"]);
        assert!(h.render("k").starts_with("#EXTM3U\n#EXT-X-MAP:URI=\"0.ts?k=k\"\n#EXTINF:4,\n1.ts?k=k\n"));
    }

    #[test]
    fn hls_refuses_master_and_garbage() {
        assert!(parse_hls("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n", "https://x/y").is_err());
        assert!(parse_hls("<html>", "https://x/y").is_err());
        assert!(parse_hls("#EXTM3U\n#EXT-X-ENDLIST\n", "https://x/y").is_err());
    }

    #[test]
    fn hls_keys_are_per_video() {
        let r = Resolver::new(1080, None);
        let k = r.hls_key("jNQXAC9IVRw");
        assert_eq!(k.len(), 32);
        assert!(r.hls_key_ok(&k, "jNQXAC9IVRw"));
        assert!(!r.hls_key_ok(&k, "dQw4w9WgXcQ"));
        assert!(!r.hls_key_ok("", "jNQXAC9IVRw"));
        assert!(!r.hls_key_ok("00000000000000000000000000000000", "jNQXAC9IVRw"));
        assert_eq!(r.hls_format(), "best[protocol^=m3u8][height<=1080][vcodec!=none][acodec!=none]");
        assert!(Resolver::new(4320, None).hls_format().contains("height<=1080"));
    }

    #[test]
    fn format_respects_max_height() {
        assert!(Resolver::new(480, None).format().starts_with("best[height<=480][ext=mp4]"));
        assert!(Resolver::new(2160, None).format().starts_with("best[height<=720][ext=mp4]"));
        assert!(Resolver::new(1080, None).format().contains("/18/"));
    }
}
