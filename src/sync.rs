//! Sync between the desktop app and your own server (`yourtube serve`).
//!
//! What syncs: watch history (with how far you got), subscriptions, playlists, searches, hidden
//! channels and For You feedback. What doesn't: the video catalogue, For You's picks and caches,
//! and downloads. Each machine rebuilds those itself.
//!
//! How: triggers on those tables note every change in `sync_state` (which row, when, and a running
//! number). A sync sends the server every local change it hasn't sent yet and gets back every change
//! the server has had since the last sync, a batch at a time. When both sides changed the same row,
//! the later change wins (clocks are NTP-synced; equal times keep what's there). Deletions travel too:
//! a changed row that no longer exists goes out as deleted.
//!
//! The first time a database is opened with this, every row it already has is noted as a change
//! (dated by its own timestamp where it has one), so the first sync merges both sides.

use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params_from_iter, types::Value as Sql};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Changes per request, each way.
pub const BATCH: usize = 1000;

pub struct Table {
    pub name: &'static str,
    pub key: &'static [&'static str],
    /// The other columns that sync. Anything else in the table (caches) stays local.
    pub cols: &'static [&'static str],
    /// When an existing row last changed, in ms, for the first sync (SQL over the row).
    seed_at: &'static str,
    /// Only these columns' updates count as changes (default: any column).
    update_of: Option<&'static str>,
}

pub const TABLES: &[Table] = &[
    Table {
        name: "history",
        key: &["video_id"],
        cols: &["title", "author", "author_id", "watched_at", "position", "length"],
        seed_at: "watched_at * 1000",
        update_of: None,
    },
    Table { name: "channels", key: &["id"], cols: &["name", "subscribed"], seed_at: "0", update_of: Some("name, subscribed") },
    Table { name: "playlists", key: &["id"], cols: &["name", "protected"], seed_at: "0", update_of: None },
    Table {
        name: "playlist_items",
        key: &["playlist_id", "video_id"],
        cols: &["title", "author", "author_id", "length", "added"],
        seed_at: "added * 1000",
        update_of: None,
    },
    Table { name: "searches", key: &["query", "at"], cols: &[], seed_at: "at * 1000", update_of: None },
    Table { name: "rec_feedback", key: &["video_id", "kind"], cols: &["channel_id", "title", "at"], seed_at: "at * 1000", update_of: None },
    Table { name: "blocked_channels", key: &["channel_id"], cols: &["name", "at"], seed_at: "at * 1000", update_of: None },
];

const NOW_MS: &str = "CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)";
/// Running numbers come from a counter that only goes up (`sync_meta.seq`). "Highest so far + 1"
/// would hand out a number again after that row is replaced, and the change would never be sent.
const BUMP_SEQ: &str = "UPDATE sync_meta SET v = v + 1 WHERE k = 'seq';";
// coalesce: if the counter row were ever missing, writing your history must still work.
const SEQ: &str = "coalesce((SELECT v FROM sync_meta WHERE k = 'seq'), 0)";

/// One row's change: its table, key, when it changed (ms) and what it is now (None: deleted).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub t: String,
    pub k: Vec<Value>,
    pub at: i64,
    pub row: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// The last server change this device has.
    pub cursor: i64,
    pub changes: Vec<Change>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// Up to which server change this answer goes.
    pub cursor: i64,
    /// More server changes are waiting.
    pub more: bool,
    pub changes: Vec<Change>,
    /// How many of the request's changes the server took (the rest were older than its own).
    pub applied: usize,
}

/// What a sync did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub sent: usize,
    pub taken_by_server: usize,
    pub received: usize,
    pub applied: usize,
}

