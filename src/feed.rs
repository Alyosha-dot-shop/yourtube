//! Subscription feed refresh.
//!
//! Three sources, cheapest first:
//! 1. The channel's RSS feed: tiny, exact upload times. YouTube's RSS endpoint
//!    fails on and off (404s for every channel at times), so after a run of
//!    failures it is skipped for the rest of the refresh.
//! 2. The channel's /videos page, read directly: about 200 KB gzipped, ages
//!    like "2 weeks ago", no Shorts.
//! 3. yt-dlp, for channels where both fail: dependable but slow to start.

use anyhow::{Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::Event;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const WORKERS: usize = 12;
/// Skip RSS for the rest of a refresh after this many failures in a row.
const RSS_GIVE_UP: usize = 12;
const YTDLP_WORKERS: usize = 4;
const YTDLP_BATCH: usize = 10;
const PER_CHANNEL: usize = 15;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Video {
    pub id: String,
    pub channel_id: String,
    /// Channel name, when the source gives it (search results, channel pages).
    pub channel: String,
    pub title: String,
    pub published: i64,
    pub views: Option<i64>,
    pub length: Option<f64>,
    pub description: Option<String>,
    /// true when `published` is an exact time (RSS), false when it is only the day (yt-dlp).
    pub exact_time: bool,
}

#[derive(Debug, Clone)]
pub enum Progress {
    Rss { done: usize, total: usize },
    Fallback { done: usize, total: usize },
    Finished { videos: usize, failed: usize },
}

/// Refresh every subscribed channel. `progress` is called from worker threads.
pub fn refresh(progress: impl Fn(Progress) + Send + Sync + 'static) -> Result<()> {
    let conn = crate::db::open()?;
    let channels: Vec<String> = {
        let mut st = conn.prepare("SELECT id FROM channels WHERE subscribed = 1 ORDER BY last_fetch")?;
        st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let progress = Arc::new(progress);
    let total = channels.len();

    // Stage 1: RSS, then the channel page, in parallel.
    let (tx, rx) = mpsc::channel::<(String, Result<Vec<Video>>)>();
    let queue = Arc::new(Mutex::new(channels.clone()));
    let rss_fails = Arc::new(AtomicUsize::new(0));
    for _ in 0..WORKERS.min(total.max(1)) {
        let (queue, tx, rss_fails) = (queue.clone(), tx.clone(), rss_fails.clone());
        thread::spawn(move || {
            loop {
                let Some(id) = queue.lock().unwrap().pop() else { break };
                let mut r = Err(anyhow!("RSS skipped"));
                if rss_fails.load(Ordering::Relaxed) < RSS_GIVE_UP {
                    r = fetch_rss(&id);
                    match &r {
                        Ok(_) => rss_fails.store(0, Ordering::Relaxed),
                        Err(_) => {
                            rss_fails.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if r.is_err() {
                    r = crate::yt::channel_videos(&id).map(|c| c.videos);
                }
                if tx.send((id, r)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut failed = Vec::new();
    let mut stored = 0;
    for (done, (id, r)) in rx.into_iter().enumerate() {
        match r {
            Ok(videos) => stored += store(&conn, &id, &videos)?,
            Err(e) => failed.push((id, e.to_string())),
        }
        progress(Progress::Rss { done: done + 1, total });
    }

    // Stage 2: yt-dlp for channels where both failed, in batches.
    let fallback: Vec<String> = failed.iter().map(|(id, _)| id.clone()).collect();
    let mut still_failed = 0;
    if !fallback.is_empty() {
        // The same proxy as the rest of the refresh, so a Tor user's subscriptions never go out
        // from their own address just because Tor got the RSS feeds blocked.
        let proxy = crate::config::load().proxy.map(|p| crate::yt::isolated_proxy(&p, "feed"));
        let batches: Vec<Vec<String>> = fallback.chunks(YTDLP_BATCH).map(|c| c.to_vec()).collect();
        let queue = Arc::new(Mutex::new(batches));
        let (tx, rx) = mpsc::channel::<(Vec<String>, Result<Vec<Video>>)>();
        for _ in 0..YTDLP_WORKERS {
            let (queue, tx, proxy) = (queue.clone(), tx.clone(), proxy.clone());
            thread::spawn(move || {
                loop {
                    let Some(batch) = queue.lock().unwrap().pop() else { break };
                    let r = fetch_ytdlp(&batch, proxy.as_deref());
                    if tx.send((batch, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut done = 0;
        for (batch, r) in rx {
            match r {
                Ok(videos) => {
                    for id in &batch {
                        let mine: Vec<Video> = videos.iter().filter(|v| &v.channel_id == id).cloned().collect();
                        if mine.is_empty() {
                            still_failed += 1;
                            mark_failed(&conn, id, "no videos from RSS or yt-dlp")?;
                        } else {
                            stored += store(&conn, id, &mine)?;
                        }
                    }
                }
                Err(e) => {
                    for id in &batch {
                        still_failed += 1;
                        mark_failed(&conn, id, &e.to_string())?;
                    }
                }
            }
            done += batch.len();
            progress(Progress::Fallback { done, total: fallback.len() });
        }
    }
    progress(Progress::Finished { videos: stored, failed: still_failed });
    Ok(())
}

fn mark_failed(conn: &Connection, channel_id: &str, err: &str) -> Result<()> {
    conn.execute("UPDATE channels SET fetch_error = ?2 WHERE id = ?1", params![channel_id, err])?;
    Ok(())
}

/// Save a channel's latest videos. Returns how many were saved.
fn store(conn: &Connection, channel_id: &str, videos: &[Video]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    for v in videos {
        // An exact RSS time is never replaced by yt-dlp's day-only date.
        tx.execute(
            "INSERT INTO videos (id, channel_id, title, published, views, length, description)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 views = COALESCE(excluded.views, videos.views),
                 length = COALESCE(excluded.length, videos.length),
                 description = COALESCE(excluded.description, videos.description),
                 published = CASE WHEN ?8 THEN excluded.published ELSE videos.published END",
            params![v.id, channel_id, v.title, v.published, v.views, v.length, v.description, v.exact_time],
        )?;
    }
    tx.execute("UPDATE channels SET last_fetch = ?2, fetch_error = NULL WHERE id = ?1", params![channel_id, crate::db::now()])?;
    tx.commit()?;
    Ok(videos.len())
}

fn fetch_rss(channel_id: &str) -> Result<Vec<Video>> {
    let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
    let body = crate::yt::agent_for(crate::yt::Purpose::Feed)
        .get(&url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .call()?
        .body_mut()
        .read_to_string()?;
    let videos = parse_rss(&body, channel_id)?;
    if videos.is_empty() {
        return Err(anyhow!("empty feed"));
    }
    Ok(videos)
}

/// Parse a YouTube channel feed. Shorts are left out.
pub fn parse_rss(xml: &str, channel_id: &str) -> Result<Vec<Video>> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<(Video, bool)> = None; // (video, is_short)
    let mut path: Vec<String> = Vec::new();
    loop {
        match r.read_event()? {
            Event::Start(e) => {
                let name = e.name().as_ref().to_owned();
                if name == "entry" {
                    cur = Some((Video { channel_id: channel_id.into(), exact_time: true, ..Default::default() }, false));
                }
                path.push(name);
            }
            Event::Empty(e) => {
                let name = e.name();
                if let Some((v, short)) = cur.as_mut() {
                    let attr = |k: &str| e.attributes().flatten().find(|a| a.key.as_ref() == k).map(|a| a.value.into_owned());
                    match name.as_ref() {
                        "link" => {
                            if attr("href").is_some_and(|h| h.contains("/shorts/")) {
                                *short = true;
                            }
                        }
                        "media:statistics" => v.views = attr("views").and_then(|x| x.parse().ok()),
                        _ => {}
                    }
                }
            }
            Event::Text(t) => {
                if let (Some((v, _)), Some(tag)) = (cur.as_mut(), path.last()) {
                    let text = t.xml10_content().into_owned();
                    match tag.as_str() {
                        "yt:videoId" => v.id = text,
                        "title" if path.iter().any(|p| p == "entry") && !path.iter().any(|p| p == "media:group") => v.title.push_str(&text),
                        "published" => v.published = parse_time(&text).unwrap_or(0),
                        "media:description" => v.description.get_or_insert_default().push_str(&text),
                        _ => {}
                    }
                }
            }
            Event::GeneralRef(e) => {
                // Entities like &amp; arrive separately; append them to the current text field.
                if let (Some((v, _)), Some(tag)) = (cur.as_mut(), path.last()) {
                    let ch = e
                        .resolve_char_ref()?
                        .map(String::from)
                        .or_else(|| quick_xml::escape::resolve_predefined_entity(&e).map(String::from));
                    if let Some(ch) = ch {
                        match tag.as_str() {
                            "title" if !path.iter().any(|p| p == "media:group") => v.title.push_str(&ch),
                            "media:description" => v.description.get_or_insert_default().push_str(&ch),
                            _ => {}
                        }
                    }
                }
            }
            Event::End(e) => {
                path.pop();
                if e.name().as_ref() == "entry" {
                    if let Some((v, short)) = cur.take() {
                        if !short && !v.id.is_empty() {
                            out.push(v);
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// RFC 3339 time ("2026-10-21T14:03:11+00:00") to unix seconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days since 1970-01-01 (proleptic Gregorian), from Howard Hinnant's algorithm.
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = days * 86400 + h * 3600 + mi * 60 + se;
    // Offset suffix like +04:00 or Z.
    let rest = &s[19..];
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        let off = oh * 3600 + om * 60;
        t -= if sign == '+' { off } else { -off };
    }
    Some(t)
}

/// yt-dlp's arguments for the newest uploads of these channels.
fn feed_args(channels: &[String], proxy: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["--flat-playlist", "-j", "--ignore-errors", "--no-warnings", "--playlist-items"].map(Into::into).into();
    a.push(format!("1:{PER_CHANNEL}"));
    a.extend(["--extractor-args".into(), "youtubetab:approximate_date".into()]);
    if let Some(p) = proxy {
        a.extend(["--proxy".into(), p.to_owned()]);
    }
    a.push("--".into());
    a.extend(channels.iter().map(|c| format!("https://www.youtube.com/channel/{c}/videos")));
    a
}

fn fetch_ytdlp(channels: &[String], proxy: Option<&str>) -> Result<Vec<Video>> {
    let out = crate::proc::ytdlp().args(feed_args(channels, proxy)).stdin(Stdio::null()).stderr(Stdio::null()).output()?;
    let mut videos = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let get = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_owned);
        let (Some(id), Some(ch)) = (get("id"), get("playlist_channel_id")) else { continue };
        videos.push(Video {
            id,
            channel_id: ch,
            channel: get("playlist_channel").unwrap_or_default(),
            title: get("title").unwrap_or_default(),
            published: e.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
            views: e.get("view_count").and_then(Value::as_i64),
            length: e.get("duration").and_then(Value::as_f64),
            description: None,
            exact_time: false,
        });
    }
    if videos.is_empty() && !out.status.success() {
        return Err(anyhow!("yt-dlp failed ({})", out.status));
    }
    Ok(videos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_fallback_uses_the_proxy() {
        let ch = vec!["UCaaaaaaaaaaaaaaaaaaaaaa".to_owned()];
        let a = feed_args(&ch, Some("socks5h://127.0.0.1:9050")).join(" ");
        assert!(a.contains("--proxy socks5h://127.0.0.1:9050 -- https://www.youtube.com/channel/UCaaaaaaaaaaaaaaaaaaaaaa/videos"), "{a}");
        assert!(!feed_args(&ch, None).contains(&"--proxy".to_owned()));
    }

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns:media="http://search.yahoo.com/mrss/" xmlns="http://www.w3.org/2005/Atom">
 <title>Some Channel</title>
 <entry>
  <id>yt:video:abc123</id>
  <yt:videoId>abc123</yt:videoId>
  <title>Fish &amp; Chips</title>
  <link rel="alternate" href="https://www.youtube.com/watch?v=abc123"/>
  <published>2026-10-21T14:03:11+00:00</published>
  <media:group>
   <media:title>Fish &amp; Chips</media:title>
   <media:description>A &lt;b&gt; desc</media:description>
   <media:community><media:statistics views="1234"/></media:community>
  </media:group>
 </entry>
 <entry>
  <yt:videoId>short1</yt:videoId>
  <title>A short</title>
  <link rel="alternate" href="https://www.youtube.com/shorts/short1"/>
  <published>2026-10-20T00:00:00+00:00</published>
 </entry>
</feed>"#;

    #[test]
    fn parses_entries_and_skips_shorts() {
        let v = parse_rss(SAMPLE, "UCx").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "abc123");
        assert_eq!(v[0].title, "Fish & Chips");
        assert_eq!(v[0].views, Some(1234));
        assert_eq!(v[0].published, 1792591391);
        assert_eq!(v[0].description.as_deref(), Some("A <b> desc"));
    }

    #[test]
    fn time_offsets() {
        assert_eq!(parse_time("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_time("1970-01-01T04:00:00+04:00"), Some(0));
        assert_eq!(parse_time("2026-10-21T14:03:11Z"), Some(1792591391));
    }
}
