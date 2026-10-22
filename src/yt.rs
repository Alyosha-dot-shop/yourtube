//! Reading YouTube's own pages: channel tabs, search, playlists, and the
//! InnerTube "continuation" calls that load more of each.
//!
//! Each page embeds its data as a `ytInitialData` JSON object. YouTube changes
//! the layout from time to time, so the parsers look for a few well-known
//! objects anywhere in the tree instead of following fixed paths, and accept
//! both the older `videoRenderer` and the newer `lockupViewModel` shapes.
//!
//! Every request goes through [`agent`], so the proxy from the config (for
//! example Tor) covers all of it. No API keys are needed: the "load more"
//! calls use the same InnerTube endpoints YouTube's own web page uses.

// Parsed data carries every field YouTube gives us, even ones no screen shows yet,
// so contributors can use them without touching the parsers.
#![allow(dead_code)]

use crate::feed::Video;
use anyhow::{Result, anyhow};
use base64::Engine as _;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
/// Used for InnerTube calls until we have read a real page and learned the current one.
/// YouTube accepts older versions for a long time, so a stale value still works.
const FALLBACK_CLIENT_VERSION: &str = "2.20261020.05.00";

/// A channel's header and newest videos.
#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub name: String,
    pub subscribers: Option<String>,
    pub videos: Vec<Video>,
    /// Largest avatar picture, banner picture and the "About" text, when the page has them.
    pub avatar: Option<String>,
    pub banner: Option<String>,
    pub description: Option<String>,
}

/// A channel found by search.
#[derive(Debug, Clone)]
pub struct FoundChannel {
    pub id: String,
    pub name: String,
    pub subscribers: Option<String>,
    pub avatar: Option<String>,
}

/// A playlist (or course, podcast, mix) found by search or on a channel's Playlists tab.
#[derive(Debug, Clone, Default)]
pub struct FoundPlaylist {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    /// "110 episodes", "16 videos", "Mix" — as YouTube shows it.
    pub video_count_text: Option<String>,
    pub video_count: Option<i64>,
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub videos: Vec<Video>,
    pub channels: Vec<FoundChannel>,
    pub playlists: Vec<FoundPlaylist>,
    /// Ids (among `videos`) that are live right now, or scheduled streams/premieres.
    pub live: Vec<String>,
    pub upcoming: Vec<String>,
    /// Token for [`search_more`]; None on the last page.
    pub next: Option<String>,
}

// ---------------------------------------------------------------------------
// Networking

/// The HTTP agent every YouTube-facing request in the app should use.
///
/// It honours `proxy` from the config (`socks5h://…`, `http://…`); without one
/// it falls back to the usual `HTTPS_PROXY`/`ALL_PROXY` environment variables.
/// The agent is rebuilt only when the proxy setting changes, so connections are
/// reused between calls.
pub fn agent() -> ureq::Agent {
    static CACHE: Mutex<Option<(Option<String>, ureq::Agent)>> = Mutex::new(None);
    let proxy = crate::config::load().proxy;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, a)) = cache.as_ref()
        && *p == proxy
    {
        return a.clone();
    }
    let a = build_agent(proxy.as_deref());
    *cache = Some((proxy, a.clone()));
    a
}

fn build_agent(proxy: Option<&str>) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).user_agent(UA);
    if let Some(p) = proxy {
        // If the configured proxy can't be parsed we must not quietly go direct:
        // someone who set up Tor expects nothing to leave without it. Point at a
        // closed local port instead so every request fails loudly.
        let parsed = ureq::Proxy::new(p).unwrap_or_else(|e| {
            eprintln!("yourtube: proxy {p:?} is not usable ({e}); blocking network requests");
            ureq::Proxy::new("socks5h://127.0.0.1:1").expect("static proxy")
        });
        builder = builder.proxy(Some(parsed));
    }
    builder.build().new_agent()
}

/// What we learned from the last page's `ytcfg.set({...})`: the web client's
/// version and visitor id, which InnerTube calls should send back.
#[derive(Default)]
struct Ytcfg {
    client_version: Option<String>,
    visitor_data: Option<String>,
}
static YTCFG: Mutex<Ytcfg> = Mutex::new(Ytcfg { client_version: None, visitor_data: None });

/// `"KEY":"value"` inside the page's ytcfg blob.
fn cfg_value<'a>(html: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":\"");
    let start = html.find(&pat)? + pat.len();
    let len = html[start..].find('"')?;
    Some(&html[start..start + len]).filter(|s| !s.is_empty())
}

fn remember_ytcfg(html: &str) {
    let mut cfg = YTCFG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = cfg_value(html, "INNERTUBE_CLIENT_VERSION") {
        cfg.client_version = Some(v.to_owned());
    }
    if let Some(v) = cfg_value(html, "VISITOR_DATA") {
        cfg.visitor_data = Some(v.to_owned());
    }
}

pub(crate) fn get_html(url: &str) -> Result<String> {
    let html = agent()
        .get(url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Cookie", "SOCS=CAI") // skips the cookie-consent page
        .call()?
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_string()?;
    remember_ytcfg(&html);
    Ok(html)
}

/// POST to an InnerTube endpoint (`next`, `browse`, `search`) as the WEB client.
/// `body` holds the request fields (usually just `continuation`); the client
/// context is added here.
pub(crate) fn innertube(endpoint: &str, mut body: Value) -> Result<Value> {
    let (version, visitor) = {
        let cfg = YTCFG.lock().unwrap_or_else(|e| e.into_inner());
        (cfg.client_version.clone().unwrap_or_else(|| FALLBACK_CLIENT_VERSION.to_owned()), cfg.visitor_data.clone())
    };
    let mut client = json!({ "clientName": "WEB", "clientVersion": version, "hl": "en", "gl": "US" });
    if let Some(v) = visitor {
        client["visitorData"] = Value::String(v);
    }
    body["context"] = json!({ "client": client });
    let text = agent()
        .post(&format!("https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"))
        .header("Content-Type", "application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Origin", "https://www.youtube.com")
        .header("X-Youtube-Client-Name", "1")
        .header("X-Youtube-Client-Version", &version)
        .send(body.to_string())?
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_string()?;
    Ok(serde_json::from_str(&text)?)
}

/// The JSON object that follows `mark` in a page (`var ytInitialData = `, …).
pub(crate) fn json_after(html: &str, mark: &str) -> Option<Value> {
    let start = html.find(mark)? + mark.len();
    serde_json::Deserializer::from_str(&html[start..]).into_iter::<Value>().next()?.ok()
}

pub(crate) fn initial_data(html: &str) -> Result<Value> {
    json_after(html, "var ytInitialData = ").ok_or_else(|| anyhow!("no ytInitialData on the page"))
}

/// The continuation token of the first "load more" item under `v`, if any.
pub(crate) fn next_token(v: &Value) -> Option<String> {
    let mut items = Vec::new();
    collect(v, "continuationItemRenderer", &mut items);
    collect(v, "continuationItemViewModel", &mut items);
    items.into_iter().find_map(|c| find_text_under(c, "token", |s| !s.is_empty()))
}

/// The "load more" token of a page or of a continuation response.
fn page_next(v: &Value) -> Option<String> {
    ["contents", "onResponseReceivedCommands", "onResponseReceivedActions", "onResponseReceivedEndpoints"]
        .iter()
        .find_map(|k| next_token(&v[*k]))
}

// ---------------------------------------------------------------------------
// Channels

/// The newest videos on a channel's Videos tab (no Shorts, no live streams).
pub fn channel_videos(channel_id: &str) -> Result<Channel> {
    let html = get_html(&format!("https://www.youtube.com/channel/{channel_id}/videos"))?;
    let ch = parse_channel(&html, channel_id, crate::db::now())?;
    if ch.videos.is_empty() {
        return Err(anyhow!("no videos on channel page"));
    }
    Ok(ch)
}

pub fn parse_channel(html: &str, channel_id: &str, now: i64) -> Result<Channel> {
    let data = initial_data(html)?;
    let meta = &data["metadata"]["channelMetadataRenderer"];
    let name = meta["title"].as_str().unwrap_or_default().to_owned();
    let info = channel_info(&data, channel_id);
    let mut ch = Channel {
        subscribers: find_text(&data["header"], |s| s.ends_with(" subscribers") || s.ends_with(" subscriber")),
        name: name.clone(),
        videos: Vec::new(),
        avatar: info.avatar,
        banner: info.banner,
        description: info.description,
    };
    let mut lockups = Vec::new();
    collect(&data, "lockupViewModel", &mut lockups);
    ch.videos = lockups.into_iter().filter_map(|l| from_lockup(l, channel_id, &name, now)).collect();
    let mut renderers = Vec::new();
    collect(&data, "videoRenderer", &mut renderers);
    ch.videos.extend(renderers.into_iter().filter_map(|r| from_renderer(r, now)).map(|mut v| {
        v.channel_id = channel_id.to_owned();
        v.channel.clone_from(&name);
        v
    }));
    Ok(ch)
}

/// A channel's header: name, @handle, counts, pictures and description.
#[derive(Debug, Clone, Default)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub handle: Option<String>,
    pub subscribers: Option<String>,
    /// "2.3K videos"
    pub video_count: Option<String>,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Videos,
    Shorts,
    Streams,
    Playlists,
}

