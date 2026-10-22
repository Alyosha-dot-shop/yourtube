//! Score candidates against the profile, keep the page varied, say why each is there.
//!
//! The score is a plain weighted sum you can read and change:
//!
//! | part      | weight | meaning |
//! |-----------|--------|---------|
//! | taste     | 0.45   | how well the title matches your topic words (TF-IDF cosine) |
//! | source    | 0.15   | how strong the reason it was found is (a recent search, a recent watch…) |
//! | channel   | 0.15   | how much you watch that channel |
//! | fresh     | 0.15   | newer uploads first (3-week scale) |
//! | quality   | 0.10   | view count, on a log scale |
//!
//! Then: picks you've been shown three times and never opened sink, at most two
//! per channel and four per reason, near-duplicate titles are skipped, at least 30% of the page
//! comes from your searches and at least a third from channels you don't subscribe to (when there are enough).

use super::profile::{Profile, tokens};
use crate::db::Item;
use crate::feed::Video;
use std::collections::{HashMap, HashSet};

pub const PAGE: usize = 60;
const MAX_PER_CHANNEL: usize = 2;
/// At most this many picks for the same reason (one search, one related video).
const MAX_PER_REASON: usize = 4;
const MIN_DISCOVERY_SHARE: f64 = 0.34;
/// What you search for leads: at least this share of the page comes from your searches (when there are enough).
const MIN_SEARCH_SHARE: f64 = 0.3;
const MIN_LENGTH: f64 = 70.0; // shorter is a Short

#[derive(Clone, Debug)]
pub enum Source {
    /// `recent`: searched in the last 30 days (otherwise a long-running interest).
    Search {
        query: String,
        weight: f64,
        recent: bool,
    },
    Related {
        seed: String,
        weight: f64,
    },
    /// A search made up from your strongest topic words (when you haven't searched much).
    Interest {
        words: String,
    },
    Subscription,
    Ai {
        why: String,
    },
}

impl Source {
    fn strength(&self) -> f64 {
        match self {
            Source::Search { weight, .. } => (0.65 + weight / 6.0).min(1.0),
            Source::Related { weight, .. } => (0.35 + weight).min(0.8),
            Source::Interest { .. } => 0.6,
            Source::Subscription => 0.5,
            Source::Ai { .. } => 0.8,
        }
    }

    fn reason(&self, v: &Video) -> String {
        match self {
            Source::Search { query, recent: true, .. } => format!("Because you searched “{query}”"),
            Source::Search { query, .. } => format!("You often search for “{query}”"),
            Source::Related { seed, .. } => format!("Related to “{}”", short(seed, 60)),
            Source::Subscription => {
                format!("New from {}, a subscription", if v.channel.is_empty() { "a channel you follow" } else { &v.channel })
            }
            Source::Interest { words } => format!("Matches what you watch: {words}"),
            Source::Ai { why } => format!("AI: {why}"),
        }
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n - 1).collect::<String>().trim_end()) }
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub video: Video,
    pub source: Source,
}

#[derive(Clone, Debug)]
pub struct Scored {
    pub video: Video,
    pub score: f64,
    pub reason: String,
    pub subscribed: bool,
    pub from_search: bool,
}

/// Merge duplicates (the strongest source wins, a second source adds a little) and drop what shouldn't be shown.
fn merge(p: &Profile, cands: Vec<Candidate>) -> Vec<(Candidate, f64)> {
    let mut by_id: HashMap<String, (Candidate, f64)> = HashMap::new();
    for c in cands {
        let v = &c.video;
        if v.id.is_empty()
            || v.title.is_empty()
            || p.hidden.contains(&v.id)
            || p.blocked_channels.contains(&v.channel_id)
            || p.seen.contains_key(&v.id)
        {
            continue;
        }
        // Shorts and live streams (no length) are out, except subscription uploads from RSS, which have no length yet.
        let ok_length = match v.length {
            Some(l) => l >= MIN_LENGTH,
            None => matches!(c.source, Source::Subscription),
        };
        if !ok_length {
            continue;
        }
        match by_id.get_mut(&v.id) {
            Some((old, bonus)) => {
                *bonus += 0.05;
                if c.source.strength() > old.source.strength() {
                    let mut merged = c;
                    // Keep the most complete metadata.
                    merged.video.views = merged.video.views.or(old.video.views);
                    merged.video.length = merged.video.length.or(old.video.length);
                    *old = merged;
                }
            }
            None => {
                by_id.insert(v.id.clone(), (c, 0.0));
            }
        }
    }
    by_id.into_values().collect()
}

