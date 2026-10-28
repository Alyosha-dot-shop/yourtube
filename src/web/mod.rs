//! The web version: `yourtube serve [ADDRESS]`.
//!
//! A small server for phones, tablets and other computers on your home network. It renders
//! plain HTML from the same database, recommender and YouTube readers as the window, and
//! relays video streams (YouTube's stream URLs only work from the IP that asked for them).
//!
//! * Threads: one accepts requests and hands them to a fixed pool of page workers, or to a
//!   separate fixed pool for video streams (a playing video holds its thread while it downloads).
//! * Profiles: each browser picks its profile with a cookie. `db::set_profile` is process-wide,
//!   so requests never use it: they open their profile's database file directly. The only code
//!   that reads the global is `feed::refresh` and `foryou::refresh`; those run one at a time
//!   under `refresh_lock`, with the global set to the asking browser's profile.
//! * Incognito per browser (cookie): nothing is written for that browser at all.
//! * Security: optional password (constant-time compare, random session token in an HttpOnly
//!   SameSite=Strict cookie), same-origin checks on every POST, Host checks against DNS
//!   rebinding when there's no password, strict id validation, and everything rendered escaped.

mod html;
mod http;
mod stream;

use crate::config::Config;
use crate::db::{self, Item};
use crate::foryou::remote as rec;
use anyhow::{Result, anyhow};
use html::{CardOpts, Ctx, Tab};
use http::{Route, esc};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const PAGE_WORKERS: usize = 8;
const STREAM_WORKERS: usize = 12;
const MAX_BODY: u64 = 64 * 1024;
const SESSION_DAYS: u64 = 30;
/// A session also ends after this long without use.
const SESSION_IDLE: Duration = Duration::from_secs(7 * 86400);
/// Wrong passwords allowed per address within the window.
const LOGIN_TRIES: u32 = 10;
const LOGIN_WINDOW: Duration = Duration::from_secs(600);
const FOR_YOU_LIMIT: usize = 120;
const FOR_YOU_STALE: i64 = 6 * 3600;
const FEED_LIMIT: i64 = 150;
const HISTORY_LIMIT: i64 = 300;
/// For You picks counted as seen per page view (about a screenful or two).
const SHOWN: usize = 24;

const STYLE: &str = include_str!("style.css");
const SCRIPT: &str = include_str!("app.js");
const MANIFEST: &str = r##"{"name":"YourTube","short_name":"YourTube","start_url":"/","display":"standalone","background_color":"#EC4A36","theme_color":"#EC4A36","icons":[{"src":"/static/icon.svg","sizes":"any","type":"image/svg+xml"}]}"##;

const COOKIE_SESSION: &str = "yt_session";
const COOKIE_PROFILE: &str = "yt_profile";
const COOKIE_INCOGNITO: &str = "yt_incognito";

pub fn serve(cfg: &crate::config::Config) -> Result<()> {
    let addr_text = address_arg().unwrap_or_else(|| cfg.web_address.clone());
    let addr: SocketAddr = addr_text
        .to_socket_addrs()
        .map_err(|e| anyhow!("can't use web_address {addr_text:?}: {e}"))?
        .next()
        .ok_or_else(|| anyhow!("can't use web_address {addr_text:?}"))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(PAGE_WORKERS + STREAM_WORKERS + 4)
        .enable_all()
        .build()?;
    let listener = rt.block_on(tokio::net::TcpListener::bind(addr)).map_err(|e| anyhow!("can't listen on {addr}: {e}"))?;
    let app = Arc::new(App::new(cfg.clone()));

    // A password typed into the config file by hand is stored as a hash from now on.
    if !cfg.web_password.is_empty() && !crate::config::is_password_hash(&cfg.web_password) {
        match crate::config::save(cfg) {
            Ok(()) => println!("Stored the web password as a hash in {}.", crate::config::path().display()),
            Err(e) => eprintln!("Couldn't rewrite the config to hash the web password: {e}"),
        }
    }
    println!("YourTube is running. Open it in a browser:");
    for url in urls(addr) {
        println!("  {url}");
    }
    if !addr.ip().is_loopback() && cfg.web_password.is_empty() {
        eprintln!(
            "\nWARNING: listening on {addr} without a password. Anyone on your network can see and change\n\
             this computer's YourTube history, subscriptions and playlists. Set web_password in\n\
             {} (or listen on 127.0.0.1 only).",
            crate::config::path().display()
        );
    }
    if app.forced_incognito {
        println!("Incognito: nothing anyone watches or searches is saved.");
    }
    println!("Press Ctrl+C to stop.");

    rt.block_on(accept_loop(listener, app));
    Ok(())
}

/// Connections at once, in total and from one address, before new ones are turned away.
const MAX_CONNECTIONS: usize = 64;
const MAX_PER_PEER: usize = 16;
/// A request's headers must arrive within this (also the keep-alive idle limit), and its body too.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(15);

async fn accept_loop(listener: tokio::net::TcpListener, app: Arc<App>) {
    let total = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let per_peer: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    // Page and stream handlers block (SQLite, yt-dlp), so they run on a bounded set of threads.
    let pages = Arc::new(tokio::sync::Semaphore::new(PAGE_WORKERS));
    let streams = Arc::new(tokio::sync::Semaphore::new(STREAM_WORKERS));
    loop {
        let Ok((sock, peer)) = listener.accept().await else { continue };
        let Ok(permit) = total.clone().try_acquire_owned() else { continue };
        let ip = peer.ip();
        {
            let mut m = per_peer.lock().unwrap();
            let n = m.entry(ip).or_default();
            if *n >= MAX_PER_PEER {
                continue;
            }
            *n += 1;
        }
        let (app, per_peer, pages, streams) = (app.clone(), per_peer.clone(), pages.clone(), streams.clone());
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |rq| {
                let (app, pages, streams) = (app.clone(), pages.clone(), streams.clone());
                async move { Ok::<_, std::convert::Infallible>(app.serve_hyper(rq, ip, pages, streams).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .max_buf_size(16 * 1024)
                .max_headers(64)
                .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                .await;
            drop(permit);
            let mut m = per_peer.lock().unwrap();
            if let Some(n) = m.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&ip);
                }
            }
        });
    }
}

type HyperBody = http_body_util::combinators::BoxBody<bytes::Bytes, std::io::Error>;

/// A short plain-text answer from the transport itself.
fn plain(status: u16, text: &'static str) -> hyper::Response<HyperBody> {
    use http_body_util::BodyExt;
    let mut r = hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from_static(text.as_bytes())).map_err(|e| match e {}).boxed());
    *r.status_mut() = hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR);
    r
}

/// `yourtube serve 0.0.0.0:8723` (or just a port) overrides web_address.
fn address_arg() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == "serve")?;
    let a = args.get(i + 1)?;
    if a.parse::<u16>().is_ok() {
        return Some(format!("127.0.0.1:{a}"));
    }
    (a.contains(':') && !a.starts_with('-')).then(|| a.clone())
}

/// Addresses to open, including this machine's LAN address when listening on all interfaces.
fn urls(addr: SocketAddr) -> Vec<String> {
    let port = addr.port();
    let fmt = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}/"),
        IpAddr::V4(v4) => format!("http://{v4}:{port}/"),
    };
    if !addr.ip().is_unspecified() {
        return vec![fmt(addr.ip())];
    }
    let mut out = vec![format!("http://127.0.0.1:{port}/  (this computer)")];
    if let Some(ip) = lan_ip() {
        out.push(format!("{}  (other devices on your network)", fmt(ip)));
    }
    out
}

