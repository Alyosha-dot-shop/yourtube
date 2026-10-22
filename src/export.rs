//! Take your data with you. Everything YourTube knows about you, in plain files:
//!
//! - `yourtube-data.json` — subscriptions, history, searches, playlists, For You feedback,
//!   hidden channels (everything, one file; YourTube can import it back),
//! - `subscriptions.csv` — Google Takeout's format (NewPipe and FreeTube import it),
//! - `subscriptions.opml` — for RSS readers,
//! - `history.csv`, `searches.csv`, `playlists.csv` — open them in any spreadsheet.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Summary {
    pub folder: PathBuf,
    pub files: Vec<String>,
    pub subscriptions: usize,
    pub history: usize,
    pub searches: usize,
    pub playlists: usize,
}

fn rows(conn: &Connection, sql: &str) -> Result<Vec<Vec<Value>>> {
    let mut st = conn.prepare(sql)?;
    let n = st.column_count();
    let out = st.query_map([], |r| {
        (0..n)
            .map(|i| {
                Ok(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(v) => json!(v),
                    rusqlite::types::ValueRef::Real(v) => json!(v),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                })
            })
            .collect::<rusqlite::Result<Vec<Value>>>()
    })?;
    Ok(out.collect::<rusqlite::Result<_>>()?)
}

fn objects(conn: &Connection, sql: &str, names: &[&str]) -> Result<Vec<Value>> {
    Ok(rows(conn, sql)?.into_iter().map(|r| Value::Object(names.iter().map(|n| n.to_string()).zip(r).collect())).collect())
}