impl Tab {
    fn path(self) -> &'static str {
        match self {
            Tab::Videos => "videos",
            Tab::Shorts => "shorts",
            Tab::Streams => "streams",
            Tab::Playlists => "playlists",
        }
    }
}

/// One page of a channel tab. Videos, Shorts and Streams fill `videos`;
/// the Playlists tab fills `playlists`.
#[derive(Debug, Clone, Default)]
pub struct TabPage {
    /// The channel's header; only on the first page (None from [`channel_more`]).
    pub info: Option<ChannelInfo>,
    pub videos: Vec<Video>,
    pub playlists: Vec<FoundPlaylist>,
    /// Ids (among `videos`) live right now / scheduled.
    pub live: Vec<String>,
    pub upcoming: Vec<String>,
    /// Token for [`channel_more`]; None on the last page.
    pub next: Option<String>,
}

/// The first page of one of a channel's tabs, with the channel's header.
///
/// Shorts and scheduled streams carry no upload age on these pages; their
/// `published` is set to the time of the request, like live videos in search.
pub fn channel_tab(channel_id: &str, tab: Tab) -> Result<TabPage> {
    let html = get_html(&format!("https://www.youtube.com/channel/{channel_id}/{}", tab.path()))?;
    parse_channel_tab(&html, channel_id, crate::db::now())
}

pub fn parse_channel_tab(html: &str, channel_id: &str, now: i64) -> Result<TabPage> {
    let data = initial_data(html)?;
    let info = channel_info(&data, channel_id);
    // Only the selected tab: the others are listed too, but empty or unrelated.
    let tabs = data["contents"]["twoColumnBrowseResultsRenderer"]["tabs"].as_array();
    let selected = tabs
        .and_then(|t| t.iter().find(|t| t["tabRenderer"]["selected"].as_bool() == Some(true)))
        .map_or(&data["contents"], |t| &t["tabRenderer"]["content"]);
    let mut page = TabPage::default();
    tab_items(selected, &info.id, &info.name, now, &mut page);
    page.next = next_token(selected);
    page.info = Some(info);
    Ok(page)
}

/// The next page of a channel tab, from [`TabPage::next`].
pub fn channel_more(token: &str) -> Result<TabPage> {
    let v = innertube("browse", json!({ "continuation": token }))?;
    Ok(parse_channel_more(&v, &channel_id_in_token(token).unwrap_or_default(), crate::db::now()))
}

/// Videos on a channel's own page don't name the channel, so the id is passed
/// in (taken from the token by [`channel_more`]). The channel's name is left
/// empty here: the UI already has it from the first page.
pub fn parse_channel_more(v: &Value, channel_id: &str, now: i64) -> TabPage {
    let mut page = TabPage::default();
    let items = &v["onResponseReceivedActions"];
    tab_items(items, channel_id, "", now, &mut page);
    page.next = next_token(items);
    page
}

/// Channel browse tokens are protobufs that contain the channel's id in
/// plain text, so we can recover it without asking the caller.
fn channel_id_in_token(token: &str) -> Option<String> {
    let bytes = b64_decode(&url_decode(token))?;
    let pos = bytes.windows(2).position(|w| w == b"UC")?;
    let id = bytes.get(pos..pos + 24)?;
    id.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_').then(|| String::from_utf8_lossy(id).into_owned())
}

fn tab_items(v: &Value, channel_id: &str, name: &str, now: i64, page: &mut TabPage) {
    let mut lockups = Vec::new();
    collect(v, "lockupViewModel", &mut lockups);
    for l in lockups {
        if l["contentType"].as_str() == Some("LOCKUP_CONTENT_TYPE_VIDEO") {
            if let Some(c) = lockup_card(l, channel_id, name, now) {
                push_card(c, &mut page.videos, &mut page.live, &mut page.upcoming);
            }
        } else if let Some(mut p) = playlist_lockup(l) {
            if p.channel_id.is_empty() {
                p.channel_id = channel_id.to_owned();
            }
            if p.channel.is_empty() {
                p.channel = name.to_owned();
            }
            page.playlists.push(p);
        }
    }
    let mut shorts = Vec::new();
    collect(v, "shortsLockupViewModel", &mut shorts);
    page.videos.extend(shorts.into_iter().filter_map(|s| from_short(s, channel_id, name, now)));
    // Older layouts.
    let mut renderers = Vec::new();
    collect(v, "videoRenderer", &mut renderers);
    collect(v, "gridVideoRenderer", &mut renderers);
    for r in renderers {
        if let Some(mut c) = renderer_card(r, now) {
            c.video.channel_id = channel_id.to_owned();
            c.video.channel = name.to_owned();
            push_card(c, &mut page.videos, &mut page.live, &mut page.upcoming);
        }
    }
    let mut reels = Vec::new();
    collect(v, "reelItemRenderer", &mut reels);
    page.videos.extend(reels.into_iter().filter_map(|r| {
        Some(Video {
            id: r["videoId"].as_str()?.to_owned(),
            channel_id: channel_id.to_owned(),
            channel: name.to_owned(),
            title: text(&r["headline"]).unwrap_or_default(),
            published: now,
            views: text(&r["viewCountText"]).and_then(|s| parse_count(&s)),
            ..Video::default()
        })
    }));
    let mut grid = Vec::new();
    collect(v, "gridPlaylistRenderer", &mut grid);
    collect(v, "playlistRenderer", &mut grid);
    page.playlists.extend(grid.into_iter().filter_map(old_playlist).map(|mut p| {
        if p.channel_id.is_empty() {
            p.channel_id = channel_id.to_owned();
            p.channel = name.to_owned();
        }
        p
    }));
}

