//! Playback in mpv, a separate process, with the position saved every 5 s.
//!
//! mpv is controlled over its JSON IPC: a Unix socket on Linux and macOS, a named
//! pipe on Windows. Every quarter second we ask mpv where it is; that drives
//! progress saving, playlist (queue) tracking and SponsorBlock skips. If mpv dies,
//! YourTube stays up and the next play resumes from the last saved position.
//!
//! - [`play`] plays one video with the settings from Preferences.
//! - [`play_queue`] plays several in one mpv window (an mpv playlist), saving each
//!   one's progress as it moves on.
//!
//! A downloaded copy (see download.rs) is played instead of streaming when there
//! is one, so this works offline too.

use crate::config::Config;
use crate::db::Item;
use crate::sponsorblock::{self, Action, Segment, Skipper};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SAVE_EVERY: Duration = Duration::from_secs(5);
/// How often we ask mpv for the position. Short enough that a skip lands within a
/// fraction of a second of a segment's start; IPC round trips are microseconds.
const POLL: Duration = Duration::from_millis(250);
/// Closer than this to the end counts as finished, so the next play starts over.
const END_MARGIN: f64 = 20.0;
/// The mpv input section that binds Enter while a skip is offered.
const ASK_SECTION: &str = "yourtube-ask";
/// What Enter sends back to us during an offer (arrives as a `client-message` event).
const SKIP_MESSAGE: &str = "yourtube-skip";

pub enum Event {
    /// mpv moved on to this video (the first one too).
    Started { video_id: String },
    /// Progress was written to the history.
    Saved { video_id: String },
    /// SponsorBlock skipped `seconds` of a `category` segment; `total` is everything
    /// skipped in this video so far ("time saved").
    #[allow(dead_code)] // the UI only shows the latest skip
    Skipped { video_id: String, category: String, seconds: f64, total: f64 },
    /// mpv closed. `video_id` is the video that was playing last.
    Exited { video_id: String, summary: String },
}

/// How to play. [`Options::from_config`] gives the Preferences settings; change any
/// field for one play (for example `audio_only` from a menu entry).
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub speed: f64,
    /// In a queue: go on to the next video by itself. Off pauses at the end of each
    /// one (Enter or Shift+> in mpv moves on).
    pub autoplay: bool,
    /// Preferred subtitle languages, "en,de" (empty = none).
    pub subtitles: String,
    pub proxy: Option<String>,
    pub max_height: u32,
    /// Sound only: the smallest stream and no video decoding (the window stays, for the controls).
    pub audio_only: bool,
    /// Play a downloaded copy when there is one.
    pub prefer_downloads: bool,
    /// None = SponsorBlock off.
    pub sponsorblock: Option<sponsorblock::Settings>,
    /// Save positions to the history (also never in incognito, whatever this says).
    pub remember_history: bool,
    /// Ask mpv for hardware decoding (`--hwdec=auto-safe`), unless your own mpv.conf
    /// already says what you want.
    pub hwdec: bool,
    /// Start the first video here (seconds) instead of where you left off, e.g. from a chapter.
    pub start_at: Option<f64>,
}

impl Options {
    pub fn from_config(c: &Config) -> Options {
        Options {
            speed: c.speed,
            autoplay: c.autoplay,
            subtitles: c.subtitles.clone(),
            proxy: c.playback_proxy(),
            max_height: c.max_height,
            audio_only: false,
            prefer_downloads: true,
            sponsorblock: sponsorblock::Settings::from_config(c),
            remember_history: c.remember_history,
            hwdec: true,
            start_at: None,
        }
    }

    fn saves(&self) -> bool {
        self.remember_history && !crate::db::incognito()
    }
}

/// Where to start: the saved position, unless the video was finished or barely started.
pub fn resume_from(position: f64, length: Option<f64>) -> f64 {
    let finished = length.is_some_and(|l| position >= l - END_MARGIN);
    if position < 10.0 || finished { 0.0 } else { position }
}

