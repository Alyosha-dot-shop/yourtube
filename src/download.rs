//! Downloads with yt-dlp, so videos play offline (the player prefers a downloaded file).
//!
//! [`start`] queues a download and returns at once; at most [`MAX_RUNNING`] run at a
//! time and the rest wait their turn. Progress comes back through a callback as
//! [`Update`]s, parsed from yt-dlp's `--progress-template` lines by pure functions
//! ([`parse_line`]) so they can be tested without the network.
//!
//! Finished downloads are recorded in the `downloads` table; [`list`], [`get`],
//! [`path_for`] and [`delete`] work on it. A row whose file was removed by hand
//! simply stops counting (see [`get`]).

// Some helpers and fields aren't used by a screen yet; they're kept for the web version
// and for contributors.
#![allow(dead_code)]

use crate::config::Config;
use crate::db::Item;
use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Two at a time is kind to YouTube (which throttles bursts) and to slow connections.
pub const MAX_RUNNING: usize = 2;

/// Create the downloads table (called from db::init).
pub fn init_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS downloads (
             video_id TEXT PRIMARY KEY,
             path TEXT NOT NULL,
             title TEXT NOT NULL,
             channel TEXT,
             channel_id TEXT,
             length REAL,                         -- seconds, when known
             bytes INTEGER,
             audio_only INTEGER NOT NULL DEFAULT 0,
             sponsors_cut INTEGER NOT NULL DEFAULT 0, -- SponsorBlock segments were cut out
             at INTEGER NOT NULL                  -- unix seconds
         );",
    )?;
    Ok(())
}

/// How to download.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub audio_only: bool,
    /// Highest video height (ignored for audio only).
    pub height: u32,
    /// Cut out the SponsorBlock categories that are set to "skip".
    pub cut_sponsors: bool,
    /// Where files go.
    pub dir: PathBuf,
    pub proxy: Option<String>,
    /// Subtitle languages to embed, e.g. "en,de" (empty = none).
    pub subtitles: String,
    /// Categories to cut when `cut_sponsors` is on, and the API to ask.
    pub sponsor_categories: Vec<String>,
    pub sponsorblock_api: String,
}

impl Options {
    /// The settings from Preferences. Change `audio_only` (or anything) per download afterwards.
    pub fn from_config(c: &Config) -> Options {
        Options {
            audio_only: false,
            height: c.download_height,
            cut_sponsors: c.download_cut_sponsors && c.sponsorblock,
            dir: dir(c),
            proxy: c.playback_proxy(),
            subtitles: c.subtitles.clone(),
            sponsor_categories: c.sponsorblock_categories.iter().filter(|(_, v)| *v == "skip").map(|(k, _)| k.clone()).collect(),
            sponsorblock_api: c.sponsorblock_api.clone(),
        }
    }
}

/// The download folder: the configured one, else Videos/YourTube (~/Videos/YourTube
/// when the system doesn't name a videos folder).
pub fn dir(c: &Config) -> PathBuf {
    if let Some(d) = c.download_dir.as_deref().filter(|d| !d.trim().is_empty()) {
        return expand_home(d.trim());
    }
    dirs::video_dir().or_else(|| dirs::home_dir().map(|h| h.join("Videos"))).unwrap_or_else(std::env::temp_dir).join("YourTube")
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}

/// One progress line from yt-dlp.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    /// 0–100 for the current file. yt-dlp fetches video and audio separately, so
    /// this restarts once for the second part; `part` tells them apart.
    pub percent: Option<f64>,
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    /// Bytes per second.
    pub speed: Option<f64>,
    /// Seconds left for the current file.
    pub eta: Option<u64>,
    /// 1 for the first file, 2 for the second (audio, usually).
    pub part: u32,
}

impl Progress {
    /// "42% · 1.3 MB/s · 0:13 left", for a status line.
    pub fn summary(&self) -> String {
        let mut bits = Vec::new();
        if let Some(p) = self.percent {
            bits.push(if self.part > 1 { format!("{p:.0}% (part {})", self.part) } else { format!("{p:.0}%") });
        }
        if let Some(s) = self.speed {
            bits.push(format!("{}/s", human_bytes(s as u64)));
        }
        if let Some(e) = self.eta {
            bits.push(format!("{} left", crate::sponsorblock::clock(e as f64)));
        }
        if bits.is_empty() { "Starting…".into() } else { bits.join(" · ") }
    }
}