fn csv_field(v: &Value) -> String {
    let s = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

fn write_csv(path: &Path, header: &[&str], rows: &[Vec<Value>]) -> Result<()> {
    let mut s = header.join(",") + "\n";
    for r in rows {
        s += &r.iter().map(csv_field).collect::<Vec<_>>().join(",");
        s += "\n";
    }
    std::fs::write(path, s)?;
    Ok(())
}

fn iso(ts: &Value) -> Value {
    ts.as_i64().map_or(Value::Null, |t| json!(crate::export::rfc3339(t)))
}

/// Unix seconds → "2026-10-22T13:05:00Z".
pub fn rfc3339(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Write everything into `folder/yourtube-export-<date>/`.
pub fn export_all(conn: &Connection, folder: &Path) -> Result<Summary> {
    let dir = folder.join(format!("yourtube-export-{}", &rfc3339(crate::db::now())[..10]));
    std::fs::create_dir_all(&dir)?;
    let mut sum = Summary { folder: dir.clone(), ..Default::default() };

    let subs = rows(conn, "SELECT id, name FROM channels WHERE subscribed = 1 ORDER BY name COLLATE NOCASE")?;
    let history =
        rows(conn, "SELECT video_id, title, author, author_id, watched_at, position, length FROM history ORDER BY watched_at DESC")?;
    let searches = rows(conn, "SELECT query, at FROM searches ORDER BY at DESC")?;
    let items = rows(
        conn,
        "SELECT p.name, i.video_id, i.title, i.author, i.author_id, i.added FROM playlist_items i JOIN playlists p ON p.id = i.playlist_id ORDER BY p.name, i.added",
    )?;
    sum.subscriptions = subs.len();
    sum.history = history.len();
    sum.searches = searches.len();

    // One file with everything.
    let playlists = objects(conn, "SELECT id, name FROM playlists ORDER BY name COLLATE NOCASE", &["id", "name"])?;
    sum.playlists = playlists.len();
    let playlists: Vec<Value> = playlists
        .into_iter()
        .map(|mut p| {
            let id = p["id"].as_str().unwrap_or_default().to_owned();
            p["videos"] = json!(
                objects(conn, &format!("SELECT video_id, title, author, author_id, length, added FROM playlist_items WHERE playlist_id = '{}' ORDER BY added", id.replace('\'', "''")), &["video_id", "title", "author", "author_id", "length", "added"])
                    .unwrap_or_default()
            );
            p
        })
        .collect();
    let all = json!({
        "app": "YourTube",
        "version": env!("CARGO_PKG_VERSION"),
        "exported_at": rfc3339(crate::db::now()),
        "profile": crate::db::profile_name(),
        "subscriptions": subs.iter().map(|r| json!({"channel_id": r[0], "name": r[1]})).collect::<Vec<_>>(),
        "history": history.iter().map(|r| json!({"video_id": r[0], "title": r[1], "channel": r[2], "channel_id": r[3], "watched_at": iso(&r[4]), "position_seconds": r[5], "length_seconds": r[6]})).collect::<Vec<_>>(),
        "searches": searches.iter().map(|r| json!({"query": r[0], "at": iso(&r[1])})).collect::<Vec<_>>(),
        "playlists": playlists,
        "feedback": objects(conn, "SELECT video_id, channel_id, title, kind, at FROM rec_feedback ORDER BY at DESC", &["video_id", "channel_id", "title", "kind", "at"])?,
        "hidden_channels": objects(conn, "SELECT channel_id, name, at FROM blocked_channels ORDER BY at DESC", &["channel_id", "name", "at"])?,
    });
    std::fs::write(dir.join("yourtube-data.json"), serde_json::to_string_pretty(&all)?)?;
    sum.files.push("yourtube-data.json".into());

    // Takeout's subscriptions.csv: "Channel Id,Channel Url,Channel Title".
    let takeout: Vec<Vec<Value>> = subs
        .iter()
        .map(|r| vec![r[0].clone(), json!(format!("http://www.youtube.com/channel/{}", r[0].as_str().unwrap_or(""))), r[1].clone()])
        .collect();
    write_csv(&dir.join("subscriptions.csv"), &["Channel Id", "Channel Url", "Channel Title"], &takeout)?;
    sum.files.push("subscriptions.csv".into());

    let mut opml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"1.1\">\n<head><title>YourTube subscriptions</title></head>\n<body>\n<outline text=\"YouTube\" title=\"YouTube\">\n",
    );
    for r in &subs {
        let (id, name) = (r[0].as_str().unwrap_or(""), r[1].as_str().unwrap_or(""));
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
        opml += &format!(
            "<outline text=\"{n}\" title=\"{n}\" type=\"rss\" xmlUrl=\"https://www.youtube.com/feeds/videos.xml?channel_id={id}\"/>\n",
            n = esc(name),
            id = esc(id)
        );
    }
    opml += "</outline>\n</body>\n</opml>\n";
    std::fs::write(dir.join("subscriptions.opml"), opml)?;
    sum.files.push("subscriptions.opml".into());

    let hist: Vec<Vec<Value>> =
        history.iter().map(|r| vec![iso(&r[4]), r[0].clone(), r[1].clone(), r[2].clone(), r[5].clone(), r[6].clone()]).collect();
    write_csv(&dir.join("history.csv"), &["watched_at", "video_id", "title", "channel", "position_seconds", "length_seconds"], &hist)?;
    sum.files.push("history.csv".into());
    let srch: Vec<Vec<Value>> = searches.iter().map(|r| vec![iso(&r[1]), r[0].clone()]).collect();
    write_csv(&dir.join("searches.csv"), &["searched_at", "query"], &srch)?;
    sum.files.push("searches.csv".into());
    let pl: Vec<Vec<Value>> = items.iter().map(|r| vec![r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone(), iso(&r[5])]).collect();
    write_csv(&dir.join("playlists.csv"), &["playlist", "video_id", "title", "channel", "added_at"], &pl)?;
    sum.files.push("playlists.csv".into());
    Ok(sum)
}

/// Delete one search, or all of them (`None`).
pub fn forget_searches(conn: &Connection, one: Option<(&str, i64)>) -> Result<usize> {
    Ok(match one {
        Some((q, at)) => conn.execute("DELETE FROM searches WHERE query = ?1 AND at = ?2", rusqlite::params![q, at])?,
        None => conn.execute("DELETE FROM searches", [])?,
    })
}

pub fn clear_history(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM history", [])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1792591391), "2026-10-21T14:03:11Z");
    }

    #[test]
    fn exports_everything() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO channels (id, name, subscribed) VALUES ('UCaaaaaaaaaaaaaaaaaaaaaa', 'A, \"quoted\" & co', 1), ('UCbbbbbbbbbbbbbbbbbbbbbb', 'Gone', 0);
             INSERT INTO history (video_id, title, author, watched_at, position, length) VALUES ('v1', 'Title, with comma', 'A', 1792591391, 30, 600);
             INSERT INTO searches (query, at) VALUES ('e30 turbo', 1792591391);
             INSERT INTO playlists (id, name) VALUES ('p1', 'Watch later');
             INSERT INTO playlist_items (playlist_id, video_id, title, added) VALUES ('p1', 'v1', 'Title, with comma', 1792591391);",
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("yourtube-export-test-{}", std::process::id()));
        let s = export_all(&conn, &dir).unwrap();
        assert_eq!((s.subscriptions, s.history, s.searches, s.playlists), (1, 1, 1, 1));
        let csv = std::fs::read_to_string(s.folder.join("subscriptions.csv")).unwrap();
        assert!(
            csv.contains("UCaaaaaaaaaaaaaaaaaaaaaa,http://www.youtube.com/channel/UCaaaaaaaaaaaaaaaaaaaaaa,\"A, \"\"quoted\"\" & co\""),
            "{csv}"
        );
        let all: Value = serde_json::from_str(&std::fs::read_to_string(s.folder.join("yourtube-data.json")).unwrap()).unwrap();
        assert_eq!(all["searches"][0]["query"], "e30 turbo");
        assert_eq!(all["playlists"][0]["videos"][0]["video_id"], "v1");
        assert!(std::fs::read_to_string(s.folder.join("subscriptions.opml")).unwrap().contains("&amp; co"));
        // And YourTube's own importer reads the Takeout-style CSV back.
        let mut fresh = Connection::open_in_memory().unwrap();
        crate::db::init(&fresh).unwrap();
        let n = crate::import::import_any(&mut fresh, &s.folder.join("subscriptions.csv")).unwrap();
        assert_eq!(n.subscriptions, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
