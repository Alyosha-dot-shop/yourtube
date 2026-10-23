//! `yourtube eval`: how well For You's scoring predicts what you actually watched.
//!
//! For each of the last few weeks, it builds your profile from only what happened before
//! that week, then scores the videos you watched during it against random videos you never
//! watched (known from your feed and searches). A good scorer puts real watches above the
//! random ones. Nothing is written, and nothing leaves the computer.
//!
//! Only the personal part of the score is measured (topic match and channel affinity):
//! freshness and popularity need upload dates and view counts, which imported history lacks,
//! so they would favour the random videos for the wrong reason.
//!
//! Limits: subscriptions and "shown before" counts have no dates, so they come from today;
//! and the random videos lean towards your subscriptions, which makes the channel signal
//! look weaker than it is. Compare scorers with each other, not against 1.0.

use super::profile::{self, tokens};
use super::rank::Taste;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;

const WEEK: i64 = 7 * 86400;
const DECOYS: usize = 1000;

struct Doc {
    id: String,
    channel: String,
    toks: HashSet<String>,
    /// The title's embedding, when a model is installed.
    emb: Vec<f32>,
}

/// What a scorer may look at.
struct Ctx<'a> {
    t: &'a Taste<'a>,
    /// Your interests as one embedding (empty without a model).
    user: &'a [f32],
    /// Your strongest searches and watched titles, embedded, with weight^0.5 scaled to 0..1.
    interests: &'a [(Vec<f32>, f32)],
    seed: u64,
}

fn nearest(c: &Ctx, d: &Doc) -> f64 {
    super::embed::nearest(c.interests, &d.emb)
}

/// One scorer's results, averaged over the weeks.
pub struct Row {
    pub name: &'static str,
    pub auc: f64,
    pub recall_100: f64,
}

pub struct Report {
    pub weeks: usize,
    pub positives: usize,
    pub rows: Vec<Row>,
}