/// Play one video in mpv with the Preferences settings and block until mpv exits.
pub fn play(video_id: &str, title: &str, on_event: impl Fn(Event)) -> Result<()> {
    let item = Item { id: video_id.into(), title: title.into(), ..Default::default() };
    play_queue(&[item], &Options::from_config(&crate::config::load()), on_event)
}

/// One playlist entry for mpv.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// A watch URL or a local file.
    pub source: String,
    pub start: f64,
    pub title: String,
}

/// Play `items` in order in one mpv and block until mpv exits.
pub fn play_queue(items: &[Item], opts: &Options, on_event: impl Fn(Event)) -> Result<()> {
    anyhow::ensure!(!items.is_empty(), "nothing to play");
    let conn = crate::db::open()?;
    let mut entries = Vec::new();
    // Files with sponsors already cut out: SponsorBlock's timestamps don't fit them.
    let mut cut = Vec::new();
    let mut lengths = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let (pos, len) = crate::db::position(&conn, &it.id)?.unwrap_or((0.0, it.length));
        let len = len.or(it.length);
        let file = if opts.prefer_downloads { crate::download::get(&conn, &it.id)? } else { None };
        cut.push(file.as_ref().is_some_and(|d| d.sponsors_cut));
        // A cut file is shorter than the video, so its saved position would be off too.
        let start = match opts.start_at.filter(|_| i == 0) {
            Some(at) => at.max(0.0),
            None if file.as_ref().is_some_and(|d| d.sponsors_cut) => 0.0,
            None => resume_from(pos, len),
        };
        lengths.push(if file.is_some() { None } else { len });
        entries.push(Entry {
            source: file.map_or_else(|| watch_url(&it.id), |d| d.path.to_string_lossy().into_owned()),
            start,
            title: if it.title.is_empty() { it.id.clone() } else { it.title.clone() },
        });
    }

    // mpv fetches the video itself and can't use a SOCKS proxy, so it would quietly go direct.
    // Downloaded files are fine: yt-dlp fetched those through the proxy.
    if opts.proxy.as_deref().is_some_and(is_socks) && entries.iter().any(|e| e.source.starts_with("https://")) {
        anyhow::bail!(
            "playback through a SOCKS proxy like Tor isn't possible yet, because mpv would fetch the video from your own address. \
             Turn off \"Playback and downloads through the proxy too\" in Preferences, use an HTTP proxy, or download the video first"
        );
    }

    let ipc_path = ipc_path(std::process::id(), next_serial());
    #[cfg(unix)]
    let _ = std::fs::remove_file(&ipc_path);
    let started = Instant::now();
    let mut child = crate::proc::mpv()
        .args(mpv_args(&entries, opts, &ipc_path, user_sets_hwdec()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // SponsorBlock lookups run on their own threads and report back here, so a slow
    // or unreachable server never delays playback.
    let (seg_tx, seg_rx) = mpsc::channel::<(String, Vec<Segment>)>();
    let mut requested: HashMap<String, Option<Vec<Segment>>> = HashMap::new();
    let lookup = |i: usize, requested: &mut HashMap<String, Option<Vec<Segment>>>| {
        let (Some(it), Some(sb)) = (items.get(i), opts.sponsorblock.clone()) else { return };
        if cut[i] || requested.contains_key(&it.id) {
            return;
        }
        requested.insert(it.id.clone(), None);
        let (id, tx) = (it.id.clone(), seg_tx.clone());
        std::thread::spawn(move || {
            // Offline or blocked: just play without skips.
            let segs = sponsorblock::fetch(&sb, &id).unwrap_or_default();
            let _ = tx.send((id, segs));
        });
    };
    lookup(0, &mut requested);

    let mut ipc: Option<Ipc> = None;
    let mut now: Option<Playing> = None;
    let mut last_save = Instant::now();
    let mut skipped_total = 0.0;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        std::thread::sleep(POLL);
        if ipc.is_none() {
            ipc = Ipc::connect(&ipc_path);
        }
        let Some(c) = ipc.as_mut() else { continue };

        // Which entry is playing? A change means the previous one is done.
        if let Some(i) = c.get("playlist-pos").and_then(|v| v.as_u64()).map(|i| i as usize).filter(|i| *i < items.len()) {
            if now.as_ref().is_none_or(|p| p.index != i) {
                if let Some(prev) = now.take() {
                    skipped_total += prev.skipper.as_ref().map_or(0.0, Skipper::saved);
                    finish(&conn, &prev, opts, &on_event)?;
                }
                c.command(&[json!("disable-section"), json!(ASK_SECTION)]);
                now = Some(Playing { index: i, id: items[i].id.clone(), pos: None, length: lengths[i], skipper: None });
                on_event(Event::Started { video_id: items[i].id.clone() });
                lookup(i, &mut requested);
                lookup(i + 1, &mut requested); // ready before it starts
                last_save = Instant::now();
            }
        }
        let Some(p) = now.as_mut() else { continue };
        if let Some(t) = c.get("time-pos").and_then(|v| v.as_f64()) {
            p.pos = Some(t);
        }
        if p.length.is_none() {
            p.length = c.get("duration").and_then(|v| v.as_f64()).filter(|d| *d > 0.0);
        }

        // SponsorBlock answers, for this video or one coming up.
        while let Ok((id, segs)) = seg_rx.try_recv() {
            requested.insert(id, Some(segs));
        }
        if p.skipper.is_none() {
            if let (Some(Some(segs)), Some(sb)) = (requested.get(&p.id), opts.sponsorblock.as_ref()) {
                let k = Skipper::new(segs.clone(), sb);
                if !k.is_empty() && !c.sections_ready {
                    c.define_ask_section();
                }
                p.skipper = Some(k);
            }
        }
        if let (Some(k), Some(pos)) = (p.skipper.as_mut(), p.pos) {
            let mut actions = Vec::new();
            for msg in c.messages.drain(..) {
                if msg.first().map(String::as_str) == Some(SKIP_MESSAGE) {
                    actions.extend(k.skip_requested(pos));
                }
            }
            actions.extend(k.tick(pos));
            for a in actions {
                match a {
                    Action::Skip { category, from, to } => {
                        c.command(&[json!("seek"), json!(to), json!("absolute")]);
                        c.osd(
                            &format!("Skipped {} ({})", sponsorblock::label(&category).to_lowercase(), sponsorblock::clock(to - from)),
                            3000,
                        );
                        p.pos = Some(to);
                        on_event(Event::Skipped { video_id: p.id.clone(), category, seconds: to - from, total: k.saved() });
                    }
                    Action::Ask { category, remaining } => {
                        let hint = if c.sections_ok { " — press Enter to skip" } else { "" };
                        c.osd(&format!("{}{hint}", sponsorblock::label(&category)), (remaining.min(10.0) * 1000.0) as u64);
                        if c.sections_ok {
                            c.command(&[json!("enable-section"), json!(ASK_SECTION)]);
                        }
                    }
                    Action::EndAsk => {
                        if c.sections_ok {
                            c.command(&[json!("disable-section"), json!(ASK_SECTION)]);
                        }
                    }
                }
            }
        } else {
            c.messages.clear();
        }

        if last_save.elapsed() >= SAVE_EVERY {
            if let (Some(pos), true) = (p.pos, opts.saves()) {
                crate::db::save_progress(&conn, &p.id, pos, p.length)?;
                on_event(Event::Saved { video_id: p.id.clone() });
            }
            last_save = Instant::now();
        }
    };
    #[cfg(unix)]
    let _ = std::fs::remove_file(&ipc_path);

    let last_id = now.as_ref().map_or_else(|| items[0].id.clone(), |p| p.id.clone());
    let last_pos = now.as_ref().and_then(|p| p.pos);
    if let Some(p) = &now {
        skipped_total += p.skipper.as_ref().map_or(0.0, Skipper::saved);
        finish(&conn, p, opts, &on_event)?;
    }

    let summary = format!(
        "{last_id} {} after {:.0}s, last position {}{}{}",
        exit_description(&status),
        started.elapsed().as_secs_f64(),
        last_pos.map_or("unknown".into(), |p| format!("{p:.0}s")),
        if items.len() > 1 { format!(", queue of {}", items.len()) } else { String::new() },
        if skipped_total > 0.0 { format!(", SponsorBlock skipped {skipped_total:.0}s") } else { String::new() },
    );
    // The log is for diagnosing crashes; in incognito it doesn't name the video.
    log(&if crate::db::incognito() { summary.replacen(&last_id, "(incognito)", 1) } else { summary.clone() });
    on_event(Event::Exited { video_id: last_id, summary });
    Ok(())
}

