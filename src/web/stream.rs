//! Playback in the browser. YouTube's direct stream URLs only work from the IP that asked for
//! them, so the server resolves a progressive MP4 (audio and video in one file) with yt-dlp
//! and relays it, forwarding the browser's Range requests so seeking works.

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
        Resolver { slots: Mutex::new(HashMap::new()), cv: Condvar::new(), max_height, proxy, agent: cfg.build().new_agent() }
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

    #[test]
    fn format_respects_max_height() {
        assert!(Resolver::new(480, None).format().starts_with("best[height<=480][ext=mp4]"));
        assert!(Resolver::new(2160, None).format().starts_with("best[height<=720][ext=mp4]"));
        assert!(Resolver::new(1080, None).format().contains("/18/"));
    }
}