pub fn human_bytes(b: u64) -> String {
    let b = b as f64;
    match b {
        _ if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        _ if b >= 1e6 => format!("{:.1} MB", b / 1e6),
        _ if b >= 1e3 => format!("{:.0} kB", b / 1e3),
        _ => format!("{b:.0} B"),
    }
}

/// What a download is doing, reported to `on_progress`.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// Waiting for a free slot.
    Queued,
    Started,
    Progress(Progress),
    /// Merging video and audio, cutting sponsors, embedding subtitles…
    Processing(String),
    Done(Downloaded),
    Failed(String),
    Cancelled,
}

/// A finished download, as stored in the table.
#[derive(Clone, Debug, PartialEq)]
pub struct Downloaded {
    pub video_id: String,
    pub path: PathBuf,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub length: Option<f64>,
    pub bytes: Option<u64>,
    pub audio_only: bool,
    /// The file has SponsorBlock segments cut out, so their timestamps no longer fit it.
    pub sponsors_cut: bool,
    pub at: i64,
}

impl Downloaded {
    /// As a card for the grids.
    pub fn item(&self) -> Item {
        Item {
            id: self.video_id.clone(),
            title: self.title.clone(),
            channel: self.channel.clone(),
            channel_id: self.channel_id.clone(),
            length: self.length,
            note: Some(format!(
                "Downloaded{}{}",
                if self.audio_only { " (audio)" } else { "" },
                self.bytes.map_or(String::new(), |b| format!(" · {}", human_bytes(b)))
            )),
            ..Default::default()
        }
    }
}

// ---- yt-dlp output parsing ----------------------------------------------------------

/// Our own markers, so yt-dlp's other chatter can't be mistaken for progress.
const P_PROGRESS: &str = "YTP-DL";
const P_POST: &str = "YTP-POST";
const P_FILE: &str = "YTP-FILE";
const P_LENGTH: &str = "YTP-LEN";

/// A line from yt-dlp, understood.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// Downloaded bytes, total (or estimate), speed, ETA. Any can be missing ("NA").
    Progress {
        downloaded: Option<u64>,
        total: Option<u64>,
        speed: Option<f64>,
        eta: Option<u64>,
    },
    /// A post-processor started (its name), e.g. "Merger", "SponsorBlock", "ModifyChapters".
    Post(String),
    /// The final file, after every post-processor.
    File(PathBuf),
    Length(f64),
    Other,
}

pub fn parse_line(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    let num = |s: Option<&str>| s.and_then(|s| s.parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 0.0);
    if let Some(rest) = line.strip_prefix(P_PROGRESS) {
        // Fields: downloaded, total, total estimate, speed, ETA (see ytdlp_args).
        let f: Vec<&str> = rest.split_whitespace().collect();
        let at = |i: usize| num(f.get(i).copied());
        let downloaded = at(0).map(|n| n as u64);
        // Fragmented formats only know an estimate of the total.
        let total = at(1).or(at(2)).map(|n| n as u64).filter(|t| *t > 0);
        return Line::Progress { downloaded, total, speed: at(3), eta: at(4).map(|n| n as u64) };
    }
    if let Some(rest) = line.strip_prefix(P_POST) {
        let mut f = rest.split_whitespace();
        return match (f.next(), f.next()) {
            (Some(name), Some("started")) => Line::Post(name.to_owned()),
            _ => Line::Other,
        };
    }
    if let Some(rest) = line.strip_prefix(P_FILE) {
        let p = rest.trim();
        return if p.is_empty() || p == "NA" { Line::Other } else { Line::File(PathBuf::from(p)) };
    }
    if let Some(rest) = line.strip_prefix(P_LENGTH) {
        return num(Some(rest.trim())).map_or(Line::Other, Line::Length);
    }
    Line::Other
}

/// Turns progress lines into [`Progress`], noticing when yt-dlp moves on to the next part.
#[derive(Default)]
pub struct Tracker {
    last: Progress,
}