pub fn rank(p: &Profile, cands: Vec<Candidate>, now: i64) -> Vec<Scored> {
    let cands = merge(p, cands);
    if cands.is_empty() {
        return Vec::new();
    }
    // IDF over the candidate titles: words in every title say little.
    let docs: Vec<HashSet<String>> = cands.iter().map(|(c, _)| tokens(&c.video.title).into_iter().collect()).collect();
    let mut df: HashMap<String, f64> = HashMap::new();
    for d in &docs {
        for t in d {
            *df.entry(t.clone()).or_default() += 1.0;
        }
    }
    let n = docs.len() as f64;
    let idf = |t: &str| ((n + 1.0) / (df.get(t).copied().unwrap_or(0.0) + 1.0)).ln() + 1.0;
    let pnorm: f64 = p.terms.iter().filter(|(_, w)| **w > 0.0).map(|(t, w)| (w * idf(t)).powi(2)).sum::<f64>().sqrt().max(1e-9);
    let max_ch = p.channels.values().cloned().fold(0.0, f64::max).max(1e-9);

    let mut scored: Vec<(Scored, HashSet<String>)> = cands
        .into_iter()
        .zip(docs)
        .map(|((c, bonus), toks)| {
            let v = c.video;
            let cnorm: f64 = toks.iter().map(|t| idf(t).powi(2)).sum::<f64>().sqrt().max(1e-9);
            let dot: f64 = toks.iter().map(|t| p.terms.get(t).copied().unwrap_or(0.0) * idf(t) * idf(t)).sum();
            let taste = (dot / (pnorm * cnorm)).clamp(-1.0, 1.0);
            let channel = (1.0 + p.channels.get(&v.channel_id).copied().unwrap_or(0.0)).ln() / (1.0 + max_ch).ln();
            let fresh = if v.published > 0 { (-((now - v.published).max(0) as f64 / 86400.0) / 21.0).exp() } else { 0.3 };
            let quality = v.views.map_or(0.3, |n| ((n as f64 + 1.0).log10() / 7.0).min(1.0));
            let mut score = 0.45 * taste + 0.15 * c.source.strength() + 0.15 * channel + 0.15 * fresh + 0.10 * quality + bonus;
            match p.shown.get(&v.id) {
                Some((times, false)) if *times >= 3 => score *= 0.5,
                Some((2, false)) => score *= 0.8,
                _ => {}
            }
            let reason = c.source.reason(&v);
            let subscribed = p.subscribed.contains(&v.channel_id);
            let from_search = matches!(c.source, Source::Search { .. });
            (Scored { video: v, score, reason, subscribed, from_search }, toks)
        })
        .collect();
    scored.sort_by(|a, b| b.0.score.total_cmp(&a.0.score).then_with(|| a.0.video.id.cmp(&b.0.video.id)));
    diversify(scored)
}

