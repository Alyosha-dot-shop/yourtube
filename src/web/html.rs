//! Server-rendered pages. Everything that comes from YouTube or the database goes through `esc`.

use super::http::{esc, url_encode};
use crate::db::Item;
use crate::ui::fmt;

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
        _ => "",
    }
}

pub fn layout(ctx: &Ctx, title: &str, tab: Tab, body: &str, query: &str) -> String {
    let nav = [
        (Tab::ForYou, "/", "For You", "foryou"),
        (Tab::Subscriptions, "/subscriptions", "Subscriptions", "subs"),
        (Tab::Library, "/library", "Library", "library"),
        (Tab::History, "/history", "History", "history"),
    ]
    .iter()
    .map(|(t, href, label, ic)| {
        let cur = if *t == tab { r#" aria-current="page""# } else { "" };
        format!(r#"<a href="{href}"{cur}>{}<span>{label}</span></a>"#, icon(ic))
    })
    .collect::<String>();
    let initial = ctx.profile.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    let badge = if ctx.incognito {
        format!(r#"<a class="incog" href="/settings" title="Incognito: nothing is saved">{}<span>Incognito</span></a>"#, icon("incognito"))
    } else {
        String::new()
    };
    format!(
        r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#EC4A36">
<meta name="referrer" content="no-referrer">
<title>{title} · YourTube</title>
<link rel="icon" href="/static/icon.svg" type="image/svg+xml">
<link rel="manifest" href="/static/manifest.webmanifest">
<link rel="stylesheet" href="/static/style.css?v={ver}">
<script src="/static/app.js?v={ver}" defer></script>
</head>
<body{incog_class}>
<header class="top">
  <a class="brand" href="/" aria-label="YourTube home">{logo}<b>YourTube</b></a>
  <form class="search" action="/search" role="search">
    <input type="search" name="q" value="{q}" placeholder="Search YouTube" aria-label="Search YouTube" enterkeyhint="search" autocomplete="off">
    <button aria-label="Search">{search}</button>
  </form>
  {badge}
  <a class="avatar" href="/settings" title="Profile: {profile}" aria-label="Profile and privacy: {profile}">{initial}</a>
</header>
<nav class="tabs">{nav}</nav>
<main>
{body}
</main>
<dialog id="sheet"><div class="sheet-title"></div><div class="sheet-actions"></div><button class="sheet-close" value="close">Close</button></dialog>
<div id="toast" role="status" aria-live="polite"></div>
</body></html>"##,
        title = esc(title),
        ver = super::asset_version(),
        incog_class = if ctx.incognito { r#" class="incognito""# } else { "" },
        logo = LOGO
            .replace(r#"width="128" height="128""#, r#"width="30" height="30" aria-hidden="true""#)
            .replace("<title>YourTube</title>", ""),
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
}

pub fn card(it: &Item, o: CardOpts) -> String {
    let mut href = format!("/watch?v={}", it.id);
    if let Some(run) = it.run_id {
        href += &format!("&amp;run={run}");
    }
    let dur = it.length.filter(|l| *l > 0.0).map(|l| format!(r#"<span class="dur">{}</span>"#, fmt::clock(l))).unwrap_or_default();
    let prog = match (it.position, it.length) {
        (Some(p), Some(l)) if l > 0.0 && p > 0.0 => {
            let pct = (p / l * 100.0).clamp(2.0, 100.0);
            format!(r#"<span class="prog"><i style="width:{pct:.1}%"></i></span>"#)
        }
        _ => String::new(),
    };
    let mut sub: Vec<String> = Vec::new();
    if !it.channel.is_empty() {
        if it.channel_id.is_empty() {
            sub.push(esc(&it.channel));
        } else {
            sub.push(format!(r#"<a href="/channel/{}">{}</a>"#, esc(&it.channel_id), esc(&it.channel)));
        }
    }
    if let Some(p) = it.published.filter(|p| *p > 0) {
        sub.push(fmt::age(p));
    }
    if let Some(v) = it.views.filter(|_| !o.hide_views) {
        sub.push(fmt::views(v));
    }
    let note = match (&it.note, o.note) {
        (Some(n), true) if !n.is_empty() => format!(r#"<p class="why">{}</p>"#, esc(n)),
        _ => String::new(),
    };
    format!(
        r#"<article class="card{done}" data-id="{id}" data-title="{title}" data-channel="{ch}" data-cid="{cid}" data-len="{len}" data-wl="{wl}" data-watched="{watched}" data-pl="{pl}">
<a class="thumb" href="{href}" tabindex="-1" aria-hidden="true"><img src="/thumb/{id}" alt="" loading="lazy" decoding="async">{dur}{prog}</a>
<div class="meta"><a class="title" href="{href}">{title}</a><p class="sub">{sub}</p>{note}<button class="more" type="button" aria-label="More actions for {title}">{more}</button></div>
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
        sub = sub.join(" · "),
        more = icon("more"),
    )
}

pub fn grid<'a>(items: &[Item], o: impl Fn(&Item) -> CardOpts<'a>) -> String {
    let mut s = String::from(r#"<section class="grid">"#);
    for it in items {
        s += &card(it, o(it));
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
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark"><meta name="theme-color" content="#EC4A36"><meta name="referrer" content="no-referrer">
<title>Log in · YourTube</title><link rel="icon" href="/static/icon.svg" type="image/svg+xml">
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
}

/// The part under the player (re-rendered by /api/info once yt-dlp has answered).
pub fn watch_info(w: &WatchInfo, ctx: &Ctx) -> String {
    let mut sub = Vec::new();
    if let Some(p) = w.published.filter(|p| *p > 0) {
        sub.push(fmt::age(p));
    }
    if let Some(v) = w.views.filter(|_| !ctx.hide_views) {
        sub.push(fmt::views(v));
    }
    let channel = if w.channel_id.is_empty() {
        format!(r#"<span class="ch-name">{}</span>"#, esc(w.channel))
    } else {
        format!(r#"<a class="ch-name" href="/channel/{}">{}</a>"#, esc(w.channel_id), esc(w.channel))
    };
    let subscribe =
        if w.channel_id.is_empty() || ctx.incognito { String::new() } else { sub_button(w.channel_id, w.channel, w.subscribed) };
    let actions = if ctx.incognito {
        r#"<p class="lede">Incognito: nothing about this video is saved.</p>"#.to_owned()
    } else {
        format!(
            r#"<div class="chips"><button class="chip" data-act="watch_later" aria-pressed="{wl}">{wll}</button><button class="chip" data-act="{wact}">{wlabel}</button><button class="chip" data-act="more">More like this</button><button class="chip" data-act="not_interested">Not interested</button>{block}<button class="chip" data-act="copy">Copy link</button></div>"#,
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
    let desc = if w.description.is_empty() {
        String::new()
    } else {
        format!("<details class=\"desc\"><summary>Description</summary><p>{}</p></details>", esc(w.description))
    };
    format!(
        r#"<h1 class="v-title">{title}</h1>
<div class="v-row"><div class="v-ch">{channel}<span class="v-sub">{sub}</span></div>{subscribe}</div>
{actions}
{desc}"#,
        title = if w.title.is_empty() { "Loading…".to_owned() } else { esc(w.title) },
        sub = if sub.is_empty() { String::new() } else { format!(" · {}", sub.join(" · ")) },
    )
}

pub fn watch_body(w: &WatchInfo, ctx: &Ctx) -> String {
    format!(
        r#"<div class="watch" data-id="{id}" data-title="{title}" data-channel="{ch}" data-cid="{cid}" data-len="{len}" data-start="{start:.0}" data-save="{save}">
<div class="player-col">
<div class="player"><video id="video" controls autoplay playsinline preload="auto" poster="/thumb/{id}" src="/stream/{id}"></video><p class="player-msg" hidden></p></div>
<div class="v-info">{info}</div>
</div>
<aside class="related-col"><h2>Related</h2><div id="related" class="grid list" data-related="{id}"><p class="lede loading">Loading related videos…</p></div></aside>
</div>"#,
        id = esc(w.id),
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
}