/// The address other devices reach this one on: the one the default route uses. Nothing is sent.
fn lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").or_else(|_| s.connect("8.8.8.8:53")).ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// A version tag for static files, so browsers can cache them and still see changes.
fn asset_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in STYLE.bytes().chain(SCRIPT.bytes()) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        format!("{:x}", h & 0xffff_ffff)
    })
}

// ---------------------------------------------------------------------------------------------
// Sessions

/// Logged-in browsers: random tokens, kept in memory (restarting the server logs everyone out).
pub struct Sessions {
    /// Token → (created, last used).
    map: Mutex<HashMap<String, (Instant, Instant)>>,
    ttl: Duration,
    idle: Duration,
}

impl Sessions {
    fn new(ttl: Duration) -> Self {
        Sessions { map: Mutex::new(HashMap::new()), ttl, idle: SESSION_IDLE.min(ttl) }
    }

    fn alive(&self, (created, used): &(Instant, Instant)) -> bool {
        created.elapsed() < self.ttl && used.elapsed() < self.idle
    }

    fn create(&self) -> Result<String> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|e| anyhow!("no randomness: {e}"))?;
        let token: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let mut m = self.map.lock().unwrap();
        m.retain(|_, s| self.alive(s));
        m.insert(token.clone(), (Instant::now(), Instant::now()));
        Ok(token)
    }

    fn check(&self, token: &str) -> bool {
        if token.len() != 64 {
            return false;
        }
        let mut m = self.map.lock().unwrap();
        // Compare every stored token without stopping early, so timing says nothing.
        let mut hit = None;
        for (t, s) in m.iter() {
            if http::ct_eq(t.as_bytes(), token.as_bytes()) && self.alive(s) {
                hit = Some(t.clone());
            }
        }
        if let Some(s) = hit.and_then(|t| m.get_mut(&t)) {
            s.1 = Instant::now();
            return true;
        }
        false
    }

    fn remove(&self, token: &str) {
        self.map.lock().unwrap().remove(token);
    }
}

// ---------------------------------------------------------------------------------------------
// Background refreshes

#[derive(Clone, Default)]
struct Job {
    running: bool,
    message: String,
    error: Option<String>,
    finished: Option<i64>,
}

type JobKey = (String, &'static str);

struct App {
    cfg: Config,
    default_profile: String,
    forced_incognito: bool,
    sessions: Sessions,
    /// Failed logins in the current window, and when the window started.
    /// Wrong passwords per address in the last ten minutes.
    login_fails: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    resolver: Arc<stream::Resolver>,
    jobs: Mutex<HashMap<JobKey, Job>>,
    /// Held while feed::refresh or foryou::refresh runs (they read the process-wide profile).
    refresh_lock: Mutex<()>,
    thumbs: crate::thumbs::Loader,
    thumb_done: Arc<(Mutex<u64>, Condvar)>,
    /// Profiles whose database was initialised in this run.
    inited: Mutex<HashSet<String>>,
}

/// One parsed request.
struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    cookies: HashMap<String, String>,
    body: Vec<u8>,
    /// The address the request came from (for the per-device login limit).
    peer: IpAddr,
}

impl Req {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(k).map(String::as_str)
    }

    fn q(&self, k: &str) -> &str {
        self.query.get(k).map(String::as_str).unwrap_or("")
    }

    /// Fields of a JSON or form body.
    fn fields(&self) -> HashMap<String, String> {
        let text = String::from_utf8_lossy(&self.body);
        if self.header("content-type").is_some_and(|c| c.starts_with("application/json")) {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            v.as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                match v {
                                    Value::String(s) => s.clone(),
                                    Value::Null => String::new(),
                                    other => other.to_string(),
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            http::parse_form(&text)
        }
    }
}

enum Body {
    Bytes(Vec<u8>),
    Stream(Box<dyn Read + Send>, Option<usize>),
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Body,
}

impl Resp {
    fn new(status: u16, ctype: &str, body: impl Into<Vec<u8>>) -> Self {
        Resp { status, headers: vec![("Content-Type".into(), ctype.into())], body: Body::Bytes(body.into()) }
    }
    fn html(body: String) -> Self {
        Resp::new(200, "text/html; charset=utf-8", body).header("Cache-Control", "no-store")
    }
    fn json(v: Value) -> Self {
        Resp::new(200, "application/json", v.to_string()).header("Cache-Control", "no-store")
    }
    fn text(status: u16, s: &str) -> Self {
        Resp::new(status, "text/plain; charset=utf-8", s.to_owned()).header("Cache-Control", "no-store")
    }
    fn redirect(to: &str) -> Self {
        Resp::new(303, "text/plain", Vec::new()).header("Location", to).header("Cache-Control", "no-store")
    }
    fn status(mut self, s: u16) -> Self {
        self.status = s;
        self
    }
    fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_owned(), v.to_owned()));
        self
    }
}

/// The profile and incognito state a browser's cookies ask for. Unknown profiles fall back.
fn browser_prefs(cookies: &HashMap<String, String>, profiles: &[String], default: &str) -> (String, bool) {
    let asked = cookies.get(COOKIE_PROFILE).map(|p| http::url_decode(p));
    let profile = asked.filter(|p| profiles.contains(p)).unwrap_or_else(|| default.to_owned());
    (profile, cookies.get(COOKIE_INCOGNITO).is_some_and(|v| v == "1"))
}

fn db_path(profile: &str) -> PathBuf {
    if profile == db::DEFAULT_PROFILE {
        db::data_dir().join("yourtube.db")
    } else {
        db::data_dir().join("profiles").join(profile).join("yourtube.db")
    }
}

impl App {
    fn new(cfg: Config) -> Self {
        let thumb_done = Arc::new((Mutex::new(0u64), Condvar::new()));
        let td = thumb_done.clone();
        let thumbs = crate::thumbs::Loader::new(4, move |_| {
            *td.0.lock().unwrap() += 1;
            td.1.notify_all();
        });
        App {
            resolver: Arc::new(stream::Resolver::new(cfg.max_height, cfg.playback_proxy())),
            default_profile: db::profile_name(),
            forced_incognito: db::incognito(),
            sessions: Sessions::new(Duration::from_secs(SESSION_DAYS * 86400)),
            login_fails: Mutex::default(),
            jobs: Mutex::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
            thumbs,
            thumb_done,
            inited: Mutex::new(HashSet::new()),
            cfg,
        }
    }

    /// Open a profile's database directly (never through the process-wide profile).
    fn conn(&self, profile: &str) -> Result<Connection> {
        let path = db_path(profile);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        if self.inited.lock().unwrap().insert(profile.to_owned()) {
            db::init(&conn)?;
        }
        Ok(conn)
    }

