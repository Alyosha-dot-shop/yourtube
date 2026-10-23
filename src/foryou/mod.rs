//! For You: recommendations from all of YouTube, made on this computer.
//!
//! 1. Build a profile from your recent searches, watches and feedback (profile.rs).
//! 2. Find candidates on YouTube: results for your recent searches, videos
//!    related to what you watched lately, new uploads from your subscriptions,
//!    and (with AI on) searches the model suggests.
//! 3. Score and order them, with a reason for each (rank.rs).
//! 4. With AI on, let the model re-order the best ones (ai.rs).
//!
//! A refresh makes about 20 requests to YouTube and takes 10–30 seconds.
//! Alternatively For You can come from an external service (remote.rs).

pub mod ai;
pub mod profile;
pub mod rank;
pub mod remote;

use crate::config::Config;
use crate::db;
use crate::feed::Video;
use anyhow::Result;
use rank::{Candidate, Source};
use std::sync::{Arc, Mutex, mpsc};

const SEARCHES: usize = 10;
const SEEDS: usize = 8;
const WORKERS: usize = 6;
/// Workers behind a proxy, where requests are spread out (see `fetch_all`).
const PACED_WORKERS: usize = 2;
/// How long YouTube's related videos for a watched video are reused.
const RELATED_TTL: i64 = 86400;
const SUBSCRIPTION_DAYS: i64 = 14;
const AI_RERANK: usize = 40;