impl Tracker {
    pub fn feed(&mut self, downloaded: Option<u64>, total: Option<u64>, speed: Option<f64>, eta: Option<u64>) -> Progress {
        let part = self.last.part.max(1);
        // Bytes going down means a new file started (the audio after the video).
        let part = match (self.last.downloaded, downloaded) {
            (Some(a), Some(b)) if b < a => part + 1,
            _ => part,
        };
        let percent = match (downloaded, total) {
            (Some(d), Some(t)) => Some((d as f64 / t as f64 * 100.0).min(100.0)),
            _ => None,
        };
        self.last = Progress { percent, downloaded, total, speed, eta, part };
        self.last.clone()
    }
}

/// yt-dlp's arguments for one download (without the program name).
pub fn ytdlp_args(video_id: &str, o: &Options) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--newline".into(),
        "--no-colors".into(),
        "--no-playlist".into(),
        // --print below implies --quiet and --simulate; undo both.
        "--no-simulate".into(),
        "--progress".into(),
        "--encoding".into(),
        "utf-8".into(),
        "--progress-template".into(),
        format!(
            "download:{P_PROGRESS} %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s"
        ),
        "--progress-template".into(),
        format!("postprocess:{P_POST} %(progress.postprocessor)s %(progress.status)s"),
        "--print".into(),
        format!("after_move:{P_LENGTH} %(duration)s"),
        "--print".into(),
        format!("after_move:{P_FILE} %(filepath)s"),
        "-P".into(),
        o.dir.to_string_lossy().into_owned(),
        "-o".into(),
        "%(title)s [%(id)s].%(ext)s".into(),
        "-f".into(),
    ];
    if o.audio_only {
        // Keep the audio as YouTube serves it: no re-encoding, no ffmpeg needed.
        a.push("bestaudio[ext=m4a]/bestaudio/best".into());
    } else {
        let h = o.height.max(144);
        a.push(format!("bestvideo[height<={h}]+bestaudio/best[height<={h}]/best"));
    }
    if let Some(p) = &o.proxy {
        a.extend(["--proxy".into(), p.clone()]);
    }
    if !o.subtitles.trim().is_empty() {
        let langs: Vec<String> = o.subtitles.split(',').map(str::trim).filter(|l| !l.is_empty()).map(|l| format!("{l}.*")).collect();
        a.extend(["--write-subs".into(), "--write-auto-subs".into(), "--embed-subs".into(), "--sub-langs".into(), langs.join(",")]);
    }
    if o.cut_sponsors && !o.sponsor_categories.is_empty() {
        a.extend(["--sponsorblock-remove".into(), o.sponsor_categories.join(",")]);
        a.extend(["--sponsorblock-api".into(), o.sponsorblock_api.clone()]);
    }
    // "--" so an ID starting with "-" can never be read as an option.
    a.extend(["--".into(), format!("https://www.youtube.com/watch?v={video_id}")]);
    a
}

// ---- the manager ------------------------------------------------------------------------

struct Slots {
    running: usize,
    waiting: VecDeque<u64>,
    /// Video IDs downloading or waiting (a handful at most, so a Vec is plenty).
    active: Vec<String>,
}

static SLOTS: Mutex<Slots> = Mutex::new(Slots { running: 0, waiting: VecDeque::new(), active: Vec::new() });
static FREED: Condvar = Condvar::new();
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

/// A download in progress (or waiting). Dropping it doesn't stop the download.
#[derive(Clone)]
pub struct Handle {
    pub video_id: String,
    cancelled: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Handle {
    /// Stop it; partial files are removed. Safe to call more than once.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = c.kill();
        }
        FREED.notify_all();
    }
}

/// Is this video being downloaded (or waiting to be) right now?
pub fn is_active(video_id: &str) -> bool {
    SLOTS.lock().unwrap_or_else(|e| e.into_inner()).active.iter().any(|a| a == video_id)
}