fn channel_info(data: &Value, channel_id: &str) -> ChannelInfo {
    let meta = &data["metadata"]["channelMetadataRenderer"];
    let header = &data["header"];
    let mut info = ChannelInfo {
        id: meta["externalId"].as_str().unwrap_or(channel_id).to_owned(),
        name: meta["title"].as_str().unwrap_or_default().to_owned(),
        description: meta["description"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
        ..ChannelInfo::default()
    };
    // Newer header: pageHeaderViewModel. Older: c4TabbedHeaderRenderer.
    let ph = &header["pageHeaderRenderer"]["content"]["pageHeaderViewModel"];
    info.avatar = best_image(&ph["image"])
        .or_else(|| best_image(&header["c4TabbedHeaderRenderer"]["avatar"]))
        .or_else(|| best_image(&meta["avatar"]));
    info.banner = best_image(&ph["banner"]).or_else(|| best_image(&header["c4TabbedHeaderRenderer"]["banner"]));
    info.handle = find_text(header, |s| s.starts_with('@') && !s.contains(' '));
    info.subscribers = find_text(header, |s| s.ends_with(" subscribers") || s.ends_with(" subscriber"));
    info.video_count = find_text(header, |s| s.ends_with(" videos") || s.ends_with(" video"));
    if info.name.is_empty() {
        info.name = ph["title"]["dynamicTextViewModel"]["text"]["content"].as_str().unwrap_or_default().to_owned();
    }
    info
}

// ---------------------------------------------------------------------------
// Search

/// Search YouTube for videos (and channels, when the query names one).
pub fn search(query: &str) -> Result<SearchResults> {
    search_with(query, &Filters::default())
}

/// Result order.
///
/// Since 2025 youtube.com only offers "Relevance" and "Popularity" (= `Views`).
/// `Date` and `Rating` are still sent the way YouTube used to accept them,
/// but YouTube now ignores them and answers in relevance order; for recent
/// videos, filter with [`Upload`] instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Relevance,
    Date,
    Views,
    Rating,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Upload {
    #[default]
    Any,
    Hour,
    Today,
    Week,
    Month,
    Year,
}

/// Video length filter: Short is under 3 minutes, Medium 3–20, Long over 20.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Length {
    #[default]
    Any,
    Short,
    Medium,
    Long,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Any,
    Video,
    Shorts,
    Channel,
    Playlist,
    Movie,
}

/// Search filters, as in the "Search filters" menu on youtube.com.
/// Build with `Filters { duration: Length::Long, ..Filters::default() }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filters {
    pub sort: Sort,
    pub upload: Upload,
    pub duration: Length,
    pub kind: Kind,
    /// Only live streams.
    pub live: bool,
    pub hd: bool,
    pub four_k: bool,
    pub subtitles: bool,
    pub creative_commons: bool,
}

/// The `sp=` URL parameter for these filters (already URL-encoded), or None
/// when nothing is filtered.
///
/// It is a small protobuf: field 1 is the sort order, field 2 a message with
/// upload date (1), type (2), duration (3) and one flag field per feature
/// (HD 4, subtitles 5, Creative Commons 6, live 8, 4K 14). Building the bytes
/// ourselves, instead of keeping a table of known strings, means any
/// combination works. The numbers are the ones youtube.com's own filter menu
/// uses (checked October 2026).
pub fn sp(f: &Filters) -> Option<String> {
    let mut inner = Proto::default();
    let upload = match f.upload {
        Upload::Any => 0,
        Upload::Hour => 1,
        Upload::Today => 2,
        Upload::Week => 3,
        Upload::Month => 4,
        Upload::Year => 5,
    };
    let kind = match f.kind {
        Kind::Any => 0,
        Kind::Video => 1,
        Kind::Channel => 2,
        Kind::Playlist => 3,
        Kind::Movie => 4,
        Kind::Shorts => 9,
    };
    // YouTube renumbered these in 2025 (under 3 minutes, 3–20) but kept 2 for "over 20".
    let duration = match f.duration {
        Length::Any => 0,
        Length::Long => 2,
        Length::Short => 4,
        Length::Medium => 5,
    };
    let flag = |on: bool| u64::from(on);
    for (field, value) in [
        (1, upload),
        (2, kind),
        (3, duration),
        (4, flag(f.hd)),
        (5, flag(f.subtitles)),
        (6, flag(f.creative_commons)),
        (8, flag(f.live)),
        (14, flag(f.four_k)),
    ] {
        if value != 0 {
            inner = inner.varint(field, value);
        }
    }
    let sort = match f.sort {
        Sort::Relevance => 0,
        Sort::Rating => 1,
        Sort::Date => 2,
        Sort::Views => 3,
    };
    let mut outer = Proto::default();
    if sort != 0 {
        outer = outer.varint(1, sort);
    }
    if !inner.0.is_empty() {
        outer = outer.message(2, inner);
    }
    (!outer.0.is_empty()).then(|| url_encode(&b64(&outer.0)))
}

/// Search with filters. Use [`SearchResults::next`] with [`search_more`] for more results.
pub fn search_with(query: &str, filters: &Filters) -> Result<SearchResults> {
    let mut url = format!("https://www.youtube.com/results?search_query={}", url_encode(query));
    if let Some(sp) = sp(filters) {
        url.push_str("&sp=");
        url.push_str(&sp);
    }
    parse_search(&get_html(&url)?, crate::db::now())
}

/// The next page of a search, from [`SearchResults::next`]. Filters carry over.
pub fn search_more(token: &str) -> Result<SearchResults> {
    let v = innertube("search", json!({ "continuation": token }))?;
    Ok(parse_search_data(&v, crate::db::now()))
}

pub fn parse_search(html: &str, now: i64) -> Result<SearchResults> {
    Ok(parse_search_data(&initial_data(html)?, now))
}

/// Works on both the first page's `ytInitialData` and a `search` continuation.
pub fn parse_search_data(data: &Value, now: i64) -> SearchResults {
    let mut out = SearchResults::default();
    let mut renderers = Vec::new();
    collect(data, "videoRenderer", &mut renderers);
    for r in renderers {
        if let Some(c) = renderer_card(r, now) {
            push_card(c, &mut out.videos, &mut out.live, &mut out.upcoming);
        }
    }
    let mut lockups = Vec::new();
    collect(data, "lockupViewModel", &mut lockups);
    for l in lockups {
        if l["contentType"].as_str() == Some("LOCKUP_CONTENT_TYPE_VIDEO") {
            if let Some(c) = lockup_card(l, "", "", now) {
                push_card(c, &mut out.videos, &mut out.live, &mut out.upcoming);
            }
        } else if let Some(p) = playlist_lockup(l) {
            out.playlists.push(p);
        }
    }
    let mut old = Vec::new();
    collect(data, "playlistRenderer", &mut old);
    out.playlists.extend(old.into_iter().filter_map(old_playlist));
    let mut chans = Vec::new();
    collect(data, "channelRenderer", &mut chans);
    out.channels = chans
        .into_iter()
        .filter_map(|c| {
            Some(FoundChannel {
                id: c["channelId"].as_str()?.to_owned(),
                name: text(&c["title"])?,
                // YouTube puts the @handle in subscriberCountText and the count in videoCountText for channel results.
                subscribers: [&c["videoCountText"], &c["subscriberCountText"]]
                    .into_iter()
                    .filter_map(text)
                    .find(|s| s.contains("subscriber")),
                avatar: best_image(&c["thumbnail"]),
            })
        })
        .collect();
    out.next = page_next(data);
    out
}

