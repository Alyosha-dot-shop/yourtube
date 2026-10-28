//! SQLite storage: subscriptions, feed videos, watch history, playlists, searches, For You.
//!
//! Each profile has its own database, so people sharing a computer account keep
//! separate histories and recommendations. The default profile lives at
//! `~/.local/share/yourtube/yourtube.db`; others under `profiles/<name>/`.
//!
//! In incognito mode nothing you watch or search is written down (see [`incognito`]).

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

static PROFILE: RwLock<Option<String>> = RwLock::new(None);
static INCOGNITO: AtomicBool = AtomicBool::new(false);

pub const DEFAULT_PROFILE: &str = "Default";

pub fn data_dir() -> PathBuf {
    dirs::data_dir().expect("no data dir").join("yourtube")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().expect("no cache dir").join("yourtube")
}

/// The profile in use (`None` = the default one).
pub fn profile() -> Option<String> {
    PROFILE.read().unwrap().clone()
}

pub fn profile_name() -> String {
    profile().unwrap_or_else(|| DEFAULT_PROFILE.to_owned())
}

pub fn set_profile(name: Option<String>) {
    *PROFILE.write().unwrap() = name.filter(|n| n != DEFAULT_PROFILE);
}

fn profile_dir(name: Option<&str>) -> PathBuf {
    match name {
        None => data_dir(),
        Some(n) => data_dir().join("profiles").join(n),
    }
}

/// All profiles, the default first.
pub fn profiles() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(data_dir().join("profiles"))
        .map(|d| d.flatten().filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    v.sort_by_key(|s| s.to_lowercase());
    v.insert(0, DEFAULT_PROFILE.to_owned());
    v
}

/// A profile name that is safe as a folder name.
pub fn valid_profile_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 40 || n.starts_with('.') || n.contains(['/', '\\', '\0']) || n.eq_ignore_ascii_case(DEFAULT_PROFILE) {
        return Err(anyhow!("Pick another name (up to 40 characters, no slashes)"));
    }
    Ok(n.to_owned())
}

pub fn create_profile(name: &str) -> Result<String> {
    let n = valid_profile_name(name)?;
    let dir = profile_dir(Some(&n));
    if dir.exists() {
        return Err(anyhow!("There's already a profile called {n}"));
    }
    std::fs::create_dir_all(&dir)?;
    Ok(n)
}

pub fn rename_profile(old: &str, new: &str) -> Result<String> {
    let n = valid_profile_name(new)?;
    if old == DEFAULT_PROFILE {
        return Err(anyhow!("The default profile can't be renamed"));
    }
    std::fs::rename(profile_dir(Some(old)), profile_dir(Some(&n)))?;
    Ok(n)
}

/// Delete a profile and everything in it. The default profile can't be deleted.
pub fn delete_profile(name: &str) -> Result<()> {
    if name == DEFAULT_PROFILE {
        return Err(anyhow!("The default profile can't be deleted"));
    }
    std::fs::remove_dir_all(profile_dir(Some(name)))?;
    Ok(())
}

/// Incognito: watching, searching and For You clicks leave no trace.
pub fn incognito() -> bool {
    INCOGNITO.load(Ordering::Relaxed)
}

pub fn set_incognito(on: bool) {
    if !INCOGNITO.swap(on, Ordering::Relaxed) || !on {
        crate::thumbs::forget_incognito();
    }
}

pub fn open() -> Result<Connection> {
    let dir = profile_dir(profile().as_deref());
    std::fs::create_dir_all(&dir)?;
    let conn = Connection::open(dir.join("yourtube.db"))?;
    init(&conn)?;
    Ok(conn)
}