/// Queue a download. `on_progress` is called from a worker thread; the last call is
/// always `Done`, `Failed` or `Cancelled`. The finished file is recorded in the
/// current profile's database.
pub fn start(item: &Item, opts: Options, on_progress: impl Fn(Update) + Send + 'static) -> Result<Handle> {
    {
        let mut s = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        if s.active.contains(&item.id) {
            return Err(anyhow!("“{}” is already downloading", item.title));
        }
        s.active.push(item.id.clone());
    }
    let h = Handle { video_id: item.id.clone(), cancelled: Arc::new(AtomicBool::new(false)), child: Arc::new(Mutex::new(None)) };
    let (item, h2) = (item.clone(), h.clone());
    std::thread::spawn(move || {
        let end = run(&item, &opts, &h2, &on_progress);
        SLOTS.lock().unwrap_or_else(|e| e.into_inner()).active.retain(|a| *a != item.id);
        on_progress(end);
    });
    Ok(h)
}

/// Wait for a slot, then download. Returns the final update.
fn run(item: &Item, o: &Options, h: &Handle, on_progress: &impl Fn(Update)) -> Update {
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::SeqCst);
    {
        let mut s = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        s.waiting.push_back(ticket);
        if s.running >= MAX_RUNNING {
            on_progress(Update::Queued);
        }
        // First come, first served: only the front of the queue may take a free slot.
        while s.running >= MAX_RUNNING || s.waiting.front() != Some(&ticket) {
            if h.cancelled.load(Ordering::SeqCst) {
                s.waiting.retain(|t| *t != ticket);
                FREED.notify_all();
                return Update::Cancelled;
            }
            s = FREED.wait_timeout(s, Duration::from_millis(300)).unwrap_or_else(|e| e.into_inner()).0;
        }
        s.waiting.pop_front();
        s.running += 1;
    }
    let result = download(item, o, h, on_progress);
    SLOTS.lock().unwrap_or_else(|e| e.into_inner()).running -= 1;
    FREED.notify_all();
    match result {
        _ if h.cancelled.load(Ordering::SeqCst) => {
            remove_partials(&o.dir, &item.id);
            Update::Cancelled
        }
        Ok(d) => Update::Done(d),
        Err(e) => {
            remove_partials(&o.dir, &item.id);
            Update::Failed(e.to_string())
        }
    }
}

fn download(item: &Item, o: &Options, h: &Handle, on_progress: &impl Fn(Update)) -> Result<Downloaded> {
    std::fs::create_dir_all(&o.dir)?;
    let mut cmd = crate::proc::ytdlp();
    cmd.args(ytdlp_args(&item.id, o))
        // Python on Windows otherwise prints in the console's code page.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| anyhow!("couldn't start yt-dlp (is it installed?): {e}"))?;
    let out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    *h.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    if h.cancelled.load(Ordering::SeqCst) {
        h.cancel(); // cancelled between the queue and the spawn
    }
    on_progress(Update::Started);

    // Read stderr on the side so a chatty yt-dlp can't fill the pipe and stall.
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let (mut tracker, mut file, mut length) = (Tracker::default(), None, None);
    for line in BufReader::new(out).split(b'\n').map_while(Result::ok) {
        match parse_line(&String::from_utf8_lossy(&line)) {
            Line::Progress { downloaded, total, speed, eta } => on_progress(Update::Progress(tracker.feed(downloaded, total, speed, eta))),
            Line::Post(name) => on_progress(Update::Processing(post_label(&name).into())),
            Line::File(p) => file = Some(p),
            Line::Length(l) => length = Some(l),
            Line::Other => {}
        }
    }
    let status = h.child.lock().unwrap_or_else(|e| e.into_inner()).take().map(|mut c| c.wait()).transpose()?;
    let errors = errors.join().unwrap_or_default();
    if h.cancelled.load(Ordering::SeqCst) {
        return Err(anyhow!("cancelled"));
    }
    if !status.is_some_and(|s| s.success()) {
        let last = errors.lines().rev().find(|l| l.contains("ERROR")).or_else(|| errors.lines().last()).unwrap_or("yt-dlp failed");
        return Err(anyhow!("{}", last.trim()));
    }
    let path = file.filter(|p| p.exists()).ok_or_else(|| anyhow!("yt-dlp finished but didn't say where the file is"))?;
    let d = Downloaded {
        video_id: item.id.clone(),
        bytes: std::fs::metadata(&path).ok().map(|m| m.len()),
        path,
        title: item.title.clone(),
        channel: item.channel.clone(),
        channel_id: item.channel_id.clone(),
        length: item.length.or(length),
        audio_only: o.audio_only,
        sponsors_cut: o.cut_sponsors && !o.sponsor_categories.is_empty(),
        at: crate::db::now(),
    };
    record(&crate::db::open()?, &d)?;
    Ok(d)
}

