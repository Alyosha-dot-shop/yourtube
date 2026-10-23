//! Bring your data into YourTube: subscriptions, watch history, searches and
//! playlists, from YouTube itself and from other apps.
//!
//! [`import_any`] is the front door. Hand it a file or a folder and it works out
//! what it is. It understands:
//!
//! - **Google Takeout**, YouTube's own export: the `.zip` you download, the
//!   folder you get by unpacking it, or a single file from it. Takeout
//!   translates folder names into your language, so we recognise files by what
//!   is inside them, not by where they sit.
//! - **NewPipe**: the subscriptions export (`.json`) and the full backup (a
//!   `.zip` with `newpipe.db` inside).
//! - **FreeTube**: its data folder (`~/.config/FreeTube`) and the single files
//!   it exports (`freetube-subscriptions-….db` and friends).
//!
//! Every reader fills in one [`Found`], and [`save`] writes it in a single
//! transaction, so an import lands completely or not at all. Importing the same
//! thing twice adds nothing the second time, and the numbers in [`Counts`] say
//! what actually changed.
//!
//! Nothing here goes online. That's why a channel known only by its `@handle`
//! can't be imported: turning a handle into a channel id needs a request to
//! YouTube. Those are counted in [`Counts::skipped`].

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

const NOT_AN_EXPORT: &str = "This doesn't look like a Google Takeout, NewPipe or FreeTube export";

/// We never read a single file bigger than this. Takeout zips also carry the
/// videos you uploaded; we only open the small text files beside them.
const MAX_FILE: u64 = 1 << 30;

/// At most this much is unpacked from one zip (counting only the files we read), and it may
/// hold at most this many files (Takeout zips with uploaded videos have a few thousand).
const MAX_UNPACKED: u64 = 2 << 30;
const MAX_ENTRIES: usize = 200_000;

/// How deep we look inside a folder you picked.
const MAX_DEPTH: usize = 8;

const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// Where FreeTube keeps its data on this computer.
pub fn freetube_dir() -> PathBuf {
    dirs::config_dir().expect("no config dir").join("FreeTube")
}

/// What an import added. Each number counts only what is new or changed, so
/// importing the same export again gives all zeros (apart from `skipped`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Channels you are now subscribed to that you weren't before.
    pub subscriptions: usize,
    /// Videos added to your history, or whose entry got a newer watch time.
    pub history: usize,
    /// Playlists that were created or got new videos.
    pub playlists: usize,
    /// Videos added to playlists.
    pub playlist_items: usize,
    /// Searches added to your search history.
    pub searches: usize,
    /// Things we found but couldn't bring over: removed videos, entries with a
    /// time we couldn't read, channels known only by `@handle` or `/c/name`,
    /// YouTube Music plays, and anything from a service other than YouTube.
    /// Ads YouTube showed you are left out on purpose and not counted here.
    pub skipped: usize,
}

/// Import whatever `path` is: a Takeout zip or folder, a NewPipe export or
/// backup, a FreeTube folder or export file, or a folder holding any of these
/// (for example the several zips a large Takeout is split into).
pub fn import_any(conn: &mut Connection, path: &Path) -> Result<Counts> {
    let mut found = Found::default();
    if path.is_dir() {
        if is_freetube_dir(path) {
            read_freetube_dir(path, &mut found)?;
        } else {
            walk(path, 0, &mut found)?;
        }
    } else {
        read_path(path, &mut found)?;
    }
    if !found.recognized {
        bail!(NOT_AN_EXPORT);
    }
    save(conn, found)
}

/// The older name for [`import_any`], from when YourTube only read FreeTube's
/// folder. It now takes anything `import_any` does.
pub fn import(conn: &mut Connection, path: &Path) -> Result<Counts> {
    import_any(conn, path)
}

// ---------------------------------------------------------------------------
// What the readers find, before anything is written.

#[derive(Default)]
struct Found {
    channels: Vec<Channel>,
    watches: Vec<Watch>,
    /// (query, unix seconds)
    searches: Vec<(String, i64)>,
    playlists: Vec<Playlist>,
    skipped: usize,
    /// Did at least one file turn out to be something we understand?
    recognized: bool,
}

struct Channel {
    id: String,
    name: String,
    thumbnail: Option<String>,
}

#[derive(Clone)]
struct Watch {
    video_id: String,
    title: Option<String>,
    author: Option<String>,
    author_id: Option<String>,
    /// Unix seconds.
    at: i64,
    /// Seconds into the video, when the source knows (Takeout doesn't).
    position: Option<f64>,
    length: Option<f64>,
}

struct Playlist {
    /// The id to use if we have to create it. Imports look up an existing
    /// playlist by this id first, then by name, so re-imports land in the same place.
    id: String,
    name: String,
    protected: bool,
    items: Vec<PlaylistItem>,
}

struct PlaylistItem {
    video_id: String,
    title: Option<String>,
    author: Option<String>,
    author_id: Option<String>,
    length: Option<f64>,
    /// Unix seconds.
    added: i64,
}

// ---------------------------------------------------------------------------
// Finding the files.

/// One file on disk: a zip, a NewPipe database, or a text file.
fn read_path(path: &Path, found: &mut Found) -> Result<()> {
    let context = || format!("reading {}", path.display());
    let mut file = std::fs::File::open(path).with_context(context)?;
    let mut head = [0u8; 16];
    let n = file.read(&mut head).with_context(context)?;
    let head = &head[..n];
    if head.starts_with(b"PK\x03\x04") {
        file.rewind().with_context(context)?;
        return read_zip(file, found).with_context(context);
    }
    if head.starts_with(SQLITE_MAGIC) {
        // Open it where it is, read-only. No need to copy.
        if read_newpipe_db(path, found).with_context(context)? {
            found.recognized = true;
        }
        return Ok(());
    }
    if file.metadata().with_context(context)?.len() > MAX_FILE {
        return Ok(());
    }
    let bytes = std::fs::read(path).with_context(context)?;
    read_file(&path.to_string_lossy(), &bytes, found).with_context(context)
}

/// Text-ish files we know how to read. Everything else (videos, images) is ignored.
fn wanted(name: &str) -> bool {
    matches!(extension(name).as_str(), "csv" | "json" | "html" | "htm" | "db")
}

fn extension(name: &str) -> String {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Read every file we care about inside a zip, without unpacking it to disk.
fn read_zip<R: Read + Seek>(reader: R, found: &mut Found) -> Result<()> {
    let mut zip = zip::ZipArchive::new(reader).context("this zip file looks damaged")?;
    anyhow::ensure!(zip.len() <= MAX_ENTRIES, "this zip has more than {MAX_ENTRIES} files; that's not an export");
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() || entry.size() > MAX_FILE {
            continue;
        }
        let name = entry.name()?.into_owned();
        if !wanted(&name) {
            continue;
        }
        // A "zip bomb" claims small and unpacks huge: count what we actually unpack, and never
        // reserve memory from the size the zip claims.
        let room = MAX_UNPACKED.saturating_sub(total);
        let mut bytes = Vec::new();
        entry.by_ref().take(room + 1).read_to_end(&mut bytes).with_context(|| format!("unpacking {name}"))?;
        total += bytes.len() as u64;
        anyhow::ensure!(total <= MAX_UNPACKED, "this zip unpacks to more than {} GB of history; that's not an export", MAX_UNPACKED >> 30);
        read_file(&name, &bytes, found).with_context(|| format!("reading {name}"))?;
    }
    Ok(())
}