    /// One request from hyper: read the (small) body, run the page code on a worker thread,
    /// and send the answer back, streaming video bodies.
    async fn serve_hyper(
        self: Arc<Self>,
        rq: hyper::Request<hyper::body::Incoming>,
        peer: IpAddr,
        pages: Arc<tokio::sync::Semaphore>,
        streams: Arc<tokio::sync::Semaphore>,
    ) -> hyper::Response<HyperBody> {
        use http_body_util::BodyExt;
        let started = Instant::now();
        let method = rq.method().as_str().to_owned();
        let (path, query) = http::split_url(&rq.uri().to_string());
        let headers: HashMap<String, String> =
            rq.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_owned()))).collect();
        let mut body = Vec::new();
        if method == "POST" {
            let limited = http_body_util::Limited::new(rq.into_body(), MAX_BODY as usize);
            match tokio::time::timeout(BODY_TIMEOUT, limited.collect()).await {
                Ok(Ok(b)) => body = b.to_bytes().to_vec(),
                _ => return plain(413, "Request too large or too slow."),
            }
        }
        let cookies = headers.get("cookie").map(|c| http::parse_cookies(c)).unwrap_or_default();
        let req = Req { method, path, query, headers, cookies, body, peer };
        let pool = if req.path.starts_with("/stream/") { streams } else { pages };
        let Ok(_slot) = pool.acquire_owned().await else { return plain(503, "Busy.") };
        let this = self.clone();
        let (req, resp) = tokio::task::spawn_blocking(move || {
            let resp = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.handle(&req))) {
                Ok(r) => r,
                Err(_) => Resp::text(500, "Something went wrong on the server."),
            };
            (req, resp)
        })
        .await
        .unwrap_or_else(|_| unreachable!("handler panics are caught"));
        let status = resp.status;
        let mut out = hyper::Response::builder().status(status);
        for (k, v) in &resp.headers {
            out = out.header(k.as_str(), v.as_str());
        }
        out = out
            .header("X-Content-Type-Options", "nosniff")
            .header("Referrer-Policy", "no-referrer")
            .header("X-Frame-Options", "DENY")
            .header("Cross-Origin-Opener-Policy", "same-origin")
            .header("Cross-Origin-Resource-Policy", "same-origin")
            .header(
                "Content-Security-Policy",
                "default-src 'self'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            );
        let body: HyperBody = match resp.body {
            Body::Bytes(b) => {
                out = out.header("Content-Length", b.len());
                http_body_util::Full::new(bytes::Bytes::from(b)).map_err(|e| match e {}).boxed()
            }
            // A known length is always sent as Content-Length (players want it), never chunked.
            Body::Stream(mut reader, len) => {
                if let Some(n) = len {
                    out = out.header("Content-Length", n);
                }
                let (mut tx, rx) = http_body_util::channel::Channel::<bytes::Bytes, std::io::Error>::new(4);
                let handle = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if handle.block_on(tx.send_data(bytes::Bytes::copy_from_slice(&buf[..n]))).is_err() {
                                    break; // the browser went away
                                }
                            }
                        }
                    }
                });
                rx.boxed()
            }
        };
        if std::env::var_os("YOURTUBE_WEB_LOG").is_some() {
            eprintln!("{} {} {} {:.1} ms", req.method, req.path, status, started.elapsed().as_secs_f64() * 1000.0);
        }
        out.body(body).unwrap_or_else(|_| plain(500, "Bad response."))
    }

    fn handle(self: &Arc<Self>, req: &Req) -> Resp {
        let route = http::route(&req.method, &req.path);
        let password = !self.cfg.web_password.is_empty();
        let host = req.header("host").unwrap_or("");

        // Without a password, a page on some public domain that rebinds its DNS to this machine
        // could read your history. Only accept names such a page can't have.
        if !password && !http::safe_host(host) {
            return Resp::text(403, "Open YourTube by its IP address or local name (e.g. http://192.168.1.20:8723), or set web_password.");
        }
        let site = req.header("sec-fetch-site");
        if !matches!(route, Route::Static(_)) && http::foreign_subrequest(site, req.header("sec-fetch-mode"), req.header("sec-fetch-dest"))
        {
            return Resp::text(403, "Other sites can't embed or call YourTube.");
        }
        if req.method == "POST" && !http::same_origin(req.header("sec-fetch-site"), req.header("origin"), Some(host)) {
            return Resp::text(403, "Cross-site request refused.");
        }
        let authed = !password || req.cookies.get(COOKIE_SESSION).is_some_and(|t| self.sessions.check(t));
        if !authed && !matches!(route, Route::Login | Route::Static(_)) {
            let wants_html = req.method == "GET"
                && !req.path.starts_with("/api/")
                && !req.path.starts_with("/stream/")
                && !req.path.starts_with("/thumb/");
            if wants_html {
                let next = match req.query.is_empty() {
                    true => req.path.clone(),
                    false => format!(
                        "{}?{}",
                        req.path,
                        req.query
                            .iter()
                            .map(|(k, v)| format!("{}={}", http::url_encode(k), http::url_encode(v)))
                            .collect::<Vec<_>>()
                            .join("&")
                    ),
                };
                return Resp::redirect(&format!("/login?next={}", http::url_encode(&next)));
            }
            return Resp::json(json!({"error": "Log in first"})).status(401);
        }

        let profiles = db::profiles();
        let (profile, incog) = browser_prefs(&req.cookies, &profiles, &self.default_profile);
        let ctx = Ctx {
            profile,
            profiles,
            incognito: incog || self.forced_incognito,
            forced_incognito: self.forced_incognito,
            password,
            hide_views: self.cfg.hide_views,
            writes: http::own_request(site),
        };
        let r = match route {
            Route::ForYou => self.for_you(&ctx),
            Route::Subscriptions => self.subscriptions(&ctx),
            Route::Search => self.search(&ctx, req.q("q")),
            Route::Channel(id) => self.channel(&ctx, &id),
            Route::Library => self.library(&ctx),
            Route::Playlist(id) => self.playlist(&ctx, &id),
            Route::History => self.history(&ctx),
            Route::Watch => self.watch(&ctx, req.q("v"), req.q("run")),
            Route::Settings => Ok(Resp::html(html::layout(&ctx, "Profile and privacy", Tab::None, &html::settings_body(&ctx), ""))),
            Route::Login => self.login(req),
            Route::Logout => {
                if let Some(t) = req.cookies.get(COOKIE_SESSION) {
                    self.sessions.remove(t);
                }
                Ok(Resp::redirect("/login").header("Set-Cookie", &http::clear_cookie(COOKIE_SESSION)))
            }
            Route::Prefs => Ok(self.prefs(&ctx, req)),
            Route::Static(name) => Ok(static_file(name)),
            Route::Thumb(id) => Ok(self.thumb(&id)),
            Route::Stream(id) => Ok(self.stream(&id, req.header("range"))),
            Route::Info(id) => self.info(&ctx, &id),
            Route::Related(id) => self.related(&ctx, &id),
            Route::Progress => self.progress(&ctx, req),
            Route::Action => self.action(&ctx, req),
            Route::Refresh => Ok(self.refresh(&ctx, req)),
            Route::Status => Ok(self.status(&ctx, req.q("what"))),
            Route::NotFound => Ok(self.error_page(&ctx, 404, "Not found", "There's nothing at this address.")),
            Route::BadMethod => Ok(Resp::text(405, "Method not allowed")),
        };
        r.unwrap_or_else(|e| {
            if req.path.starts_with("/api/") {
                Resp::json(json!({"error": e.to_string()})).status(500)
            } else {
                self.error_page(&ctx, 500, "Something went wrong", &e.to_string())
            }
        })
    }

    fn error_page(&self, ctx: &Ctx, status: u16, title: &str, text: &str) -> Resp {
        Resp::html(html::layout(ctx, title, Tab::None, &html::empty(title, text), "")).status(status)
    }

    fn page(&self, ctx: &Ctx, title: &str, tab: Tab, body: &str) -> Resp {
        Resp::html(html::layout(ctx, title, tab, body, ""))
    }

    // -- Pages ----------------------------------------------------------------------------

    fn for_you(self: &Arc<Self>, ctx: &Ctx) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        let wl = watch_later_ids(&conn);
        let (items, sub, job) = if let Some(base) = &self.cfg.recommender {
            let mut picks = rec::picks(base, FOR_YOU_LIMIT).unwrap_or_default();
            db::add_progress(&conn, &mut picks)?;
            let hidden = Hidden::load(&conn);
            picks.retain(|it| hidden.allows(it));
            (picks, format!("From your recommender at {base}"), None)
        } else {
            let picks = db::rec_picks(&conn)?;
            let last = db::rec_last_run(&conn)?;
            let mut job = self.job(&ctx.profile, "foryou");
            if !job.running && job.error.is_none() && last.is_none_or(|t| db::now() - t > FOR_YOU_STALE) {
                self.start_job(&ctx.profile, "foryou");
                job = self.job(&ctx.profile, "foryou");
            }
            let sub = match last {
                Some(t) => format!("Picked for {} {}", ctx.profile, crate::fmt::age(t)),
                None => "Made on this computer from your searches and what you watch.".into(),
            };
            (picks, sub, Some(job))
        };
        if !ctx.incognito {
            self.report_shown(&conn, &items);
        }
        let running = job.as_ref().is_some_and(|j| j.running);
        let msg = job.as_ref().map(job_text).unwrap_or_default();
        let mut body = html::heading("For You", &sub, Some(("foryou", running, &msg)));
        if items.is_empty() {
            body += &html::empty(
                if running { "Finding videos for you…" } else { "Nothing here yet" },
                if running {
                    "This takes 10–30 seconds the first time."
                } else {
                    "Search for something or watch a few videos, then refresh."
                },
            );
        } else {
            body += &html::grid(&items, |it| CardOpts {
                in_watch_later: wl.contains(&it.id),
                note: true,
                hide_views: ctx.hide_views,
                ..Default::default()
            });
        }
        Ok(self.page(ctx, "For You", Tab::ForYou, &body))
    }

    /// Count the first picks as seen, once per run (picks you keep skipping sink).
    fn report_shown(&self, conn: &Connection, items: &[Item]) {
        let Some(run) = items.iter().find_map(|i| i.run_id) else { return };
        let ids: Vec<String> = items.iter().take(SHOWN).filter(|i| i.run_id == Some(run)).map(|i| i.id.clone()).collect();
        match self.cfg.recommender.clone() {
            Some(base) => {
                std::thread::spawn(move || rec::impressions(&base, run, &ids));
            }
            None => {
                let _ = db::mark_shown(conn, &ids, run);
            }
        }
    }

    fn subscriptions(self: &Arc<Self>, ctx: &Ctx) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        let hidden = Hidden::load(&conn);
        let mut items = db::feed(&conn, FEED_LIMIT)?;
        items.retain(|it| hidden.allows(it));
        let wl = watch_later_ids(&conn);
        let job = self.job(&ctx.profile, "feed");
        let channels: i64 = conn.query_row("SELECT COUNT(*) FROM channels WHERE subscribed = 1", [], |r| r.get(0))?;
        let last: Option<i64> = conn.query_row("SELECT MAX(last_fetch) FROM channels WHERE subscribed = 1", [], |r| r.get(0))?;
        let sub = match (channels, last.filter(|t| *t > 0)) {
            (0, _) => String::new(),
            (n, Some(t)) => format!("{n} channels · checked {}", crate::fmt::age(t)),
            (n, None) => format!("{n} channels"),
        };
        let mut body = html::heading("Subscriptions", &sub, Some(("feed", job.running, &job_text(&job))));
        if items.is_empty() {
            body += &html::empty(
                "No videos yet",
                if channels == 0 {
                    "Subscribe to channels from their page, or import your subscriptions in the app."
                } else {
                    "Refresh to check your channels for new videos."
                },
            );
        } else {
            body += &html::grid(&items, |it| CardOpts {
                in_watch_later: wl.contains(&it.id),
                hide_views: ctx.hide_views,
                ..Default::default()
            });
        }
        Ok(self.page(ctx, "Subscriptions", Tab::Subscriptions, &body))
    }

    fn search(self: &Arc<Self>, ctx: &Ctx, q: &str) -> Result<Resp> {
        let q = q.trim();
        if q.is_empty() {
            let body = html::heading("Search", "", None)
                + &html::empty("Search YouTube", "Type what you're looking for above, or paste a video link.");
            return Ok(Resp::html(html::layout(ctx, "Search", Tab::None, &body, "")));
        }
        if let Some(id) = crate::links::video_id_from(q) {
            return Ok(Resp::redirect(&format!("/watch?v={id}")));
        }
        let conn = self.conn(&ctx.profile)?;
        let results = crate::yt::search(q);
        if !ctx.incognito && ctx.writes && self.cfg.remember_searches {
            let _ = db::record_search(&conn, q);
        }
        let mut body = html::heading(&format!("Results for “{q}”"), "", None);
        match results {
            Ok(r) => {
                let hidden = Hidden::load(&conn);
                let mut items: Vec<Item> = r.videos.into_iter().map(Item::from).filter(|it| hidden.allows(it)).collect();
                db::add_progress(&conn, &mut items)?;
                let wl = watch_later_ids(&conn);
                body += &html::search_channels(&r.channels);
                if items.is_empty() {
                    body += &html::empty("No videos found", "Try other words.");
                } else {
                    body += &html::grid(&items, |it| CardOpts {
                        in_watch_later: wl.contains(&it.id),
                        hide_views: ctx.hide_views,
                        ..Default::default()
                    });
                }
            }
            Err(e) => body += &html::empty("YouTube didn't answer", &e.to_string()),
        }
        Ok(Resp::html(html::layout(ctx, &format!("{q} – Search"), Tab::None, &body, q)))
    }

    fn channel(self: &Arc<Self>, ctx: &Ctx, id: &str) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        let subscribed = db::is_subscribed(&conn, id)?;
        let known: Option<String> = conn.query_row("SELECT name FROM channels WHERE id = ?1", [id], |r| r.get(0)).ok();
        let (name, subs, list) = match crate::yt::channel_videos(id) {
            Ok(ch) => {
                let mut items: Vec<Item> = ch.videos.iter().cloned().map(Item::from).collect();
                db::add_progress(&conn, &mut items)?;
                if subscribed && !ctx.incognito {
                    let _ = db::store_videos(&conn, &ch.videos);
                }
                let wl = watch_later_ids(&conn);
                let g = html::grid(&items, |it| CardOpts {
                    in_watch_later: wl.contains(&it.id),
                    hide_views: ctx.hide_views,
                    ..Default::default()
                });
                (
                    Some(ch.name).filter(|n| !n.is_empty()).or(known).unwrap_or_else(|| "Channel".into()),
                    ch.subscribers.unwrap_or_default(),
                    g,
                )
            }
            Err(e) => (
                known.unwrap_or_else(|| "Channel".into()),
                String::new(),
                html::empty("No videos", &format!("This channel has no regular videos, or YouTube didn't answer ({e}).")),
            ),
        };
        let mut body = format!(r#"<div class="head"><div><h1>{}</h1><p class="lede">{}</p></div>"#, esc(&name), esc(&subs));
        if !ctx.incognito {
            body += &html::sub_button(id, &name, subscribed);
        }
        body += "</div>";
        body += &list;
        Ok(self.page(ctx, &name, Tab::None, &body))
    }

    fn library(self: &Arc<Self>, ctx: &Ctx) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        if !ctx.incognito {
            db::watch_later(&conn)?; // make sure it exists
        }
        let lists = db::playlists(&conn)?;
        let mut body = html::heading("Library", "", None);
        body += if lists.is_empty() {
            html::empty("No playlists", "Save a video to Watch later and it shows up here.")
        } else {
            html::playlist_rows(&lists)
        }
        .as_str();
        let blocked = db::blocked_channels(&conn)?;
        if !blocked.is_empty() {
            body += r#"<h2 class="section">Hidden channels</h2><ul class="rows hidden-chans">"#;
            for (id, name) in &blocked {
                let n = if name.is_empty() { id.as_str() } else { name.as_str() };
                body += &format!(
                    r#"<li><a href="/channel/{id}"><span><b>{n}</b></span></a>{btn}</li>"#,
                    id = esc(id),
                    n = esc(n),
                    btn = if ctx.incognito {
                        String::new()
                    } else {
                        format!(r#"<button class="btn" data-unblock="{}">Show again</button>"#, esc(id))
                    },
                );
            }
            body += "</ul>";
        }
        Ok(self.page(ctx, "Library", Tab::Library, &body))
    }

    fn playlist(self: &Arc<Self>, ctx: &Ctx, id: &str) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        let Some(pl) = db::playlists(&conn)?.into_iter().find(|p| p.id == id) else {
            return Ok(self.error_page(ctx, 404, "No such playlist", "It may have been deleted."));
        };
        let items: Vec<Item> = db::playlist_items(&conn, id)?
            .into_iter()
            .map(|(mut it, added)| {
                it.note = Some(format!("Saved {}", crate::fmt::age(added)));
                it
            })
            .collect();
        let wl = watch_later_ids(&conn);
        let n = if items.len() == 1 { "1 video".to_owned() } else { format!("{} videos", items.len()) };
        let mut body = html::heading(&pl.name, &n, None);
        if items.is_empty() {
            body += &html::empty("Nothing saved yet", "Use a video's ⋮ menu to save it here.");
        } else {
            body += &html::grid(&items, |it| CardOpts {
                in_watch_later: wl.contains(&it.id),
                playlist: Some(&pl.id),
                note: true,
                hide_views: ctx.hide_views,
            });
        }
        Ok(self.page(ctx, &pl.name, Tab::Library, &body))
    }

    fn history(self: &Arc<Self>, ctx: &Ctx) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        let items: Vec<Item> = db::history(&conn, HISTORY_LIMIT)?
            .into_iter()
            .map(|(mut it, at)| {
                it.note = Some(format!("Watched {}", crate::fmt::age(at)));
                it
            })
            .collect();
        let wl = watch_later_ids(&conn);
        let sub = if !self.cfg.remember_history {
            "History is turned off in Preferences."
        } else if ctx.incognito {
            "Incognito: what you watch now isn't added."
        } else {
            ""
        };
        let mut body = html::heading("History", sub, None);
        if items.is_empty() {
            body += &html::empty("Nothing watched yet", "Videos you watch show up here, with where you stopped.");
        } else {
            body += &html::grid(&items, |it| CardOpts {
                in_watch_later: wl.contains(&it.id),
                note: true,
                hide_views: ctx.hide_views,
                ..Default::default()
            });
        }
        Ok(self.page(ctx, "History", Tab::History, &body))
    }

    fn watch(self: &Arc<Self>, ctx: &Ctx, v: &str, run: &str) -> Result<Resp> {
        let Some(id) = Some(v).filter(|v| http::valid_video_id(v)).map(str::to_owned).or_else(|| crate::links::video_id_from(v)) else {
            return Ok(self.error_page(ctx, 404, "No such video", "That doesn't look like a YouTube video id."));
        };
        self.resolver.prefetch(&id);
        let conn = self.conn(&ctx.profile)?;
        if let (Ok(run), false, true) = (run.parse::<i64>(), ctx.incognito, ctx.writes) {
            // A For You pick was opened: that's the strongest "yes".
            match self.cfg.recommender.clone() {
                Some(base) => {
                    let id = id.clone();
                    std::thread::spawn(move || rec::click(&base, run, &id));
                }
                None => {
                    let _ = db::mark_clicked(&conn, &id);
                }
            }
        }
        let w = self.watch_info(ctx, &conn, &id, self.resolver.peek(&id).as_deref())?;
        let body = html::watch_body(&w.borrow(), ctx);
        let title = if w.title.is_empty() { "Watch".to_owned() } else { w.title.clone() };
        Ok(self.page(ctx, &title, Tab::None, &body))
    }

    /// What we know about a video: from the database, improved by yt-dlp's answer when there is one.
    fn watch_info(&self, ctx: &Ctx, conn: &Connection, id: &str, r: Option<&stream::Resolved>) -> Result<OwnedWatch> {
        let mut w = OwnedWatch { id: id.to_owned(), ..Default::default() };
        if let Some(k) = lookup(conn, id) {
            (w.title, w.channel, w.channel_id, w.published, w.views, w.length) =
                (k.title, k.channel, k.channel_id, k.published, k.views, k.length);
        }
        if let Some(r) = r {
            if !r.title.is_empty() {
                w.title = r.title.clone();
            }
            if !r.channel.is_empty() {
                w.channel = r.channel.clone();
            }
            if http::valid_channel_id(&r.channel_id) {
                w.channel_id = r.channel_id.clone();
            }
            w.views = r.views.or(w.views);
            w.published = r.timestamp.or(w.published);
            w.length = r.duration.or(w.length);
            w.description = r.description.clone();
        }
        if !http::valid_channel_id(&w.channel_id) {
            w.channel_id.clear();
        }
        let saved = db::position(conn, id)?;
        w.watched = saved.is_some();
        w.start = saved.map_or(0.0, |(p, l)| crate::player::resume_from(p, l.or(w.length)));
        if w.length.is_none() {
            w.length = saved.and_then(|s| s.1);
        }
        w.save = !ctx.incognito && self.cfg.remember_history;
        w.subscribed = !w.channel_id.is_empty() && db::is_subscribed(conn, &w.channel_id)?;
        w.in_watch_later = watch_later_ids(conn).contains(id);
        Ok(w)
    }

    // -- JSON and fragments ---------------------------------------------------------------

    /// Details from yt-dlp (it is asked anyway to find the stream), with the info block re-rendered.
    fn info(&self, ctx: &Ctx, id: &str) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        match self.resolver.get(id) {
            Ok(r) => {
                let w = self.watch_info(ctx, &conn, id, Some(&r))?;
                Ok(Resp::json(json!({
                    "ok": true,
                    "title": w.title,
                    "channel": w.channel,
                    "channel_id": w.channel_id,
                    "length": w.length,
                    "height": r.height,
                    "html": html::watch_info(&w.borrow(), ctx),
                })))
            }
            Err(e) => Ok(Resp::json(json!({"ok": false, "error": e.to_string()}))),
        }
    }

    fn related(&self, ctx: &Ctx, id: &str) -> Result<Resp> {
        let conn = self.conn(&ctx.profile)?;
        let html = match crate::yt::related(id) {
            Ok(v) => {
                let hidden = Hidden::load(&conn);
                let mut items: Vec<Item> = v.into_iter().map(Item::from).filter(|it| hidden.allows(it) && it.id != id).collect();
                db::add_progress(&conn, &mut items)?;
                let wl = watch_later_ids(&conn);
                if items.is_empty() {
                    r#"<p class="lede">No related videos.</p>"#.to_owned()
                } else {
                    items
                        .iter()
                        .map(|it| {
                            html::card(
                                it,
                                CardOpts { in_watch_later: wl.contains(&it.id), hide_views: ctx.hide_views, ..Default::default() },
                            )
                        })
                        .collect()
                }
            }
            Err(e) => format!(r#"<p class="lede">YouTube didn't answer: {}</p>"#, esc(&e.to_string())),
        };
        Ok(Resp::html(html))
    }

    fn progress(&self, ctx: &Ctx, req: &Req) -> Result<Resp> {
        let f = req.fields();
        let id = f.get("id").map(String::as_str).unwrap_or("");
        let pos: f64 = f
            .get("position")
            .and_then(|p| p.parse().ok())
            .filter(|p: &f64| p.is_finite() && *p >= 0.0)
            .ok_or_else(|| anyhow!("bad position"))?;
        let len: Option<f64> = f.get("length").and_then(|p| p.parse().ok()).filter(|l: &f64| l.is_finite() && *l > 0.0);
        if !http::valid_video_id(id) {
            return Ok(Resp::json(json!({"error": "bad id"})).status(400));
        }
        if ctx.incognito || !self.cfg.remember_history || db::incognito() {
            return Ok(Resp::json(json!({"saved": false})));
        }
        let conn = self.conn(&ctx.profile)?;
        db::save_progress(&conn, id, pos, len)?;
        // Videos that aren't in the feed (search, related) would be listed by their id: name them.
        let (title, channel, cid) = match self.resolver.peek(id) {
            Some(r) => (r.title.clone(), r.channel.clone(), r.channel_id.clone()),
            None => (
                f.get("title").cloned().unwrap_or_default(),
                f.get("channel").cloned().unwrap_or_default(),
                f.get("cid").cloned().unwrap_or_default(),
            ),
        };
        if !title.is_empty() {
            conn.execute(
                "UPDATE history SET title = ?2, author = COALESCE(author, NULLIF(?3, '')), author_id = COALESCE(author_id, NULLIF(?4, ''))
                 WHERE video_id = ?1 AND (title = video_id OR author IS NULL)",
                rusqlite::params![id, title, channel, if http::valid_channel_id(&cid) { cid.as_str() } else { "" }],
            )?;
        }
        Ok(Resp::json(json!({"saved": true})))
    }

    fn action(self: &Arc<Self>, ctx: &Ctx, req: &Req) -> Result<Resp> {
        if ctx.incognito {
            return Ok(Resp::json(json!({"error": "Incognito: nothing is saved in this browser"})).status(403));
        }
        let f = req.fields();
        let get = |k: &str| f.get(k).cloned().unwrap_or_default();
        let (act, id, cid, name) = (get("act"), get("id"), get("cid"), get("channel"));
        let cid = if http::valid_channel_id(&cid) { cid } else { String::new() };
        let it = Item {
            id: id.clone(),
            title: get("title").chars().take(300).collect(),
            channel: name.chars().take(200).collect(),
            channel_id: cid.clone(),
            length: get("len").parse::<f64>().ok().filter(|l| l.is_finite() && *l > 0.0),
            ..Default::default()
        };
        let needs_video = matches!(act.as_str(), "watch_later" | "watched" | "unwatched" | "more" | "not_interested" | "remove");
        if needs_video && !http::valid_video_id(&id) {
            return Ok(Resp::json(json!({"error": "bad video id"})).status(400));
        }
        let needs_channel = matches!(act.as_str(), "block" | "unblock" | "subscribe" | "unsubscribe");
        if needs_channel && cid.is_empty() {
            return Ok(Resp::json(json!({"error": "bad channel id"})).status(400));
        }
        let conn = self.conn(&ctx.profile)?;
        let who = if it.channel.is_empty() { "this channel".to_owned() } else { it.channel.clone() };
        let out = match act.as_str() {
            "watch_later" => {
                let wl = db::watch_later(&conn)?;
                let saved = db::in_playlist(&conn, &wl, &id)?;
                if saved {
                    db::remove_from_playlist(&conn, &wl, &id)?
                } else {
                    db::add_to_playlist(&conn, &wl, &it)?
                }
                json!({"ok": true, "on": !saved, "msg": if saved { "Removed from Watch later" } else { "Saved to Watch later" }})
            }
            "watched" | "unwatched" => {
                db::set_watched(&conn, &id, act == "watched")?;
                json!({"ok": true, "on": act == "watched", "msg": if act == "watched" { "Marked as watched" } else { "Marked as unwatched" }})
            }
            "more" | "not_interested" => {
                db::add_feedback(&conn, &it, &act)?;
                if let Some(base) = self.cfg.recommender.clone() {
                    let kind = if act == "more" { rec::Feedback::More } else { rec::Feedback::NotInterested };
                    let id = id.clone();
                    std::thread::spawn(move || rec::feedback(&base, &id, kind));
                }
                if act == "more" {
                    json!({"ok": true, "msg": "Got it — more like this on For You"})
                } else {
                    json!({"ok": true, "hide": true, "msg": "Hidden. You'll see less like this"})
                }
            }
            "block" => {
                db::block_channel(&conn, &cid, &it.channel)?;
                json!({"ok": true, "hide_channel": cid, "msg": format!("{who} won't show up anymore")})
            }
            "unblock" => {
                db::unblock_channel(&conn, &cid)?;
                json!({"ok": true, "msg": "The channel shows up again"})
            }
            "subscribe" | "unsubscribe" => {
                let on = act == "subscribe";
                db::set_subscribed(&conn, &cid, &it.channel, on)?;
                if on {
                    // Fill the feed with the channel's videos right away, as the app does.
                    let (app, profile, cid) = (self.clone(), ctx.profile.clone(), cid.clone());
                    std::thread::spawn(move || {
                        if let (Ok(ch), Ok(c)) = (crate::yt::channel_videos(&cid), app.conn(&profile)) {
                            let _ = db::store_videos(&c, &ch.videos);
                        }
                    });
                }
                json!({"ok": true, "on": on, "msg": if on { format!("Subscribed to {who}") } else { format!("Unsubscribed from {who}") }})
            }
            "remove" => {
                let pl = get("pl");
                if !http::valid_playlist_id(&pl) {
                    return Ok(Resp::json(json!({"error": "bad playlist id"})).status(400));
                }
                db::remove_from_playlist(&conn, &pl, &id)?;
                json!({"ok": true, "hide": true, "msg": "Removed from the playlist"})
            }
            _ => return Ok(Resp::json(json!({"error": "unknown action"})).status(400)),
        };
        Ok(Resp::json(out))
    }

    fn refresh(self: &Arc<Self>, ctx: &Ctx, req: &Req) -> Resp {
        let what = req.fields().get("what").cloned().unwrap_or_default();
        let kind = match what.as_str() {
            "foryou" if self.cfg.recommender.is_some() => return Resp::json(json!({"reload": true})),
            "foryou" => "foryou",
            "feed" => "feed",
            _ => return Resp::json(json!({"error": "unknown refresh"})).status(400),
        };
        self.start_job(&ctx.profile, kind);
        self.status(ctx, kind)
    }

    fn status(&self, ctx: &Ctx, what: &str) -> Resp {
        let kind = match what {
            "foryou" => "foryou",
            "feed" => "feed",
            _ => return Resp::json(json!({"error": "unknown job"})).status(400),
        };
        let j = self.job(&ctx.profile, kind);
        Resp::json(json!({"running": j.running, "message": job_text(&j), "error": j.error, "finished": j.finished}))
    }

    fn job(&self, profile: &str, kind: &'static str) -> Job {
        self.jobs.lock().unwrap().get(&(profile.to_owned(), kind)).cloned().unwrap_or_default()
    }

    fn set_job(&self, profile: &str, kind: &'static str, f: impl FnOnce(&mut Job)) {
        f(self.jobs.lock().unwrap().entry((profile.to_owned(), kind)).or_default());
    }

    /// Run a feed or For You refresh for a profile in the background (once at a time per profile and kind).
    fn start_job(self: &Arc<Self>, profile: &str, kind: &'static str) {
        {
            let mut jobs = self.jobs.lock().unwrap();
            let j = jobs.entry((profile.to_owned(), kind)).or_default();
            if j.running {
                return;
            }
            *j = Job { running: true, message: "Starting…".into(), ..Default::default() };
        }
        let (app, profile) = (self.clone(), profile.to_owned());
        std::thread::spawn(move || {
            let guard = match app.refresh_lock.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    app.set_job(&profile, kind, |j| j.message = "Waiting for another refresh to finish…".into());
                    app.refresh_lock.lock().unwrap_or_else(|e| e.into_inner())
                }
            };
            // feed::refresh and foryou::refresh open the database of the process-wide profile.
            db::set_profile(Some(profile.clone()));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
                if kind == "foryou" {
                    let s = crate::foryou::refresh(&app.cfg, |m| app.set_job(&profile, kind, |j| j.message = m.to_owned()))?;
                    Ok(match s.ai_error {
                        Some(e) => format!("{} picks (without AI: {e})", s.picks),
                        None => format!("{} picks", s.picks),
                    })
                } else {
                    let (a, p) = (app.clone(), profile.clone());
                    let last = Arc::new(Mutex::new(String::new()));
                    let l2 = last.clone();
                    crate::feed::refresh(move |pr| {
                        let m = match pr {
                            crate::feed::Progress::Rss { done, total } => format!("Checking channels… {done} of {total}"),
                            crate::feed::Progress::Fallback { done, total } => {
                                format!("Asking yt-dlp about {total} channels… {done} of {total}")
                            }
                            crate::feed::Progress::Finished { videos, failed } => {
                                let m = if failed > 0 {
                                    format!("{videos} videos ({failed} channels failed)")
                                } else {
                                    format!("{videos} videos")
                                };
                                *l2.lock().unwrap() = m.clone();
                                m
                            }
                        };
                        a.set_job(&p, "feed", |j| j.message = m);
                    })?;
                    Ok(last.lock().unwrap().clone())
                }
            }));
            db::set_profile(Some(app.default_profile.clone()));
            drop(guard);
            let (msg, err) = match r {
                Ok(Ok(m)) => (format!("Done: {m}"), None),
                Ok(Err(e)) => (String::new(), Some(e.to_string())),
                Err(_) => (String::new(), Some("the refresh crashed".to_owned())),
            };
            app.set_job(&profile, kind, |j| *j = Job { running: false, message: msg, error: err, finished: Some(db::now()) });
        });
    }

    fn prefs(&self, ctx: &Ctx, req: &Req) -> Resp {
        let f = req.fields();
        let mut cookies = Vec::new();
        let mut to = "/settings";
        if let Some(p) = f.get("profile").filter(|p| ctx.profiles.contains(p)) {
            cookies.push(http::set_cookie(COOKIE_PROFILE, &http::cookie_encode(p), Some(400 * 86400)));
            to = "/";
        }
        match f.get("incognito").map(String::as_str) {
            Some("on") => cookies.push(http::set_cookie(COOKIE_INCOGNITO, "1", Some(400 * 86400))),
            Some("off") => cookies.push(http::clear_cookie(COOKIE_INCOGNITO)),
            _ => {}
        }
        cookies.iter().fold(Resp::redirect(to), |r, c| r.header("Set-Cookie", c))
    }

    fn login(&self, req: &Req) -> Result<Resp> {
        let pw = &self.cfg.web_password;
        if pw.is_empty() {
            return Ok(Resp::redirect("/"));
        }
        if req.method != "POST" {
            return Ok(Resp::html(html::login_page("", &http::safe_next(req.q("next")))));
        }
        let f = req.fields();
        let next = http::safe_next(f.get("next").map(String::as_str).unwrap_or("/"));
        // A limit per address: one device guessing can't lock everyone else out.
        {
            let mut fails = self.login_fails.lock().unwrap();
            fails.retain(|_, (_, since)| since.elapsed() < LOGIN_WINDOW);
            if fails.get(&req.peer).is_some_and(|(n, _)| *n >= LOGIN_TRIES) {
                return Ok(Resp::html(html::login_page("Too many wrong passwords. Try again in a few minutes.", &next)).status(429));
            }
        }
        let given = f.get("password").map(String::as_str).unwrap_or("");
        if crate::config::password_matches(pw, given) {
            self.login_fails.lock().unwrap().remove(&req.peer);
            let token = self.sessions.create()?;
            return Ok(Resp::redirect(&next).header("Set-Cookie", &http::set_cookie(COOKIE_SESSION, &token, Some(SESSION_DAYS * 86400))));
        }
        self.login_fails.lock().unwrap().entry(req.peer).or_insert((0, Instant::now())).0 += 1;
        Ok(Resp::html(html::login_page("That's not the password.", &next)).status(401))
    }

    fn thumb(&self, id: &str) -> Resp {
        let path = crate::thumbs::path(id);
        if !path.exists() {
            self.thumbs.request(id);
            let deadline = Instant::now() + Duration::from_secs(8);
            let (m, cv) = &*self.thumb_done;
            let mut n = m.lock().unwrap();
            while !path.exists() && Instant::now() < deadline {
                n = cv.wait_timeout(n, Duration::from_millis(250)).unwrap().0;
            }
        }
        match std::fs::read(&path) {
            // Behind a password, shared caches must not keep them.
            Ok(b) => Resp::new(200, "image/jpeg", b).header(
                "Cache-Control",
                if self.cfg.web_password.is_empty() { "public, max-age=604800" } else { "private, max-age=604800" },
            ),
            Err(_) => Resp::new(
                404,
                "image/svg+xml",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 9"><rect width="16" height="9" fill="#8885"/></svg>"##,
            )
            .header("Cache-Control", "public, max-age=300"),
        }
    }

    fn stream(&self, id: &str, range: Option<&str>) -> Resp {
        match stream::relay(&self.resolver, id, range) {
            Ok(r) => Resp {
                status: r.status,
                headers: r.headers.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
                body: Body::Stream(r.body, r.length),
            },
            Err(e) => Resp::text(502, &format!("Can't play this video: {e}")),
        }
    }
}