/// The video currently playing.
struct Playing {
    index: usize,
    id: String,
    pos: Option<f64>,
    length: Option<f64>,
    skipper: Option<Skipper>,
}

/// Final save for a video. Near the end counts as watched to the end.
fn finish(conn: &rusqlite::Connection, p: &Playing, opts: &Options, on_event: &impl Fn(Event)) -> Result<()> {
    if let (Some(mut pos), true) = (p.pos, opts.saves()) {
        if let Some(l) = p.length.filter(|l| pos >= l - END_MARGIN) {
            pos = l;
        }
        crate::db::save_progress(conn, &p.id, pos, p.length)?;
        on_event(Event::Saved { video_id: p.id.clone() });
    }
    Ok(())
}

fn exit_description(status: &std::process::ExitStatus) -> String {
    if let Some(c) = status.code() {
        return format!("exited with code {c}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(s) = status.signal() {
            return format!("KILLED by signal {s}");
        }
    }
    "ended".into()
}

pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// Everything mpv is started with. `user_hwdec`: the user's mpv.conf picks a decoder itself.
/// socks4://, socks5://, socks5h://…
pub fn is_socks(proxy: &str) -> bool {
    proxy.trim().to_ascii_lowercase().starts_with("socks")
}

pub fn mpv_args(entries: &[Entry], o: &Options, ipc: &str, user_hwdec: bool) -> Vec<String> {
    let mut a = vec![
        format!("--input-ipc-server={ipc}"),
        "--force-window=immediate".into(),
        // mpv fills in the property, so the window title follows the playlist.
        "--title=${media-title} — YourTube".into(),
    ];
    if (o.speed - 1.0).abs() > 1e-6 {
        a.push(format!("--speed={}", o.speed));
    }
    if o.hwdec && !user_hwdec {
        // Hardware decoding where it's known to work (VA-API, NVDEC, VideoToolbox,
        // D3D11VA…), software otherwise. Cuts CPU use by more than half for 1080p60.
        // Your own `hwdec=` in mpv.conf wins: we leave the option out then.
        a.push("--hwdec=auto-safe".into());
    }
    if o.audio_only {
        a.push("--ytdl-format=bestaudio/best".into());
        a.push("--vid=no".into());
    } else {
        let h = o.max_height.max(144);
        a.push(format!("--ytdl-format=bestvideo[height<=?{h}]+bestaudio/best[height<=?{h}]/best"));
        // Among streams of the best allowed resolution and frame rate, prefer the codec
        // most GPUs decode: H.264, then VP9, then AV1 (which needs a recent GPU and is
        // otherwise decoded in software). Resolution comes first, so quality never
        // drops below max_height to get a friendlier codec.
        a.push(format!("--ytdl-raw-options-append=format-sort=res:{h},fps,+vcodec:avc"));
    }
    let langs: Vec<&str> = o.subtitles.split(',').map(str::trim).filter(|l| !l.is_empty()).collect();
    if !langs.is_empty() {
        a.push(format!("--slang={}", langs.join(",")));
        // mpv only gets the subtitle tracks yt-dlp was asked for. "-append" takes the
        // value whole, so the commas between languages are fine.
        a.push("--ytdl-raw-options-append=write-subs=".into());
        a.push("--ytdl-raw-options-append=write-auto-subs=".into());
        a.push(format!("--ytdl-raw-options-append=sub-langs={}", langs.iter().map(|l| format!("{l}.*")).collect::<Vec<_>>().join(",")));
    }
    a.extend(crate::proc::MPV_YTDL_SAFE.iter().map(|o| format!("--ytdl-raw-options-append={o}")));
    if let Some(p) = &o.proxy {
        a.push(format!("--ytdl-raw-options-append=proxy={p}"));
        // The video data itself is fetched by mpv (ffmpeg), which only speaks HTTP
        // proxies. A SOCKS proxy covers yt-dlp's page and API requests only.
        if p.starts_with("http://") || p.starts_with("https://") {
            a.push(format!("--http-proxy={p}"));
        }
    }
    if entries.len() > 1 && !o.autoplay {
        // Pause at the end of each video instead of going on.
        a.push("--keep-open=always".into());
    }
    for e in entries {
        // --{ … --} gives each playlist entry its own start position and title.
        a.push("--{".into());
        if e.start > 0.0 {
            a.push(format!("--start={:.0}", e.start));
        }
        a.push(format!("--force-media-title={}", e.title));
        a.push(e.source.clone());
        a.push("--}".into());
    }
    a
}