/// A fixed pseudo-random number per (seed, text), so runs are repeatable.
fn hash(seed: u64, s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ seed;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// AUC: the chance that a random real watch scores above a random decoy (ties count half).
fn auc(pos: &[f64], neg: &[f64]) -> f64 {
    let mut wins = 0.0;
    for p in pos {
        for n in neg {
            wins += if p > n {
                1.0
            } else if p == n {
                0.5
            } else {
                0.0
            };
        }
    }
    wins / (pos.len() * neg.len()).max(1) as f64
}

/// Share of real watches among the top 100 of watches + decoys.
fn recall_at(pos: &[f64], neg: &[f64], k: usize) -> f64 {
    let mut all: Vec<(f64, bool)> = pos.iter().map(|s| (*s, true)).chain(neg.iter().map(|s| (*s, false))).collect();
    all.sort_by(|a, b| b.0.total_cmp(&a.0));
    all.iter().take(k).filter(|(_, p)| *p).count() as f64 / pos.len().max(1) as f64
}

/// The embedding of a profile: its searches and watched titles averaged by weight.
pub fn user_vector(m: &super::embed::Model, p: &super::profile::Profile) -> Vec<f32> {
    let mut v = vec![0f32; m.dim()];
    for (text, w) in &p.texts {
        for (a, b) in v.iter_mut().zip(m.encode(text)) {
            *a += *w as f32 * b;
        }
    }
    super::embed::normalize(&mut v);
    v
}

pub fn run(conn: &Connection, weeks: usize, model: Option<&super::embed::Model>) -> Result<Report> {
    let emb = |title: &str| model.map(|m| m.encode(title)).unwrap_or_default();
    let mut st = conn.prepare(
        "SELECT h.video_id, h.title, COALESCE(NULLIF(h.author_id, ''), v.channel_id, ''), h.watched_at
         FROM history h LEFT JOIN videos v ON v.id = h.video_id WHERE h.title != ''",
    )?;
    let history: Vec<(Doc, i64)> = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))?
        .filter_map(|r| r.ok())
        .map(|(id, title, channel, at)| (Doc { id, channel, toks: tokens(&title).into_iter().collect(), emb: emb(&title) }, at))
        .collect();
    let watched: HashSet<&str> = history.iter().map(|(d, _)| d.id.as_str()).collect();
    let mut st = conn.prepare("SELECT id, title, channel_id FROM videos WHERE title != ''")?;
    let pool: Vec<Doc> = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
        .filter_map(|r| r.ok())
        .filter(|(id, _, _)| !watched.contains(id.as_str()))
        .map(|(id, title, channel)| Doc { id, channel, toks: tokens(&title).into_iter().collect(), emb: emb(&title) })
        .collect();
    let last = history.iter().map(|(_, at)| *at).max().unwrap_or(0);

    type Scorer = (&'static str, fn(&Ctx, &Doc) -> f64);
    let mut scorers: Vec<Scorer> = vec![
        ("random", |c, d| (hash(c.seed, &d.id) % 1_000_000) as f64),
        ("channel only", |c, d| c.t.channel(&d.channel)),
        ("topics only", |c, d| c.t.taste(&d.toks)),
        ("current (topics + channel)", |c, d| 0.45 * c.t.taste(&d.toks) + 0.15 * c.t.channel(&d.channel)),
    ];
    if model.is_some() {
        scorers.push(("average of you (embedding)", |c, d| super::embed::dot(c.user, &d.emb) as f64));
        scorers.push(("nearest interest", nearest));
        scorers.push(("with the topic model (live)", |c, d| {
            0.45 * c.t.taste(&d.toks) + 0.15 * c.t.channel(&d.channel) + super::rank::NEAREST_WEIGHT * nearest(c, d)
        }));
    }
    let mut sums = vec![(0.0, 0.0); scorers.len()];
    let (mut used, mut positives) = (0, 0);
    for k in 1..=weeks {
        let cutoff = last - k as i64 * WEEK;
        let pos: Vec<&Doc> = history.iter().filter(|(_, at)| (cutoff..cutoff + WEEK).contains(at)).map(|(d, _)| d).collect();
        if pos.len() < 5 || pool.len() < 50 {
            continue;
        }
        let seed = cutoff as u64;
        let mut neg: Vec<&Doc> = pool.iter().collect();
        neg.sort_by_key(|d| hash(seed, &d.id));
        neg.truncate(DECOYS);
        let p = profile::build(conn, cutoff)?;
        let docs: Vec<HashSet<String>> = pos.iter().chain(neg.iter()).map(|d| d.toks.clone()).collect();
        let t = Taste::new(&p, &docs);
        let user = model.map(|m| user_vector(m, &p)).unwrap_or_default();
        let ints = model.map(|m| super::embed::interests(m, &p, super::rank::INTERESTS)).unwrap_or_default();
        let c = Ctx { t: &t, user: &user, interests: &ints, seed };
        for (i, (_, f)) in scorers.iter().enumerate() {
            let ps: Vec<f64> = pos.iter().map(|d| f(&c, d)).collect();
            let ns: Vec<f64> = neg.iter().map(|d| f(&c, d)).collect();
            sums[i].0 += auc(&ps, &ns);
            sums[i].1 += recall_at(&ps, &ns, 100);
        }
        used += 1;
        positives += pos.len();
    }
    let n = used.max(1) as f64;
    let rows = scorers.iter().zip(sums).map(|((name, _), (a, r))| Row { name, auc: a / n, recall_100: r / n }).collect();
    Ok(Report { weeks: used, positives, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics() {
        assert_eq!(auc(&[3.0, 2.0], &[1.0, 0.0]), 1.0);
        assert_eq!(auc(&[0.0], &[1.0]), 0.0);
        assert_eq!(auc(&[1.0], &[1.0]), 0.5);
        assert_eq!(recall_at(&[5.0, 0.1], &[1.0, 2.0], 2), 0.5);
        assert_eq!(hash(1, "a"), hash(1, "a"));
        assert_ne!(hash(1, "a"), hash(2, "a"));
    }
}
