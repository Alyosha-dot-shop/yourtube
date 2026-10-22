//! What you're into right now, built only from data on this computer.
//!
//! Signals, strongest first:
//! - **searches** you made in YourTube (recent ones count most),
//! - **videos you watched**, weighted by how much of each you watched,
//! - **feedback** on For You picks ("more like this", "not interested", "fewer from this channel"),
//! - **subscriptions**.
//!
//! Everything decays with age, so the page follows what you're into lately.

use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

const DAY: f64 = 86400.0;
pub const SEARCH_HALF_LIFE_DAYS: f64 = 10.0;
/// Older searches still say what you're into for the long run; they fade slowly and count less.
pub const LONG_SEARCH_HALF_LIFE_DAYS: f64 = 180.0;
const LONG_SEARCH_WEIGHT: f64 = 0.6;
const LONG_SEARCH_WINDOW_DAYS: i64 = 3 * 365;
pub const WATCH_HALF_LIFE_DAYS: f64 = 21.0;
const SEARCH_WINDOW_DAYS: i64 = 60;
const WATCH_WINDOW_DAYS: i64 = 120;
/// A search says more about what you want than a single watch does.
const SEARCH_WEIGHT: f64 = 3.0;

/// Words that say nothing about a video's topic.
const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from", "has", "have", "how", "i", "if", "in",
    "into", "is", "it", "its", "it's", "just", "me", "my", "no", "not", "of", "on", "or", "our", "out", "so", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "to", "too", "up", "us", "vs", "was", "we", "what", "when", "where",
    "which", "who", "why", "will", "with", "you", "your", "you're", "i'm", "ft", "feat", "official", "video", "videos", "new", "full",
    "part", "episode", "ep", "pt", "best", "most", "ever", "every", "all", "get", "got", "make", "made", "one", "two", "first", "day",
    "days", "time", "here", "now", "live", "shorts", "short", "watch", "youtube", "really", "very", "much", "more", "some", "about",
    "after", "before", "over", "only", "like", "go", "going", "did", "would", "should", "could", "been", "being", "had", "him", "her",
    "his", "she", "he",
];

/// Lowercase topic words of a title or query.
pub fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| w.chars().count() >= 2 && !w.chars().all(|c| c.is_ascii_digit()) && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

pub fn decay(age_secs: i64, half_life_days: f64) -> f64 {
    0.5f64.powf((age_secs.max(0) as f64 / DAY) / half_life_days)
}

/// A recently watched video, used to find related ones.
#[derive(Clone, Debug)]
pub struct Seed {
    pub id: String,
    pub title: String,
    pub weight: f64,
}

/// A video you started but didn't finish.
#[derive(Clone, Debug)]
pub struct Unfinished {
    pub id: String,
    pub position: f64,
    pub length: f64,
    pub watched_at: i64,
}

#[derive(Default, Debug)]
pub struct Profile {
    /// Topic word → interest weight.
    pub terms: HashMap<String, f64>,
    /// Channel id → affinity.
    pub channels: HashMap<String, f64>,
    /// Distinct searches: (query, weight, last searched), strongest first.
    pub searches: Vec<(String, f64, i64)>,
    /// Recently watched videos, strongest first.
    pub seeds: Vec<Seed>,
    pub unfinished: Vec<Unfinished>,
    /// Every video in your history → watched to more than half (or unknown length).
    pub seen: HashMap<String, bool>,
    pub subscribed: HashSet<String>,
    pub hidden: HashSet<String>,
    pub blocked_channels: HashSet<String>,
    /// Video id → (times shown in earlier runs, clicked).
    pub shown: HashMap<String, (i64, bool)>,
    /// Titles you said "not interested" to (for the AI prompt).
    pub disliked: Vec<String>,
}

impl Profile {
    fn add_terms(&mut self, text: &str, w: f64) {
        let toks: HashSet<String> = tokens(text).into_iter().collect();
        for t in toks {
            *self.terms.entry(t).or_default() += w;
        }
    }

