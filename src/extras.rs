//! Community extras FreeTube also shows: Return YouTube Dislike's vote counts
//! and DeArrow's crowd-sourced titles and thumbnails.
//!
//! Both go through `yt::agent()`, so they use the configured proxy too.

// Parsed data carries every field YouTube gives us, even ones no screen shows yet,
// so contributors can use them without touching the parsers.
#![allow(dead_code)]

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Return YouTube Dislike

/// Votes as estimated by Return YouTube Dislike (dislikes are extrapolated
/// from its users' votes since YouTube hid the real count).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Votes {
    pub likes: i64,
    pub dislikes: i64,
    /// 1 to 5 stars, the way YouTube used to show ratings.
    pub rating: f64,
    pub views: Option<i64>,
}

pub fn dislikes(id: &str) -> Result<Votes> {
    let body =
        crate::yt::agent().get(&format!("https://returnyoutubedislikeapi.com/votes?videoId={id}")).call()?.body_mut().read_to_string()?;
    parse_votes(&body)
}

pub fn parse_votes(json: &str) -> Result<Votes> {
    let v: Value = serde_json::from_str(json)?;
    Ok(Votes {
        likes: v["likes"].as_i64().unwrap_or(0),
        dislikes: v["dislikes"].as_i64().unwrap_or(0),
        rating: v["rating"].as_f64().unwrap_or(0.0),
        views: v["viewCount"].as_i64(),
    })
}

// ---------------------------------------------------------------------------
// DeArrow

/// What DeArrow suggests for a video. Fields are None when the community
/// hasn't replaced that part (or voted to keep the original).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Branding {
    pub title: Option<String>,
    /// Second of the video to use as thumbnail; see [`dearrow_thumbnail_url`].
    pub thumbnail_time: Option<f64>,
    /// DeArrow's per-video random moment (seconds), for clients that show a
    /// random frame when nobody submitted a thumbnail. Only known when
    /// DeArrow knows the video's length.
    pub random_time: Option<f64>,
}

/// The first 4 hex digits of the video id's SHA-256. DeArrow answers for
/// every video with that prefix, so it never learns which one we wanted.
pub fn hash_prefix(id: &str) -> String {
    Sha256::digest(id.as_bytes()).iter().take(2).map(|b| format!("{b:02x}")).collect()
}

/// DeArrow's suggestion for one video. A video nobody submitted anything for
/// gives `Branding::default()`.
pub fn dearrow(id: &str) -> Result<Branding> {
    Ok(parse_branding(&fetch_prefix(&hash_prefix(id))?, id))
}

/// DeArrow for many videos at once (a feed page): one request per hash
/// prefix, a few at a time. Videos without suggestions, or whose request
/// failed, are left out of the map.
pub fn dearrow_many(ids: &[&str]) -> HashMap<String, Branding> {
    let mut by_prefix: HashMap<String, Vec<&str>> = HashMap::new();
    for id in ids {
        by_prefix.entry(hash_prefix(id)).or_default().push(id);
    }
    let groups: Vec<(String, Vec<&str>)> = by_prefix.into_iter().collect();
    let mut out = HashMap::new();
    // Eight requests in flight is quick without hammering a volunteer-run server.
    for chunk in groups.chunks(8) {
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|(prefix, ids)| s.spawn(move || (ids, fetch_prefix(prefix)))).collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (ids, r) in results {
            let Ok(v) = r else { continue };
            for id in ids.iter() {
                let b = parse_branding(&v, id);
                if b != Branding::default() {
                    out.insert((*id).to_owned(), b);
                }
            }
        }
    }
    out
}

fn fetch_prefix(prefix: &str) -> Result<Value> {
    let r = crate::yt::agent().get(&format!("https://sponsor.ajay.app/api/branding/{prefix}")).call();
    match r {
        Ok(mut resp) => Ok(serde_json::from_str(&resp.body_mut().with_config().limit(8 * 1024 * 1024).read_to_string()?)?),
        // 404 just means no video with this prefix has any submissions.
        Err(ureq::Error::StatusCode(404)) => Ok(Value::Object(Default::default())),
        Err(e) => Err(e.into()),
    }
}