/// Does mpv.conf in the user's mpv folder set `hwdec`?
fn user_sets_hwdec() -> bool {
    mpv_config_dirs().iter().any(|d| std::fs::read_to_string(d.join("mpv.conf")).is_ok_and(|t| conf_sets(&t, "hwdec")))
}

/// Where mpv reads mpv.conf: $MPV_HOME if set, else ~/.config/mpv (Linux, macOS,
/// and also Windows), $XDG_CONFIG_HOME/mpv, and %APPDATA%\mpv on Windows.
fn mpv_config_dirs() -> Vec<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("MPV_HOME") {
        return vec![h.into()];
    }
    let mut v = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        v.push(std::path::PathBuf::from(x).join("mpv"));
    }
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".config").join("mpv"));
    }
    if cfg!(windows) {
        if let Some(c) = dirs::config_dir() {
            v.push(c.join("mpv"));
        }
    }
    v
}

/// Is `key` set by an (uncommented) line of an mpv.conf?
pub fn conf_sets(text: &str, key: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim();
        !l.starts_with('#') && l.split('=').next().is_some_and(|k| k.trim().trim_start_matches("--") == key) && l.contains('=')
    })
}

fn next_serial() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// The IPC address for one mpv: a socket file on Unix, a named pipe on Windows.
fn ipc_path(pid: u32, serial: u64) -> String {
    if cfg!(windows) {
        windows_pipe_name(pid, serial)
    } else {
        // The runtime dir (/run/user/…) is private to you; macOS has none, and its
        // per-user temp dir is private too. Socket paths must stay short (~104 bytes).
        let dir = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
        dir.join(format!("yourtube-mpv-{pid}-{serial}.sock")).to_string_lossy().into_owned()
    }
}