    /// The strongest topic words, for reasons and prompts.
    pub fn top_terms(&self, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self.terms.iter().filter(|(_, w)| **w > 0.0).map(|(t, w)| (t.clone(), *w)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

pub fn build(conn: &Connection, now: i64) -> Result<Profile> {
    let mut p = Profile::default();

    // Searches.
    // Searches: a fast-fading "lately" part plus a slow "for the long run" part.
    let mut by_query: HashMap<String, (String, f64, i64)> = HashMap::new();
    for (q, at) in crate::db::recent_searches(conn, now - LONG_SEARCH_WINDOW_DAYS * 86400)? {
        let mut w = LONG_SEARCH_WEIGHT * decay(now - at, LONG_SEARCH_HALF_LIFE_DAYS);
        if now - at <= SEARCH_WINDOW_DAYS * 86400 {
            w += SEARCH_WEIGHT * decay(now - at, SEARCH_HALF_LIFE_DAYS);
        }
        p.add_terms(&q, w);
        let e = by_query.entry(q.to_lowercase()).or_insert((q.clone(), 0.0, at));
        e.1 += w;
        e.2 = e.2.max(at);
    }
    p.searches = by_query.into_values().collect();
    p.searches.sort_by(|a, b| b.1.total_cmp(&a.1));

    // Watch history.
    let mut st = conn.prepare(
        "SELECT h.video_id, h.title, COALESCE(h.author_id, v.channel_id), h.watched_at, h.position, COALESCE(h.length, v.length)
         FROM history h LEFT JOIN videos v ON v.id = h.video_id",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, Option<f64>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, title, channel, at, pos, len) = row?;
        let completion = match len {
            Some(l) if l > 0.0 => (pos / l).clamp(0.0, 1.0),
            // Length unknown (much of an imported history): judge by the position alone.
            _ if pos > 120.0 => 0.7,
            _ if pos > 0.0 => 0.4,
            _ => 0.3,
        };
        p.seen.insert(id.clone(), completion >= 0.5);
        if now - at > WATCH_WINDOW_DAYS * 86400 {
            continue;
        }
        let w = (0.25 + completion) * decay(now - at, WATCH_HALF_LIFE_DAYS);
        p.add_terms(&title, w);
        if let Some(c) = channel.filter(|c| !c.is_empty()) {
            *p.channels.entry(c).or_default() += w;
        }
        if let Some(l) = len.filter(|l| *l > 0.0) {
            if pos >= 30.0 && pos / l < 0.9 && now - at < 14 * 86400 {
                p.unfinished.push(Unfinished { id: id.clone(), position: pos, length: l, watched_at: at });
            }
        }
        if completion >= 0.4 {
            p.seeds.push(Seed { id, title, weight: w });
        }
    }
    p.seeds.sort_by(|a, b| b.weight.total_cmp(&a.weight));
    p.unfinished.sort_by_key(|u| -u.watched_at);

    // Subscriptions: a small, steady affinity.
    let mut st = conn.prepare("SELECT id FROM channels WHERE subscribed = 1")?;
    for id in st.query_map([], |r| r.get::<_, String>(0))? {
        let id = id?;
        *p.channels.entry(id.clone()).or_default() += 0.3;
        p.subscribed.insert(id);
    }

    // Feedback never decays: you said it on purpose.
    let mut st = conn.prepare("SELECT video_id, channel_id, COALESCE(title, ''), kind FROM rec_feedback")?;
    let rows =
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    for row in rows {
        let (id, channel, title, kind) = row?;
        match kind.as_str() {
            "more" => {
                p.add_terms(&title, 2.0);
                if let Some(c) = channel {
                    *p.channels.entry(c).or_default() += 2.0;
                }
            }
            "not_interested" => {
                p.add_terms(&title, -1.0);
                p.hidden.insert(id);
                p.disliked.push(title);
            }
            "fewer" => {
                if let Some(c) = channel {
                    p.blocked_channels.insert(c);
                }
            }
            _ => {}
        }
    }

    let mut st = conn.prepare("SELECT channel_id FROM blocked_channels")?;
    for id in st.query_map([], |r| r.get::<_, String>(0))? {
        p.blocked_channels.insert(id?);
    }

    let mut st = conn.prepare("SELECT video_id, times, clicked FROM rec_shown")?;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, bool>(2)?)))? {
        let (id, times, clicked) = row?;
        p.shown.insert(id, (times, clicked));
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_keep_topic_words() {
        assert_eq!(tokens("How I built a TURBO E30 (Part 2) | It's alive!"), ["built", "turbo", "e30", "alive"]);
        assert_eq!(tokens("2024 vs 2025"), Vec::<String>::new());
    }

    #[test]
    fn decay_halves() {
        assert!((decay(0, 10.0) - 1.0).abs() < 1e-9);
        assert!((decay(10 * 86400, 10.0) - 0.5).abs() < 1e-9);
    }
}