// ---------------------------------------------------------------------------
// Playlists

#[derive(Debug, Clone, Default)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub description: Option<String>,
    /// As YouTube counts it (includes videos hidden from the list).
    pub video_count: Option<i64>,
    pub views: Option<i64>,
    pub videos: Vec<Video>,
    /// Token for [`playlist_more`]; None when all videos are here.
    pub next: Option<String>,
}

/// More videos of a playlist.
#[derive(Debug, Clone, Default)]
pub struct PlaylistPage {
    pub videos: Vec<Video>,
    pub next: Option<String>,
}

/// A playlist's details and its first 100 videos.
pub fn playlist(id: &str) -> Result<Playlist> {
    let html = get_html(&format!("https://www.youtube.com/playlist?list={}", url_encode(id)))?;
    parse_playlist(&html, crate::db::now())
}

/// The next (up to) 100 videos of a playlist, from [`Playlist::next`].
pub fn playlist_more(token: &str) -> Result<PlaylistPage> {
    let v = innertube("browse", json!({ "continuation": token }))?;
    Ok(parse_playlist_more(&v, crate::db::now()))
}

pub fn parse_playlist(html: &str, now: i64) -> Result<Playlist> {
    let data = initial_data(html)?;
    let header = &data["header"];
    // Older header: playlistHeaderRenderer. Newer: pageHeaderRenderer with view models.
    let old = &header["playlistHeaderRenderer"];
    let ph = &header["pageHeaderRenderer"];
    let meta = &data["metadata"]["playlistMetadataRenderer"];
    let title = text(&old["title"])
        .or_else(|| ph["pageTitle"].as_str().map(str::to_owned))
        .or_else(|| meta["title"].as_str().map(str::to_owned))
        .ok_or_else(|| anyhow!("not a playlist page"))?;
    let channel = text(&old["ownerText"]).or_else(|| {
        let mut stacks = Vec::new();
        collect(ph, "avatarStackViewModel", &mut stacks);
        stacks.first().and_then(|s| s["text"]["content"].as_str()).map(|s| s.trim_start_matches("by ").to_owned())
    });
    let count_text = text(&old["numVideosText"]).or_else(|| find_text(ph, is_count_text));
    let id = old["playlistId"].as_str().map(str::to_owned).or_else(|| {
        data["microformat"]["microformatDataRenderer"]["urlCanonical"]
            .as_str()
            .and_then(|u| u.split("list=").nth(1))
            .map(|s| s.split('&').next().unwrap_or(s).to_owned())
    });
    let mut items = PlaylistPage::default();
    playlist_items(&data["contents"], now, &mut items);
    Ok(Playlist {
        id: id.unwrap_or_default(),
        title,
        channel: channel.unwrap_or_default(),
        channel_id: find_text_under(header, "browseId", |s| s.starts_with("UC")).unwrap_or_default(),
        description: text(&old["descriptionText"]).or_else(|| meta["description"].as_str().map(str::to_owned)).filter(|s| !s.is_empty()),
        video_count: count_text.as_deref().and_then(parse_count),
        views: text(&old["viewCountText"])
            .or_else(|| find_text(ph, |s| s.ends_with(" views") || s.ends_with(" view")))
            .and_then(|s| parse_count(&s)),
        videos: items.videos,
        next: items.next,
    })
}

pub fn parse_playlist_more(v: &Value, now: i64) -> PlaylistPage {
    let mut page = PlaylistPage::default();
    playlist_items(&v["onResponseReceivedActions"], now, &mut page);
    page
}

