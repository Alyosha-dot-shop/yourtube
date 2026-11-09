//! Server-rendered pages. Everything that comes from YouTube or the database goes through `esc`.

use super::http::{esc, url_encode};
use crate::db::Item;
use crate::fmt;

/// What every page needs to know about the browser asking.
pub struct Ctx {
    pub profile: String,
    pub profiles: Vec<String>,
    /// This browser is incognito (or the whole server runs with --incognito).
    pub incognito: bool,
    /// The server was started with --incognito: the toggle can't be turned off.
    pub forced_incognito: bool,
    /// A password is set (so there's a Log out button).
    pub password: bool,
    pub hide_views: bool,
    /// The request came from our own pages, a bookmark or the address bar, so it may record
    /// searches and clicks. A link from another site still shows the page but records nothing.
    pub writes: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    ForYou,
    Subscriptions,
    Library,
    History,
    None,
}

pub const LOGO: &str = include_str!("../../data/icons/io.github.yourtube.YourTube.svg");

fn icon(name: &str) -> &'static str {
    match name {
        "foryou" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3l1.9 4.6L18.5 9l-4.6 1.9L12 15.5l-1.9-4.6L5.5 9l4.6-1.4z"/><path d="M18 14l.9 2.1L21 17l-2.1.9L18 20l-.9-2.1L15 17l2.1-.9z"/></svg>"#
        }
        "subs" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="7" width="18" height="13" rx="2.5"/><path d="M6 4h12M10 11v5l4-2.5z"/></svg>"#
        }
        "library" => r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5h11M4 10h11M4 15h7M17 13v8l4-2.5-4-2.5"/></svg>"#,
        "history" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 12a8.5 8.5 0 1 0 2.6-6.1L3.5 8.5"/><path d="M3.5 3.5v5h5M12 7.5V12l3 2"/></svg>"#
        }
        "search" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5L21 21"/></svg>"#
        }
        "more" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="5" r="1.6"/><circle cx="12" cy="12" r="1.6"/><circle cx="12" cy="19" r="1.6"/></svg>"#
        }
        "refresh" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20 12a8 8 0 1 1-2.3-5.7L20 8.5"/><path d="M20 3.5v5h-5"/></svg>"#
        }
        "incognito" => {
            r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 11h18M6 11l2-6h8l2 6"/><circle cx="7.5" cy="16" r="3"/><circle cx="16.5" cy="16" r="3"/><path d="M10.5 16h3"/></svg>"#
        }
        "back" => r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M15 4.5L7.5 12l7.5 7.5"/></svg>"#,
        "play" => {
            r#"<svg class="i-play" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5.6v12.8a1 1 0 0 0 1.5.86l10.4-6.4a1 1 0 0 0 0-1.72L9.5 4.74A1 1 0 0 0 8 5.6z" fill="currentColor" stroke="none"/></svg>"#
        }
        "pause" => {
            r#"<svg class="i-pause" viewBox="0 0 24 24" aria-hidden="true"><rect x="6.5" y="5" width="4" height="14" rx="1.2" fill="currentColor" stroke="none"/><rect x="13.5" y="5" width="4" height="14" rx="1.2" fill="currentColor" stroke="none"/></svg>"#
        }
        _ => "",
    }
}

/// The four sections, in the order of the tab bar.
const TABS: [(Tab, &str, &str, &str); 4] = [
    (Tab::ForYou, "/", "For You", "foryou"),
    (Tab::Subscriptions, "/subscriptions", "Subscriptions", "subs"),
    (Tab::Library, "/library", "Library", "library"),
    (Tab::History, "/history", "History", "history"),
];

/// A stand-in card while videos load: grey bars with a moving shine.
const SKELETON_CARD: &str = r#"<div class="card sk" aria-hidden="true"><span class="thumb"></span><div class="meta"><span class="bone"></span><span class="bone mid"></span><span class="bone short"></span></div></div>"#;