/// Look through a folder (and the folders inside it) for anything we can read.
fn walk(dir: &Path, depth: usize, found: &mut Found) -> Result<()> {
    let mut entries: Vec<_> =
        std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        // file_type() doesn't follow symlinks, so a link loop can't trap us.
        let Ok(kind) = e.file_type() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        if kind.is_dir() && depth < MAX_DEPTH {
            if is_freetube_dir(&path) {
                read_freetube_dir(&path, found)?;
            } else {
                walk(&path, depth + 1, found)?;
            }
        } else if kind.is_file() && (wanted(&name) || extension(&name) == "zip") {
            read_path(&path, found)?;
        }
    }
    Ok(())
}

/// Work out what one file is from its contents (and, for CSV and HTML, its
/// extension), and read it.
fn read_file(name: &str, bytes: &[u8], found: &mut Found) -> Result<()> {
    if bytes.starts_with(SQLITE_MAGIC) {
        // A database inside a zip: SQLite needs a real file, so copy it out.
        let tmp = TempFile::new(bytes)?;
        if read_newpipe_db(&tmp.0, found)? {
            found.recognized = true;
        }
        return Ok(());
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}');
    let ok = match extension(name).as_str() {
        "csv" => read_csv(name, text, found),
        "html" | "htm" => read_activity_html(text, found),
        // JSON, FreeTube's .db files, or a file with no telling extension.
        // NeDB goes first: a one-line FreeTube file is valid JSON too.
        _ => read_nedb(text, found) || read_json(text, found) || read_activity_html(text, found),
    };
    found.recognized |= ok;
    Ok(())
}

/// A copy of a database pulled out of a zip, deleted again when we're done. It holds someone's
/// watch history, so it goes in YourTube's private folder under a random name, readable only by you.
struct TempFile(PathBuf);

impl TempFile {
    fn new(bytes: &[u8]) -> Result<Self> {
        use std::io::Write;
        let mut r = [0u8; 12];
        getrandom::fill(&mut r).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        let name: String = r.iter().map(|b| format!("{b:02x}")).collect();
        let path = crate::proc::run_dir().join(format!("import-{name}.db"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path).with_context(|| format!("writing {}", path.display()))?;
        let file = TempFile(path);
        f.write_all(bytes)?;
        Ok(file)
    }
}

/// Open a database someone gave us, the way SQLite advises for untrusted files: read-only, no
/// triggers or views, schema not trusted, and checked for damage before anything is read.
fn open_untrusted(path: &Path) -> Result<Connection> {
    use rusqlite::config::DbConfig;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false)?;
    let check: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    anyhow::ensure!(check == "ok", "this database is damaged ({check})");
    Ok(db)
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// FreeTube.
//
// FreeTube stores data as NeDB files: one JSON object per line, appended on
// every change. The last line for an `_id` wins, and `"$$deleted": true`
// removes the entry. Times are in milliseconds. Its export files use the same
// format, so we tell them apart by what each record looks like.

fn is_freetube_dir(dir: &Path) -> bool {
    ["profiles.db", "history.db", "playlists.db"].iter().any(|f| dir.join(f).is_file())
}

fn read_freetube_dir(dir: &Path, found: &mut Found) -> Result<()> {
    for f in ["profiles.db", "history.db", "playlists.db"] {
        let path = dir.join(f);
        if path.is_file() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            found.recognized |= read_nedb(&text, found);
        }
    }
    Ok(())
}

/// Current records of a NeDB file, in first-seen order.
fn nedb_records(text: &str) -> Vec<Value> {
    let mut order = Vec::new();
    let mut latest: HashMap<String, Value> = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(id) = v.get("_id").and_then(Value::as_str).map(str::to_owned) else { continue };
        if !latest.contains_key(&id) {
            order.push(id.clone());
        }
        latest.insert(id, v);
    }
    order.into_iter().filter_map(|id| latest.remove(&id)).filter(|v| v.get("$$deleted").and_then(Value::as_bool) != Some(true)).collect()
}

/// Read FreeTube records: profiles (subscriptions), history and playlists.
/// Returns whether any of them looked like FreeTube's.
fn read_nedb(text: &str, found: &mut Found) -> bool {
    let mut any = false;
    for r in nedb_records(text) {
        if let Some(subs) = r.get("subscriptions").and_then(Value::as_array) {
            // A profile. "All Channels" holds every subscription; the others
            // are subsets of it, so reading them all changes nothing.
            any = true;
            for c in subs {
                match (s(c, "id"), s(c, "name")) {
                    (Some(id), Some(name)) if is_channel_id(&id) => found.channels.push(Channel { id, name, thumbnail: s(c, "thumbnail") }),
                    _ => found.skipped += 1,
                }
            }
        } else if let (Some(id), Some(ms)) = (s(&r, "videoId"), r.get("timeWatched").and_then(Value::as_i64)) {
            any = true;
            found.watches.push(Watch {
                title: s(&r, "title"),
                author: s(&r, "author"),
                author_id: s(&r, "authorId"),
                at: ms / 1000,
                position: Some(r.get("watchProgress").and_then(Value::as_f64).unwrap_or(0.0)),
                length: positive(&r, "lengthSeconds"),
                video_id: id,
            });
        } else if let (Some(id), Some(name)) = (s(&r, "_id"), s(&r, "playlistName")) {
            any = true;
            let items = r
                .get("videos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| {
                    Some(PlaylistItem {
                        video_id: s(v, "videoId")?,
                        title: s(v, "title"),
                        author: s(v, "author"),
                        author_id: s(v, "authorId"),
                        length: positive(v, "lengthSeconds"),
                        added: v.get("timeAdded").and_then(Value::as_i64).unwrap_or(0) / 1000,
                    })
                })
                .collect();
            let protected = r.get("protected").and_then(Value::as_bool).unwrap_or(false);
            found.playlists.push(Playlist { id, name, protected, items });
        }
    }
    any
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned)
}

fn positive(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64).filter(|x| *x > 0.0)
}

// ---------------------------------------------------------------------------
// JSON: Takeout activity, NewPipe subscriptions, and the old Takeout
// subscriptions format (which FreeTube can also export).

fn read_json(text: &str, found: &mut Found) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    if let Some(subs) = v.get("subscriptions").and_then(Value::as_array).filter(|_| v.get("_id").is_none()) {
        // NewPipe: {"app_version": …, "subscriptions": [{"service_id": 0, "url": …, "name": …}]}
        for sub in subs {
            newpipe_channel(sub.get("service_id").and_then(Value::as_i64).unwrap_or(0), s(sub, "url"), s(sub, "name"), None, found);
        }
        return true;
    }
    let Some(list) = v.as_array() else { return false };
    if list.iter().any(|e| e.get("time").is_some() && (e.get("titleUrl").is_some() || e.get("title").is_some())) {
        read_activity_json(list, found);
        return true;
    }
    if list.iter().any(|e| e.pointer("/snippet/resourceId/channelId").is_some()) {
        // Old Takeout subscriptions.json: [{"snippet": {"resourceId": {"channelId": …}, "title": …}}]
        for e in list {
            let id = e.pointer("/snippet/resourceId/channelId").and_then(Value::as_str).unwrap_or_default();
            if is_channel_id(id) {
                let name = e.pointer("/snippet/title").and_then(Value::as_str).unwrap_or(id);
                let thumbnail = e.pointer("/snippet/thumbnails/default/url").and_then(Value::as_str).map(str::to_owned);
                found.channels.push(Channel { id: id.to_owned(), name: name.to_owned(), thumbnail });
            } else {
                found.skipped += 1;
            }
        }
        return true;
    }
    false
}