/// Create the tables (safe to run on an existing database).
pub fn init(conn: &Connection) -> Result<()> {
    // WAL lets the UI read while the feed refresh and the player write.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA secure_delete=ON; -- deleted history is overwritten, not left in free pages
         CREATE TABLE IF NOT EXISTS channels (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             thumbnail TEXT,
             subscribed INTEGER NOT NULL DEFAULT 1,
             last_fetch INTEGER NOT NULL DEFAULT 0,
             fetch_error TEXT
         );
         CREATE TABLE IF NOT EXISTS videos (
             id TEXT PRIMARY KEY,
             channel_id TEXT NOT NULL,
             title TEXT NOT NULL,
             published INTEGER NOT NULL,      -- unix seconds
             views INTEGER,
             length REAL,                     -- seconds, when known
             description TEXT
         );
         CREATE INDEX IF NOT EXISTS videos_published ON videos(published DESC);
         CREATE TABLE IF NOT EXISTS history (
             video_id TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             author TEXT,
             author_id TEXT,
             watched_at INTEGER NOT NULL,     -- unix seconds
             position REAL NOT NULL DEFAULT 0, -- seconds
             length REAL                      -- seconds, when known
         );
         CREATE TABLE IF NOT EXISTS playlists (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             protected INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS playlist_items (
             playlist_id TEXT NOT NULL,
             video_id TEXT NOT NULL,
             title TEXT NOT NULL,
             author TEXT,
             author_id TEXT,
             length REAL,
             added INTEGER NOT NULL,
             PRIMARY KEY (playlist_id, video_id)
         );
         -- For You: what it learns from, and what it found last time.
         CREATE TABLE IF NOT EXISTS searches (
             query TEXT NOT NULL,
             at INTEGER NOT NULL,
             PRIMARY KEY (query, at)
         );
         CREATE TABLE IF NOT EXISTS rec_feedback (
             video_id TEXT NOT NULL,
             channel_id TEXT,
             title TEXT,
             kind TEXT NOT NULL CHECK (kind IN ('more', 'not_interested', 'fewer')),
             at INTEGER NOT NULL,
             PRIMARY KEY (video_id, kind)
         );
         -- YouTube's related videos for a watched video, kept a day so For You doesn't ask again.
         CREATE TABLE IF NOT EXISTS related_cache (
             video_id TEXT PRIMARY KEY,
             at INTEGER NOT NULL,
             videos TEXT NOT NULL  -- JSON list
         );
         CREATE TABLE IF NOT EXISTS rec_shown (
             video_id TEXT PRIMARY KEY,
             times INTEGER NOT NULL DEFAULT 0,  -- runs it was shown in
             clicked INTEGER NOT NULL DEFAULT 0,
             last_run INTEGER NOT NULL DEFAULT 0
         );
         -- Every For You pick that was shown, with where and why, and whether it was opened:
         -- what `yourtube eval` and any later tuning of the score's weights learn from.
         CREATE TABLE IF NOT EXISTS rec_impressions (
             run_at INTEGER NOT NULL,
             video_id TEXT NOT NULL,
             rank INTEGER NOT NULL,
             reason TEXT NOT NULL,
             shown_at INTEGER NOT NULL,
             clicked_at INTEGER,
             PRIMARY KEY (run_at, video_id)
         );
         CREATE TABLE IF NOT EXISTS rec_picks (
             rank INTEGER PRIMARY KEY,
             video_id TEXT NOT NULL,
             title TEXT NOT NULL,
             channel TEXT,
             channel_id TEXT,
             published INTEGER,
             views INTEGER,
             length REAL,
             reason TEXT NOT NULL,
             run_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS blocked_channels (
             channel_id TEXT PRIMARY KEY,
             name TEXT NOT NULL DEFAULT '',
             at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS history_recent ON history(watched_at DESC);",
    )?;
    crate::download::init_tables(conn)?;
    crate::sync::init(conn)?;
    Ok(())
}

/// One video card, on any page.
#[derive(Clone, Debug, Default)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    /// Upload time (unix seconds), when known.
    pub published: Option<i64>,
    pub views: Option<i64>,
    pub length: Option<f64>,
    /// Saved position, when the video is in your history.
    pub position: Option<f64>,
    pub watched: bool,
    /// Extra line under the card: why it was recommended, when you watched or saved it.
    pub note: Option<String>,
    /// The recommender run this pick came from (For You only).
    pub run_id: Option<i64>,
}

impl Item {
    /// Watched to (nearly) the end.
    pub fn finished(&self) -> bool {
        match (self.position, self.length) {
            (Some(p), Some(l)) if l > 0.0 => p / l >= 0.97,
            _ => self.watched,
        }
    }
}

impl From<crate::feed::Video> for Item {
    fn from(v: crate::feed::Video) -> Self {
        Item {
            id: v.id,
            title: v.title,
            channel: v.channel,
            channel_id: v.channel_id,
            published: (v.published > 0).then_some(v.published),
            views: v.views,
            length: v.length,
            ..Default::default()
        }
    }
}

fn item_row(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get("id")?,
        title: r.get("title")?,
        channel: r.get::<_, Option<String>>("channel")?.unwrap_or_default(),
        channel_id: r.get::<_, Option<String>>("channel_id")?.unwrap_or_default(),
        published: r.get("published")?,
        views: r.get("views")?,
        length: r.get("length")?,
        position: r.get("position")?,
        watched: r.get("watched")?,
        note: None,
        run_id: None,
    })
}