/// Picks our video's entry out of a prefix response.
///
/// DeArrow sorts submissions best first. Like the DeArrow extension, we take
/// the first one that is locked by a moderator or not voted down, and treat
/// an "original" winner as "keep YouTube's own".
pub fn parse_branding(v: &Value, id: &str) -> Branding {
    let entry = &v[id];
    let usable = |x: &&Value| x["locked"].as_bool() == Some(true) || x["votes"].as_i64().unwrap_or(0) >= 0;
    let title = entry["titles"]
        .as_array()
        .and_then(|t| t.iter().find(usable))
        .filter(|t| t["original"].as_bool() != Some(true))
        .and_then(|t| t["title"].as_str())
        .map(clean_title)
        .filter(|t| !t.is_empty());
    let thumbnail_time = entry["thumbnails"]
        .as_array()
        .and_then(|t| t.iter().find(usable))
        .filter(|t| t["original"].as_bool() != Some(true))
        .and_then(|t| t["timestamp"].as_f64());
    let random_time = match (entry["randomTime"].as_f64(), entry["videoDuration"].as_f64()) {
        (Some(r), Some(d)) if d > 0.0 => Some(r * d),
        _ => None,
    };
    Branding { title, thumbnail_time, random_time }
}

/// DeArrow titles may put `>` in front of a word to say "don't auto-format
/// this word" (keep "iPhone" from becoming "Iphone"). It is a marker for
/// formatters, never meant to be shown, so it is removed.
pub fn clean_title(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut prev_space = true;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '>' && prev_space && chars.peek().is_some_and(|n| !n.is_whitespace()) {
            continue;
        }
        prev_space = c.is_whitespace();
        out.push(c);
    }
    out.trim().to_owned()
}

/// The image of `time` seconds into the video, rendered by DeArrow's thumbnail server.
pub fn dearrow_thumbnail_url(id: &str, time: f64) -> String {
    format!("https://dearrow-thumb.ajay.app/api/v1/getThumbnail?videoID={id}&time={time}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix() {
        // sha256("dQw4w9WgXcQ") starts with 5f6b.
        assert_eq!(hash_prefix("dQw4w9WgXcQ"), "5f6b");
    }

    #[test]
    fn votes_fixture() {
        let v = parse_votes(include_str!("../tests/fixtures/yt/ryd-votes.json")).unwrap();
        assert_eq!(v, Votes { likes: 19491666, dislikes: 521552, rating: 4.8957584932118365, views: Some(1828950179) });
    }

    #[test]
    fn branding_fixture() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/yt/dearrow-branding.json")).unwrap();
        // Locked title wins; the locked thumbnail too.
        let b = parse_branding(&v, "dQw4w9WgXcQ");
        assert_eq!(b.title.as_deref(), Some("Rick Astley - Never Gonna Give You Up (Official Music Video)"));
        assert_eq!(b.thumbnail_time, Some(3.92349));
        assert_eq!(b.random_time, Some(0.5 * 212.0));
        // "original" winners mean "keep YouTube's"; voted-down titles are skipped.
        let b = parse_branding(&v, "originalWin");
        assert_eq!((b.title, b.thumbnail_time), (None, None));
        let b = parse_branding(&v, "downvoted");
        assert_eq!(b.title.as_deref(), Some("The fair title"));
        assert_eq!(b.random_time, None);
        // Not in the response at all.
        assert_eq!(parse_branding(&v, "missing"), Branding::default());
    }

    #[test]
    fn title_markers() {
        assert_eq!(clean_title(">iPhone 17 review: the >USB-C one"), "iPhone 17 review: the USB-C one");
        assert_eq!(clean_title("a -> b and 3 > 2"), "a -> b and 3 > 2");
    }

    #[test]
    fn thumb_url() {
        assert_eq!(
            dearrow_thumbnail_url("dQw4w9WgXcQ", 3.5),
            "https://dearrow-thumb.ajay.app/api/v1/getThumbnail?videoID=dQw4w9WgXcQ&time=3.5"
        );
    }

    #[test]
    #[ignore]
    fn live_extras() {
        let v = dislikes("dQw4w9WgXcQ").unwrap();
        println!("RYD: {v:?}");
        assert!(v.likes > 1_000_000 && v.dislikes > 0);
        // Popular videos with DeArrow submissions.
        let ids = ["dQw4w9WgXcQ", "jNQXAC9IVRw", "kJQP7kiw5Fk", "9bZkp7q19f0", "BpPEoZW5IiY"];
        for id in &ids[..2] {
            println!("DeArrow {id}: {:?}", dearrow(id).unwrap());
        }
        let many = dearrow_many(&ids);
        println!("DeArrow many: {many:#?}");
        assert!(many.values().any(|b| b.title.is_some()));
    }
}