/// Takeout's watch-history.json and search-history.json (and My Activity in
/// JSON form). Each entry is one thing you did, with a link and a time.
fn read_activity_json(list: &[Value], found: &mut Found) {
    for e in list {
        // My Activity can mix in other Google products; "YouTube" and
        // "YouTube Music" are product names, which Takeout doesn't translate.
        if s(e, "header").is_some_and(|h| !h.starts_with("YouTube")) {
            continue;
        }
        let is_ad = e.get("details").and_then(Value::as_array).is_some_and(|d| !d.is_empty());
        if is_ad {
            continue;
        }
        let at = s(e, "time").and_then(|t| parse_iso(&t));
        let title = s(e, "title").unwrap_or_default();
        let Some(url) = s(e, "titleUrl") else {
            // "Watched a video that has been removed" comes without a link.
            if title.starts_with("Watched") {
                found.skipped += 1;
            }
            continue;
        };
        match (parse_link(&url), at) {
            (Link::Video(id), Some(at)) => {
                let sub = e.get("subtitles").and_then(|x| x.get(0));
                let title = title.strip_prefix("Watched ").unwrap_or(&title).trim();
                found.watches.push(Watch {
                    title: real_title(title),
                    author: sub.and_then(|x| s(x, "name")),
                    author_id: sub.and_then(|x| s(x, "url")).and_then(|u| channel_id_in(&u)),
                    at,
                    position: None,
                    length: None,
                    video_id: id,
                });
            }
            (Link::Search(q), Some(at)) => found.searches.push((q, at)),
            (Link::Video(_) | Link::Search(_), None) | (Link::Music, _) => found.skipped += 1,
            (Link::Other, _) => {} // community posts, visited pages: not something we keep
        }
    }
}

/// A removed or private video is listed with its URL as the title; that's no title at all.
fn real_title(t: &str) -> Option<String> {
    let t = t.trim();
    (!t.is_empty() && !t.starts_with("http://") && !t.starts_with("https://")).then(|| t.to_owned())
}

// ---------------------------------------------------------------------------
// HTML: Takeout's default format for watch and search history.
//
// Each entry is a card like this (trimmed):
//
//   <div class="outer-cell …">
//     <p class="mdl-typography--title">YouTube<br></p>
//     <div class="… mdl-typography--body-1">Watched <a href="https://www.youtube.com/watch?v=ID">Title</a><br>
//       <a href="https://www.youtube.com/channel/UC…">Channel</a><br>Oct 21, 2026, 3:04:05 PM GMT+04:00<br></div>
//     <div class="… mdl-typography--caption"><b>Products:</b>…<b>Why is this here?</b>…</div>
//   </div>
//
// Ads carry a third label in the caption ("Details: From Google Ads"). We go by
// the links rather than the words ("Watched", "Searched for"), because the words
// are translated and the links aren't.

fn read_activity_html(text: &str, found: &mut Found) -> bool {
    if !text.contains("outer-cell") {
        return false;
    }
    let mut any = false;
    for cell in text.split("<div class=\"outer-cell").skip(1) {
        let header = between(cell, "mdl-typography--title\">", "<").map(str::trim).unwrap_or_default();
        if !header.is_empty() && !header.starts_with("YouTube") {
            continue; // another Google product
        }
        let Some(body) = between(cell, "mdl-typography--body-1\">", "</div>") else { continue };
        any = true;
        let caption = cell.split("mdl-typography--caption\">").nth(1).unwrap_or_default();
        let is_ad = caption.matches("<b>").count() >= 3 || cell.contains("From Google Ads");
        if is_ad {
            continue;
        }
        let links = links(body);
        let at = body.split("<br>").map(str::trim).filter(|p| !p.is_empty()).last().and_then(|p| parse_stamp(&unescape(&strip_tags(p))));
        let Some((href, label)) = links.first() else {
            // A removed video is listed as plain text, without a link.
            if strip_tags(body).trim_start().starts_with("Watched") {
                found.skipped += 1;
            }
            continue;
        };
        match (parse_link(href), at) {
            (Link::Video(id), Some(at)) => {
                let channel = links.get(1).filter(|(h, _)| h.contains("/channel/"));
                found.watches.push(Watch {
                    title: real_title(&unescape(&strip_tags(label))),
                    author: channel.map(|(_, name)| unescape(&strip_tags(name)).trim().to_owned()).filter(|n| !n.is_empty()),
                    author_id: channel.and_then(|(h, _)| channel_id_in(h)),
                    at,
                    position: None,
                    length: None,
                    video_id: id,
                });
            }
            (Link::Search(q), Some(at)) => {
                // The link text is the query as you typed it; the URL is a fallback.
                let typed = unescape(&strip_tags(label)).trim().to_owned();
                found.searches.push((if typed.is_empty() { q } else { typed }, at));
            }
            (Link::Video(_) | Link::Search(_), None) | (Link::Music, _) => found.skipped += 1,
            (Link::Other, _) => {}
        }
    }
    any
}

/// The text between `start` and the next `end`.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &s[s.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

/// Every `<a href="…">text</a>` in a piece of HTML, as (href, inner HTML).
fn links(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<a href=\"") {
        rest = &rest[i + 9..];
        let Some(q) = rest.find('"') else { break };
        let href = unescape(&rest[..q]);
        rest = &rest[q..];
        let Some(gt) = rest.find('>') else { break };
        rest = &rest[gt + 1..];
        let end = rest.find("</a>").unwrap_or(rest.len());
        out.push((href, rest[..end].to_owned()));
        rest = &rest[end..];
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Turn `&amp;`, `&#39;` and friends back into characters.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').filter(|&j| j <= 10).and_then(|j| {
            let c = match &rest[1..j] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                e if e.starts_with("#x") || e.starts_with("#X") => char::from_u32(u32::from_str_radix(&e[2..], 16).ok()?)?,
                e if e.starts_with('#') => char::from_u32(e[1..].parse().ok()?)?,
                _ => return None,
            };
            Some((c, j))
        });
        match decoded {
            Some((c, j)) => {
                out.push(c);
                rest = &rest[j + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// CSV: Takeout subscriptions and playlists.

fn read_csv(name: &str, text: &str, found: &mut Found) -> bool {
    let rows = csv_rows(text);

    // subscriptions.csv: Channel Id, Channel Url, Channel Title. The header is
    // translated, so we look for rows whose second column is a YouTube link.
    // (That also keeps out Takeout's files about your own channel, whose
    // second column is a title or an image link.)
    let mut subscriptions = false;
    for r in rows.iter().filter(|r| r.len() >= 2 && is_youtube_url(&r[1])) {
        let id = if is_channel_id(&r[0]) { Some(r[0].clone()) } else { channel_id_in(&r[1]) };
        match id {
            Some(id) => {
                let name = r.get(2).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| id.clone());
                found.channels.push(Channel { id, name, thumbnail: None });
                subscriptions = true;
            }
            None if is_named_channel_url(&r[1]) => {
                found.skipped += 1;
                subscriptions = true;
            }
            None => {}
        }
    }
    if subscriptions {
        return true;
    }

    // A playlist: rows of (video id, time added). Today's Takeout writes one
    // "<title>-videos.csv" per playlist; older ones started with a block about
    // the playlist (with its title) before the video rows.
    let items: Vec<PlaylistItem> = rows
        .iter()
        .filter(|r| r.len() == 2 && is_video_id(&r[0]))
        .filter_map(|r| {
            Some(PlaylistItem {
                video_id: r[0].clone(),
                added: parse_iso(&r[1])?,
                title: None,
                author: None,
                author_id: None,
                length: None,
            })
        })
        .collect();
    if items.is_empty() {
        return false;
    }
    let old_title =
        rows.first().and_then(|h| h.iter().position(|c| c.eq_ignore_ascii_case("title"))).and_then(|i| rows.get(1)?.get(i).cloned());
    let title = old_title.filter(|t| !t.is_empty()).unwrap_or_else(|| playlist_name_from_file(name));
    found.playlists.push(Playlist { id: format!("import-{title}"), name: title, protected: false, items });
    true
}

/// "Takeout/…/playlists/Road trip-videos.csv" → "Road trip".
fn playlist_name_from_file(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = file.strip_suffix(".csv").or_else(|| file.strip_suffix(".CSV")).unwrap_or(file);
    let lower = stem.to_lowercase();
    match lower.strip_suffix("-videos") {
        Some(_) if stem.len() > 7 => stem[..stem.len() - 7].to_owned(),
        _ => stem.to_owned(),
    }
}

/// Split CSV into rows of trimmed fields. Handles quoted fields with commas,
/// doubled quotes and line breaks inside them. Trailing empty fields and blank
/// rows are dropped.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let finish_row = |row: &mut Vec<String>, rows: &mut Vec<Vec<String>>| {
        let mut r: Vec<String> = std::mem::take(row).into_iter().map(|f| f.trim().to_owned()).collect();
        while r.last().is_some_and(String::is_empty) {
            r.pop();
        }
        if !r.is_empty() {
            rows.push(r);
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                finish_row(&mut row, &mut rows);
            }
            _ => field.push(c),
        }
    }
    row.push(field);
    finish_row(&mut row, &mut rows);
    rows
}