/// Tables and triggers; db::init runs this on every open (safe to repeat).
pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_state (
             tbl TEXT NOT NULL,
             key TEXT NOT NULL,      -- JSON array of the key columns
             at INTEGER NOT NULL,    -- when it last changed, unix ms
             seq INTEGER NOT NULL,   -- running number; -1 = came from the server, nothing to send
             PRIMARY KEY (tbl, key)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS sync_state_seq ON sync_state(seq);
         CREATE TABLE IF NOT EXISTS sync_meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL);
         INSERT OR IGNORE INTO sync_meta (k, v) SELECT 'seq', coalesce(max(seq), 0) FROM sync_state;",
    )?;
    let mut sql = String::new();
    for t in TABLES {
        let key = |row: &str| t.key.iter().map(|k| format!("{row}.{k}")).collect::<Vec<_>>().join(", ");
        // DELETE then INSERT, not INSERT OR REPLACE: a statement's own OR clause (say INSERT OR IGNORE)
        // overrides the conflict handling of everything its triggers do.
        let note = |row: &str| {
            format!(
                "DELETE FROM sync_state WHERE tbl = '{n}' AND key = json_array({k}); {BUMP_SEQ} \
                 INSERT INTO sync_state (tbl, key, at, seq) VALUES ('{n}', json_array({k}), {NOW_MS}, {SEQ});",
                n = t.name,
                k = key(row)
            )
        };
        let of = t.update_of.map(|c| format!(" OF {c}")).unwrap_or_default();
        sql += &format!(
            "CREATE TRIGGER IF NOT EXISTS sync_{n}_ins AFTER INSERT ON {n} BEGIN {new} END;
             CREATE TRIGGER IF NOT EXISTS sync_{n}_upd AFTER UPDATE{of} ON {n} BEGIN {old} {new} END;
             CREATE TRIGGER IF NOT EXISTS sync_{n}_del AFTER DELETE ON {n} BEGIN {old} END;\n",
            n = t.name,
            new = note("NEW"),
            old = note("OLD"),
        );
    }
    conn.execute_batch(&sql)?;
    let seeded: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM sync_meta WHERE k = 'seeded')", [], |r| r.get(0))?;
    if !seeded {
        let tx = conn.unchecked_transaction()?;
        for t in TABLES {
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO sync_state (tbl, key, at, seq)
                     SELECT '{n}', json_array({k}), {at}, {SEQ} + row_number() OVER ()
                     FROM {n}",
                    n = t.name,
                    k = t.key.join(", "),
                    at = t.seed_at
                ),
                [],
            )?;
            tx.execute("UPDATE sync_meta SET v = (SELECT max(v, coalesce((SELECT max(seq) FROM sync_state), 0))) WHERE k = 'seq'", [])?;
        }
        tx.execute("INSERT INTO sync_meta (k, v) VALUES ('seeded', 1)", [])?;
        tx.commit()?;
    }
    Ok(())
}

fn table(name: &str) -> Result<&'static Table> {
    TABLES.iter().find(|t| t.name == name).ok_or_else(|| anyhow!("unknown table {name:?}"))
}

fn to_sql(v: &Value) -> Result<Sql> {
    Ok(match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Sql::Integer(i),
            None => Sql::Real(n.as_f64().ok_or_else(|| anyhow!("bad number"))?),
        },
        Value::String(s) => Sql::Text(s.clone()),
        _ => bail!("only plain values sync"),
    })
}

fn to_json(v: Sql) -> Value {
    match v {
        Sql::Null | Sql::Blob(_) => Value::Null,
        Sql::Integer(i) => i.into(),
        Sql::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        Sql::Text(s) => s.into(),
    }
}

fn meta(conn: &Connection, k: &str) -> Result<i64> {
    Ok(conn.query_row("SELECT v FROM sync_meta WHERE k = ?1", [k], |r| r.get(0)).optional()?.unwrap_or(0))
}

fn set_meta(conn: &Connection, k: &str, v: i64) -> Result<()> {
    conn.execute("INSERT INTO sync_meta (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO UPDATE SET v = excluded.v", rusqlite::params![k, v])?;
    Ok(())
}