/// `\\.\pipe\yourtube-mpv-<pid>-<n>`: the form mpv's --input-ipc-server wants on Windows.
pub fn windows_pipe_name(pid: u32, serial: u64) -> String {
    format!(r"\\.\pipe\yourtube-mpv-{pid}-{serial}")
}

/// Append one line to the player log (~/.local/state/yourtube/player.log on Linux).
fn log(line: &str) {
    let dir = dirs::state_dir().or_else(dirs::data_local_dir).unwrap_or_else(std::env::temp_dir).join("yourtube");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("player.log")) {
        let _ = writeln!(f, "{} {line}", crate::db::now());
    }
}

// ---- mpv JSON IPC ------------------------------------------------------------------------

/// One request line: `{"command": [...], "request_id": N}` and a newline.
pub fn frame(command: &[Value], request_id: u64) -> String {
    format!("{}\n", json!({"command": command, "request_id": request_id}))
}

/// A line mpv sent us.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// The answer to one of our requests. `error` is "success" when it worked.
    Reply {
        request_id: u64,
        error: String,
        data: Value,
    },
    /// `script-message …` from a key binding: our skip key.
    ClientMessage(Vec<String>),
    /// Any other event (we don't need them).
    Event(String),
    Junk,
}

pub fn parse_incoming(line: &str) -> Incoming {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return Incoming::Junk };
    if let Some(ev) = v["event"].as_str() {
        if ev == "client-message" {
            let args = v["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            return Incoming::ClientMessage(args);
        }
        return Incoming::Event(ev.to_owned());
    }
    match v["request_id"].as_u64() {
        Some(id) => Incoming::Reply { request_id: id, error: v["error"].as_str().unwrap_or("").to_owned(), data: v["data"].clone() },
        None => Incoming::Junk,
    }
}