// ---------------------------------------------------------------------------
// NewPipe.

/// Add one NewPipe subscription, or count it as skipped if it isn't a YouTube
/// channel we can identify offline.
fn newpipe_channel(service: i64, url: Option<String>, name: Option<String>, thumbnail: Option<String>, found: &mut Found) {
    // Service 0 is YouTube; others are SoundCloud, PeerTube and so on.
    match url.as_deref().filter(|_| service == 0).and_then(channel_id_in) {
        Some(id) => found.channels.push(Channel { name: name.unwrap_or_else(|| id.clone()), id, thumbnail }),
        None => found.skipped += 1,
    }
}

/// One row of NewPipe's `streams`: service, url, title, uploader, uploader url, length.
type StreamRow = (i64, String, Option<String>, Option<String>, Option<String>, Option<f64>);

/// Read NewPipe's backup database. Returns false if it isn't NewPipe's.
///
/// The tables we use (NewPipe has kept them stable for years):
/// `subscriptions(service_id, url, name, avatar_url)`,
/// `streams(uid, service_id, url, title, uploader, uploader_url, duration)`,
/// `stream_history(stream_id, access_date)` with times in milliseconds,
/// `stream_state(stream_id, progress_time)` in milliseconds,
/// `search_history(service_id, search, creation_date)`,
/// `playlists(uid, name)` and `playlist_stream_join(playlist_id, stream_id, join_index)`.
fn read_newpipe_db(path: &Path, found: &mut Found) -> Result<bool> {
    let db = open_untrusted(path)?;
    let tables: HashSet<String> =
        db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if !tables.contains("subscriptions") || !tables.contains("streams") {
        return Ok(false);
    }
    let has_column = |table: &str, column: &str| -> bool { db.prepare(&format!("SELECT {column} FROM {table} LIMIT 0")).is_ok() };
    let unexpected = |what: &str| format!("NewPipe backup: couldn't read {what} (the database layout isn't one we know)");

    let avatar = if has_column("subscriptions", "avatar_url") { "avatar_url" } else { "NULL" };
    let mut st =
        db.prepare(&format!("SELECT service_id, url, name, {avatar} FROM subscriptions")).with_context(|| unexpected("subscriptions"))?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    for row in rows {
        let (service, url, name, thumb) = row.with_context(|| unexpected("subscriptions"))?;
        newpipe_channel(service, url, name, thumb, found);
    }

    let stream = |r: &rusqlite::Row| -> rusqlite::Result<StreamRow> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, Option<f64>>(5)?.filter(|d| *d > 0.0)))
    };
    let uploader_url = if has_column("streams", "uploader_url") { "s.uploader_url" } else { "NULL" };

    if tables.contains("stream_history") {
        let progress =
            if tables.contains("stream_state") { "(SELECT progress_time FROM stream_state WHERE stream_id = s.uid)" } else { "NULL" };
        let mut st = db
            .prepare(&format!(
                "SELECT s.service_id, s.url, s.title, s.uploader, {uploader_url}, s.duration, MAX(h.access_date), {progress}
                 FROM stream_history h JOIN streams s ON s.uid = h.stream_id GROUP BY s.uid"
            ))
            .with_context(|| unexpected("watch history"))?;
        let rows = st.query_map([], |r| Ok((stream(r)?, r.get::<_, i64>(6)?, r.get::<_, Option<f64>>(7)?)))?;
        for row in rows {
            let ((service, url, title, uploader, uploader_url, length), ms, progress_ms) =
                row.with_context(|| unexpected("watch history"))?;
            match parse_link(&url) {
                Link::Video(id) if service == 0 => found.watches.push(Watch {
                    title,
                    author: uploader,
                    author_id: uploader_url.and_then(|u| channel_id_in(&u)),
                    at: ms / 1000,
                    position: Some(progress_ms.unwrap_or(0.0) / 1000.0),
                    length,
                    video_id: id,
                }),
                _ => found.skipped += 1,
            }
        }
    }

    if tables.contains("search_history") {
        let mut st =
            db.prepare("SELECT service_id, search, creation_date FROM search_history").with_context(|| unexpected("search history"))?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            match row.with_context(|| unexpected("search history"))? {
                (0, Some(q), ms) if !q.trim().is_empty() => found.searches.push((q.trim().to_owned(), ms / 1000)),
                _ => found.skipped += 1,
            }
        }
    }

    if tables.contains("playlists") && tables.contains("playlist_stream_join") {
        let mut lists = db.prepare("SELECT uid, name FROM playlists").with_context(|| unexpected("playlists"))?;
        let lists: Vec<(i64, String)> = lists.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut st = db
            .prepare(&format!(
                "SELECT s.service_id, s.url, s.title, s.uploader, {uploader_url}, s.duration
                 FROM playlist_stream_join j JOIN streams s ON s.uid = j.stream_id
                 WHERE j.playlist_id = ?1 ORDER BY j.join_index"
            ))
            .with_context(|| unexpected("playlists"))?;
        // NewPipe doesn't record when a video was added. Counting down from
        // now keeps NewPipe's order, since playlists show the newest first.
        let now = crate::db::now();
        for (uid, name) in lists {
            let mut items = Vec::new();
            for (i, row) in st.query_map([uid], stream)?.enumerate() {
                let (service, url, title, uploader, uploader_url, length) = row.with_context(|| unexpected("playlists"))?;
                match parse_link(&url) {
                    Link::Video(id) if service == 0 => items.push(PlaylistItem {
                        video_id: id,
                        title,
                        author: uploader,
                        author_id: uploader_url.and_then(|u| channel_id_in(&u)),
                        length,
                        added: now - i as i64,
                    }),
                    _ => found.skipped += 1,
                }
            }
            found.playlists.push(Playlist { id: format!("import-{name}"), name, protected: false, items });
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Links and ids.

enum Link {
    Video(String),
    Search(String),
    /// A YouTube Music play. We leave these out so your video history isn't
    /// flooded with songs.
    Music,
    /// Community posts, channel pages and anything else.
    Other,
}

fn parse_link(url: &str) -> Link {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").or_else(|| host.strip_prefix("m.")).unwrap_or(&host);
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let param = |key: &str| query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| percent_decode(v)));
    let video = |id: Option<String>| id.filter(|i| is_video_id(i)).map_or(Link::Other, Link::Video);
    match (host, path) {
        ("youtube.com", "watch") => video(param("v")),
        ("youtube.com", p) if p.starts_with("shorts/") => video(p.strip_prefix("shorts/").map(str::to_owned)),
        ("youtube.com", "results") => {
            param("search_query").map(|q| q.trim().to_owned()).filter(|q| !q.is_empty()).map_or(Link::Other, Link::Search)
        }
        ("youtu.be", id) => video(Some(id.to_owned())),
        ("music.youtube.com", "watch") => Link::Music,
        _ => Link::Other,
    }
}