/// Walk the list best-first, skipping what would make the page repetitive.
fn diversify(mut pool: Vec<(Scored, HashSet<String>)>) -> Vec<Scored> {
    let mut discovery_left = pool.iter().filter(|(s, _)| !s.subscribed).count();
    let mut search_left = pool.iter().filter(|(s, _)| s.from_search).count();
    let mut out: Vec<(Scored, HashSet<String>)> = Vec::new();
    let mut per_channel: HashMap<String, usize> = HashMap::new();
    let mut per_reason: HashMap<String, usize> = HashMap::new();
    while out.len() < PAGE {
        let discovery = out.iter().filter(|(s, _)| !s.subscribed).count();
        let need_discovery = discovery_left > 0 && (discovery as f64) < MIN_DISCOVERY_SHARE * (out.len() + 1) as f64;
        let searched = out.iter().filter(|(s, _)| s.from_search).count();
        let need_search = search_left > 0 && (searched as f64) < MIN_SEARCH_SHARE * (out.len() + 1) as f64;
        let fits = |s: &Scored, toks: &HashSet<String>| {
            let ch = &s.video.channel_id;
            (ch.is_empty() || per_channel.get(ch).copied().unwrap_or(0) < MAX_PER_CHANNEL)
                && per_reason.get(&s.reason).copied().unwrap_or(0) < MAX_PER_REASON
                && !out.iter().any(|(_, o)| jaccard(o, toks) > 0.6)
        };
        let i = pool
            .iter()
            .position(|(s, t)| fits(s, t) && (!need_search || s.from_search) && (!need_discovery || !s.subscribed))
            .or_else(|| pool.iter().position(|(s, t)| fits(s, t) && (!need_search || s.from_search)))
            .or_else(|| pool.iter().position(|(s, t)| fits(s, t)));
        let Some(i) = i else { break };
        let (s, toks) = pool.remove(i);
        if !s.subscribed {
            discovery_left -= 1;
        }
        if s.from_search {
            search_left -= 1;
        }
        *per_channel.entry(s.video.channel_id.clone()).or_default() += 1;
        *per_reason.entry(s.reason.clone()).or_default() += 1;
        out.push((s, toks));
    }
    out.into_iter().map(|(s, _)| s).collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

pub fn to_item(s: &Scored) -> Item {
    let mut it = Item::from(s.video.clone());
    it.note = Some(s.reason.clone());
    it
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vid(id: &str, title: &str, ch: &str, views: i64) -> Video {
        Video {
            id: id.into(),
            title: title.into(),
            channel_id: ch.into(),
            channel: ch.into(),
            published: 1_000_000,
            views: Some(views),
            length: Some(600.0),
            ..Default::default()
        }
    }

    fn profile() -> Profile {
        let mut p = Profile::default();
        for (t, w) in [("turbo", 3.0), ("e30", 3.0), ("build", 1.0)] {
            p.terms.insert(t.into(), w);
        }
        p.subscribed.insert("SUB".into());
        p
    }

    #[test]
    fn matches_beat_misses_and_rules_hold() {
        let p = {
            let mut p = profile();
            p.hidden.insert("hidden".into());
            p.blocked_channels.insert("BLOCKED".into());
            p.seen.insert("seen".into(), true);
            p
        };
        let src = || Source::Search { query: "e30 turbo".into(), weight: 3.0, recent: true };
        let mut cands = vec![
            Candidate { video: vid("a", "E30 turbo build part 1", "X", 1000), source: src() },
            Candidate { video: vid("b", "Cooking pasta at home", "Y", 1_000_000), source: src() },
            Candidate { video: vid("hidden", "E30 turbo", "Z", 10), source: src() },
            Candidate { video: vid("blk", "E30 turbo", "BLOCKED", 10), source: src() },
            Candidate { video: vid("seen", "E30 turbo", "Z", 10), source: src() },
            Candidate { video: Video { length: Some(30.0), ..vid("short", "E30 turbo short", "Z", 10) }, source: src() },
        ];
        // Three more from channel X: only two per channel make it.
        for i in 0..3 {
            cands.push(Candidate {
                video: vid(&format!("x{i}"), &format!("turbo kit review {i} different words here"), "X", 10),
                source: src(),
            });
        }
        let r = rank(&p, cands, 1_000_000);
        let ids: Vec<&str> = r.iter().map(|s| s.video.id.as_str()).collect();
        assert_eq!(ids[0], "a", "{ids:?}");
        assert!(!ids.iter().any(|i| ["hidden", "blk", "seen", "short"].contains(i)), "{ids:?}");
        assert_eq!(r.iter().filter(|s| s.video.channel_id == "X").count(), 2);
        assert_eq!(r[0].reason, "Because you searched “e30 turbo”");
    }

    #[test]
    fn duplicates_merge_and_discovery_gets_room() {
        let p = profile();
        let mut cands = Vec::new();
        for i in 0..6 {
            cands.push(Candidate { video: vid(&format!("s{i}"), &format!("e30 turbo {i} sub"), "SUB", 100), source: Source::Subscription });
            cands.push(Candidate { video: vid(&format!("s{i}"), &format!("e30 turbo {i} sub"), "SUB", 100), source: Source::Subscription });
        }
        for i in 0..6 {
            cands.push(Candidate {
                video: vid(&format!("d{i}"), &format!("something else {i}"), &format!("D{i}"), 100),
                source: Source::Related { seed: "x".into(), weight: 0.1 },
            });
        }
        let r = rank(&p, cands, 1_000_000);
        let n: HashSet<&str> = r.iter().map(|s| s.video.id.as_str()).collect();
        assert_eq!(n.len(), r.len(), "no duplicates");
        let disc = r.iter().filter(|s| !s.subscribed).count();
        assert!(disc as f64 >= MIN_DISCOVERY_SHARE * r.len() as f64 - 1.0, "{disc} of {}", r.len());
    }
}