pub fn layout(ctx: &Ctx, title: &str, tab: Tab, body: &str, query: &str) -> String {
    let nav = TABS
        .iter()
        .map(|(t, href, label, ic)| {
            let cur = if *t == tab { r#" aria-current="page""# } else { "" };
            format!(r#"<a href="{href}"{cur}>{}<span>{label}</span></a>"#, icon(ic))
        })
        .collect::<String>();
    // The pill behind the current tab; pages outside the four tabs have none.
    let current = TABS.iter().position(|(t, ..)| *t == tab).map(|i| i.to_string()).unwrap_or_default();
    let initial = ctx.profile.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    let badge = if ctx.incognito {
        format!(r#"<a class="incog" href="/settings" title="Incognito: nothing is saved">{}<span>Incognito</span></a>"#, icon("incognito"))
    } else {
        String::new()
    };
    // The four tabs start at the logo; every other page (a video, a search, a channel) has a Back button.
    let lead = if tab == Tab::None {
        format!(r#"<a class="back" href="/" data-back aria-label="Back">{}<span>Back</span></a>"#, icon("back"))
    } else {
        format!(r#"<a class="brand" href="/" aria-label="YourTube home"><span class="tile">{}</span><b>YourTube</b></a>"#, icon("play"))
    };
    format!(
        r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" media="(prefers-color-scheme: light)" content="#faf8f6">
<meta name="theme-color" media="(prefers-color-scheme: dark)" content="#141211">
<meta name="apple-mobile-web-app-status-bar-style" content="default">
<meta name="referrer" content="no-referrer">
<title>{title} · YourTube</title>
<link rel="icon" href="/static/icon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/static/apple-touch-icon.png">
<meta name="apple-mobile-web-app-title" content="YourTube">
<link rel="manifest" href="/static/manifest.webmanifest">
<link rel="stylesheet" href="/static/style.css?v={ver}">
<script src="/static/app.js?v={ver}" defer blocking="render"></script>
</head>
<body{incog_class}>
<header class="top">
  {lead}
  <form class="search" action="/search" role="search">
    <input type="search" name="q" value="{q}" placeholder="Search YouTube" aria-label="Search YouTube" enterkeyhint="search" autocomplete="off">
    <button aria-label="Search">{search}</button>
  </form>
  {badge}
  <a class="avatar" href="/settings" title="Profile: {profile}" aria-label="Profile and privacy: {profile}">{initial}</a>
</header>
<nav class="tabs" data-tab="{current}" aria-label="Sections"><span class="pill" aria-hidden="true"></span>{nav}</nav>
<main>
{body}
</main>
<dialog id="sheet"><div class="sheet-title"></div><div class="sheet-actions"></div><button class="sheet-close" value="close">Close</button></dialog>
<div id="toast" role="status" aria-live="polite"></div>
</body></html>"##,
        title = esc(title),
        ver = super::asset_version(),
        incog_class = if ctx.incognito { r#" class="incognito""# } else { "" },
        q = esc(query),
        search = icon("search"),
        profile = esc(&ctx.profile),
        initial = esc(&initial),
    )
}

/// Per-card options.
#[derive(Default, Clone, Copy)]
pub struct CardOpts<'a> {
    pub in_watch_later: bool,
    /// The playlist this card is shown in (for "Remove from playlist").
    pub playlist: Option<&'a str>,
    pub hide_views: bool,
    /// Show the For You "why" note (or another note).
    pub note: bool,
    /// Live right now (search results): a LIVE badge instead of the length.
    pub live: bool,
    /// The card's place in its list. The first screenfuls rise in a wave by it.
    pub wave: Option<usize>,
}

/// Cards past this many appear without the entrance wave (they're off screen anyway).
const WAVE_CARDS: usize = 24;

pub fn card(it: &Item, o: CardOpts) -> String {
    let mut href = format!("/watch?v={}", it.id);
    if let Some(run) = it.run_id {
        href += &format!("&amp;run={run}");
    }
    // The bar shows once there's more than a few seconds to come back to, as in the app.
    let prog = match (it.position, it.length) {
        (Some(p), Some(l)) if l > 0.0 && p > 5.0 => {
            let pct = (p / l * 100.0).clamp(2.0, 100.0);
            format!(r#"<span class="prog"><i style="width:{pct:.1}%"></i></span>"#)
        }
        _ => String::new(),
    };
    let badge = if o.live {
        r#"<span class="dur live">LIVE</span>"#.to_owned()
    } else {
        it.length.filter(|l| *l > 0.0).map(|l| format!(r#"<span class="dur">{}</span>"#, fmt::clock(l))).unwrap_or_default()
    };
    let channel = match (it.channel.is_empty(), it.channel_id.is_empty()) {
        (true, _) => String::new(),
        (false, true) => format!(r#"<p class="ch">{}</p>"#, esc(&it.channel)),
        (false, false) => format!(r#"<p class="ch"><a href="/channel/{}">{}</a></p>"#, esc(&it.channel_id), esc(&it.channel)),
    };
    let mut sub: Vec<String> = Vec::new();
    // A live stream's "upload time" is just when we asked, so it says "Live now" instead.
    if o.live {
        sub.push("Live now".to_owned());
    }
    if let Some(v) = it.views.filter(|_| !o.hide_views) {
        sub.push(fmt::views(v));
    }
    if let Some(p) = it.published.filter(|p| *p > 0 && !o.live) {
        sub.push(fmt::age(p));
    }
    let sub = if sub.is_empty() { String::new() } else { format!(r#"<p class="sub">{}</p>"#, sub.join(" · ")) };
    let note = match (&it.note, o.note) {
        (Some(n), true) if !n.is_empty() => format!(r#"<p class="why">{}</p>"#, esc(n)),
        _ => String::new(),
    };
    let (wave, wave_at) = match o.wave.filter(|i| *i < WAVE_CARDS) {
        Some(i) => (" in", format!(r#" style="--i:{i}""#)),
        None => ("", String::new()),
    };
    format!(
        r#"<article class="card{done}{wave}"{wave_at} data-id="{id}" data-title="{title}" data-channel="{ch}" data-cid="{cid}" data-len="{len}" data-wl="{wl}" data-watched="{watched}" data-pl="{pl}">
<a class="thumb{has_prog}" href="{href}" tabindex="-1" aria-hidden="true"><img src="/thumb/{id}" alt="" loading="lazy" decoding="async">{prog}{badge}</a>
<div class="meta"><a class="title" href="{href}">{title}</a>{channel}{sub}{note}<button class="more" type="button" aria-label="More actions for {title}">{more}</button></div>
</article>"#,
        done = if it.finished() { " done" } else { "" },
        id = esc(&it.id),
        title = esc(&it.title),
        ch = esc(&it.channel),
        cid = esc(&it.channel_id),
        len = it.length.map(|l| format!("{l:.0}")).unwrap_or_default(),
        wl = if o.in_watch_later { "1" } else { "" },
        watched = if it.watched { "1" } else { "" },
        pl = o.playlist.map(esc).unwrap_or_default(),
        has_prog = if prog.is_empty() { "" } else { " has-prog" },
        more = icon("more"),
    )
}

pub fn grid<'a>(items: &[Item], o: impl Fn(&Item) -> CardOpts<'a>) -> String {
    let mut s = String::from(r#"<section class="grid">"#);
    for (i, it) in items.iter().enumerate() {
        s += &card(it, CardOpts { wave: Some(i), ..o(it) });
    }
    s += "</section>";
    s
}

pub fn empty(title: &str, text: &str) -> String {
    format!(r#"<div class="empty"><h2>{}</h2><p>{}</p></div>"#, esc(title), esc(text))
}

/// A page heading with an optional Refresh button and a status line (for background jobs).
pub fn heading(title: &str, sub: &str, refresh: Option<(&str, bool, &str)>) -> String {
    let (btn, job) = match refresh {
        Some((what, running, msg)) => (
            format!(
                r#"<button class="btn refresh" type="button" data-refresh="{what}"{dis}>{}<span>Refresh</span></button>"#,
                icon("refresh"),
                dis = if running { " disabled" } else { "" }
            ),
            format!(r#"<p class="status" data-job="{what}" data-running="{}">{}</p>"#, if running { "1" } else { "" }, esc(msg)),
        ),
        None => (String::new(), String::new()),
    };
    let sub = if sub.is_empty() { String::new() } else { format!(r#"<p class="lede">{}</p>"#, esc(sub)) };
    format!(r#"<div class="head"><div><h1>{}</h1>{sub}</div>{btn}</div>{job}"#, esc(title))
}

pub fn login_page(error: &str, next: &str) -> String {
    let err = if error.is_empty() { String::new() } else { format!(r#"<p class="error" role="alert">{}</p>"#, esc(error)) };
    format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark"><meta name="theme-color" media="(prefers-color-scheme: light)" content="#faf8f6">
<meta name="theme-color" media="(prefers-color-scheme: dark)" content="#141211"><meta name="apple-mobile-web-app-status-bar-style" content="default">
<meta name="referrer" content="no-referrer">
<title>Log in · YourTube</title><link rel="icon" href="/static/icon.svg" type="image/svg+xml"><link rel="apple-touch-icon" href="/static/apple-touch-icon.png">
<meta name="apple-mobile-web-app-title" content="YourTube"><link rel="manifest" href="/static/manifest.webmanifest">
<link rel="stylesheet" href="/static/style.css?v={ver}"></head>
<body class="login"><form class="login-box" method="post" action="/login">
{logo}<h1>YourTube</h1>{err}
<input type="hidden" name="next" value="{next}">
<label>Password<input type="password" name="password" autocomplete="current-password" autofocus required></label>
<button class="btn primary" type="submit">Log in</button>
</form></body></html>"##,
        ver = super::asset_version(),
        logo = LOGO
            .replace(r#"width="128" height="128""#, r#"width="72" height="72" aria-hidden="true""#)
            .replace("<title>YourTube</title>", ""),
        next = esc(next),
    )
}

pub fn settings_body(ctx: &Ctx) -> String {
    let mut s = heading("Profile and privacy", "These settings are kept in this browser only.", None);
    s += r#"<section class="panel"><h2>Profile</h2><p class="lede">Each profile has its own history, subscriptions and For You.</p><form method="post" action="/prefs" class="profiles">"#;
    for p in &ctx.profiles {
        let cur = p == &ctx.profile;
        s += &format!(
            r#"<button class="profile{}" name="profile" value="{v}"{dis}><span class="avatar">{i}</span>{v}</button>"#,
            if cur { " current" } else { "" },
            v = esc(p),
            i = esc(&p.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
            dis = if cur { r#" aria-current="true""# } else { "" },
        );
    }
    s += "</form></section>";
    s += r#"<section class="panel"><h2>Incognito</h2>"#;
    if ctx.forced_incognito {
        s += r#"<p class="lede">The server was started with <code>--incognito</code>: nothing anyone watches or searches is saved.</p>"#;
    } else {
        s += &format!(
            r#"<p class="lede">While incognito, nothing you do in this browser is saved: no history, no searches, no For You clicks, no changes to playlists or subscriptions.</p>
<form method="post" action="/prefs"><input type="hidden" name="incognito" value="{v}"><button class="btn{p}">{label}</button></form>"#,
            v = if ctx.incognito { "off" } else { "on" },
            p = if ctx.incognito { " primary" } else { "" },
            label = if ctx.incognito { "Turn incognito off" } else { "Turn incognito on" },
        );
    }
    s += "</section>";
    if ctx.password {
        s += r#"<section class="panel"><h2>Session</h2><form method="post" action="/logout"><button class="btn">Log out</button></form></section>"#;
    }
    s
}

pub struct WatchInfo<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub channel: &'a str,
    pub channel_id: &'a str,
    pub published: Option<i64>,
    pub views: Option<i64>,
    pub start: f64,
    pub save: bool,
    pub subscribed: bool,
    pub in_watch_later: bool,
    pub watched: bool,
    pub length: Option<f64>,
    pub description: &'a str,
    /// yt-dlp has answered, so the description (and its chapters) is as complete as it gets.
    pub loaded: bool,
}

/// Chapters listed on the video page at most (a few descriptions list hundreds of timestamps).
const MAX_CHAPTERS: usize = 60;

/// The part under the player (re-rendered by /api/info once yt-dlp has answered).
pub fn watch_info(w: &WatchInfo, ctx: &Ctx) -> String {
    let mut facts = Vec::new();
    if let Some(v) = w.views.filter(|_| !ctx.hide_views) {
        facts.push(fmt::views(v));
    }
    if let Some(p) = w.published.filter(|p| *p > 0) {
        facts.push(fmt::age(p));
    }
    if let Some(l) = w.length.filter(|l| *l > 0.0) {
        facts.push(fmt::clock(l));
    }
    let facts = if facts.is_empty() { String::new() } else { format!(r#"<p class="facts">{}</p>"#, facts.join(" · ")) };
    let channel = if w.channel_id.is_empty() {
        format!(r#"<span class="ch-name">{}</span>"#, esc(w.channel))
    } else {
        format!(r#"<a class="ch-name" href="/channel/{}">{}</a>"#, esc(w.channel_id), esc(w.channel))
    };
    let avatar = match w.channel.chars().next() {
        Some(c) => format!(r#"<span class="ch-av" aria-hidden="true">{}</span>"#, esc(&c.to_uppercase().to_string())),
        None => String::new(),
    };
    let subscribe =
        if w.channel_id.is_empty() || ctx.incognito { String::new() } else { sub_button(w.channel_id, w.channel, w.subscribed) };
    let play = format!(r#"<button class="play-pill" type="button" data-play>{}{}<span>Play</span></button>"#, icon("play"), icon("pause"));
    let actions = if ctx.incognito {
        format!(r#"<div class="chips">{play}</div><p class="lede">Incognito: nothing about this video is saved.</p>"#)
    } else {
        format!(
            r#"<div class="chips">{play}<button class="chip" data-act="watch_later" aria-pressed="{wl}">{wll}</button><button class="chip" data-act="{wact}">{wlabel}</button><button class="chip" data-act="more">More like this</button><button class="chip" data-act="not_interested">Not interested</button>{block}<button class="chip" data-act="copy">Copy link</button></div>"#,
            wl = w.in_watch_later,
            wll = if w.in_watch_later { "Saved to Watch later" } else { "Watch later" },
            wact = if w.watched { "unwatched" } else { "watched" },
            wlabel = if w.watched { "Mark as unwatched" } else { "Mark as watched" },
            block = if w.channel_id.is_empty() {
                String::new()
            } else {
                r#"<button class="chip" data-act="block">Don't show this channel</button>"#.to_owned()
            },
        )
    };
    let desc = w.description.trim();
    let about = if !w.loaded {
        // Until yt-dlp answers: what the database knows, and shining bars for the description.
        format!(
            r#"<div class="about loading" aria-busy="true">{facts}<span class="bone"></span><span class="bone"></span><span class="bone short"></span></div>"#
        )
    } else if desc.is_empty() && facts.is_empty() {
        String::new()
    } else if desc.is_empty() {
        format!(r#"<div class="about">{facts}</div>"#)
    } else {
        format!(
            r#"<div class="about">{facts}<p class="desc-text">{}</p><button class="more-text" type="button" aria-expanded="false" hidden>more</button></div>"#,
            esc(desc)
        )
    };
    let chapters = crate::video::description_chapters(desc);
    let chapters = if chapters.is_empty() {
        String::new()
    } else {
        let rows: String = chapters
            .iter()
            .take(MAX_CHAPTERS)
            .map(|c| {
                format!(
                    r#"<li><button class="chap" type="button" data-t="{:.0}"><span class="chap-t">{}</span><span class="chap-n">{}</span></button></li>"#,
                    c.start,
                    fmt::clock(c.start),
                    esc(&c.title)
                )
            })
            .collect();
        format!(r#"<section class="chapters"><h2>Chapters · {}</h2><ol>{rows}</ol></section>"#, chapters.len())
    };
    format!(
        r#"<h1 class="v-title">{title}</h1>
<div class="v-row">{avatar}<div class="v-ch">{channel}</div>{subscribe}</div>
{actions}
{about}
{chapters}"#,
        title = if w.title.is_empty() { "Loading…".to_owned() } else { esc(w.title) },
    )
}

/// `hls_key` unlocks this video's HLS addresses (see stream.rs).
pub fn watch_body(w: &WatchInfo, ctx: &Ctx, hls_key: &str) -> String {
    format!(
        r#"<div class="watch" data-id="{id}" data-title="{title}" data-channel="{ch}" data-cid="{cid}" data-len="{len}" data-start="{start:.0}" data-save="{save}">
<div class="player-col">
<div class="player-band"><div class="player"><video id="video" controls autoplay playsinline preload="auto" poster="/thumb/{id}"><source src="/hls/{id}/index.m3u8?k={key}" type="application/vnd.apple.mpegurl"><source src="/stream/{id}" type="video/mp4"></video><p class="player-msg" hidden></p></div></div>
<div class="v-info">{info}</div>
</div>
<aside class="related-col"><h2>Related</h2><div id="related" class="grid list" data-related="{id}" aria-busy="true"><p class="sr-only">Loading related videos…</p>{sk}{sk}{sk}</div></aside>
</div>"#,
        sk = SKELETON_CARD,
        id = esc(w.id),
        key = esc(hls_key),
        title = esc(w.title),
        ch = esc(w.channel),
        cid = esc(w.channel_id),
        len = w.length.map(|l| format!("{l:.0}")).unwrap_or_default(),
        start = w.start,
        save = if w.save { "1" } else { "" },
        info = watch_info(w, ctx),
    )
}

pub fn sub_button(channel_id: &str, name: &str, subscribed: bool) -> String {
    format!(
        r#"<button class="btn sub{on}" data-sub="{cid}" data-name="{name}" aria-pressed="{subscribed}">{label}</button>"#,
        on = if subscribed { "" } else { " primary" },
        cid = esc(channel_id),
        name = esc(name),
        label = if subscribed { "Subscribed" } else { "Subscribe" },
    )
}

pub fn search_channels(chans: &[crate::yt::FoundChannel]) -> String {
    if chans.is_empty() {
        return String::new();
    }
    let mut s = String::from(r#"<div class="chans">"#);
    for c in chans.iter().take(6) {
        s += &format!(
            r#"<a class="chan" href="/channel/{}"><span class="avatar">{}</span><span><b>{}</b><small>{}</small></span></a>"#,
            esc(&c.id),
            esc(&c.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
            esc(&c.name),
            esc(c.subscribers.as_deref().unwrap_or("Channel")),
        );
    }
    s += "</div>";
    s
}

pub fn playlist_rows(lists: &[crate::db::Playlist]) -> String {
    let mut s = String::from(r#"<ul class="rows">"#);
    for p in lists {
        let n = if p.count == 1 { "1 video".to_owned() } else { format!("{} videos", p.count) };
        s += &format!(
            r#"<li><a href="/playlist/{}"><span class="row-ic">{}</span><span><b>{}</b><small>{n}</small></span></a></li>"#,
            url_encode(&p.id),
            if p.name == crate::db::WATCH_LATER { icon("history") } else { icon("library") },
            esc(&p.name),
        );
    }
    s += "</ul>";
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            profile: "Default".into(),
            profiles: vec!["Default".into(), "<b>Kid</b>".into()],
            incognito: false,
            forced_incognito: false,
            password: false,
            hide_views: false,
            writes: true,
        }
    }

    #[test]
    fn cards_escape_everything() {
        let it = Item {
            id: "jNQXAC9IVRw".into(),
            title: "<img src=x onerror=alert(1)> \"quoted\"".into(),
            channel: "Evil & Co".into(),
            channel_id: "UC4QobU6STFB0P71PMvOGN5A".into(),
            length: Some(19.0),
            position: Some(9.5),
            note: Some("<script>".into()),
            run_id: Some(42),
            ..Default::default()
        };
        let html = card(&it, CardOpts { note: true, ..Default::default() });
        assert!(!html.contains("<img src=x"), "{html}");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt; &quot;quoted&quot;"));
        assert!(html.contains("Evil &amp; Co"));
        assert!(html.contains("/watch?v=jNQXAC9IVRw&amp;run=42"));
        assert!(html.contains(r#"<span class="dur">0:19</span>"#));
        assert!(html.contains("width:50.0%"));
        let page = layout(&ctx(), "<x>", Tab::ForYou, &html, "\"><script>");
        assert!(!page.contains("\"><script>"));
        assert!(page.contains(r#"aria-current="page""#));
        assert!(settings_body(&ctx()).contains("&lt;b&gt;Kid&lt;/b&gt;"));
        assert!(!login_page("<b>", "\"><x").contains("\"><x"));
    }

    #[test]
    fn card_badges_progress_and_wave() {
        let it = Item { id: "jNQXAC9IVRw".into(), title: "t".into(), length: Some(600.0), position: Some(4.0), ..Default::default() };
        // A few seconds watched: no bar yet, and the length badge stays put.
        let c = card(&it, CardOpts::default());
        assert!(!c.contains(r#"class="prog""#) && c.contains(r#"class="thumb""#) && c.contains(r#"<span class="dur">10:00</span>"#), "{c}");
        let c = card(&Item { position: Some(90.0), ..it.clone() }, CardOpts::default());
        assert!(c.contains(r#"class="thumb has-prog""#) && c.contains("width:15.0%"), "{c}");
        // Live: a LIVE badge instead of the length.
        let c = card(&it, CardOpts { live: true, ..Default::default() });
        assert!(c.contains(r#"<span class="dur live">LIVE</span>"#) && !c.contains("10:00") && c.contains("Live now"), "{c}");
        // Only the first cards of a list wave in, each with its place.
        let g = grid(&vec![it.clone(); 30], |_| CardOpts::default());
        assert!(g.contains(r#"class="card in" style="--i:0""#) && g.contains(r#"style="--i:23""#), "{g}");
        assert!(!g.contains(r#"style="--i:24""#));
        assert_eq!(g.matches(r#"class="card""#).count(), 6);
    }

    #[test]
    fn header_and_tabs() {
        let page = layout(&ctx(), "x", Tab::History, "", "");
        assert!(page.contains(r#"<nav class="tabs" data-tab="3""#) && page.contains(r#"class="brand""#) && !page.contains("data-back"));
        // Pages outside the four tabs: no pill, and a Back button instead of the logo.
        let page = layout(&ctx(), "x", Tab::None, "", "");
        assert!(page.contains(r#"<nav class="tabs" data-tab="""#) && page.contains("data-back") && !page.contains(r#"class="brand""#));
        // The script runs before the first frame where browsers allow it (for the thumbnail flight back).
        assert!(page.contains(r#"defer blocking="render""#));
    }

    #[test]
    fn watch_info_about_and_chapters() {
        let mut w = WatchInfo {
            id: "jNQXAC9IVRw",
            title: "Clip",
            channel: "<i>Chan</i>",
            channel_id: "",
            published: None,
            views: Some(1500),
            start: 0.0,
            save: true,
            subscribed: false,
            in_watch_later: false,
            watched: false,
            length: Some(185.0),
            description: "",
            loaded: false,
        };
        let b = watch_info(&w, &ctx());
        assert!(b.contains(r#"<div class="about loading""#) && b.contains("1.5K views · 3:05"), "{b}");
        assert!(b.contains(r#"<span class="ch-av" aria-hidden="true">&lt;</span>"#) && b.contains("&lt;i&gt;Chan&lt;/i&gt;"));
        assert!(b.contains("data-play"));
        w.loaded = true;
        w.description = "Parts:\n0:00 Start <b>\n1:05 Middle & more\n2:30 End";
        let b = watch_info(&w, &ctx());
        assert!(!b.contains("loading") && b.contains(r#"<p class="desc-text">Parts:"#), "{b}");
        assert!(
            b.contains("Chapters · 3") && b.contains(r#"data-t="65""#) && b.contains("Middle &amp; more") && b.contains("Start &lt;b&gt;")
        );
        assert!(!b.contains("<b>"));
        // No description and no facts: no card at all.
        let b = watch_info(&WatchInfo { views: None, length: None, description: "", ..w }, &ctx());
        assert!(!b.contains("about") && !b.contains("chapters"), "{b}");
    }
}
