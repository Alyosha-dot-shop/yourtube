//! SponsorBlock (sponsor.ajay.app): crowd-sourced timestamps of sponsors, intros and
//! other segments, so the player can skip them.
//!
//! Privacy: we never send the video ID. The API lets us ask by the first four hex
//! digits of the ID's SHA-256 hash instead; the server answers with every video
//! sharing that prefix (usually a few dozen), and we pick ours out locally. The
//! server can't tell which of them we're watching.
//!
//! This module has two halves:
//! - [`fetch`] / [`parse_response`]: the lookup.
//! - [`Skipper`]: a small state machine the player feeds the playback position into.
//!   It decides when to skip, when to offer a skip ("ask"), and re-arms segments
//!   when you seek back before them. It knows nothing about mpv, so it's easy to test.

// Some helpers and fields aren't used by a screen yet; they're kept for the web version
// and for contributors.
#![allow(dead_code)]

use crate::config::Config;
use anyhow::{Result, anyhow};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// What to do when playback enters a segment of a category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Jump over it straight away.
    Skip,
    /// Offer it: show a hint and let Enter skip it.
    Ask,
}

impl Mode {
    /// "skip" / "ask" from the config; "off" (or anything else) means the category is ignored.
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "skip" => Some(Mode::Skip),
            "ask" => Some(Mode::Ask),
            _ => None,
        }
    }
}

/// One segment of one video, in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub category: String,
    pub start: f64,
    pub end: f64,
    /// SponsorBlock's ID for the segment (handy if we ever report "skipped" views back).
    pub uuid: String,
}

/// What the lookup needs, taken from the config.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub api: String,
    /// Enabled categories only, with what to do for each.
    pub categories: Vec<(String, Mode)>,
    pub proxy: Option<String>,
}

impl Settings {
    /// None when SponsorBlock is off or every category is "off": then we don't ask the server at all.
    pub fn from_config(c: &Config) -> Option<Settings> {
        if !c.sponsorblock {
            return None;
        }
        let categories: Vec<(String, Mode)> =
            c.sponsorblock_categories.iter().filter_map(|(k, v)| Some((k.clone(), Mode::parse(v)?))).collect();
        (!categories.is_empty()).then(|| Settings {
            api: c.sponsorblock_api.trim_end_matches('/').to_owned(),
            categories,
            proxy: c.proxy.clone(),
        })
    }

    pub fn mode(&self, category: &str) -> Option<Mode> {
        self.categories.iter().find(|(k, _)| k == category).map(|(_, m)| *m)
    }
}

/// A friendly name for a category ("selfpromo" → "Self-promotion").
pub fn label(category: &str) -> &str {
    crate::config::SPONSORBLOCK_DEFAULTS.iter().find(|(k, _, _)| *k == category).map_or(category, |(_, l, _)| l)
}

/// The first four hex digits of SHA-256(video ID): all the server learns about what we watch.
pub fn hash_prefix(video_id: &str) -> String {
    let digest = Sha256::digest(video_id.as_bytes());
    digest.iter().take(2).map(|b| format!("{b:02x}")).collect()
}

/// The JSON list of categories the API expects, e.g. `["sponsor","intro"]`.
pub fn categories_param(s: &Settings) -> String {
    Value::from(s.categories.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()).to_string()
}

/// Look up a video's segments (only the enabled categories). Short timeout: playback
/// never waits for this, but a slow server shouldn't leave a thread hanging for long.
pub fn fetch(s: &Settings, video_id: &str) -> Result<Vec<Segment>> {
    let mut cfg = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(6))).http_status_as_error(false);
    if let Some(p) = &s.proxy {
        cfg = cfg.proxy(Some(ureq::Proxy::new(&crate::yt::isolated_proxy(p, "sponsorblock"))?));
    }
    let mut resp = cfg
        .build()
        .new_agent()
        .get(&format!("{}/api/skipSegments/{}", s.api, hash_prefix(video_id)))
        .query("categories", categories_param(s))
        .call()?;
    match resp.status().as_u16() {
        // 404 means "no video with this prefix has segments", which is a normal answer.
        404 => Ok(Vec::new()),
        200 => parse_response(&serde_json::from_str(&resp.body_mut().read_to_string()?)?, video_id),
        code => Err(anyhow!("SponsorBlock answered {code}")),
    }
}