fn is_youtube_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split('/').next().unwrap_or_default().to_ascii_lowercase();
    matches!(host.as_str(), "youtube.com" | "www.youtube.com" | "m.youtube.com")
}

/// The `UC…` id in a channel link, if it has one.
fn channel_id_in(url: &str) -> Option<String> {
    let rest = &url[url.find("/channel/")? + 9..];
    let id = rest.split(['/', '?', '#']).next()?;
    is_channel_id(id).then(|| id.to_owned())
}

/// A channel link that names the channel (`@handle`, `/c/name`, `/user/name`)
/// instead of giving its id. Looking those up needs YouTube, so we skip them.
fn is_named_channel_url(url: &str) -> bool {
    url.contains("/@") || url.contains("/c/") || url.contains("/user/")
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_channel_id(s: &str) -> bool {
    s.len() == 24 && s.starts_with("UC") && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Times. We parse these by hand to avoid pulling in a date library.

/// Unix seconds for a calendar date and time, `offset` seconds ahead of UTC.
fn unix_time(y: i64, mo: i64, d: i64, h: i64, mi: i64, sec: i64, offset: i64) -> Option<i64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..=60).contains(&sec) {
        return None;
    }
    // Days since 1970-01-01, from Howard Hinnant's civil-date algorithm.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

fn digits(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// "+04:00", "-0530", "+4" → seconds ahead of UTC.
fn parse_offset(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some(hm) => hm,
        None if rest.len() == 4 => rest.split_at(2),
        None => (rest, "0"),
    };
    let (h, m) = (digits(h)?, digits(m)?);
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// A time zone as Takeout writes it: "GMT+04:00", "UTC", or a common abbreviation.
fn parse_zone(s: &str) -> Option<i64> {
    const NAMED: &[(&str, i64)] = &[
        ("UTC", 0),
        ("GMT", 0),
        ("Z", 0),
        ("WET", 0),
        ("WEST", 1),
        ("BST", 1),
        ("CET", 1),
        ("CEST", 2),
        ("EET", 2),
        ("EEST", 3),
        ("MSK", 3),
        ("JST", 9),
        ("KST", 9),
        ("AEST", 10),
        ("AEDT", 11),
        ("NZST", 12),
        ("NZDT", 13),
        ("HST", -10),
        ("AKST", -9),
        ("AKDT", -8),
        ("PST", -8),
        ("PDT", -7),
        ("MST", -7),
        ("MDT", -6),
        ("CST", -6),
        ("CDT", -5),
        ("EST", -5),
        ("EDT", -4),
    ];
    let upper = s.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix("GMT").or_else(|| upper.strip_prefix("UTC")) {
        return if rest.is_empty() { Some(0) } else { parse_offset(rest) };
    }
    parse_offset(s).or_else(|| NAMED.iter().find(|(n, _)| *n == upper).map(|(_, h)| h * 3600))
}

/// RFC 3339 and close relatives, as used in Takeout's JSON and CSV files:
/// "2026-10-21T11:04:05.123Z", "2026-03-30T22:20:23+00:00", "2020-08-09 15:26:22 UTC".
/// A time without a zone is taken as UTC.
fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z).and_then(digits);
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let rest = rest.trim();
    let offset = if rest.is_empty() { 0 } else { parse_zone(rest)? };
    unix_time(num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?, offset)
}

fn month_number(word: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let w = word.trim_end_matches('.').to_ascii_lowercase();
    if w.len() < 3 {
        return None;
    }
    let full = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    MONTHS.iter().zip(full).position(|(short, long)| w == *short || w == long || (w == "sept" && *short == "sep")).map(|i| i as i64 + 1)
}