#[derive(Debug, Default)]
pub struct Stats {
    pub picks: usize,
    pub candidates: usize,
    /// Candidates per source before filtering, e.g. ("search", 180).
    pub by_source: Vec<(&'static str, usize)>,
    pub failed_requests: usize,
    /// Set when AI was on but didn't work; the page is made without it.
    pub ai_error: Option<String>,
}

enum Fetch {
    Search(String, Source),
    Related(String, Source),
}

pub fn refresh(cfg: &Config, say: impl Fn(&str)) -> Result<Stats> {
    let conn = db::open()?;
    let now = db::now();
    say("Reading your history…");
    let p = profile::build(&conn, now)?;
    let mut stats = Stats::default();
    let ai_cfg = cfg.ai();

    // What to look up on YouTube.
    let mut jobs: Vec<Fetch> = p
        .searches
        .iter()
        .take(SEARCHES)
        .map(|(q, w, at)| Fetch::Search(q.clone(), Source::Search { query: q.clone(), weight: *w, recent: now - at < 30 * 86400 }))
        .collect();
    if p.searches.len() < 3 {
        // Few searches yet: search for your strongest topic words instead.
        let top: Vec<String> = p.top_terms(6).into_iter().map(|(t, _)| t).collect();
        for pair in top.chunks(2) {
            let words = pair.join(" ");
            jobs.push(Fetch::Search(words.clone(), Source::Interest { words: pair.join(", ") }));
        }
    }
    for s in p.seeds.iter().take(SEEDS) {
        jobs.push(Fetch::Related(s.id.clone(), Source::Related { seed: s.title.clone(), weight: s.weight }));
    }
    if let Some(ai) = &ai_cfg {
        say("Asking the AI what to explore…");
        let searches: Vec<String> = p.searches.iter().map(|(q, _, _)| q.clone()).collect();
        let watched = recent_watched(&conn, 30)?;
        match ai::explore(ai, &searches, &watched, &p.disliked) {
            Ok(qs) => jobs.extend(
                qs.into_iter()
                    .map(|(q, why)| Fetch::Search(q.clone(), Source::Ai { why: if why.is_empty() { format!("try “{q}”") } else { why } })),
            ),
            Err(e) => stats.ai_error = Some(e.to_string()),
        }
    }

    say(&format!("Looking on YouTube ({} searches)…", jobs.len()));
    let (mut cands, failed) = fetch_all(jobs, cfg.proxy.is_some());
    stats.failed_requests = failed;
    cands.extend(subscription_uploads(&conn, now)?);
    stats.candidates = cands.len();
    for (name, n) in [
        ("search", cands.iter().filter(|c| matches!(c.source, Source::Search { .. })).count()),
        ("related", cands.iter().filter(|c| matches!(c.source, Source::Related { .. })).count()),
        ("subscriptions", cands.iter().filter(|c| matches!(c.source, Source::Subscription)).count()),
        ("interest", cands.iter().filter(|c| matches!(c.source, Source::Interest { .. })).count()),
        ("ai", cands.iter().filter(|c| matches!(c.source, Source::Ai { .. })).count()),
    ] {
        stats.by_source.push((name, n));
    }

    say("Ranking…");
    let mut ranked = rank::rank(&p, cands, now);

    if let (Some(ai), None) = (&ai_cfg, &stats.ai_error) {
        say("Asking the AI to order the best ones…");
        let top: Vec<(String, String)> = ranked.iter().take(AI_RERANK).map(|s| (s.video.title.clone(), s.video.channel.clone())).collect();
        let interests: Vec<String> = p.top_terms(12).into_iter().map(|(t, _)| t).collect();
        match ai::rerank(ai, &interests, &top) {
            Ok(order) if !order.is_empty() => {
                let mut chosen: Vec<rank::Scored> = Vec::new();
                for (i, why) in &order {
                    let mut s = ranked[*i].clone();
                    if !why.is_empty() {
                        s.reason = format!("AI: {why}");
                    }
                    chosen.push(s);
                }
                let picked: std::collections::HashSet<usize> = order.iter().map(|(i, _)| *i).collect();
                chosen.extend(ranked.into_iter().enumerate().filter(|(i, _)| !picked.contains(i)).map(|(_, s)| s));
                ranked = chosen;
            }
            Ok(_) => {}
            Err(e) => stats.ai_error = Some(e.to_string()),
        }
    }

    // Unfinished videos go first.
    let mut items = continue_watching(&conn, &p)?;
    items.extend(ranked.iter().map(rank::to_item));
    stats.picks = items.len();
    save(&conn, &items, now)?;
    Ok(stats)
}

/// Run the lookups. Behind a proxy (Tor) they go out two at a time, in random order, with
/// random pauses, on For You's own circuit: a burst of ~20 searches built from your history,
/// all from one exit within seconds, would be a fingerprint of its own.
fn fetch_all(mut jobs: Vec<Fetch>, paced: bool) -> (Vec<Candidate>, usize) {
    let random = |n: u64| {
        let mut b = [0u8; 8];
        let _ = getrandom::getrandom(&mut b);
        u64::from_le_bytes(b) % n.max(1)
    };
    for i in (1..jobs.len()).rev() {
        jobs.swap(i, random(i as u64 + 1) as usize);
    }
    let queue = Arc::new(Mutex::new(jobs));
    let (tx, rx) = mpsc::channel::<Result<Vec<Candidate>>>();
    for _ in 0..if paced { PACED_WORKERS } else { WORKERS } {
        let (queue, tx) = (queue.clone(), tx.clone());
        std::thread::spawn(move || {
            crate::yt::with_purpose(crate::yt::Purpose::ForYou, || {
                let conn = db::open().ok();
                loop {
                    let Some(job) = queue.lock().unwrap().pop() else { break };
                    let mut asked = true;
                    let r = match job {
                        Fetch::Search(q, src) => crate::yt::search(&q).map(|r| tag(r.videos, &src)),
                        Fetch::Related(id, src) => match conn.as_ref().and_then(|c| db::related_cached(c, &id, RELATED_TTL)) {
                            Some(v) => {
                                asked = false;
                                Ok(tag(v, &src))
                            }
                            None => crate::yt::related(&id)
                                .inspect(|v| {
                                    if let Some(c) = &conn {
                                        let _ = db::cache_related(c, &id, v);
                                    }
                                })
                                .map(|v| tag(v, &src)),
                        },
                    };
                    if tx.send(r).is_err() {
                        break;
                    }
                    if paced && asked {
                        std::thread::sleep(std::time::Duration::from_millis(1000 + random(3000)));
                    }
                }
            })
        });
    }
    drop(tx);
    let mut out = Vec::new();
    let mut failed = 0;
    for r in rx {
        match r {
            Ok(c) => out.extend(c),
            Err(_) => failed += 1,
        }
    }
    (out, failed)
}

fn tag(videos: Vec<Video>, src: &Source) -> Vec<Candidate> {
    videos.into_iter().map(|video| Candidate { video, source: src.clone() }).collect()
}

/// Recent uploads from your subscriptions (already in the feed table).
fn subscription_uploads(conn: &rusqlite::Connection, now: i64) -> Result<Vec<Candidate>> {
    let mut st = conn.prepare(
        "SELECT v.id, v.channel_id, c.name, v.title, v.published, v.views, v.length
         FROM videos v JOIN channels c ON c.id = v.channel_id
         WHERE c.subscribed = 1 AND v.published >= ?1",
    )?;
    let rows = st.query_map([now - SUBSCRIPTION_DAYS * 86400], |r| {
        Ok(Candidate {
            video: Video {
                id: r.get(0)?,
                channel_id: r.get(1)?,
                channel: r.get(2)?,
                title: r.get(3)?,
                published: r.get(4)?,
                views: r.get(5)?,
                length: r.get(6)?,
                ..Default::default()
            },
            source: Source::Subscription,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn recent_watched(conn: &rusqlite::Connection, n: i64) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare("SELECT title, COALESCE(author, '') FROM history ORDER BY watched_at DESC LIMIT ?1")?;
    let rows = st.query_map([n], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn continue_watching(conn: &rusqlite::Connection, p: &profile::Profile) -> Result<Vec<db::Item>> {
    let mut out = Vec::new();
    let mut st = conn.prepare(
        "SELECT h.title, COALESCE(h.author, c.name, ''), COALESCE(h.author_id, v.channel_id, ''), v.published, v.views
         FROM history h LEFT JOIN videos v ON v.id = h.video_id LEFT JOIN channels c ON c.id = v.channel_id
         WHERE h.video_id = ?1",
    )?;
    for u in p.unfinished.iter().take(3) {
        let Ok((title, channel, channel_id, published, views)) = st.query_row([&u.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        }) else {
            continue;
        };
        let v = Video {
            id: u.id.clone(),
            title,
            channel,
            channel_id,
            published: published.unwrap_or(0),
            views,
            length: Some(u.length),
            ..Default::default()
        };
        let mut it = db::Item::from(v);
        it.note = Some(format!("Continue watching · {} left", crate::ui::fmt::clock(u.length - u.position)));
        it.position = Some(u.position);
        it.watched = true;
        out.push(it);
    }
    Ok(out)
}

fn save(conn: &rusqlite::Connection, items: &[db::Item], now: i64) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM rec_picks", [])?;
    for (rank, it) in items.iter().enumerate() {
        tx.execute(
            "INSERT INTO rec_picks (rank, video_id, title, channel, channel_id, published, views, length, reason, run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                rank as i64,
                it.id,
                it.title,
                it.channel,
                it.channel_id,
                it.published,
                it.views,
                it.length,
                it.note.as_deref().unwrap_or(""),
                now
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}