/// Pick our video out of a hash-prefix answer: `[{videoID, segments: [{category, actionType, segment: [s, e], UUID}]}]`.
/// Only "skip" actions are kept: "mute", "full" (the whole video is a sponsor) and
/// "poi" (a highlight) aren't things you can jump over.
pub fn parse_response(v: &Value, video_id: &str) -> Result<Vec<Segment>> {
    let videos = v.as_array().ok_or_else(|| anyhow!("SponsorBlock's answer isn't a list"))?;
    let Some(ours) = videos.iter().find(|x| x["videoID"].as_str() == Some(video_id)) else {
        return Ok(Vec::new());
    };
    let mut segs: Vec<Segment> = ours["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["actionType"].as_str().unwrap_or("skip") == "skip")
        .filter_map(|s| {
            let (start, end) = (s["segment"][0].as_f64()?, s["segment"][1].as_f64()?);
            // Sub-second segments are noise; skipping them is just a stutter.
            (end - start >= 1.0).then(|| Segment {
                category: s["category"].as_str().unwrap_or_default().to_owned(),
                start,
                end,
                uuid: s["UUID"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect();
    segs.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(segs)
}

/// What the player should do after feeding the position into the [`Skipper`].
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Seek to `to`; `from` is where we were, so `to - from` seconds were saved.
    Skip { category: String, from: f64, to: f64 },
    /// Offer a skip: show the hint and bind Enter until [`Action::EndAsk`].
    Ask { category: String, remaining: f64 },
    /// The offered segment is over (or was skipped): take the hint and binding away.
    EndAsk,
}

/// How close to a segment's end we stop bothering: skipping the last half second is
/// just a stutter, and mpv sometimes lands a hair before the exact end after a seek.
const TAIL: f64 = 0.5;

struct Armed {
    seg: Segment,
    mode: Mode,
    /// Skipped (or offered) already; stays so until playback goes back before the start.
    used: bool,
}

/// Decides skips and offers from the playback position. Feed it with [`Skipper::tick`]
/// every time you read `time-pos`, and with [`Skipper::skip_requested`] when the user
/// presses Enter.
pub struct Skipper {
    segs: Vec<Armed>,
    asking: Option<usize>,
    saved: f64,
}

impl Skipper {
    /// Segments whose category isn't enabled in `settings` are dropped here.
    pub fn new(segments: Vec<Segment>, settings: &Settings) -> Skipper {
        let segs = segments.into_iter().filter_map(|seg| Some(Armed { mode: settings.mode(&seg.category)?, seg, used: false })).collect();
        Skipper { segs, asking: None, saved: 0.0 }
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// Seconds skipped so far in this video.
    pub fn saved(&self) -> f64 {
        self.saved
    }

    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segs.iter().map(|a| &a.seg)
    }

    pub fn tick(&mut self, pos: f64) -> Vec<Action> {
        let mut out = Vec::new();
        // Going back before a segment (seeking, or a loop restarting the video) means
        // you'll meet it again, so it should be skipped or offered again.
        for a in &mut self.segs {
            if pos < a.seg.start - 0.25 {
                a.used = false;
            }
        }
        if let Some(i) = self.asking {
            let s = &self.segs[i].seg;
            if pos < s.start || pos >= s.end - TAIL {
                self.asking = None;
                out.push(Action::EndAsk);
            }
        }
        // At most one decision per tick; if a skip lands inside the next segment,
        // the next tick handles that one.
        let hit = self.segs.iter().position(|a| !a.used && pos >= a.seg.start && pos < a.seg.end - TAIL);
        if let Some(i) = hit {
            let a = &mut self.segs[i];
            a.used = true;
            match a.mode {
                Mode::Skip => {
                    self.saved += a.seg.end - pos;
                    out.push(Action::Skip { category: a.seg.category.clone(), from: pos, to: a.seg.end });
                }
                Mode::Ask => {
                    let ask = Action::Ask { category: a.seg.category.clone(), remaining: a.seg.end - pos };
                    if self.asking.replace(i).is_some() {
                        out.push(Action::EndAsk);
                    }
                    out.push(ask);
                }
            }
        }
        out
    }

    /// Enter was pressed during an offer: skip the offered segment if we're still in it.
    pub fn skip_requested(&mut self, pos: f64) -> Vec<Action> {
        let Some(i) = self.asking.take() else { return Vec::new() };
        let s = &self.segs[i].seg;
        let mut out = vec![Action::EndAsk];
        if pos >= s.start - 0.25 && pos < s.end {
            self.saved += s.end - pos.max(s.start);
            out.push(Action::Skip { category: s.category.clone(), from: pos, to: s.end });
        }
        out
    }
}

/// "0:42" or "1:02:03" for OSD messages.
pub fn clock(secs: f64) -> String {
    let t = secs.max(0.0).round() as u64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            api: "https://sponsor.ajay.app".into(),
            categories: vec![("sponsor".into(), Mode::Skip), ("intro".into(), Mode::Ask)],
            proxy: None,
        }
    }

    fn seg(cat: &str, start: f64, end: f64) -> Segment {
        Segment { category: cat.into(), start, end, uuid: String::new() }
    }

    #[test]
    fn hash_prefix_matches_sha256() {
        // sha256("jNQXAC9IVRw") — checked with `printf jNQXAC9IVRw | sha256sum`.
        assert_eq!(hash_prefix("jNQXAC9IVRw"), "6745");
        assert_eq!(hash_prefix("").len(), 4);
        assert_eq!(hash_prefix(""), "e3b0"); // the well-known empty-string hash
    }

    #[test]
    fn settings_from_config() {
        let mut c = Config::default();
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.mode("sponsor"), Some(Mode::Skip));
        assert_eq!(s.mode("intro"), Some(Mode::Ask));
        assert_eq!(s.mode("filler"), None, "off categories aren't even requested");
        assert!(!categories_param(&s).contains("filler"));
        assert!(categories_param(&s).starts_with("[\""));
        c.sponsorblock = false;
        assert!(Settings::from_config(&c).is_none());
        c.sponsorblock = true;
        for v in c.sponsorblock_categories.values_mut() {
            *v = "off".into();
        }
        assert!(Settings::from_config(&c).is_none());
    }

    #[test]
    fn picks_our_video_from_the_prefix_answer() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/player/skipsegments.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let segs = parse_response(&v, "ourVideo123").unwrap();
        assert_eq!(segs.len(), 2, "mute/poi actions and the tiny segment are dropped: {segs:?}");
        assert_eq!(segs[0], Segment { category: "intro".into(), start: 3.0, end: 12.5, uuid: "b".into() }, "sorted by start");
        assert_eq!(segs[1].category, "sponsor");
        assert!(parse_response(&v, "notThere").unwrap().is_empty());
        assert!(parse_response(&serde_json::json!({"x": 1}), "a").is_err());
    }

    #[test]
    fn skips_once_and_rearms_after_seeking_back() {
        let mut k = Skipper::new(vec![seg("sponsor", 30.0, 60.0), seg("filler", 70.0, 80.0)], &settings());
        assert_eq!(k.segments().count(), 1, "disabled categories are dropped");
        assert!(k.tick(10.0).is_empty());
        assert!(k.tick(29.9).is_empty());
        assert_eq!(k.tick(30.2), vec![Action::Skip { category: "sponsor".into(), from: 30.2, to: 60.0 }]);
        // mpv hasn't caught up with the seek yet, or the user seeks back into the middle:
        // don't fight them.
        assert!(k.tick(30.5).is_empty());
        assert!(k.tick(45.0).is_empty());
        assert!(k.tick(59.8).is_empty(), "the last half second isn't worth a skip");
        assert!((k.saved() - 29.8).abs() < 1e-9);
        // Back before the start: it's armed again.
        assert!(k.tick(20.0).is_empty());
        assert!(matches!(k.tick(31.0).as_slice(), [Action::Skip { .. }]));
        assert!((k.saved() - 58.8).abs() < 1e-9);
    }

    #[test]
    fn starting_inside_a_segment_skips_it() {
        let mut k = Skipper::new(vec![seg("sponsor", 0.0, 15.0)], &settings());
        assert!(matches!(k.tick(0.0).as_slice(), [Action::Skip { to, .. }] if *to == 15.0));
    }

    #[test]
    fn ask_offers_then_ends_or_skips_on_enter() {
        let mut k = Skipper::new(vec![seg("intro", 5.0, 20.0)], &settings());
        assert_eq!(k.tick(6.0), vec![Action::Ask { category: "intro".into(), remaining: 14.0 }]);
        assert!(k.tick(7.0).is_empty(), "offered once");
        assert_eq!(k.tick(19.6), vec![Action::EndAsk], "binding goes away near the end");
        assert!(k.skip_requested(19.7).is_empty(), "Enter after the offer does nothing");
        // Seek back and accept this time.
        assert!(k.tick(1.0).is_empty());
        assert!(matches!(k.tick(5.0).as_slice(), [Action::Ask { .. }]));
        assert_eq!(k.skip_requested(8.0), vec![Action::EndAsk, Action::Skip { category: "intro".into(), from: 8.0, to: 20.0 }]);
        assert_eq!(k.saved(), 12.0);
        assert!(k.tick(20.0).is_empty());
        // Seeking back out of an offer before it ends also takes the binding away.
        k.tick(0.0);
        k.tick(6.0);
        assert_eq!(k.tick(2.0), vec![Action::EndAsk]);
    }

    #[test]
    fn back_to_back_segments_are_handled_one_per_tick() {
        let mut k = Skipper::new(vec![seg("sponsor", 10.0, 20.0), seg("sponsor", 20.0, 30.0), seg("intro", 30.0, 40.0)], &settings());
        assert!(matches!(k.tick(10.0).as_slice(), [Action::Skip { to, .. }] if *to == 20.0));
        assert!(matches!(k.tick(20.0).as_slice(), [Action::Skip { to, .. }] if *to == 30.0));
        assert!(matches!(k.tick(30.0).as_slice(), [Action::Ask { .. }]));
        assert_eq!(k.saved(), 20.0);
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(42.4), "0:42");
        assert_eq!(clock(62.0), "1:02");
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(label("selfpromo"), "Self-promotion");
        assert_eq!(label("weird"), "weird");
    }

    /// Real lookups (network): `cargo test live_lookup -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_lookup() {
        let s = Settings::from_config(&Config::default()).unwrap();
        let mut found = 0;
        for id in ["L_jWHffIx5E", "LGA4ZFr-hfo", "aV81SqB6HQg", "jNQXAC9IVRw"] {
            let segs = fetch(&s, id).unwrap();
            println!("{id} (prefix {}): {segs:?}", hash_prefix(id));
            found += segs.len();
        }
        assert!(found > 0);
    }
}