/// The key as sync_state stores it: SQLite's own json_array, so both sides spell it the same.
fn key_text(conn: &Connection, key: &[Value]) -> Result<String> {
    let marks = vec!["?"; key.len()].join(", ");
    let vals = key.iter().map(to_sql).collect::<Result<Vec<_>>>()?;
    Ok(conn.query_row(&format!("SELECT json_array({marks})"), params_from_iter(vals), |r| r.get(0))?)
}

fn read_row(conn: &Connection, t: &Table, key: &[Value]) -> Result<Option<Map<String, Value>>> {
    let cols = if t.cols.is_empty() { "1".to_owned() } else { t.cols.join(", ") };
    let cond = t.key.iter().map(|k| format!("{k} = ?")).collect::<Vec<_>>().join(" AND ");
    let vals = key.iter().map(to_sql).collect::<Result<Vec<_>>>()?;
    let row = conn
        .query_row(&format!("SELECT {cols} FROM {} WHERE {cond}", t.name), params_from_iter(vals), |r| {
            let mut m = Map::new();
            for (i, c) in t.cols.iter().enumerate() {
                m.insert((*c).to_owned(), to_json(r.get(i)?));
            }
            Ok(m)
        })
        .optional()?;
    Ok(row)
}

/// Changes noted after `after` (a running number), oldest first, at most `limit`.
/// Returns them, the running number of the last one (`after` if none) and whether more are waiting.
pub fn collect(conn: &Connection, after: i64, limit: usize) -> Result<(Vec<Change>, i64, bool)> {
    let mut stmt = conn.prepare("SELECT tbl, key, at, seq FROM sync_state WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
    let rows = stmt
        .query_map(rusqlite::params![after, limit as i64 + 1], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() > limit;
    let (mut out, mut last) = (Vec::new(), after);
    for (tbl, key, at, seq) in rows.into_iter().take(limit) {
        let Ok(t) = table(&tbl) else { continue };
        let k: Vec<Value> = serde_json::from_str(&key)?;
        out.push(Change { row: read_row(conn, t, &k)?, t: tbl, k, at });
        last = seq;
    }
    Ok((out, last, more))
}

/// Who gets this change next: the server passes changes on (a new running number); a device that
/// received one from the server has nothing to send back (-1).
#[derive(Clone, Copy)]
pub enum Then {
    PassOn,
    Keep,
}

/// Take one change from the other side, unless what's here is as new or newer.
/// Returns whether it was taken.
pub fn receive(conn: &Connection, c: &Change, then: Then) -> Result<bool> {
    let t = table(&c.t)?;
    if c.k.len() != t.key.len() {
        bail!("{}: the key needs {} parts", t.name, t.key.len());
    }
    let key = key_text(conn, &c.k)?;
    let here: Option<i64> =
        conn.query_row("SELECT at FROM sync_state WHERE tbl = ?1 AND key = ?2", [t.name, key.as_str()], |r| r.get(0)).optional()?;
    if here.is_some_and(|h| h >= c.at) {
        return Ok(false);
    }
    let mut vals = c.k.iter().map(to_sql).collect::<Result<Vec<_>>>()?;
    match &c.row {
        Some(row) => {
            if let Some(bad) = row.keys().find(|k| !t.cols.contains(&k.as_str())) {
                bail!("{}: unknown column {bad:?}", t.name);
            }
            let cols: Vec<&str> = t.cols.iter().copied().filter(|c| row.contains_key(*c)).collect();
            for col in &cols {
                vals.push(to_sql(&row[*col])?);
            }
            let names = t.key.iter().chain(cols.iter()).copied().collect::<Vec<_>>().join(", ");
            let marks = vec!["?"; t.key.len() + cols.len()].join(", ");
            let sql = if cols.is_empty() {
                format!("INSERT OR IGNORE INTO {} ({names}) VALUES ({marks})", t.name)
            } else {
                let set = cols.iter().map(|c| format!("{c} = excluded.{c}")).collect::<Vec<_>>().join(", ");
                format!("INSERT INTO {} ({names}) VALUES ({marks}) ON CONFLICT ({}) DO UPDATE SET {set}", t.name, t.key.join(", "))
            };
            conn.execute(&sql, params_from_iter(vals))?;
        }
        None => {
            let cond = t.key.iter().map(|k| format!("{k} = ?")).collect::<Vec<_>>().join(" AND ");
            conn.execute(&format!("DELETE FROM {} WHERE {cond}", t.name), params_from_iter(vals))?;
        }
    }
    // The triggers just noted this as a local change; record it as the other side's instead.
    conn.execute("DELETE FROM sync_state WHERE tbl = ?1 AND key = ?2", [t.name, key.as_str()])?;
    let seq = match then {
        Then::PassOn => {
            conn.execute(BUMP_SEQ, [])?;
            conn.query_row(&format!("SELECT {SEQ}"), [], |r| r.get::<_, i64>(0))?
        }
        Then::Keep => -1,
    };
    conn.execute("INSERT INTO sync_state (tbl, key, at, seq) VALUES (?1, ?2, ?3, ?4)", rusqlite::params![t.name, key, c.at, seq])?;
    Ok(true)
}

/// The server's side of one request: take the device's changes, answer with what the device
/// hasn't seen yet.
pub fn serve(conn: &mut Connection, req: &Request) -> Result<Response> {
    if req.changes.len() > 5 * BATCH {
        bail!("too many changes in one request");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut applied = 0;
    for c in &req.changes {
        applied += receive(&tx, c, Then::PassOn)? as usize;
    }
    let (changes, cursor, more) = collect(&tx, req.cursor, BATCH)?;
    tx.commit()?;
    Ok(Response { cursor, more, changes, applied })
}

/// The device's side: send what's new here and take what's new there, until both are done.
/// `send` delivers one request to the server (over HTTP, or straight to `serve` in tests).
pub fn run(conn: &mut Connection, mut send: impl FnMut(&Request) -> Result<Response>) -> Result<Stats> {
    let mut stats = Stats::default();
    for _ in 0..10_000 {
        let (pushed, cursor) = (meta(conn, "pushed")?, meta(conn, "cursor")?);
        let (changes, last, more_here) = collect(conn, pushed, BATCH)?;
        let resp = send(&Request { cursor, changes })?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for c in &resp.changes {
            stats.applied += receive(&tx, c, Then::Keep)? as usize;
        }
        set_meta(&tx, "pushed", last)?;
        set_meta(&tx, "cursor", resp.cursor)?;
        tx.commit()?;
        stats.sent += (last - pushed).max(0) as usize; // approximate: running numbers can have gaps
        stats.taken_by_server += resp.applied;
        stats.received += resp.changes.len();
        if !more_here && !resp.more {
            return Ok(stats);
        }
    }
    bail!("sync didn't finish")
}

/// The last sync on this device: when (unix seconds) and how it went.
pub static STATUS: std::sync::Mutex<Option<(i64, std::result::Result<Stats, String>)>> = std::sync::Mutex::new(None);
/// One sync at a time (the background one and "Sync now").
static RUNNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Is sync set up, and is this the main profile, not incognito?
pub fn wanted(cfg: &crate::config::Config) -> bool {
    !cfg.sync_server.is_empty() && !cfg.sync_key.is_empty() && crate::db::profile().is_none() && !crate::db::incognito()
}

/// While the app is open: sync now and every two minutes after (re-reading the config each time).
pub fn start_background() {
    std::thread::spawn(|| {
        loop {
            let cfg = crate::config::load();
            if wanted(&cfg) {
                let _ = sync_now(&cfg);
            }
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
    });
}

/// One line for Preferences.
pub fn status_text() -> String {
    match &*STATUS.lock().unwrap() {
        None => "Not synced yet".into(),
        Some((at, Ok(s))) => format!("Synced {}: sent {}, got {}", crate::fmt::age(*at), s.sent, s.applied),
        Some((at, Err(e))) => format!("Couldn't sync {}: {e}", crate::fmt::age(*at)),
    }
}

/// Sync this device with the server in the config (`sync_server`, `sync_key`), and note the
/// outcome in STATUS. Not for other profiles (they'd be merged into the server's main one) or
/// incognito.
pub fn sync_now(cfg: &crate::config::Config) -> Result<Stats> {
    let _one = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    let r = sync_once(cfg);
    *STATUS.lock().unwrap() = Some((crate::db::now(), r.as_ref().map(Clone::clone).map_err(|e| e.to_string())));
    r
}

fn sync_once(cfg: &crate::config::Config) -> Result<Stats> {
    if cfg.sync_server.is_empty() || cfg.sync_key.is_empty() {
        bail!("set the server address and sync key first");
    }
    if crate::db::profile().is_some() || crate::db::incognito() {
        bail!("only the main profile syncs");
    }
    let url = format!("{}/api/sync", cfg.sync_server.trim_end_matches('/'));
    if !(url.starts_with("https://") || url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost")) {
        bail!("the server address must start with https:// (the sync key would travel in the clear)");
    }
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .new_agent();
    let auth = format!("Bearer {}", cfg.sync_key);
    let mut conn = crate::db::open()?;
    run(&mut conn, |req| {
        let body = serde_json::to_vec(req)?;
        let resp = agent.post(&url).header("Authorization", &auth).header("Content-Type", "application/json").send(&body[..])?;
        let status = resp.status().as_u16();
        let text = resp.into_body().with_config().limit(64 << 20).read_to_string()?;
        match status {
            200 => Ok(serde_json::from_str(&text)?),
            401 => bail!("the server refused the sync key"),
            404 => bail!("the server doesn't sync yet (run `yourtube sync-key` there)"),
            s => bail!("the server answered {s}: {}", text.chars().take(200).collect::<String>()),
        }
    })
}

/// A new sync key: random, shown once. The server keeps only its SHA-256.
pub fn new_key() -> Result<(String, String)> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow!("no randomness: {e}"))?;
    let key: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok((key.clone(), key_hash(&key)))
}

pub fn key_hash(key: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(key.as_bytes());
    format!("sha256:{}", h.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

/// Does this Authorization header carry the key whose hash the server keeps?
pub fn key_ok(stored_hash: &str, authorization: Option<&str>) -> bool {
    let Some(key) = authorization.and_then(|a| a.strip_prefix("Bearer ")) else { return false };
    if stored_hash.is_empty() || key.is_empty() {
        return false;
    }
    let (a, b) = (key_hash(key.trim()), stored_hash);
    // Same length, every byte compared: no early exit to time.
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::db::init(&c).unwrap();
        c
    }

    fn watch(c: &Connection, id: &str, at: i64, pos: f64) {
        c.execute(
            "INSERT INTO history (video_id, title, watched_at, position) VALUES (?1, ?1, ?2, ?3)
             ON CONFLICT (video_id) DO UPDATE SET watched_at = excluded.watched_at, position = excluded.position",
            rusqlite::params![id, at, pos],
        )
        .unwrap();
    }

    fn position(c: &Connection, id: &str) -> Option<f64> {
        c.query_row("SELECT position FROM history WHERE video_id = ?1", [id], |r| r.get(0)).optional().unwrap()
    }

    /// A device syncing straight into a server connection.
    fn sync(device: &mut Connection, server: &mut Connection) -> Stats {
        // Changes are dated to the millisecond and equal times keep what's there. A fast machine
        // can make a change, sync and make the next one inside a millisecond, which people can't:
        // keep the steps apart, so each test checks what it means to.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let stats = run(device, |req| {
            let json = serde_json::to_string(req).unwrap(); // the real thing goes over JSON
            serve(server, &serde_json::from_str(&json).unwrap())
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        stats
    }

    #[test]
    fn existing_rows_merge_on_the_first_sync() {
        let (mut laptop, mut server) = (fresh(), fresh());
        // Rows from before sync existed: noted at their own times when the tables were made.
        watch(&laptop, "aaaaaaaaaaa", 100, 10.0);
        watch(&laptop, "bbbbbbbbbbb", 300, 30.0);
        watch(&server, "bbbbbbbbbbb", 200, 20.0); // older watch of the same video
        watch(&server, "ccccccccccc", 250, 25.0);
        for c in [&laptop, &server] {
            c.execute_batch("DELETE FROM sync_meta; DELETE FROM sync_state;").unwrap();
            init(c).unwrap(); // as on the first open with sync: every row noted at its own time
        }
        sync(&mut laptop, &mut server);
        for c in [&laptop, &server] {
            assert_eq!(position(c, "aaaaaaaaaaa"), Some(10.0));
            assert_eq!(position(c, "bbbbbbbbbbb"), Some(30.0), "the later watch wins");
            assert_eq!(position(c, "ccccccccccc"), Some(25.0));
        }
    }

    #[test]
    fn changes_go_both_ways_and_later_wins() {
        let (mut laptop, mut phone, mut server) = (fresh(), fresh(), fresh());
        watch(&laptop, "aaaaaaaaaaa", 100, 10.0);
        sync(&mut laptop, &mut server);
        sync(&mut phone, &mut server);
        assert_eq!(position(&phone, "aaaaaaaaaaa"), Some(10.0));
        // The phone carries on watching; the laptop then gets the new position.
        watch(&phone, "aaaaaaaaaaa", 200, 90.0);
        sync(&mut phone, &mut server);
        let s = sync(&mut laptop, &mut server);
        assert_eq!(position(&laptop, "aaaaaaaaaaa"), Some(90.0));
        assert_eq!(s.applied, 1);
        // Nothing new: nothing moves, and the laptop's copy isn't echoed back.
        let s = sync(&mut laptop, &mut server);
        assert_eq!((s.sent, s.applied), (0, 0));
    }

    #[test]
    fn an_older_change_loses() {
        let (mut laptop, mut server) = (fresh(), fresh());
        watch(&laptop, "aaaaaaaaaaa", 100, 10.0);
        sync(&mut laptop, &mut server);
        // The server changes it now; the laptop's change below was made "earlier" (its clock).
        watch(&server, "aaaaaaaaaaa", 300, 70.0);
        watch(&laptop, "aaaaaaaaaaa", 200, 50.0);
        laptop.execute("UPDATE sync_state SET at = 1 WHERE tbl = 'history'", []).unwrap();
        sync(&mut laptop, &mut server);
        assert_eq!(position(&server, "aaaaaaaaaaa"), Some(70.0));
        assert_eq!(position(&laptop, "aaaaaaaaaaa"), Some(70.0));
    }

    #[test]
    fn deletions_unsubscribes_and_playlists_travel() {
        let (mut laptop, mut server) = (fresh(), fresh());
        laptop
            .execute_batch(
                "INSERT INTO channels (id, name, subscribed) VALUES ('UC_aaaaaaaaaaaaaaaaaaaaa', 'Car Guy', 1);
                 INSERT INTO playlists (id, name) VALUES ('wl', 'Watch later');
                 INSERT INTO playlist_items (playlist_id, video_id, title, added) VALUES ('wl', 'aaaaaaaaaaa', 'A', 5);
                 INSERT INTO searches (query, at) VALUES ('camaro', 7);
                 INSERT INTO blocked_channels (channel_id, name, at) VALUES ('UC_bbbbbbbbbbbbbbbbbbbbb', 'Spam', 9);",
            )
            .unwrap();
        watch(&laptop, "aaaaaaaaaaa", 100, 10.0);
        sync(&mut laptop, &mut server);
        let count = |c: &Connection, sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count(&server, "SELECT count(*) FROM channels WHERE subscribed = 1"), 1);
        assert_eq!(count(&server, "SELECT count(*) FROM playlist_items"), 1);
        assert_eq!(count(&server, "SELECT count(*) FROM searches"), 1);
        assert_eq!(count(&server, "SELECT count(*) FROM blocked_channels"), 1);

        // Unsubscribe and clear things on the server (the web version); the laptop follows.
        server
            .execute_batch(
                "UPDATE channels SET subscribed = 0;
                 DELETE FROM playlist_items; DELETE FROM searches; DELETE FROM blocked_channels;
                 DELETE FROM history;",
            )
            .unwrap();
        sync(&mut laptop, &mut server);
        assert_eq!(count(&laptop, "SELECT count(*) FROM channels WHERE subscribed = 1"), 0);
        assert_eq!(count(&laptop, "SELECT count(*) FROM playlist_items"), 0);
        assert_eq!(count(&laptop, "SELECT count(*) FROM searches"), 0);
        assert_eq!(count(&laptop, "SELECT count(*) FROM blocked_channels"), 0);
        assert_eq!(position(&laptop, "aaaaaaaaaaa"), None);
    }

    #[test]
    fn cache_columns_dont_count_as_changes() {
        let (mut laptop, mut server) = (fresh(), fresh());
        laptop.execute("INSERT INTO channels (id, name) VALUES ('UC_aaaaaaaaaaaaaaaaaaaaa', 'Car Guy')", []).unwrap();
        sync(&mut laptop, &mut server);
        laptop.execute("UPDATE channels SET last_fetch = 123, fetch_error = 'x'", []).unwrap(); // a feed refresh
        assert_eq!(sync(&mut laptop, &mut server).sent, 0);
        let fetched: i64 = server.query_row("SELECT last_fetch FROM channels", [], |r| r.get(0)).unwrap();
        assert_eq!(fetched, 0, "the server keeps its own cache columns");
    }

    #[test]
    fn many_changes_go_in_batches() {
        let (mut laptop, mut server) = (fresh(), fresh());
        let tx = laptop.transaction().unwrap();
        for i in 0..(BATCH * 2 + 17) {
            watch(&tx, &format!("v{i:010}"), i as i64, 1.0);
        }
        tx.commit().unwrap();
        sync(&mut laptop, &mut server);
        let n: i64 = server.query_row("SELECT count(*) FROM history", [], |r| r.get(0)).unwrap();
        assert_eq!(n as usize, BATCH * 2 + 17);
        // And a second device gets them all too, a batch at a time.
        let mut phone = fresh();
        sync(&mut phone, &mut server);
        let n: i64 = phone.query_row("SELECT count(*) FROM history", [], |r| r.get(0)).unwrap();
        assert_eq!(n as usize, BATCH * 2 + 17);
    }

    #[test]
    fn bad_changes_are_refused() {
        let server = fresh();
        let bad =
            |t: &str, k: Vec<Value>, row: Option<Value>| Change { t: t.into(), k, at: 1, row: row.map(|r| r.as_object().unwrap().clone()) };
        assert!(receive(&server, &bad("videos", vec!["x".into()], None), Then::PassOn).is_err());
        assert!(receive(&server, &bad("history", vec![], None), Then::PassOn).is_err());
        let evil = serde_json::json!({"title": "x", "watched_at); DROP TABLE history; --": 1});
        assert!(receive(&server, &bad("history", vec!["aaaaaaaaaaa".into()], Some(evil)), Then::PassOn).is_err());
        let n: i64 = server.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'history'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn sync_keys() {
        let (key, hash) = new_key().unwrap();
        assert_eq!(key.len(), 64);
        assert!(hash.starts_with("sha256:"));
        assert!(key_ok(&hash, Some(&format!("Bearer {key}"))));
        assert!(!key_ok(&hash, Some("Bearer nope")));
        assert!(!key_ok(&hash, Some(&key)));
        assert!(!key_ok(&hash, None));
        assert!(!key_ok("", Some(&format!("Bearer {key}"))));
    }
}