struct Ipc {
    r: BufReader<Box<dyn Read + Send>>,
    w: Box<dyn Write + Send>,
    next_id: u64,
    /// `client-message` events seen while waiting for replies.
    messages: Vec<Vec<String>>,
    /// We tried to define the Enter-to-skip section…
    sections_ready: bool,
    /// …and mpv took it.
    sections_ok: bool,
}

impl Ipc {
    fn connect(path: &str) -> Option<Self> {
        let (r, w) = connect_raw(path).ok()?;
        Some(Ipc { r: BufReader::new(r), w, next_id: 1, messages: Vec::new(), sections_ready: false, sections_ok: false })
    }

    /// Send a command and wait for its answer: Ok(data) or Err(mpv's error).
    fn request(&mut self, command: &[Value]) -> Option<std::result::Result<Value, String>> {
        let id = self.next_id;
        self.next_id += 1;
        self.w.write_all(frame(command, id).as_bytes()).ok()?;
        self.w.flush().ok()?;
        // Events can arrive before our reply; keep the ones we care about.
        let mut line = String::new();
        for _ in 0..200 {
            line.clear();
            if self.r.read_line(&mut line).ok()? == 0 {
                return None;
            }
            match parse_incoming(&line) {
                Incoming::Reply { request_id, error, data } if request_id == id => {
                    return Some(if error == "success" { Ok(data) } else { Err(error) });
                }
                Incoming::ClientMessage(args) => self.messages.push(args),
                _ => {}
            }
        }
        None
    }

    fn get(&mut self, prop: &str) -> Option<Value> {
        self.request(&[json!("get_property"), json!(prop)])?.ok()
    }

    fn command(&mut self, command: &[Value]) -> bool {
        matches!(self.request(command), Some(Ok(_)))
    }

    fn osd(&mut self, text: &str, ms: u64) {
        self.command(&[json!("show-text"), json!(text), json!(ms)]);
    }

    /// Bind Enter (and keypad Enter) to our skip message, in a section we switch on
    /// only while a skip is offered. A section rather than `keybind`: turning it off
    /// gives Enter back to whatever it did before (your input.conf or mpv's default),
    /// which `keybind` can't undo. If this mpv doesn't know sections, offers just show
    /// the category without the Enter hint.
    fn define_ask_section(&mut self) {
        self.sections_ready = true;
        let keys = format!("ENTER script-message {SKIP_MESSAGE}\nKP_ENTER script-message {SKIP_MESSAGE}");
        self.sections_ok = self.command(&[json!("define-section"), json!(ASK_SECTION), json!(keys), json!("force")]);
    }
}

type Halves = (Box<dyn Read + Send>, Box<dyn Write + Send>);

#[cfg(unix)]
fn connect_raw(path: &str) -> std::io::Result<Halves> {
    let s = std::os::unix::net::UnixStream::connect(path)?;
    // mpv answers in microseconds; this only matters if it hangs.
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    Ok((Box::new(s.try_clone()?), Box::new(s)))
}