fn post_label(name: &str) -> &str {
    match name {
        "Merger" => "Joining video and audio",
        "SponsorBlock" => "Looking up sponsors",
        "ModifyChapters" => "Cutting out sponsors",
        "EmbedSubtitle" => "Adding subtitles",
        "MoveFiles" => "Finishing",
        _ => "Processing",
    }
}

/// yt-dlp's leftovers for a video: `Title [id].mp4.part`, `….f137.mp4`, `….ytdl` and so on.
/// Only names with "[id]" and a temporary marker, so a finished file is never touched.
fn remove_partials(dir: &Path, video_id: &str) {
    let tag = format!("[{video_id}]");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let temp = name.ends_with(".part")
            || name.ends_with(".ytdl")
            || name.contains(".part-Frag")
            || name.contains(".temp.")
            || is_format_part(&name);
        if name.contains(&tag) && temp {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `Title [id].f137.mp4` (or `.f251-drc.webm`): one stream of a download that never got merged.
fn is_format_part(name: &str) -> bool {
    name.rsplit('.').nth(1).is_some_and(|f| {
        f.starts_with('f')
            && f[1..].starts_with(|c: char| c.is_ascii_digit())
            && f[1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// On Windows a console program started from a GUI app pops up a black console
/// window; this flag keeps it hidden. Elsewhere there's nothing to do.
#[cfg(windows)]
fn no_console_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_cmd: &mut Command) {}

// ---- the table ------------------------------------------------------------------------

pub fn record(conn: &Connection, d: &Downloaded) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO downloads (video_id, path, title, channel, channel_id, length, bytes, audio_only, sponsors_cut, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            d.video_id,
            d.path.to_string_lossy(),
            d.title,
            d.channel,
            d.channel_id,
            d.length,
            d.bytes.map(|b| b as i64),
            d.audio_only,
            d.sponsors_cut,
            d.at
        ],
    )?;
    Ok(())
}

const COLUMNS: &str = "video_id, path, title, channel, channel_id, length, bytes, audio_only, sponsors_cut, at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Downloaded> {
    Ok(Downloaded {
        video_id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        title: r.get(2)?,
        channel: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        channel_id: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        length: r.get(5)?,
        bytes: r.get::<_, Option<i64>>(6)?.map(|b| b as u64),
        audio_only: r.get(7)?,
        sponsors_cut: r.get(8)?,
        at: r.get(9)?,
    })
}

/// Every download, newest first (including ones whose file has gone; check `path.exists()`).
pub fn list(conn: &Connection) -> Result<Vec<Downloaded>> {
    let mut st = conn.prepare(&format!("SELECT {COLUMNS} FROM downloads ORDER BY at DESC"))?;
    Ok(st.query_map([], row)?.collect::<rusqlite::Result<_>>()?)
}

/// The download of a video, if its file is still there.
pub fn get(conn: &Connection, video_id: &str) -> Result<Option<Downloaded>> {
    let d = conn.query_row(&format!("SELECT {COLUMNS} FROM downloads WHERE video_id = ?1"), [video_id], row).optional()?;
    Ok(d.filter(|d| d.path.is_file()))
}

/// The downloaded file of a video, if it's still there.
pub fn path_for(conn: &Connection, video_id: &str) -> Result<Option<PathBuf>> {
    Ok(get(conn, video_id)?.map(|d| d.path))
}

/// Forget a download and delete its file.
pub fn delete(conn: &Connection, video_id: &str) -> Result<()> {
    let path: Option<String> = conn.query_row("SELECT path FROM downloads WHERE video_id = ?1", [video_id], |r| r.get(0)).optional()?;
    if let Some(p) = path {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(anyhow!("couldn't delete {p}: {e}")),
            _ => {}
        }
    }
    conn.execute("DELETE FROM downloads WHERE video_id = ?1", [video_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_lines() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/player/ytdlp-output.txt")).unwrap();
        let lines: Vec<Line> = text.lines().map(parse_line).collect();
        assert_eq!(lines[0], Line::Other, "yt-dlp's own chatter is ignored");
        assert_eq!(lines[1], Line::Progress { downloaded: Some(1024), total: Some(524288), speed: None, eta: None });
        // No exact total: the estimate is used.
        assert_eq!(lines[2], Line::Progress { downloaded: Some(262144), total: Some(530000), speed: Some(1048576.5), eta: Some(3) });
        assert_eq!(lines[3], Line::Progress { downloaded: Some(524288), total: Some(524288), speed: Some(2000000.0), eta: Some(0) });
        assert_eq!(lines[4], Line::Progress { downloaded: Some(4096), total: Some(65536), speed: Some(500000.0), eta: Some(1) });
        assert_eq!(lines[5], Line::Post("Merger".into()));
        assert_eq!(lines[6], Line::Other, "finished post-processors don't count");
        assert_eq!(lines[7], Line::Length(19.0));
        assert_eq!(lines[8], Line::File(PathBuf::from("/tmp/x/Me at the zoo [jNQXAC9IVRw].mkv")));
        assert_eq!(parse_line("YTP-DL NA NA NA NA NA"), Line::Progress { downloaded: None, total: None, speed: None, eta: None });
        assert_eq!(parse_line("YTP-FILE NA"), Line::Other);

        let mut t = Tracker::default();
        let mut parts = Vec::new();
        for l in &lines {
            if let Line::Progress { downloaded, total, speed, eta } = l {
                parts.push(t.feed(*downloaded, *total, *speed, *eta));
            }
        }
        assert_eq!(parts[0].part, 1);
        assert_eq!(parts[2].percent, Some(100.0));
        assert_eq!(parts[3].part, 2, "bytes went down: the audio started");
        assert_eq!(parts[1].summary(), "49% · 1.0 MB/s · 0:03 left");
        assert_eq!(parts[3].summary(), "6% (part 2) · 500 kB/s · 0:01 left");
        assert_eq!(Progress::default().summary(), "Starting…");
    }

    #[test]
    fn builds_ytdlp_arguments() {
        let mut c = Config::default();
        c.download_dir = Some("/tmp/dl".into());
        c.download_cut_sponsors = true;
        c.proxy = Some("socks5h://127.0.0.1:9050".into());
        assert_eq!(Options::from_config(&c).proxy, None, "downloads skip the proxy unless proxy_playback is on");
        c.proxy_playback = true;
        let mut o = Options::from_config(&c);
        assert_eq!(o.dir, PathBuf::from("/tmp/dl"));
        let a = ytdlp_args("-abc", &o).join(" ");
        assert!(a.contains("-P /tmp/dl -o %(title)s [%(id)s].%(ext)s"), "{a}");
        assert!(a.contains("--proxy socks5h://yt-play-") && a.contains(":x@127.0.0.1:9050"), "own Tor circuit: {a}");
        assert!(a.contains("--sponsorblock-remove interaction,music_offtopic,selfpromo,sponsor"), "only skip categories: {a}");
        assert!(a.contains("bestvideo[height<=1080]"));
        assert!(!a.contains("--sub-langs"));
        assert!(a.ends_with("-- https://www.youtube.com/watch?v=-abc"));
        o.audio_only = true;
        o.cut_sponsors = false;
        o.proxy = None;
        o.subtitles = "en, de".into();
        let a = ytdlp_args("x", &o).join(" ");
        assert!(a.contains("-f bestaudio"));
        assert!(!a.contains("--sponsorblock") && !a.contains("--proxy"));
        assert!(a.contains("--sub-langs en.*,de.*"));
        c.download_dir = None;
        assert!(dir(&c).ends_with("YourTube"));
    }

    #[test]
    fn partial_file_names() {
        assert!(is_format_part("Me at the zoo [jNQXAC9IVRw].f137.mp4"));
        assert!(is_format_part("x [id].f251-drc.webm"));
        assert!(!is_format_part("Me at the zoo [jNQXAC9IVRw].mp4"));
        assert!(!is_format_part("for.mp4"));
        let d = std::env::temp_dir().join(format!("yourtube-partials-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for n in ["A [id1].mp4", "A [id1].mp4.part", "A [id1].f137.mp4", "B [id2].mp4.part"] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        remove_partials(&d, "id1");
        let mut left: Vec<String> =
            std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["A [id1].mp4", "B [id2].mp4.part"]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn table_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        let dir = std::env::temp_dir().join(format!("yourtube-dltable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Me at the zoo [jNQXAC9IVRw].mp4");
        std::fs::write(&file, b"video").unwrap();
        let d = Downloaded {
            video_id: "jNQXAC9IVRw".into(),
            path: file.clone(),
            title: "Me at the zoo".into(),
            channel: "jawed".into(),
            channel_id: "UC4QobU6STFB0P71PMvOGN5A".into(),
            length: Some(19.0),
            bytes: Some(5),
            audio_only: false,
            sponsors_cut: true,
            at: 1,
        };
        record(&conn, &d).unwrap();
        assert_eq!(list(&conn).unwrap(), vec![d.clone()]);
        assert_eq!(get(&conn, "jNQXAC9IVRw").unwrap(), Some(d.clone()));
        assert_eq!(path_for(&conn, "jNQXAC9IVRw").unwrap(), Some(file.clone()));
        assert_eq!(path_for(&conn, "nope").unwrap(), None);
        assert!(d.item().note.unwrap().contains("Downloaded"));
        // A file removed by hand no longer counts, but the row stays until deleted.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(path_for(&conn, "jNQXAC9IVRw").unwrap(), None);
        std::fs::write(&file, b"video").unwrap();
        delete(&conn, "jNQXAC9IVRw").unwrap();
        assert!(!file.exists(), "delete removes the file");
        assert!(list(&conn).unwrap().is_empty());
        delete(&conn, "jNQXAC9IVRw").unwrap(); // twice is fine
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn at_most_two_run_and_cancel_works_while_queued() {
        // Uses the real queue with a fake "download": hold slots directly.
        let mut s = SLOTS.lock().unwrap();
        let before = s.running;
        s.running = MAX_RUNNING;
        drop(s);
        let item = Item { id: "queued-test".into(), title: "t".into(), ..Default::default() };
        let (tx, rx) = std::sync::mpsc::channel();
        let o = Options { dir: std::env::temp_dir(), ..Options::from_config(&Config::default()) };
        let h = start(&item, o.clone(), move |u| tx.send(u).unwrap()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Update::Queued);
        assert!(is_active("queued-test"));
        assert!(start(&item, o, |_| {}).is_err(), "no duplicate downloads");
        h.cancel();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Update::Cancelled);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!is_active("queued-test"));
        SLOTS.lock().unwrap().running = before;
    }

    /// A real download (network, ~1 MB). Records into the database of XDG_DATA_HOME,
    /// so it refuses to run against your real data:
    /// `XDG_DATA_HOME=/tmp/yt-test cargo test live_download -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_download() {
        let data = std::env::var("XDG_DATA_HOME").unwrap_or_default();
        assert!(data.starts_with("/tmp"), "set XDG_DATA_HOME to a temporary folder");
        let dir = PathBuf::from(&data).join("downloads");
        let item = Item { id: "jNQXAC9IVRw".into(), title: "Me at the zoo".into(), channel: "jawed".into(), ..Default::default() };
        let o = Options { dir: dir.clone(), height: 240, ..Options::from_config(&Config::default()) };
        let (tx, rx) = std::sync::mpsc::channel();
        start(&item, o, move |u| tx.send(u).unwrap()).unwrap();
        let mut progress = 0;
        let done = loop {
            match rx.recv_timeout(Duration::from_secs(120)).unwrap() {
                Update::Progress(p) => {
                    progress += 1;
                    println!("progress: {}", p.summary());
                }
                Update::Done(d) => break d,
                Update::Failed(e) => panic!("{e}"),
                u => println!("{u:?}"),
            }
        };
        println!("done: {done:?}");
        assert!(progress > 0 && done.path.is_file() && done.path.starts_with(&dir));
        let conn = crate::db::open().unwrap();
        assert_eq!(path_for(&conn, "jNQXAC9IVRw").unwrap(), Some(done.path.clone()));
        delete(&conn, "jNQXAC9IVRw").unwrap();
        assert!(!done.path.exists());
    }
}