/// Newest videos from subscribed channels.
pub fn feed(conn: &Connection, limit: i64) -> Result<Vec<Item>> {
    let mut st = conn.prepare(
        "SELECT v.id, v.title, c.name AS channel, v.channel_id, v.published, v.views,
                COALESCE(v.length, h.length) AS length, h.position, h.video_id IS NOT NULL AS watched
         FROM videos v JOIN channels c ON c.id = v.channel_id
         LEFT JOIN history h ON h.video_id = v.id
         WHERE c.subscribed = 1
         ORDER BY v.published DESC LIMIT ?1",
    )?;
    let rows = st.query_map([limit], item_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Watch history, most recent first. The note says when.
pub fn history(conn: &Connection, limit: i64) -> Result<Vec<(Item, i64)>> {
    let mut st = conn.prepare(
        "SELECT h.video_id AS id, h.title, COALESCE(h.author, c.name) AS channel,
                COALESCE(h.author_id, v.channel_id) AS channel_id, v.published, v.views,
                COALESCE(h.length, v.length) AS length, h.position, 1 AS watched, h.watched_at
         FROM history h LEFT JOIN videos v ON v.id = h.video_id LEFT JOIN channels c ON c.id = v.channel_id
         ORDER BY h.watched_at DESC LIMIT ?1",
    )?;
    let rows = st.query_map([limit], |r| Ok((item_row(r)?, r.get("watched_at")?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub count: i64,
}

pub const WATCH_LATER: &str = "Watch later";

pub fn playlists(conn: &Connection) -> Result<Vec<Playlist>> {
    let mut st = conn.prepare(
        "SELECT p.id, p.name, p.protected, COUNT(i.video_id)
         FROM playlists p LEFT JOIN playlist_items i ON i.playlist_id = p.id
         GROUP BY p.id ORDER BY p.name = ?1 DESC, p.name COLLATE NOCASE",
    )?;
    let rows = st.query_map([WATCH_LATER], |r| Ok(Playlist { id: r.get(0)?, name: r.get(1)?, count: r.get(3)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The Watch later playlist, created if it doesn't exist yet.
pub fn watch_later(conn: &Connection) -> Result<String> {
    let found = conn
        .query_row("SELECT id FROM playlists WHERE name = ?1 COLLATE NOCASE ORDER BY protected DESC", [WATCH_LATER], |r| r.get(0))
        .optional()?;
    match found {
        Some(id) => Ok(id),
        None => create_playlist(conn, WATCH_LATER),
    }
}

pub fn create_playlist(conn: &Connection, name: &str) -> Result<String> {
    let id = format!("yourtube-{}-{}", now(), std::process::id());
    conn.execute("INSERT INTO playlists (id, name) VALUES (?1, ?2)", params![id, name])?;
    Ok(id)
}

pub fn delete_playlist(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id])?;
    conn.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
    Ok(())
}

pub fn rename_playlist(conn: &Connection, id: &str, name: &str) -> Result<()> {
    conn.execute("UPDATE playlists SET name = ?2 WHERE id = ?1", params![id, name])?;
    Ok(())
}

/// A playlist's videos, newest additions first.
pub fn playlist_items(conn: &Connection, playlist_id: &str) -> Result<Vec<(Item, i64)>> {
    let mut st = conn.prepare(
        "SELECT i.video_id AS id, i.title, COALESCE(i.author, c.name) AS channel,
                COALESCE(i.author_id, v.channel_id) AS channel_id, v.published, v.views,
                COALESCE(i.length, v.length, h.length) AS length, h.position,
                h.video_id IS NOT NULL AS watched, i.added
         FROM playlist_items i LEFT JOIN videos v ON v.id = i.video_id
         LEFT JOIN channels c ON c.id = v.channel_id LEFT JOIN history h ON h.video_id = i.video_id
         WHERE i.playlist_id = ?1 ORDER BY i.added DESC",
    )?;
    let rows = st.query_map([playlist_id], |r| Ok((item_row(r)?, r.get("added")?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn add_to_playlist(conn: &Connection, playlist_id: &str, it: &Item) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO playlist_items (playlist_id, video_id, title, author, author_id, length, added)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![playlist_id, it.id, it.title, it.channel, it.channel_id, it.length, now()],
    )?;
    Ok(())
}

pub fn remove_from_playlist(conn: &Connection, playlist_id: &str, video_id: &str) -> Result<()> {
    conn.execute("DELETE FROM playlist_items WHERE playlist_id = ?1 AND video_id = ?2", [playlist_id, video_id])?;
    Ok(())
}

pub fn in_playlist(conn: &Connection, playlist_id: &str, video_id: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM playlist_items WHERE playlist_id = ?1 AND video_id = ?2", [playlist_id, video_id], |_| Ok(()))
        .optional()?
        .is_some())
}

pub fn is_subscribed(conn: &Connection, channel_id: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT subscribed FROM channels WHERE id = ?1", [channel_id], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false))
}

pub fn set_subscribed(conn: &Connection, channel_id: &str, name: &str, on: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO channels (id, name, subscribed) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET subscribed = excluded.subscribed,
             name = CASE WHEN excluded.name != '' THEN excluded.name ELSE channels.name END",
        params![channel_id, name, on],
    )?;
    Ok(())
}

/// Fill in saved progress for videos that came from outside the database (search, channel pages, For You).
pub fn add_progress(conn: &Connection, items: &mut [Item]) -> Result<()> {
    let mut st = conn.prepare("SELECT position, length FROM history WHERE video_id = ?1")?;
    for it in items {
        if let Some((p, l)) = st.query_row([&it.id], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, Option<f64>>(1)?))).optional()? {
            it.position = Some(p);
            it.watched = true;
            if it.length.is_none() {
                it.length = l;
            }
        }
    }
    Ok(())
}

/// Save videos seen on a channel page into the feed table, so subscribed channels show them right away.
pub fn store_videos(conn: &Connection, videos: &[crate::feed::Video]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for v in videos {
        tx.execute(
            "INSERT INTO videos (id, channel_id, title, published, views, length) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title,
                 views = COALESCE(excluded.views, videos.views), length = COALESCE(excluded.length, videos.length)",
            params![v.id, v.channel_id, v.title, v.published, v.views, v.length],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Saved position for a video, if any.
pub fn position(conn: &Connection, video_id: &str) -> Result<Option<(f64, Option<f64>)>> {
    Ok(conn.query_row("SELECT position, length FROM history WHERE video_id = ?1", [video_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
}

/// Record that a video was watched up to `position` seconds.
pub fn save_progress(conn: &Connection, video_id: &str, position: f64, length: Option<f64>) -> Result<()> {
    let now = now();
    // Title and channel come from the feed when the video is in it.
    conn.execute(
        "INSERT INTO history (video_id, title, author, author_id, watched_at, position, length)
         SELECT ?1, COALESCE(v.title, ?1), c.name, v.channel_id, ?2, ?3, ?4
         FROM (SELECT 1) LEFT JOIN videos v ON v.id = ?1 LEFT JOIN channels c ON c.id = v.channel_id
         ON CONFLICT(video_id) DO UPDATE SET
             watched_at = excluded.watched_at,
             position = excluded.position,
             length = COALESCE(excluded.length, history.length)",
        params![video_id, now, position, length],
    )?;
    Ok(())
}

/// Mark as watched (to the end) or remove from history.
pub fn set_watched(conn: &Connection, video_id: &str, watched: bool) -> Result<()> {
    if watched {
        let len = position(conn, video_id)?.and_then(|p| p.1);
        save_progress(conn, video_id, len.unwrap_or(0.0), len)?;
    } else {
        conn.execute("DELETE FROM history WHERE video_id = ?1", [video_id])?;
        let _ = std::fs::remove_file(crate::thumbs::path(video_id));
    }
    Ok(())
}

/// After a big delete: fold the write-ahead log back in and shrink the file, so the deleted rows
/// don't linger in either.
pub fn compact(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Remember a YouTube search; For You weighs recent searches most.
pub fn record_search(conn: &Connection, query: &str) -> Result<()> {
    if incognito() {
        return Ok(());
    }
    let q = query.trim();
    if !q.is_empty() {
        conn.execute("INSERT OR IGNORE INTO searches (query, at) VALUES (?1, ?2)", params![q, now()])?;
    }
    Ok(())
}

/// Recent searches, newest first, as (query, unix seconds).
pub fn recent_searches(conn: &Connection, since: i64) -> Result<Vec<(String, i64)>> {
    let mut st = conn.prepare("SELECT query, at FROM searches WHERE at >= ?1 ORDER BY at DESC")?;
    let rows = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The picks from the last For You run, in order.
pub fn rec_picks(conn: &Connection) -> Result<Vec<Item>> {
    let mut st = conn.prepare(
        "SELECT p.video_id AS id, p.title, p.channel, p.channel_id, p.published, p.views,
                COALESCE(p.length, h.length) AS length, h.position, h.video_id IS NOT NULL AS watched, p.reason, p.run_at
         FROM rec_picks p LEFT JOIN history h ON h.video_id = p.video_id
         WHERE p.video_id NOT IN (SELECT video_id FROM rec_feedback WHERE kind = 'not_interested')
           AND COALESCE(p.channel_id, '') NOT IN (SELECT channel_id FROM rec_feedback WHERE kind = 'fewer' AND channel_id IS NOT NULL)
           AND COALESCE(p.channel_id, '') NOT IN (SELECT channel_id FROM blocked_channels)
         ORDER BY p.rank",
    )?;
    let rows = st.query_map([], |r| {
        let mut it = item_row(r)?;
        it.note = Some(r.get("reason")?);
        it.run_id = Some(r.get("run_at")?);
        Ok(it)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// When For You last ran (unix seconds), if ever.
pub fn rec_last_run(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT MAX(run_at) FROM rec_picks", [], |r| r.get(0))?)
}

pub fn add_feedback(conn: &Connection, it: &Item, kind: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO rec_feedback (video_id, channel_id, title, kind, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![it.id, (!it.channel_id.is_empty()).then_some(&it.channel_id), it.title, kind, now()],
    )?;
    Ok(())
}

/// Count a For You pick as shown in this run (once per run) or as clicked.
pub fn mark_shown(conn: &Connection, ids: &[String], run_at: i64) -> Result<()> {
    if incognito() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for id in ids {
        tx.execute(
            "INSERT INTO rec_shown (video_id, times, last_run) VALUES (?1, 1, ?2)
             ON CONFLICT(video_id) DO UPDATE SET times = times + 1, last_run = ?2 WHERE rec_shown.last_run != ?2",
            params![id, run_at],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO rec_impressions (run_at, video_id, rank, reason, shown_at)
             SELECT run_at, video_id, rank, reason, ?3 FROM rec_picks WHERE video_id = ?1 AND run_at = ?2",
            params![id, run_at, now()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Related videos fetched for `id` within `max_age` seconds.
pub fn related_cached(conn: &Connection, id: &str, max_age: i64) -> Option<Vec<crate::feed::Video>> {
    let json: String = conn
        .query_row("SELECT videos FROM related_cache WHERE video_id = ?1 AND at >= ?2", params![id, now() - max_age], |r| r.get(0))
        .ok()?;
    serde_json::from_str(&json).ok()
}

pub fn cache_related(conn: &Connection, id: &str, videos: &[crate::feed::Video]) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO related_cache (video_id, at, videos) VALUES (?1, ?2, ?3)",
        params![id, now(), serde_json::to_string(videos)?],
    )?;
    conn.execute("DELETE FROM related_cache WHERE at < ?1", [now() - 7 * 86400])?;
    Ok(())
}

pub fn mark_clicked(conn: &Connection, id: &str) -> Result<()> {
    if incognito() {
        return Ok(());
    }
    conn.execute("INSERT INTO rec_shown (video_id, clicked) VALUES (?1, 1) ON CONFLICT(video_id) DO UPDATE SET clicked = 1", [id])?;
    conn.execute(
        "UPDATE rec_impressions SET clicked_at = ?2
         WHERE video_id = ?1 AND clicked_at IS NULL AND run_at = (SELECT MAX(run_at) FROM rec_impressions WHERE video_id = ?1)",
        params![id, now()],
    )?;
    Ok(())
}

/// Hide a channel everywhere (and unsubscribe from it).
pub fn block_channel(conn: &Connection, channel_id: &str, name: &str) -> Result<()> {
    conn.execute("INSERT OR REPLACE INTO blocked_channels (channel_id, name, at) VALUES (?1, ?2, ?3)", params![channel_id, name, now()])?;
    conn.execute("UPDATE channels SET subscribed = 0 WHERE id = ?1", [channel_id])?;
    Ok(())
}

pub fn unblock_channel(conn: &Connection, channel_id: &str) -> Result<()> {
    conn.execute("DELETE FROM blocked_channels WHERE channel_id = ?1", [channel_id])?;
    Ok(())
}

/// Hidden channels as (id, name), most recent first.
pub fn blocked_channels(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare("SELECT channel_id, name FROM blocked_channels ORDER BY at DESC")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Videos you marked "not interested".
pub fn rejected_videos(conn: &Connection) -> Result<Vec<String>> {
    let mut st = conn.prepare("SELECT video_id FROM rec_feedback WHERE kind = 'not_interested'")?;
    let rows = st.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn remove_feedback(conn: &Connection, video_id: &str, kind: &str) -> Result<()> {
    conn.execute("DELETE FROM rec_feedback WHERE video_id = ?1 AND kind = ?2", [video_id, kind])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impressions_are_logged_once_and_clicks_land() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO rec_picks (rank, video_id, title, reason, run_at) VALUES (0, 'a', 'A', 'Because you searched x', 100), (1, 'b', 'B', 'Related to y', 100);",
        )
        .unwrap();
        let ids = vec!["a".to_owned(), "b".to_owned()];
        mark_shown(&conn, &ids, 100).unwrap();
        mark_shown(&conn, &ids, 100).unwrap();
        mark_clicked(&conn, "b").unwrap();
        let rows: Vec<(String, i64, String, bool)> = conn
            .prepare("SELECT video_id, rank, reason, clicked_at IS NOT NULL FROM rec_impressions ORDER BY rank")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows, vec![("a".into(), 0, "Because you searched x".into(), false), ("b".into(), 1, "Related to y".into(), true)]);
    }

    #[test]
    fn deleted_history_leaves_no_trace_in_the_file() {
        let dir = std::env::temp_dir().join(format!("yourtube-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let conn = Connection::open(&path).unwrap();
        init(&conn).unwrap();
        let marker = "UNIQUE-MARKER-title-7f3a9c";
        conn.execute("INSERT INTO history (video_id, title, watched_at, position) VALUES ('abcdefghijk', ?1, 1, 1)", [marker]).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let has = |p: &std::path::Path| std::fs::read(p).is_ok_and(|b| b.windows(marker.len()).any(|w| w == marker.as_bytes()));
        assert!(has(&path), "the marker should be in the file before deleting");
        conn.execute("DELETE FROM history", []).unwrap();
        compact(&conn).unwrap();
        drop(conn);
        for f in ["t.db", "t.db-wal", "t.db-shm"] {
            assert!(!has(&dir.join(f)), "{f} still holds deleted history");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