fn static_file(name: &str) -> Resp {
    let (ctype, body): (&str, &str) = match name {
        "style.css" => ("text/css; charset=utf-8", STYLE),
        "app.js" => ("text/javascript; charset=utf-8", SCRIPT),
        "icon.svg" => ("image/svg+xml", html::LOGO),
        "manifest.webmanifest" => ("application/manifest+json", MANIFEST),
        _ => return Resp::text(404, "Not found"),
    };
    Resp::new(200, ctype, body).header("Cache-Control", "public, max-age=86400")
}

fn job_text(j: &Job) -> String {
    match &j.error {
        Some(e) => format!("Refresh failed: {e}"),
        None => j.message.clone(),
    }
}

/// Owned copy of the watch page's data.
#[derive(Default)]
struct OwnedWatch {
    id: String,
    title: String,
    channel: String,
    channel_id: String,
    published: Option<i64>,
    views: Option<i64>,
    length: Option<f64>,
    description: String,
    start: f64,
    save: bool,
    subscribed: bool,
    in_watch_later: bool,
    watched: bool,
}

impl OwnedWatch {
    fn borrow(&self) -> html::WatchInfo<'_> {
        html::WatchInfo {
            id: &self.id,
            title: &self.title,
            channel: &self.channel,
            channel_id: &self.channel_id,
            published: self.published,
            views: self.views,
            start: self.start,
            save: self.save,
            subscribed: self.subscribed,
            in_watch_later: self.in_watch_later,
            watched: self.watched,
            length: self.length,
            description: &self.description,
        }
    }
}