/// On Windows mpv's IPC is a named pipe, which opens like a file. Until mpv has
/// created it, opening fails and we retry on the next poll. Reads can't time out on
/// a pipe, but mpv always answers, and if it dies the pipe breaks and the read fails.
#[cfg(windows)]
fn connect_raw(path: &str) -> std::io::Result<Halves> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    Ok((Box::new(f.try_clone()?), Box::new(f)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn opts() -> Options {
        Options {
            speed: 1.0,
            autoplay: true,
            subtitles: String::new(),
            proxy: None,
            max_height: 1080,
            audio_only: false,
            prefer_downloads: true,
            sponsorblock: None,
            remember_history: true,
            hwdec: true,
            start_at: None,
        }
    }

    fn entry(src: &str, start: f64) -> Entry {
        Entry { source: src.into(), start, title: "T, with comma".into() }
    }

    #[test]
    fn resume_rules() {
        assert_eq!(resume_from(5.0, Some(600.0)), 0.0);
        assert_eq!(resume_from(300.0, Some(600.0)), 300.0);
        assert_eq!(resume_from(590.0, Some(600.0)), 0.0);
        assert_eq!(resume_from(300.0, None), 300.0);
    }

    #[test]
    fn command_framing() {
        let f = frame(&[json!("show-text"), json!("Skipped \"sponsor\" (0:42)\nx"), json!(3000)], 7);
        assert!(f.ends_with('\n') && f.matches('\n').count() == 1, "one line per command, newlines escaped: {f:?}");
        let v: Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v, json!({"command": ["show-text", "Skipped \"sponsor\" (0:42)\nx", 3000], "request_id": 7}));
    }

    #[test]
    fn parses_mpv_lines() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/player/mpv-ipc.jsonl")).unwrap();
        let got: Vec<Incoming> = text.lines().map(parse_incoming).collect();
        assert_eq!(got[0], Incoming::Event("file-loaded".into()));
        assert_eq!(got[1], Incoming::Reply { request_id: 3, error: "success".into(), data: json!(12.5) });
        assert_eq!(got[2], Incoming::Reply { request_id: 4, error: "property unavailable".into(), data: Value::Null });
        assert_eq!(got[3], Incoming::ClientMessage(vec!["yourtube-skip".into()]));
        assert_eq!(got[4], Incoming::Junk);
    }

    #[test]
    fn mpv_arguments() {
        let mut o = opts();
        let a = mpv_args(&[entry("https://www.youtube.com/watch?v=x", 95.4)], &o, "/run/user/1000/s.sock", false);
        assert_eq!(a[0], "--input-ipc-server=/run/user/1000/s.sock");
        assert!(a.contains(&"--hwdec=auto-safe".into()));
        assert!(
            !a.iter().any(|x| x.starts_with("--speed") || x.starts_with("--slang") || x.contains("proxy") || x.starts_with("--keep-open"))
        );
        assert!(a.contains(&"--ytdl-raw-options-append=format-sort=res:1080,fps,+vcodec:avc".into()));
        for safe in ["no-exec=", "no-remote-components=", "js-runtimes=deno"] {
            assert!(a.contains(&format!("--ytdl-raw-options-append={safe}")), "missing {safe}");
        }
        assert!(is_socks("socks5h://127.0.0.1:9050") && is_socks("SOCKS4://x") && !is_socks("http://p:3128"));
        assert_eq!(
            a[a.len() - 5..],
            ["--{", "--start=95", "--force-media-title=T, with comma", "https://www.youtube.com/watch?v=x", "--}"]
        );
        assert!(!mpv_args(&[entry("x", 0.0)], &o, "p", true).iter().any(|x| x.starts_with("--hwdec")), "the user's mpv.conf wins");

        o.speed = 1.5;
        o.subtitles = "en, de".into();
        o.proxy = Some("socks5h://127.0.0.1:9050".into());
        o.audio_only = true;
        o.autoplay = false;
        let a = mpv_args(&[entry("/v/a.mkv", 0.0), entry("/v/b.mkv", 0.0)], &o, r"\\.\pipe\p", false);
        for want in [
            "--speed=1.5",
            "--slang=en,de",
            "--ytdl-raw-options-append=sub-langs=en.*,de.*",
            "--ytdl-raw-options-append=write-subs=",
            "--ytdl-raw-options-append=proxy=socks5h://127.0.0.1:9050",
            "--ytdl-format=bestaudio/best",
            "--vid=no",
            "--keep-open=always",
            r"--input-ipc-server=\\.\pipe\p",
        ] {
            assert!(a.contains(&want.to_string()), "missing {want} in {a:?}");
        }
        assert!(!a.iter().any(|x| x.starts_with("--http-proxy")), "ffmpeg can't use SOCKS");
        assert!(!a.iter().any(|x| x.starts_with("--start")), "nothing to resume");
        assert_eq!(a.iter().filter(|x| *x == "--{").count(), 2);
        o.proxy = Some("http://proxy:3128".into());
        assert!(mpv_args(&[entry("x", 0.0)], &o, "p", false).contains(&"--http-proxy=http://proxy:3128".into()));
    }

    #[test]
    fn pipe_and_socket_names() {
        assert_eq!(windows_pipe_name(4242, 3), r"\\.\pipe\yourtube-mpv-4242-3");
        let p = ipc_path(4242, 3);
        if cfg!(windows) {
            assert!(p.starts_with(r"\\.\pipe\"));
        } else {
            assert!(p.ends_with("yourtube-mpv-4242-3.sock") && p.len() < 104, "{p}");
        }
    }

    #[test]
    fn reads_hwdec_from_mpv_conf() {
        assert!(conf_sets("# mine\nhwdec=vaapi\n", "hwdec"));
        assert!(conf_sets("  hwdec = no", "hwdec"));
        assert!(conf_sets("--hwdec=auto", "hwdec"));
        assert!(!conf_sets("#hwdec=vaapi\nhwdec-codecs=all\nprofile=fast", "hwdec"));
        assert!(!conf_sets("", "hwdec"));
    }

    #[test]
    fn options_follow_config() {
        let mut c = Config::default();
        c.speed = 1.25;
        c.sponsorblock = false;
        c.remember_history = false;
        let o = Options::from_config(&c);
        assert_eq!(o.speed, 1.25);
        assert!(o.sponsorblock.is_none());
        assert!(!o.saves());
        assert!(Options::from_config(&Config::default()).sponsorblock.is_some());
    }

    /// A real, silent and invisible queue (network): "Me at the zoo" (19 s), then Smash
    /// Mouth's "All Star", which has a "non-music" segment from 0 to ~37 s that is
    /// skipped by default.
    /// MPV_HOME must hold an mpv.conf with `vo=null`, `ao=null` and `end=50`, and
    /// XDG_DATA_HOME a temporary folder (progress is saved there):
    /// `MPV_HOME=/tmp/m XDG_DATA_HOME=/tmp/d cargo test live_skip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_skip() {
        let conf = std::fs::read_to_string(PathBuf::from(std::env::var("MPV_HOME").unwrap()).join("mpv.conf")).unwrap();
        assert!(conf.contains("vo=null") && conf.contains("ao=null"), "must be silent and invisible");
        assert!(std::env::var("XDG_DATA_HOME").unwrap().starts_with("/tmp"));
        // A previous run's saved position would resume past the segment.
        crate::db::open().unwrap().execute("DELETE FROM history WHERE video_id IN ('jNQXAC9IVRw', 'L_jWHffIx5E')", []).unwrap();
        let items = [
            Item { id: "jNQXAC9IVRw".into(), title: "Me at the zoo".into(), ..Default::default() },
            Item { id: "L_jWHffIx5E".into(), title: "All Star".into(), ..Default::default() },
        ];
        let o = Options { max_height: 360, ..Options::from_config(&Config::default()) };
        let (skipped, started) = (std::cell::Cell::new(false), std::cell::RefCell::new(Vec::new()));
        play_queue(&items, &o, |e| match e {
            Event::Started { video_id } => {
                println!("started {video_id}");
                started.borrow_mut().push(video_id);
            }
            Event::Saved { video_id } => println!("saved {video_id}"),
            Event::Skipped { video_id, category, seconds, total } => {
                println!("SKIPPED {video_id}: {category} {seconds:.1}s (total {total:.1}s)");
                skipped.set(true);
            }
            Event::Exited { summary, .. } => println!("exited: {summary}"),
        })
        .unwrap();
        assert!(skipped.get());
        assert_eq!(*started.borrow(), ["jNQXAC9IVRw", "L_jWHffIx5E"]);
        let conn = crate::db::open().unwrap();
        assert_eq!(crate::db::position(&conn, "jNQXAC9IVRw").unwrap().map(|p| p.0.round()), Some(19.0), "the first one counts as finished");
    }
}