/// The English date Takeout's HTML shows, like "Oct 21, 2026, 3:04:05 PM GMT+04:00".
/// Also accepts the day before the month ("21 Oct 2026, 15:04:05 GMT+04:00") and
/// a 24-hour clock. Takeout puts a narrow no-break space before "PM"; any kind
/// of space works. Dates in other languages give `None` and are skipped.
fn parse_stamp(text: &str) -> Option<i64> {
    let text = text.replace(['\u{202f}', '\u{a0}'], " ");
    let (mut month, mut day, mut year, mut clock, mut pm, mut zone) = (None, None, None, None, None, None);
    for tok in text.split([' ', ',']).map(str::trim).filter(|t| !t.is_empty()) {
        if let Some(m) = month_number(tok).filter(|_| month.is_none()) {
            month = Some(m);
        } else if tok.eq_ignore_ascii_case("AM") || tok.eq_ignore_ascii_case("PM") {
            pm = Some(tok.eq_ignore_ascii_case("PM"));
        } else if clock.is_none() && tok.contains(':') && tok.starts_with(|c: char| c.is_ascii_digit()) {
            let mut parts = tok.split(':');
            let h = digits(parts.next()?)?;
            let m = digits(parts.next()?)?;
            let sec = parts.next().map_or(Some(0), digits)?;
            clock = Some((h, m, sec));
        } else if let Some(n) = digits(tok.trim_end_matches('.')) {
            match tok.trim_end_matches('.').len() {
                4 if year.is_none() => year = Some(n),
                1 | 2 if day.is_none() => day = Some(n),
                _ => return None,
            }
        } else if zone.is_none() {
            zone = Some(parse_zone(tok)?);
        } else {
            return None;
        }
    }
    let (mut h, mi, sec) = clock?;
    match pm {
        Some(_) if !(1..=12).contains(&h) => return None,
        Some(true) if h < 12 => h += 12,
        Some(false) if h == 12 => h = 0,
        _ => {}
    }
    unix_time(year?, month?, day?, h, mi, sec, zone.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Saving.

/// Write everything found, in one transaction.
///
/// The rules, so re-imports are safe:
/// - **History:** one entry per video, and the newer watch wins. An older
///   export never overwrites something you watched more recently in YourTube.
///   When the export doesn't know how far you got (Takeout), your saved
///   position is kept.
/// - **Subscriptions:** added, or switched back on. Names are refreshed.
/// - **Playlists:** matched to an existing one by id, then by name (so
///   Takeout's "Watch later" lands in YourTube's), otherwise created. Videos
///   already in a playlist stay as they are.
/// - **Searches:** the same query at the same second is only stored once.
/// - Unknown titles show the video id until something better turns up: we
///   reuse a title from your history or feed when we have one.
fn save(conn: &mut Connection, found: Found) -> Result<Counts> {
    let mut n = Counts { skipped: found.skipped, ..Default::default() };
    let tx = conn.transaction()?;

    let mut seen = HashSet::new();
    for c in found.channels {
        if !seen.insert(c.id.clone()) {
            continue;
        }
        let was_subscribed: Option<bool> =
            tx.query_row("SELECT subscribed FROM channels WHERE id = ?1", [&c.id], |r| r.get(0)).optional()?;
        tx.execute(
            "INSERT INTO channels (id, name, thumbnail, subscribed) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(id) DO UPDATE SET
                 name = CASE WHEN excluded.name = excluded.id THEN channels.name ELSE excluded.name END,
                 thumbnail = COALESCE(excluded.thumbnail, channels.thumbnail),
                 subscribed = 1",
            params![c.id, c.name, c.thumbnail],
        )?;
        if was_subscribed != Some(true) {
            n.subscriptions += 1;
        }
    }

    // Exports list every time you watched something; we keep the latest, and
    // fill in any details it lacks from the other times.
    let mut latest: HashMap<String, Watch> = HashMap::new();
    let mut order = Vec::new();
    for w in found.watches {
        match latest.get_mut(&w.video_id) {
            None => {
                order.push(w.video_id.clone());
                latest.insert(w.video_id.clone(), w);
            }
            Some(old) => {
                let (mut newer, older) = if w.at > old.at { (w, old.clone()) } else { (old.clone(), w) };
                newer.title = newer.title.or(older.title);
                newer.author = newer.author.or(older.author);
                newer.author_id = newer.author_id.or(older.author_id);
                newer.length = newer.length.or(older.length);
                *old = newer;
            }
        }
    }
    for w in order.iter().filter_map(|id| latest.get(id)) {
        n.history += tx.execute(
            "INSERT INTO history (video_id, title, author, author_id, watched_at, position, length)
             VALUES (?1, COALESCE(?2, (SELECT title FROM videos WHERE id = ?1), ?1), ?3, ?4, ?5, COALESCE(?6, 0), ?7)
             ON CONFLICT(video_id) DO UPDATE SET
                 title = CASE WHEN excluded.title = excluded.video_id THEN history.title ELSE excluded.title END,
                 author = COALESCE(excluded.author, history.author),
                 author_id = COALESCE(excluded.author_id, history.author_id),
                 watched_at = excluded.watched_at,
                 position = COALESCE(?6, history.position),
                 length = COALESCE(excluded.length, history.length)
             WHERE excluded.watched_at > history.watched_at",
            params![w.video_id, w.title, w.author, w.author_id, w.at, w.position, w.length],
        )?;
    }

    for (query, at) in found.searches {
        n.searches += tx.execute("INSERT OR IGNORE INTO searches (query, at) VALUES (?1, ?2)", params![query, at])?;
    }

    for p in found.playlists {
        let existing: Option<String> = tx.query_row("SELECT id FROM playlists WHERE id = ?1", [&p.id], |r| r.get(0)).optional()?.or(tx
            .query_row("SELECT id FROM playlists WHERE name = ?1 COLLATE NOCASE ORDER BY protected DESC, id LIMIT 1", [&p.name], |r| {
                r.get(0)
            })
            .optional()?);
        let created = existing.is_none();
        let id = match existing {
            Some(id) => id,
            None => {
                tx.execute("INSERT INTO playlists (id, name, protected) VALUES (?1, ?2, ?3)", params![p.id, p.name, p.protected])?;
                p.id
            }
        };
        let mut added = 0;
        for it in p.items {
            added += tx.execute(
                "INSERT INTO playlist_items (playlist_id, video_id, title, author, author_id, length, added)
                 VALUES (?1, ?2,
                     COALESCE(?3, (SELECT title FROM history WHERE video_id = ?2), (SELECT title FROM videos WHERE id = ?2), ?2),
                     COALESCE(?4, (SELECT author FROM history WHERE video_id = ?2)),
                     COALESCE(?5, (SELECT author_id FROM history WHERE video_id = ?2), (SELECT channel_id FROM videos WHERE id = ?2)),
                     ?6, ?7)
                 ON CONFLICT(playlist_id, video_id) DO NOTHING",
                params![id, it.video_id, it.title, it.author, it.author_id, it.length, it.added],
            )?;
        }
        n.playlist_items += added;
        if created || added > 0 {
            n.playlists += 1;
        }
    }

    tx.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_copies_are_private_and_cleaned_up() {
        let tmp = TempFile::new(b"SQLite format 3\0not really").unwrap();
        let path = tmp.0.clone();
        assert!(path.starts_with(crate::proc::run_dir()), "{path:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(open_untrusted(&path).is_err(), "damaged databases are refused");
        drop(tmp);
        assert!(!path.exists());
    }
    use std::io::Write;

    /// A fresh database with YourTube's real schema, in memory.
    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        conn
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import")
    }

    /// A scratch folder for one test, removed when the test ends.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("yourtube-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Zip up a fixture folder, keeping its paths, the way Google hands it out.
    fn zip_dir(src: &Path, dest: &Path) {
        fn add(zip: &mut zip::ZipWriter<std::fs::File>, root: &Path, dir: &Path) {
            let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    add(zip, root, &path);
                } else {
                    let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                    zip.write_all(&std::fs::read(&path).unwrap()).unwrap();
                }
            }
        }
        let mut zip = zip::ZipWriter::new(std::fs::File::create(dest).unwrap());
        add(&mut zip, src, src);
        // A big "uploaded video" we must not try to read.
        zip.start_file("Takeout/YouTube and YouTube Music/videos/My upload.mp4", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(&[0u8; 4096]).unwrap();
        zip.finish().unwrap();
    }

    const CH1: &str = "UCfixturechannel00000001";
    const CH2: &str = "UCfixturechannel00000002";

    #[test]
    fn last_line_wins_and_deleted_is_dropped() {
        let recs = nedb_records(
            "{\"_id\":\"a\",\"n\":1}\n{\"_id\":\"b\",\"n\":1}\n{\"_id\":\"a\",\"n\":2}\n{\"_id\":\"b\",\"$$deleted\":true}\nnot json\n",
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["n"], 2);
    }

    #[test]
    fn takeout_folder_with_html_history() {
        let mut conn = test_db();
        let n = import_any(&mut conn, &fixtures().join("takeout")).unwrap();

        // subscriptions.csv: two channels with ids, one known only by @handle.
        assert_eq!(n.subscriptions, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM channels WHERE subscribed = 1"), 2);
        let name: String = conn.query_row("SELECT name FROM channels WHERE id = ?1", [CH1], |r| r.get(0)).unwrap();
        assert_eq!(name, "Fixture Cooking, Etc.");

        // watch-history.html: the ad, the removed video, the music play and the
        // entry with an unreadable date are all left out.
        assert_eq!(n.history, 3);
        let (title, author, author_id, at): (String, String, String, i64) = conn
            .query_row("SELECT title, author, author_id, watched_at FROM history WHERE video_id = 'fixturevid1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        assert_eq!(title, "Knife skills & you");
        assert_eq!(author, "Fixture Cooking, Etc.");
        assert_eq!(author_id, CH1);
        // Watched twice; the later time wins. Oct 21, 2026, 3:04:05 PM GMT+04:00 = 11:04:05 UTC.
        assert_eq!(at, 1_792_580_645);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM history WHERE video_id IN ('fixtureadv1', 'fixturemus1')"), 0);
        // A private video shows its URL as the title; we show the id instead.
        let t: String = conn.query_row("SELECT title FROM history WHERE video_id = 'fixturevid3'", [], |r| r.get(0)).unwrap();
        assert_eq!(t, "fixturevid3");

        // search-history.html: two searches (the ad in that file is ignored).
        assert_eq!(n.searches, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM searches WHERE query = 'rust gtk4 tutorial'"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM searches WHERE query = 'fish & chips'"), 1);

        // Playlists: Watch later and Road trip, plus an old-format list.
        assert_eq!(n.playlists, 3);
        assert_eq!(n.playlist_items, 5);
        let wl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM playlist_items i JOIN playlists p ON p.id = i.playlist_id WHERE p.name = 'Watch later'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wl, 2);
        // The title comes from history when the CSV doesn't have one.
        let t: String = conn.query_row("SELECT title FROM playlist_items WHERE video_id = 'fixturevid1'", [], |r| r.get(0)).unwrap();
        assert_eq!(t, "Knife skills & you");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM playlists WHERE name = 'Old favourites'"), 1);

        // The @handle channel, the removed video, the music play and the bad date.
        assert_eq!(n.skipped, 4);
    }

    #[test]
    fn takeout_import_is_idempotent() {
        let mut conn = test_db();
        let first = import_any(&mut conn, &fixtures().join("takeout")).unwrap();
        let rows = |c: &Connection| {
            ["channels", "history", "searches", "playlists", "playlist_items"].map(|t| count(c, &format!("SELECT COUNT(*) FROM {t}")))
        };
        let before = rows(&conn);
        let again = import_any(&mut conn, &fixtures().join("takeout")).unwrap();
        assert_eq!(rows(&conn), before);
        assert_eq!(again, Counts { skipped: first.skipped, ..Default::default() });
    }

    #[test]
    fn takeout_zip_matches_the_folder() {
        let scratch = Scratch::new("takeout-zip");
        let zip_path = scratch.0.join("takeout-20261022T000000Z-001.zip");
        zip_dir(&fixtures().join("takeout"), &zip_path);
        let mut from_zip = test_db();
        let a = import_any(&mut from_zip, &zip_path).unwrap();
        let mut from_dir = test_db();
        let b = import_any(&mut from_dir, &fixtures().join("takeout")).unwrap();
        assert_eq!(a, b);

        // A folder holding the zip (Takeout splits big exports into several) works too.
        let mut from_parent = test_db();
        assert_eq!(import_any(&mut from_parent, &scratch.0).unwrap(), b);
    }

    #[test]
    fn takeout_single_files_and_json() {
        let mut conn = test_db();
        let n = import_any(&mut conn, &fixtures().join("takeout-json/watch-history.json")).unwrap();
        // The ad and the removed video are left out; the music play is skipped.
        assert_eq!(n.history, 2);
        assert_eq!(n.skipped, 2);
        let (title, author_id, at): (String, String, i64) = conn
            .query_row("SELECT title, author_id, watched_at FROM history WHERE video_id = 'fixturevid4'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(title, "How sourdough works");
        assert_eq!(author_id, CH2);
        assert_eq!(at, 1_792_580_645);

        let n = import_any(&mut conn, &fixtures().join("takeout-json/search-history.json")).unwrap();
        assert_eq!(n.searches, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM searches WHERE query = 'how to fix a bike chain'"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM searches WHERE query = 'café & croissants'"), 1);

        // One file picked straight out of an unpacked Takeout.
        let csv = fixtures().join("takeout/Takeout/YouTube and YouTube Music/subscriptions/subscriptions.csv");
        let n = import_any(&mut conn, &csv).unwrap();
        assert_eq!((n.subscriptions, n.skipped), (2, 1));
    }

    #[test]
    fn newer_history_wins() {
        let mut conn = test_db();
        // Watched in YourTube after the Takeout entry: keep ours untouched.
        conn.execute(
            "INSERT INTO history (video_id, title, watched_at, position, length) VALUES ('fixturevid1', 'Mine', 1900000000, 42, 600)",
            [],
        )
        .unwrap();
        // Watched in YourTube before the Takeout entry: take the newer time, keep the saved position.
        conn.execute(
            "INSERT INTO history (video_id, title, watched_at, position, length) VALUES ('fixturevid2', 'Old', 1000, 30, 300)",
            [],
        )
        .unwrap();
        import_any(&mut conn, &fixtures().join("takeout")).unwrap();
        let mine: (String, i64, f64) = conn
            .query_row("SELECT title, watched_at, position FROM history WHERE video_id = 'fixturevid1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(mine, ("Mine".to_owned(), 1_900_000_000, 42.0));
        let old: (i64, f64, f64) = conn
            .query_row("SELECT watched_at, position, length FROM history WHERE video_id = 'fixturevid2'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert!(old.0 > 1000);
        assert_eq!((old.1, old.2), (30.0, 300.0));
    }

    #[test]
    fn takeout_watch_later_joins_the_existing_one() {
        let mut conn = test_db();
        let wl = crate::db::watch_later(&conn).unwrap();
        import_any(&mut conn, &fixtures().join("takeout")).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM playlists WHERE name = 'Watch later'"), 1);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM playlist_items WHERE playlist_id = ?1", [&wl], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn newpipe_subscriptions_json() {
        let mut conn = test_db();
        let path = fixtures().join("newpipe-subscriptions.json");
        let n = import_any(&mut conn, &path).unwrap();
        // Two YouTube channels; the @handle one and the SoundCloud one are skipped.
        assert_eq!((n.subscriptions, n.skipped), (2, 2));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM channels WHERE subscribed = 1"), 2);
        assert_eq!(import_any(&mut conn, &path).unwrap().subscriptions, 0);
    }

    #[test]
    fn newpipe_backup_zip() {
        let scratch = Scratch::new("newpipe");
        let db_path = scratch.0.join("newpipe.db");
        {
            let db = Connection::open(&db_path).unwrap();
            db.execute_batch(&format!(
                "CREATE TABLE subscriptions (uid INTEGER PRIMARY KEY, service_id INTEGER NOT NULL, url TEXT, name TEXT, avatar_url TEXT,
                     subscriber_count INTEGER, description TEXT, notification_mode INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE streams (uid INTEGER PRIMARY KEY, service_id INTEGER NOT NULL, url TEXT NOT NULL, title TEXT NOT NULL,
                     stream_type TEXT NOT NULL, duration INTEGER NOT NULL, uploader TEXT NOT NULL, uploader_url TEXT,
                     thumbnail_url TEXT, view_count INTEGER, textual_upload_date TEXT, upload_date INTEGER,
                     is_upload_date_approximation INTEGER);
                 CREATE TABLE stream_history (stream_id INTEGER NOT NULL, access_date INTEGER NOT NULL, repeat_count INTEGER NOT NULL,
                     PRIMARY KEY (stream_id, access_date));
                 CREATE TABLE stream_state (stream_id INTEGER PRIMARY KEY, progress_time INTEGER NOT NULL);
                 CREATE TABLE search_history (creation_date INTEGER, service_id INTEGER NOT NULL, search TEXT, id INTEGER PRIMARY KEY);
                 CREATE TABLE playlists (uid INTEGER PRIMARY KEY, name TEXT, is_thumbnail_permanent INTEGER, thumbnail_stream_id INTEGER);
                 CREATE TABLE playlist_stream_join (playlist_id INTEGER NOT NULL, stream_id INTEGER NOT NULL, join_index INTEGER NOT NULL,
                     PRIMARY KEY (playlist_id, join_index));
                 INSERT INTO subscriptions (service_id, url, name) VALUES
                     (0, 'https://www.youtube.com/channel/{CH1}', 'Fixture Cooking'),
                     (0, 'https://www.youtube.com/@somehandle', 'Handle only');
                 INSERT INTO streams (uid, service_id, url, title, stream_type, duration, uploader, uploader_url) VALUES
                     (1, 0, 'https://www.youtube.com/watch?v=fixturevid1', 'Knife skills', 'VIDEO_STREAM', 600, 'Fixture Cooking',
                         'https://www.youtube.com/channel/{CH1}'),
                     (2, 0, 'https://www.youtube.com/watch?v=fixturevid2', 'Bread', 'VIDEO_STREAM', 300, 'Fixture Baking', NULL),
                     (3, 1, 'https://soundcloud.com/someone/song', 'A song', 'AUDIO_STREAM', 200, 'Someone', NULL);
                 INSERT INTO stream_history VALUES (1, 1792580645000, 1), (1, 1700000000000, 1), (3, 1792580645000, 1);
                 INSERT INTO stream_state VALUES (1, 125000);
                 INSERT INTO search_history (creation_date, service_id, search) VALUES (1792580645000, 0, 'knife sharpening');
                 INSERT INTO playlists (uid, name) VALUES (7, 'Weekend');
                 INSERT INTO playlist_stream_join VALUES (7, 2, 0), (7, 1, 1);"
            ))
            .unwrap();
        }
        let zip_path = scratch.0.join("NewPipeData-20261022_120000.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        zip.start_file("newpipe.db", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(&std::fs::read(&db_path).unwrap()).unwrap();
        zip.start_file("newpipe.settings", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"not ours").unwrap();
        zip.finish().unwrap();

        let mut conn = test_db();
        let n = import_any(&mut conn, &zip_path).unwrap();
        assert_eq!(n.subscriptions, 1);
        assert_eq!(n.history, 1);
        assert_eq!(n.searches, 1);
        assert_eq!((n.playlists, n.playlist_items), (1, 2));
        assert_eq!(n.skipped, 2); // the @handle channel and the SoundCloud play
        let (at, pos, len): (i64, f64, f64) = conn
            .query_row("SELECT watched_at, position, length FROM history WHERE video_id = 'fixturevid1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((at, pos, len), (1_792_580_645, 125.0, 600.0));
        // Playlist order is kept: first video shows first (newest "added").
        let first: String = conn.query_row("SELECT video_id FROM playlist_items ORDER BY added DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(first, "fixturevid2");
        assert_eq!(import_any(&mut conn, &zip_path).unwrap(), Counts { skipped: 2, ..Default::default() });
    }

    #[test]
    fn freetube_export_files() {
        let mut conn = test_db();
        let dir = fixtures().join("freetube");
        let n = import_any(&mut conn, &dir.join("freetube-subscriptions-2026-10-22.db")).unwrap();
        assert_eq!(n.subscriptions, 2);
        let n = import_any(&mut conn, &dir.join("freetube-history-2026-10-22.db")).unwrap();
        assert_eq!(n.history, 2);
        let pos: f64 = conn.query_row("SELECT position FROM history WHERE video_id = 'fixturevid1'", [], |r| r.get(0)).unwrap();
        assert_eq!(pos, 61.5);
        let n = import_any(&mut conn, &dir.join("freetube-playlists-2026-10-22.db")).unwrap();
        assert_eq!((n.playlists, n.playlist_items), (2, 3));

        // The whole folder at once, again: nothing new.
        assert_eq!(import_any(&mut conn, &dir).unwrap(), Counts::default());
    }

    #[test]
    fn freetube_data_folder() {
        let scratch = Scratch::new("freetube-dir");
        let dir = fixtures().join("freetube");
        std::fs::copy(dir.join("freetube-subscriptions-2026-10-22.db"), scratch.0.join("profiles.db")).unwrap();
        std::fs::copy(dir.join("freetube-history-2026-10-22.db"), scratch.0.join("history.db")).unwrap();
        std::fs::copy(dir.join("freetube-playlists-2026-10-22.db"), scratch.0.join("playlists.db")).unwrap();
        let mut conn = test_db();
        let n = import(&mut conn, &scratch.0).unwrap();
        assert_eq!((n.subscriptions, n.history, n.playlists, n.playlist_items), (2, 2, 2, 3));
        assert_eq!(import_any(&mut conn, &scratch.0).unwrap(), Counts::default());
    }

    #[test]
    fn unrecognized_input_says_so() {
        let scratch = Scratch::new("unknown");
        let file = scratch.0.join("notes.txt");
        std::fs::write(&file, "just some notes").unwrap();
        let mut conn = test_db();
        let err = import_any(&mut conn, &file).unwrap_err().to_string();
        assert_eq!(err, NOT_AN_EXPORT);
        let err = import_any(&mut conn, &scratch.0).unwrap_err().to_string();
        assert_eq!(err, NOT_AN_EXPORT);
    }

    /// Try the importer on your own exports without touching your database:
    ///
    ///   YOURTUBE_IMPORT=path/to/takeout.zip:path/to/other cargo test real_export -- --ignored --nocapture
    ///
    /// Everything goes into a throwaway in-memory database; only the counts are printed.
    #[test]
    #[ignore]
    fn real_export() {
        let paths = std::env::var("YOURTUBE_IMPORT").expect("set YOURTUBE_IMPORT to one or more paths, separated by ':'");
        let mut conn = test_db();
        for path in paths.split(':').filter(|p| !p.is_empty()) {
            let started = std::time::Instant::now();
            match import_any(&mut conn, Path::new(path)) {
                Ok(n) => println!("{path}: {n:?} in {:.1?}", started.elapsed()),
                Err(e) => println!("{path}: error: {e:#}"),
            }
        }
        for t in ["channels", "history", "searches", "playlists", "playlist_items"] {
            println!("{t}: {} rows", count(&conn, &format!("SELECT COUNT(*) FROM {t}")));
        }
    }

    #[test]
    fn dates_and_links() {
        // English Takeout dates, with the narrow no-break space Google uses.
        assert_eq!(parse_stamp("Oct 21, 2026, 3:04:05\u{202f}PM GMT+04:00"), Some(1_792_580_645));
        assert_eq!(parse_stamp("21 Oct 2026, 15:04:05 GMT+04:00"), Some(1_792_580_645));
        assert_eq!(parse_stamp("Oct 21, 2026, 11:04:05 AM UTC"), Some(1_792_580_645));
        assert_eq!(parse_stamp("Oct 21, 2026, 4:04:05 AM PDT"), Some(1_792_580_645));
        assert_eq!(parse_stamp("Oct 21, 2026, 12:00:00 AM GMT"), Some(1_792_540_800));
        assert_eq!(parse_stamp("21 ott 2026, 15:04:05 CEST"), None);
        assert_eq!(parse_stamp("Oct 21, 2026, 3:04:05 PM Mars/Olympus"), None);
        assert_eq!(parse_iso("2026-10-21T11:04:05.123Z"), Some(1_792_580_645));
        assert_eq!(parse_iso("2026-10-21T15:04:05+04:00"), Some(1_792_580_645));
        assert_eq!(parse_iso("2026-10-21 11:04:05 UTC"), Some(1_792_580_645));
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("yesterday"), None);

        assert!(matches!(parse_link("https://www.youtube.com/watch?v=fixturevid1&t=3s"), Link::Video(v) if v == "fixturevid1"));
        assert!(matches!(parse_link("https://youtu.be/fixturevid1"), Link::Video(_)));
        assert!(
            matches!(parse_link("https://www.youtube.com/results?search_query=fish+%26+chips"), Link::Search(q) if q == "fish & chips")
        );
        assert!(matches!(parse_link("https://music.youtube.com/watch?v=fixturemus1"), Link::Music));
        assert!(matches!(parse_link("https://www.youtube.com/post/Ugkx123"), Link::Other));
        assert_eq!(channel_id_in("http://www.youtube.com/channel/UCfixturechannel00000001"), Some(CH1.to_owned()));
        assert_eq!(channel_id_in("https://www.youtube.com/@handle"), None);
        assert_eq!(percent_decode("caf%C3%A9%2"), "café%2");
        assert_eq!(unescape("a &amp; b &#39;c&#x27; &bogus; &"), "a & b 'c' &bogus; &");
        assert_eq!(csv_rows("a,\"b, \"\"c\"\"\",,\r\n\r\nd\n"), vec![vec!["a".to_owned(), "b, \"c\"".to_owned()], vec!["d".to_owned()]]);
    }
}