fn playlist_items(v: &Value, now: i64, page: &mut PlaylistPage) {
    let mut lockups = Vec::new();
    collect(v, "lockupViewModel", &mut lockups);
    page.videos.extend(
        lockups
            .into_iter()
            .filter(|l| l["contentType"].as_str() == Some("LOCKUP_CONTENT_TYPE_VIDEO"))
            .filter_map(|l| lockup_card(l, "", "", now))
            .map(|c| c.video),
    );
    let mut old = Vec::new();
    collect(v, "playlistVideoRenderer", &mut old);
    page.videos.extend(old.into_iter().filter_map(|r| {
        let owner = &r["shortBylineText"]["runs"][0];
        let info: Vec<String> = r["videoInfo"]["runs"].as_array()?.iter().filter_map(|x| x["text"].as_str().map(str::to_owned)).collect();
        Some(Video {
            id: r["videoId"].as_str()?.to_owned(),
            channel_id: owner["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or_default().to_owned(),
            channel: owner["text"].as_str().unwrap_or_default().to_owned(),
            title: text(&r["title"]).unwrap_or_default(),
            published: info.iter().find_map(|s| parse_ago(s)).map_or(now, |a| now - a),
            views: info.iter().find(|s| s.contains("view")).and_then(|s| parse_count(s)),
            length: r["lengthSeconds"].as_str().and_then(|s| s.parse().ok()),
            ..Video::default()
        })
    }));
    page.next = next_token(v);
}

// ---------------------------------------------------------------------------
// Related videos

/// Videos YouTube lists next to a video ("related"). The first item is the video's own channel, title.
pub fn related(video_id: &str) -> Result<Vec<Video>> {
    let html = get_html(&format!("https://www.youtube.com/watch?v={video_id}"))?;
    parse_related(&html, crate::db::now())
}

pub fn parse_related(html: &str, now: i64) -> Result<Vec<Video>> {
    Ok(related_from(&initial_data(html)?, now))
}

pub(crate) fn related_from(data: &Value, now: i64) -> Vec<Video> {
    let side = &data["contents"]["twoColumnWatchNextResults"]["secondaryResults"];
    let mut lockups = Vec::new();
    collect(side, "lockupViewModel", &mut lockups);
    let mut out: Vec<Video> = lockups.into_iter().filter_map(|l| from_lockup(l, "", "", now)).collect();
    let mut renderers = Vec::new();
    collect(side, "compactVideoRenderer", &mut renderers);
    out.extend(renderers.into_iter().filter_map(|r| from_renderer(r, now)));
    out
}

// ---------------------------------------------------------------------------
// Video cards

/// A parsed video card plus what `Video` has no room for.
struct Card {
    video: Video,
    live: bool,
    upcoming: bool,
    /// false when the card shows no upload age (live, scheduled, some playlist items).
    has_age: bool,
}

fn push_card(c: Card, videos: &mut Vec<Video>, live: &mut Vec<String>, upcoming: &mut Vec<String>) {
    if c.live {
        live.push(c.video.id.clone());
    }
    if c.upcoming {
        upcoming.push(c.video.id.clone());
    }
    videos.push(c.video);
}

/// A newer-style video card (`lockupViewModel`). Skips playlists, live streams and premieres.
fn from_lockup(l: &Value, channel_id: &str, channel: &str, now: i64) -> Option<Video> {
    lockup_card(l, channel_id, channel, now).filter(|c| c.has_age).map(|c| c.video)
}

fn lockup_card(l: &Value, channel_id: &str, channel: &str, now: i64) -> Option<Card> {
    if l["contentType"].as_str() != Some("LOCKUP_CONTENT_TYPE_VIDEO") {
        return None;
    }
    let id = l["contentId"].as_str()?;
    let meta = &l["metadata"]["lockupMetadataViewModel"];
    let mut views = None;
    let mut published = None;
    let mut upcoming = false;
    let mut live = false;
    let mut channel_name = channel.to_owned();
    let mut parts = Vec::new();
    collect(&meta["metadata"], "metadataParts", &mut parts);
    for part in parts.iter().filter_map(|p| p.as_array()).flatten() {
        let label = part["accessibilityLabel"].as_str().or(part["text"]["content"].as_str()).unwrap_or("");
        if label.ends_with("views") || label.ends_with("view") {
            views = parse_count(label);
        } else if label.ends_with("watching") {
            views = parse_count(label);
            live = true;
        } else if let Some(ago) = parse_ago(label) {
            published = Some(now - ago);
        } else if label.starts_with("Scheduled for") || label.starts_with("Premieres") {
            upcoming = true;
        } else if channel_name.is_empty() {
            // Outside channel pages the first line is the channel's name.
            channel_name = part["text"]["content"].as_str().unwrap_or_default().to_owned();
        }
    }
    let mut badges = Vec::new();
    collect(&l["contentImage"], "thumbnailBadgeViewModel", &mut badges);
    let length = badges.iter().find_map(|b| b["text"].as_str().and_then(parse_clock));
    for b in &badges {
        let style = b["badgeStyle"].as_str().unwrap_or("");
        let t = b["text"].as_str().unwrap_or("");
        live |= style.ends_with("_LIVE") || t == "LIVE";
        upcoming |= style.ends_with("_UPCOMING") || t == "UPCOMING" || t == "PREMIERE";
    }
    Some(Card {
        video: Video {
            id: id.to_owned(),
            channel_id: if channel_id.is_empty() {
                find_text_under(l, "browseId", |s| s.starts_with("UC")).unwrap_or_default()
            } else {
                channel_id.to_owned()
            },
            channel: channel_name,
            title: meta["title"]["content"].as_str().unwrap_or_default().to_owned(),
            published: published.unwrap_or(now),
            views,
            length,
            description: None,
            exact_time: false,
        },
        live,
        upcoming,
        has_age: published.is_some(),
    })
}

/// An older-style video card (`videoRenderer`), still used by search results.
fn from_renderer(r: &Value, now: i64) -> Option<Video> {
    let id = r["videoId"].as_str()?;
    let owner = &r["ownerText"]["runs"][0];
    let owner = if owner.is_null() { &r["shortBylineText"]["runs"][0] } else { owner };
    Some(Video {
        id: id.to_owned(),
        channel_id: owner["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or_default().to_owned(),
        channel: owner["text"].as_str().unwrap_or_default().to_owned(),
        title: text(&r["title"]).unwrap_or_default(),
        // Live streams and premieres have no age; they get "now" so they still show in search.
        published: text(&r["publishedTimeText"]).and_then(|s| parse_ago(&s)).map_or(now, |a| now - a),
        views: text(&r["viewCountText"]).and_then(|s| parse_count(&s)),
        length: text(&r["lengthText"]).and_then(|s| parse_clock(&s)),
        description: None,
        exact_time: false,
    })
}

fn renderer_card(r: &Value, now: i64) -> Option<Card> {
    let video = from_renderer(r, now)?;
    let live =
        find_text(&r["badges"], |s| s.contains("LIVE_NOW")).is_some() || find_text(&r["thumbnailOverlays"], |s| s == "LIVE").is_some();
    Some(Card {
        has_age: text(&r["publishedTimeText"]).and_then(|s| parse_ago(&s)).is_some(),
        live,
        upcoming: r["upcomingEventData"].is_object(),
        video,
    })
}

/// A Short on a channel's Shorts tab. Shorts list no age or length.
fn from_short(s: &Value, channel_id: &str, channel: &str, now: i64) -> Option<Video> {
    let id = find_text_under(&s["onTap"], "videoId", |_| true)
        .or_else(|| s["entityId"].as_str().and_then(|e| e.strip_prefix("shorts-shelf-item-")).map(str::to_owned))?;
    let om = &s["overlayMetadata"];
    Some(Video {
        id,
        channel_id: channel_id.to_owned(),
        channel: channel.to_owned(),
        title: om["primaryText"]["content"].as_str().unwrap_or_default().to_owned(),
        published: now,
        views: om["secondaryText"]["content"].as_str().and_then(parse_count),
        ..Video::default()
    })
}

fn is_count_text(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_digit())
        && ["video", "videos", "episode", "episodes", "lesson", "lessons"].iter().any(|w| s.ends_with(&format!(" {w}")))
}

/// A playlist-like `lockupViewModel`: playlist, mix, course, podcast, show.
fn playlist_lockup(l: &Value) -> Option<FoundPlaylist> {
    let kind = l["contentType"].as_str()?;
    if kind == "LOCKUP_CONTENT_TYPE_VIDEO" || kind == "LOCKUP_CONTENT_TYPE_CHANNEL" {
        return None;
    }
    let meta = &l["metadata"]["lockupMetadataViewModel"];
    let mut badges = Vec::new();
    collect(&l["contentImage"], "thumbnailBadgeViewModel", &mut badges);
    let count_text = badges.iter().filter_map(|b| b["text"].as_str()).find(|t| !t.is_empty()).map(str::to_owned);
    // The channel is the metadata part that links to a UC… channel.
    let mut parts = Vec::new();
    collect(&meta["metadata"], "metadataParts", &mut parts);
    let owner = parts.iter().filter_map(|p| p.as_array()).flatten().find(|p| {
        let t = p["text"]["content"].as_str().unwrap_or("");
        !t.is_empty()
            && !["Playlist", "Course", "Podcast", "Mix", "Show", "Updated today", "Updated yesterday"].contains(&t)
            && find_text_under(p, "browseId", |s| s.starts_with("UC")).is_some()
    });
    Some(FoundPlaylist {
        id: l["contentId"].as_str()?.to_owned(),
        title: meta["title"]["content"].as_str().unwrap_or_default().to_owned(),
        channel: owner.and_then(|p| p["text"]["content"].as_str()).unwrap_or_default().to_owned(),
        channel_id: owner.and_then(|p| find_text_under(p, "browseId", |s| s.starts_with("UC"))).unwrap_or_default(),
        video_count: count_text
            .as_deref()
            .and_then(parse_count)
            .filter(|_| count_text.as_deref().is_some_and(|t| t.starts_with(|c: char| c.is_ascii_digit()))),
        video_count_text: count_text,
        thumbnail: best_image(&l["contentImage"]),
    })
}

/// `playlistRenderer` (search) and `gridPlaylistRenderer` (older channel pages).
fn old_playlist(r: &Value) -> Option<FoundPlaylist> {
    let owner = &r["shortBylineText"]["runs"][0];
    let count_text = r["videoCount"]
        .as_str()
        .map(|n| format!("{n} videos"))
        .or_else(|| text(&r["videoCountText"]))
        .or_else(|| text(&r["videoCountShortText"]));
    Some(FoundPlaylist {
        id: r["playlistId"].as_str()?.to_owned(),
        title: text(&r["title"]).unwrap_or_default(),
        channel: owner["text"].as_str().unwrap_or_default().to_owned(),
        channel_id: owner["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or_default().to_owned(),
        video_count: count_text.as_deref().and_then(parse_count),
        video_count_text: count_text,
        thumbnail: best_image(&r["thumbnails"]).or_else(|| best_image(&r["thumbnail"])),
    })
}

// ---------------------------------------------------------------------------
// Small helpers

/// The widest picture under `v`, from either `sources: [{url, width}]` or
/// `thumbnails: [{url, width}]` lists. Protocol-relative URLs get https.
pub(crate) fn best_image(v: &Value) -> Option<String> {
    let mut lists = Vec::new();
    collect(v, "sources", &mut lists);
    collect(v, "thumbnails", &mut lists);
    let best = lists
        .iter()
        .filter_map(|l| l.as_array())
        .flatten()
        .filter(|t| t["url"].as_str().is_some_and(|u| u.contains("//")))
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))?;
    let url = best["url"].as_str()?;
    Some(if url.starts_with("//") { format!("https:{url}") } else { url.to_owned() })
}