struct Known {
    title: String,
    channel: String,
    channel_id: String,
    published: Option<i64>,
    views: Option<i64>,
    length: Option<f64>,
}

/// A video's title and channel from wherever this profile has seen it.
fn lookup(conn: &Connection, id: &str) -> Option<Known> {
    conn.query_row(
        "SELECT title, channel, channel_id, published, views, length FROM (
             SELECT v.title, c.name AS channel, v.channel_id, v.published, v.views, v.length, 1 AS pri
               FROM videos v LEFT JOIN channels c ON c.id = v.channel_id WHERE v.id = ?1
             UNION ALL SELECT title, channel, channel_id, published, views, length, 2 FROM rec_picks WHERE video_id = ?1
             UNION ALL SELECT title, author, author_id, NULL, NULL, length, 3 FROM playlist_items WHERE video_id = ?1
             UNION ALL SELECT title, author, author_id, NULL, NULL, length, 4 FROM history WHERE video_id = ?1 AND title != video_id
         ) ORDER BY pri LIMIT 1",
        [id],
        |r| {
            Ok(Known {
                title: r.get(0)?,
                channel: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                channel_id: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                published: r.get(3)?,
                views: r.get(4)?,
                length: r.get(5)?,
            })
        },
    )
    .ok()
}

fn watch_later_ids(conn: &Connection) -> HashSet<String> {
    let Ok(mut st) =
        conn.prepare("SELECT i.video_id FROM playlist_items i JOIN playlists p ON p.id = i.playlist_id WHERE p.name = ?1 COLLATE NOCASE")
    else {
        return HashSet::new();
    };
    st.query_map([db::WATCH_LATER], |r| r.get(0)).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

/// Not-interested videos and hidden channels, left out of the feed, search and related.
struct Hidden {
    videos: HashSet<String>,
    channels: HashSet<String>,
}

impl Hidden {
    fn load(conn: &Connection) -> Self {
        Hidden {
            videos: db::rejected_videos(conn).unwrap_or_default().into_iter().collect(),
            channels: db::blocked_channels(conn).unwrap_or_default().into_iter().map(|(id, _)| id).collect(),
        }
    }
    fn allows(&self, it: &Item) -> bool {
        !self.videos.contains(&it.id) && !self.channels.contains(&it.channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions() {
        let s = Sessions::new(Duration::from_secs(60));
        let a = s.create().unwrap();
        let b = s.create().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(s.check(&a) && s.check(&b));
        assert!(!s.check(""));
        assert!(!s.check(&"0".repeat(64)));
        assert!(!s.check(&a[..63]));
        s.remove(&a);
        assert!(!s.check(&a));
        assert!(s.check(&b));
        let expired = Sessions::new(Duration::ZERO);
        let t = expired.create().unwrap();
        assert!(!expired.check(&t));
    }

    #[test]
    fn browser_profile_cookies() {
        let profiles = vec!["Default".to_owned(), "Kid's".to_owned()];
        let c = http::parse_cookies("yt_profile=Kid%27s; yt_incognito=1");
        assert_eq!(browser_prefs(&c, &profiles, "Default"), ("Kid's".to_owned(), true));
        let c = http::parse_cookies("yt_profile=..%2F..%2Fetc");
        assert_eq!(browser_prefs(&c, &profiles, "Default"), ("Default".to_owned(), false));
        let c = http::parse_cookies("yt_incognito=0");
        assert_eq!(browser_prefs(&c, &profiles, "Kid's"), ("Kid's".to_owned(), false));
        assert!(db_path("Default").ends_with("yourtube/yourtube.db"));
        assert!(db_path("Kid's").ends_with("yourtube/profiles/Kid's/yourtube.db"));
    }

    #[test]
    fn watch_page_parts() {
        let ctx = Ctx {
            profile: "Default".into(),
            profiles: vec![],
            incognito: false,
            forced_incognito: false,
            password: false,
            hide_views: false,
            writes: true,
        };
        let w = OwnedWatch {
            id: "jNQXAC9IVRw".into(),
            title: "Me at the zoo".into(),
            channel: "jawed".into(),
            channel_id: "UC4QobU6STFB0P71PMvOGN5A".into(),
            start: 12.0,
            save: true,
            ..Default::default()
        };
        let b = html::watch_info(&w.borrow(), &ctx);
        assert!(b.starts_with(r#"<h1 class="v-title">Me at the zoo</h1>"#), "{b}");
        assert!(b.contains(r#"data-sub="UC4QobU6STFB0P71PMvOGN5A""#));
        assert!(!b.contains("<video"));
        let page = html::watch_body(&w.borrow(), &ctx);
        assert!(page.contains(r#"src="/stream/jNQXAC9IVRw""#) && page.contains(r#"data-start="12""#) && page.contains(r#"data-save="1""#));
        let incog = Ctx { incognito: true, ..ctx };
        let page = html::watch_body(&OwnedWatch { save: false, ..w }.borrow(), &incog);
        assert!(!page.contains("data-sub=") && !page.contains("data-act=") && page.contains(r#"data-save="""#));
    }

    #[test]
    fn address_override_and_urls() {
        let u = urls("127.0.0.1:8723".parse().unwrap());
        assert_eq!(u, vec!["http://127.0.0.1:8723/"]);
        let u = urls("0.0.0.0:8723".parse().unwrap());
        assert!(u[0].starts_with("http://127.0.0.1:8723/"));
    }
}