/// The first string stored under `key` anywhere under `v` that matches.
pub(crate) fn find_text_under(v: &Value, key: &str, want: impl Fn(&str) -> bool + Copy) -> Option<String> {
    match v {
        Value::Object(m) => m.iter().find_map(|(k, x)| match x {
            Value::String(s) if k == key && want(s) => Some(s.clone()),
            _ => find_text_under(x, key, want),
        }),
        Value::Array(a) => a.iter().find_map(|x| find_text_under(x, key, want)),
        _ => None,
    }
}

/// Text of a `{simpleText}`, `{runs: [{text}]}` or `{content}` object.
pub(crate) fn text(v: &Value) -> Option<String> {
    if let Some(s) = v["simpleText"].as_str().or(v["content"].as_str()) {
        return Some(s.to_owned());
    }
    let runs = v["runs"].as_array()?;
    Some(runs.iter().filter_map(|r| r["text"].as_str()).collect())
}

/// The first string anywhere under `v` that matches.
pub(crate) fn find_text(v: &Value, want: impl Fn(&str) -> bool + Copy) -> Option<String> {
    match v {
        Value::String(s) if want(s) => Some(s.clone()),
        Value::Object(m) => m.values().find_map(|x| find_text(x, want)),
        Value::Array(a) => a.iter().find_map(|x| find_text(x, want)),
        _ => None,
    }
}

/// Every value stored under `key`, anywhere in the tree (not looking inside matches).
pub(crate) fn collect<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    out.push(x);
                } else {
                    collect(x, key, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect(x, key, out)),
        _ => {}
    }
}

pub(crate) fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Undo %XX escapes (tokens on pages arrive with `%3D` for `=`).
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(n) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(n);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Accepts both alphabets, with or without padding, as YouTube mixes them.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s: String = s
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(s).ok()
}

/// Just enough protobuf writing for YouTube's `sp` filters and continuation tokens.
#[derive(Debug, Clone, Default)]
pub(crate) struct Proto(pub Vec<u8>);

impl Proto {
    fn raw_varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    pub fn varint(mut self, field: u64, v: u64) -> Self {
        self.raw_varint(field << 3);
        self.raw_varint(v);
        self
    }

    pub fn bytes(mut self, field: u64, b: &[u8]) -> Self {
        self.raw_varint((field << 3) | 2);
        self.raw_varint(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }

    pub fn string(self, field: u64, s: &str) -> Self {
        self.bytes(field, s.as_bytes())
    }

    pub fn message(self, field: u64, m: Proto) -> Self {
        self.bytes(field, &m.0)
    }
}

/// "1.4 million views", "327,804 views", "1.4M views", "No views" -> count.
pub fn parse_count(s: &str) -> Option<i64> {
    let s = s.to_lowercase();
    if s.starts_with("no ") {
        return Some(0);
    }
    let first = s.split_whitespace().next()?.replace(',', "");
    let (num, suffix) = first.split_at(first.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(first.len()));
    let n: f64 = num.parse().ok()?;
    let mult = match (suffix, s.split_whitespace().nth(1)) {
        ("k", _) | (_, Some("thousand")) => 1e3,
        ("m", _) | (_, Some("million")) => 1e6,
        ("b", _) | (_, Some("billion")) => 1e9,
        _ => 1.0,
    };
    Some((n * mult).round() as i64)
}

/// "2 weeks ago", "Streamed 3 days ago", "6y ago", "10mo ago" -> seconds.
pub fn parse_ago(s: &str) -> Option<i64> {
    let s = s.to_lowercase();
    let s = s.strip_suffix(" ago")?;
    let words: Vec<&str> = s.split_whitespace().collect();
    let (n, unit) = match words.as_slice() {
        [.., n, unit] if n.chars().all(|c| c.is_ascii_digit()) => (n.parse::<i64>().ok()?, *unit),
        [.., compact] => {
            let split = compact.find(|c: char| !c.is_ascii_digit())?;
            (compact[..split].parse::<i64>().ok()?, &compact[split..])
        }
        [] => return None,
    };
    let secs = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        "w" | "wk" | "wks" | "week" | "weeks" => 604800,
        "mo" | "mos" | "month" | "months" => 2629800,
        "y" | "yr" | "yrs" | "year" | "years" => 31557600,
        _ => return None,
    };
    Some(n * secs)
}

/// "48:40" or "1:02:03" -> seconds.
pub fn parse_clock(s: &str) -> Option<f64> {
    let mut t = 0.0;
    let mut parts = 0;
    for p in s.trim().split(':') {
        t = t * 60.0 + p.parse::<f64>().ok()?;
        parts += 1;
    }
    (parts >= 2).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(parse_count("1.4 million views"), Some(1_400_000));
        assert_eq!(parse_count("327,804 views"), Some(327_804));
        assert_eq!(parse_count("1.4M views"), Some(1_400_000));
        assert_eq!(parse_count("12K views"), Some(12_000));
        assert_eq!(parse_count("No views"), Some(0));
        assert_eq!(parse_ago("2 weeks ago"), Some(2 * 604800));
        assert_eq!(parse_ago("Streamed 1 day ago"), Some(86400));
        assert_eq!(parse_ago("6y ago"), Some(6 * 31557600));
        assert_eq!(parse_ago("10mo ago"), Some(10 * 2629800));
        assert_eq!(parse_ago("Streamed 5h ago"), Some(5 * 3600));
        assert_eq!(parse_ago("Scheduled for 10/30/26"), None);
        assert_eq!(parse_clock("48:40"), Some(2920.0));
        assert_eq!(parse_clock("1:02:03"), Some(3723.0));
        assert_eq!(parse_clock("LIVE"), None);
        assert_eq!(url_encode("e30 turbo & co"), "e30+turbo+%26+co");
        assert_eq!(url_decode("abc%3D%3D"), "abc==");
    }

    fn f(sort: Sort, upload: Upload, duration: Length, kind: Kind) -> Filters {
        Filters { sort, upload, duration, kind, ..Filters::default() }
    }

    #[test]
    fn sp_encoding() {
        assert_eq!(sp(&Filters::default()), None);
        // These match what youtube.com itself puts in the URL for the same filters.
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Any, Kind::Video)).as_deref(), Some("EgIQAQ%3D%3D"));
        assert_eq!(sp(&f(Sort::Date, Upload::Any, Length::Any, Kind::Any)).as_deref(), Some("CAI%3D"));
        assert_eq!(sp(&f(Sort::Views, Upload::Any, Length::Any, Kind::Any)).as_deref(), Some("CAM%3D"));
        assert_eq!(sp(&f(Sort::Rating, Upload::Any, Length::Any, Kind::Any)).as_deref(), Some("CAE%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Today, Length::Any, Kind::Any)).as_deref(), Some("EgIIAg%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Any, Kind::Channel)).as_deref(), Some("EgIQAg%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Any, Kind::Playlist)).as_deref(), Some("EgIQAw%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Short, Kind::Any)).as_deref(), Some("EgIYBA%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Long, Kind::Any)).as_deref(), Some("EgIYAg%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Medium, Kind::Any)).as_deref(), Some("EgIYBQ%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Year, Length::Any, Kind::Any)).as_deref(), Some("EgIIBQ%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Any, Kind::Shorts)).as_deref(), Some("EgIQCQ%3D%3D"));
        assert_eq!(sp(&f(Sort::Relevance, Upload::Any, Length::Any, Kind::Movie)).as_deref(), Some("EgIQBA%3D%3D"));
        let feature = |g: fn(&mut Filters)| {
            let mut x = Filters::default();
            g(&mut x);
            sp(&x)
        };
        assert_eq!(feature(|x| x.live = true).as_deref(), Some("EgJAAQ%3D%3D"));
        assert_eq!(feature(|x| x.hd = true).as_deref(), Some("EgIgAQ%3D%3D"));
        assert_eq!(feature(|x| x.subtitles = true).as_deref(), Some("EgIoAQ%3D%3D"));
        assert_eq!(feature(|x| x.creative_commons = true).as_deref(), Some("EgIwAQ%3D%3D"));
        assert_eq!(feature(|x| x.four_k = true).as_deref(), Some("EgJwAQ%3D%3D"));
        assert_eq!(feature(|x| x.sort = Sort::Views).as_deref(), Some("CAM%3D"));
        // Combined: sorted by views, this month, over 20 minutes, videos only.
        assert_eq!(sp(&f(Sort::Views, Upload::Month, Length::Long, Kind::Video)).as_deref(), Some("CAMSBggEEAEYAg%3D%3D"));
        assert_eq!(sp(&f(Sort::Date, Upload::Hour, Length::Any, Kind::Video)).as_deref(), Some("CAISBAgBEAE%3D"));
    }

    #[test]
    fn proto_and_tokens() {
        // A varint over 127 takes two bytes.
        assert_eq!(Proto::default().varint(1, 300).0, vec![0x08, 0xac, 0x02]);
        let token = b64(&Proto::default().message(80226972, Proto::default().string(2, "UC8butISFwT-Wl7EV0hUK0BQ")).0);
        assert_eq!(channel_id_in_token(&url_encode(&token)).as_deref(), Some("UC8butISFwT-Wl7EV0hUK0BQ"));
    }

    // Saved copies of real pages, when present (kept out of git; see tests/data/README).
    #[test]
    fn real_channel_page() {
        let Ok(html) = std::fs::read_to_string("tests/data/channel.html") else { return };
        let c = parse_channel(&html, "UCx", 1_000_000_000).unwrap();
        assert!(!c.name.is_empty());
        assert!(c.subscribers.as_deref().is_some_and(|s| s.contains("subscriber")), "{:?}", c.subscribers);
        assert!(c.videos.len() >= 10, "{}", c.videos.len());
        assert!(c.videos.iter().all(|x| !x.title.is_empty() && x.length.is_some() && x.views.is_some()));
        assert!(c.avatar.as_deref().is_some_and(|u| u.starts_with("https://")), "{:?}", c.avatar);
    }

    #[test]
    fn real_watch_page() {
        let Ok(html) = std::fs::read_to_string("tests/data/watch.html") else { return };
        let r = parse_related(&html, 1_000_000_000).unwrap();
        assert!(r.len() >= 10, "{}", r.len());
        assert!(r.iter().all(|v| !v.title.is_empty() && v.channel_id.starts_with("UC") && !v.channel.is_empty()), "{r:?}");
    }

    #[test]
    fn real_search_page() {
        let Ok(html) = std::fs::read_to_string("tests/data/search.html") else { return };
        let r = parse_search(&html, 1_000_000_000).unwrap();
        assert!(r.videos.len() >= 10, "{}", r.videos.len());
        assert!(r.videos.iter().all(|v| !v.title.is_empty() && !v.channel_id.is_empty() && !v.channel.is_empty()));
        assert!(r.next.is_some());
    }

    #[test]
    fn real_filtered_search_page() {
        // "rust tutorial", this month, over 20 minutes, sorted by views.
        let Ok(html) = std::fs::read_to_string("tests/data/search-filtered.html") else { return };
        let now = 2_000_000_000;
        let r = parse_search(&html, now).unwrap();
        assert!(r.videos.len() >= 10, "{}", r.videos.len());
        assert!(r.videos.iter().all(|v| v.length.is_some_and(|l| l > 20.0 * 60.0)), "all long");
        assert!(r.next.is_some());
    }

    #[test]
    fn real_search_playlists() {
        // Plain "rust tutorial" search, which shows a few courses/playlists among the videos.
        let Ok(html) = std::fs::read_to_string("tests/data/search-playlists.html") else { return };
        let r = parse_search(&html, 1_000_000_000).unwrap();
        assert!(!r.playlists.is_empty());
        assert!(
            r.playlists
                .iter()
                .all(|p| p.id.starts_with("PL") && !p.title.is_empty() && !p.channel.is_empty() && p.channel_id.starts_with("UC")),
            "{:?}",
            r.playlists
        );
        assert!(r.playlists.iter().all(|p| p.video_count.is_some()), "{:?}", r.playlists);
    }

    #[test]
    fn real_playlist_page() {
        // A playlist with more than 100 videos, so the page has a continuation.
        let Ok(html) = std::fs::read_to_string("tests/data/playlist.html") else { return };
        let p = parse_playlist(&html, 2_000_000_000).unwrap();
        assert!(!p.title.is_empty() && !p.channel.is_empty() && p.channel_id.starts_with("UC"), "{p:?}");
        assert!(p.id.starts_with("PL"), "{}", p.id);
        assert_eq!(p.videos.len(), 100);
        assert!(p.video_count.is_some_and(|n| n > 100), "{:?}", p.video_count);
        assert!(p.next.is_some());
        assert!(p.videos.iter().all(|v| !v.title.is_empty() && v.channel_id.starts_with("UC") && v.length.is_some()));
    }

    #[test]
    fn real_channel_streams_tab() {
        let Ok(html) = std::fs::read_to_string("tests/data/channel-streams.html") else { return };
        let t = parse_channel_tab(&html, "UCx", 2_000_000_000).unwrap();
        let info = t.info.as_ref().unwrap();
        assert!(info.id.starts_with("UC") && !info.name.is_empty());
        assert!(info.avatar.is_some() && info.banner.is_some() && info.description.is_some(), "{info:?}");
        assert!(info.handle.as_deref().is_some_and(|h| h.starts_with('@')), "{:?}", info.handle);
        assert!(t.videos.len() >= 10, "{}", t.videos.len());
        assert!(t.videos.iter().all(|v| !v.title.is_empty() && v.channel_id == info.id));
        assert!(t.next.is_some());
    }

    #[test]
    fn real_channel_playlists_tab() {
        let Ok(html) = std::fs::read_to_string("tests/data/channel-playlists.html") else { return };
        let t = parse_channel_tab(&html, "UCx", 2_000_000_000).unwrap();
        assert!(t.playlists.len() >= 10, "{}", t.playlists.len());
        assert!(t.playlists.iter().all(|p| !p.id.is_empty() && !p.title.is_empty() && p.channel_id.starts_with("UC")));
        assert!(t.playlists.iter().all(|p| p.channel != "Playlist" && p.channel != "Course"), "{:?}", t.playlists);
    }

    #[test]
    fn real_channel_shorts_tab() {
        let Ok(html) = std::fs::read_to_string("tests/data/channel-shorts.html") else { return };
        let t = parse_channel_tab(&html, "UCx", 2_000_000_000).unwrap();
        assert!(t.videos.len() >= 10, "{}", t.videos.len());
        assert!(t.videos.iter().all(|v| v.id.len() == 11 && !v.title.is_empty() && v.views.is_some()));
    }

    #[test]
    #[ignore]
    fn live_search_filters() {
        let f = Filters { sort: Sort::Views, upload: Upload::Month, duration: Length::Long, kind: Kind::Video, ..Filters::default() };
        let r = search_with("rust tutorial", &f).unwrap();
        let now = crate::db::now();
        println!(
            "page 1: {} videos, {} playlists, {} channels, next {}",
            r.videos.len(),
            r.playlists.len(),
            r.channels.len(),
            r.next.is_some()
        );
        for v in r.videos.iter().take(5) {
            println!("  {:?} views {:?} len {:?} age {}d | {}", v.title, v.views, v.length, (now - v.published) / 86400, v.channel);
        }
        assert!(r.videos.iter().all(|v| v.length.unwrap_or(0.0) > 1200.0 && now - v.published <= 32 * 86400));
        let r2 = search_more(r.next.as_deref().unwrap()).unwrap();
        println!("page 2: {} videos, next {}", r2.videos.len(), r2.next.is_some());
        assert!(!r2.videos.is_empty());
        assert!(r2.videos.iter().all(|v| v.length.unwrap_or(0.0) > 1200.0 && now - v.published <= 32 * 86400));
        assert!(r2.videos.iter().all(|v| !r.videos.iter().any(|o| o.id == v.id)));
        // Sorted by date: newest first.
        let d = search_with("rust tutorial", &Filters { sort: Sort::Date, ..Filters::default() }).unwrap();
        println!("by date: ages {:?}", d.videos.iter().take(8).map(|v| (now - v.published) / 3600).collect::<Vec<_>>());
        // Playlists only.
        let p = search_with("rust tutorial", &Filters { kind: Kind::Playlist, ..Filters::default() }).unwrap();
        println!("playlists only: {} playlists, {} videos; first {:?}", p.playlists.len(), p.videos.len(), p.playlists.first());
        assert!(p.playlists.len() >= 5);
        for (d, lo, hi) in [(Length::Short, 0.0, 180.0), (Length::Medium, 180.0, 1200.0)] {
            let r = search_with("rust tutorial", &Filters { duration: d, ..Filters::default() }).unwrap();
            let lens: Vec<f64> = r.videos.iter().filter_map(|v| v.length).collect();
            println!("{d:?}: {} videos, lengths {:?}", r.videos.len(), &lens[..lens.len().min(8)]);
            assert!(!lens.is_empty() && lens.iter().all(|l| (lo..=hi).contains(l)));
        }
        let l = search_with("news", &Filters { live: true, ..Filters::default() }).unwrap();
        println!("live: {} videos, {} marked live", l.videos.len(), l.live.len());
        assert!(l.live.len() >= l.videos.len() / 2);
        let c = search_with("linus tech tips", &Filters { kind: Kind::Channel, ..Filters::default() }).unwrap();
        println!("channels only: {} channels; first {:?}", c.channels.len(), c.channels.first());
        assert!(!c.channels.is_empty());
    }

    #[test]
    #[ignore]
    fn live_playlist_paging() {
        for id in ["PLWKjhJtqVAbm04DK8TSUCRheRjW2P9TR7", "PLRqwX-V7Uu6ZiZxtDDRCi6uhfTH4FilpH"] {
            let p = playlist(id).unwrap();
            println!("{} by {} ({}): claims {:?}, first page {}", p.title, p.channel, p.channel_id, p.video_count, p.videos.len());
            let mut all = p.videos.clone();
            let mut next = p.next.clone();
            let mut pages = 1;
            while let Some(t) = next {
                let more = playlist_more(&t).unwrap();
                all.extend(more.videos);
                next = more.next;
                pages += 1;
            }
            let mut ids: Vec<&str> = all.iter().map(|v| v.id.as_str()).collect();
            ids.sort();
            ids.dedup();
            println!("  {} videos in {pages} pages, {} unique, last {:?}", all.len(), ids.len(), all.last().map(|v| &v.title));
            assert!(all.len() > 100 && ids.len() == all.len());
        }
    }

    #[test]
    #[ignore]
    fn live_channel_tabs() {
        let cid = "UC8butISFwT-Wl7EV0hUK0BQ";
        let t = channel_tab(cid, Tab::Streams).unwrap();
        println!("info: {:?}", t.info);
        println!("streams page 1: {} videos, live {:?}, upcoming {:?}, next {}", t.videos.len(), t.live, t.upcoming, t.next.is_some());
        for v in t.videos.iter().take(3) {
            println!("  {:?} views {:?} len {:?}", v.title, v.views, v.length);
        }
        let more = channel_more(t.next.as_deref().unwrap()).unwrap();
        println!("streams page 2: {} videos, channel ids {:?}", more.videos.len(), more.videos.first().map(|v| &v.channel_id));
        assert!(!more.videos.is_empty() && more.videos.iter().all(|v| v.channel_id == cid));
        let v = channel_tab(cid, Tab::Videos).unwrap();
        let v2 = channel_more(v.next.as_deref().unwrap()).unwrap();
        println!("videos: {} + {}", v.videos.len(), v2.videos.len());
        let s = channel_tab(cid, Tab::Shorts).unwrap();
        println!("shorts: {}, first {:?}", s.videos.len(), s.videos.first().map(|v| (&v.title, v.views)));
        let p = channel_tab(cid, Tab::Playlists).unwrap();
        println!("playlists: {}, first {:?}, next {}", p.playlists.len(), p.playlists.first(), p.next.is_some());
        let p2 = channel_more(p.next.as_deref().unwrap()).unwrap();
        println!("playlists page 2: {}", p2.playlists.len());
        assert!(!p2.playlists.is_empty());
    }

    /// Needs Tor on 127.0.0.1:9050. Checks that `socks5h://` really goes through the proxy.
    #[test]
    #[ignore]
    fn live_socks_proxy() {
        let body = build_agent(Some("socks5h://127.0.0.1:9050"))
            .get("https://check.torproject.org/api/ip")
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        println!("{body}");
        assert!(body.contains("\"IsTor\":true"));
        // A broken proxy setting must fail, not go direct.
        assert!(build_agent(Some("socks5h://127.0.0.1:1")).get("https://www.youtube.com/").call().is_err());
    }
}
