//! Everything the video page shows about one video: details, chapters,
//! comments and replies.
//!
//! Details come from the watch page (its `ytInitialData` and
//! `ytInitialPlayerResponse`). Comments come from InnerTube's `next` endpoint,
//! the same call youtube.com makes when you scroll down to them.

// Parsed data carries every field YouTube gives us, even ones no screen shows yet,
// so contributors can use them without touching the parsers.
#![allow(dead_code)]

use crate::feed::Video;
use crate::yt::{self, Proto, best_image, collect, find_text, find_text_under, json_after, parse_count, text};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Details {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub channel_avatar: Option<String>,
    pub channel_verified: bool,
    /// "12M subscribers"
    pub subscribers: Option<String>,
    /// As YouTube writes it under the video: "Jun 8, 2023", "Streamed live on …", "Premieres …".
    pub published_text: Option<String>,
    /// "3 years ago"
    pub published_ago: Option<String>,
    /// Unix time of publishing, from the page's exact date.
    pub published: Option<i64>,
    pub views: Option<i64>,
    pub likes: Option<i64>,
    /// Plain text with line breaks; links and timestamps stay as written.
    pub description: String,
    pub keywords: Vec<String>,
    pub category: Option<String>,
    pub chapters: Vec<Chapter>,
    /// true when the chapters were made by YouTube, not by the uploader.
    pub chapters_auto: bool,
    pub length: Option<f64>,
    pub thumbnail: Option<String>,
    /// Live right now.
    pub live: bool,
    /// A scheduled stream or premiere that hasn't started.
    pub upcoming: bool,
    /// Started out as a live stream (also true for past streams).
    pub was_live: bool,
    /// Unix start time of an upcoming stream/premiere.
    pub scheduled: Option<i64>,
    pub unlisted: bool,
    /// Why YouTube won't play it, e.g. "Sign in to confirm you’re not a bot"
    /// (common over Tor/VPNs; the rest of the page still loads), "Private video".
    pub unplayable: Option<String>,
    /// "827" — the comment count, when YouTube shows it on the page.
    pub comment_count: Option<String>,
    /// Pass to [`comments`]; None when comments are turned off.
    pub comments: Option<String>,
    /// The videos YouTube suggests next to this one (same as `yt::related`).
    pub related: Vec<Video>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    /// Seconds from the start.
    pub start: f64,
    pub title: String,
}

/// Fetch everything about a video from its watch page.
pub fn details(id: &str) -> Result<Details> {
    let html = yt::get_html(&format!("https://www.youtube.com/watch?v={id}&hl=en"))?;
    parse_details(&html, crate::db::now())
}

pub fn parse_details(html: &str, now: i64) -> Result<Details> {
    let data = yt::initial_data(html)?;
    let player = json_after(html, "var ytInitialPlayerResponse = ").unwrap_or(Value::Null);
    let vd = &player["videoDetails"];
    let mf = &player["microformat"]["playerMicroformatRenderer"];
    let results = &data["contents"]["twoColumnWatchNextResults"]["results"]["results"]["contents"];
    let mut primary = &Value::Null;
    let mut secondary = &Value::Null;
    for c in results.as_array().into_iter().flatten() {
        if c["videoPrimaryInfoRenderer"].is_object() {
            primary = &c["videoPrimaryInfoRenderer"];
        }
        if c["videoSecondaryInfoRenderer"].is_object() {
            secondary = &c["videoSecondaryInfoRenderer"];
        }
    }
    let owner = &secondary["owner"]["videoOwnerRenderer"];
    let id = vd["videoId"]
        .as_str()
        .or(data["currentVideoEndpoint"]["watchEndpoint"]["videoId"].as_str())
        .ok_or_else(|| anyhow!("no video on the page (removed, private or blocked?)"))?
        .to_owned();

    // The player's copy of the description is plain text with full links;
    // the page's copy shortens links, so it is only the fallback.
    let description = vd["shortDescription"]
        .as_str()
        .or(secondary["attributedDescription"]["content"].as_str())
        .or(mf["description"]["simpleText"].as_str())
        .unwrap_or_default()
        .to_owned();

    let (chapters, chapters_auto) = chapters(&data, &description);
    let live_details = &mf["liveBroadcastDetails"];
    let scheduled = find_text_under(&player["playabilityStatus"], "scheduledStartTime", |_| true)
        .and_then(|s| s.parse().ok())
        .or_else(|| live_details["startTimestamp"].as_str().and_then(iso_to_unix).filter(|t| *t > now));
    let upcoming = vd["isUpcoming"].as_bool().unwrap_or(false);

    Ok(Details {
        title: vd["title"].as_str().map(str::to_owned).or_else(|| text(&primary["title"])).unwrap_or_default(),
        channel: vd["author"].as_str().map(str::to_owned).or_else(|| text(&owner["title"])).unwrap_or_default(),
        channel_id: vd["channelId"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| find_text_under(&owner["navigationEndpoint"], "browseId", |_| true))
            .unwrap_or_default(),
        channel_avatar: best_image(&owner["thumbnail"]),
        channel_verified: find_text(&owner["badges"], |s| s.contains("VERIFIED")).is_some(),
        subscribers: text(&owner["subscriberCountText"]),
        published_text: text(&primary["dateText"]),
        published_ago: text(&primary["relativeDateText"]),
        published: mf["publishDate"]
            .as_str()
            .or(mf["uploadDate"].as_str())
            .and_then(iso_to_unix)
            .or_else(|| text(&primary["dateText"]).and_then(|t| date_text_to_unix(&t))),
        views: vd["viewCount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| text(&primary["viewCount"]["videoViewCountRenderer"]["viewCount"]).and_then(|s| parse_count(&s))),
        likes: mf["likeCount"].as_str().and_then(|s| s.parse().ok()).or_else(|| likes_from_button(primary)),
        description,
        keywords: vd["keywords"].as_array().map(|k| k.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        category: mf["category"].as_str().map(str::to_owned),
        chapters,
        chapters_auto,
        length: vd["lengthSeconds"].as_str().and_then(|s| s.parse().ok()).filter(|l: &f64| *l > 0.0),
        thumbnail: best_image(&vd["thumbnail"]),
        live: vd["isLive"].as_bool().unwrap_or(false) || live_details["isLiveNow"].as_bool().unwrap_or(false),
        upcoming,
        was_live: vd["isLiveContent"].as_bool().unwrap_or(false),
        scheduled: scheduled.filter(|_| upcoming),
        unlisted: mf["isUnlisted"].as_bool().unwrap_or(false),
        unplayable: match player["playabilityStatus"]["status"].as_str() {
            None | Some("OK") => None,
            Some(status) => Some(player["playabilityStatus"]["reason"].as_str().unwrap_or(status).to_owned()),
        },
        comment_count: comment_count(&data),
        comments: comments_section_token(&data),
        related: yt::related_from(&data, now),
        id,
    })
}

/// "like this video along with 26,474 other people" -> 26474, else the short "26K".
fn likes_from_button(primary: &Value) -> Option<i64> {
    let mut buttons = Vec::new();
    collect(&primary["videoActions"], "likeButtonViewModel", &mut buttons);
    let b = buttons.first()?;
    if let Some(a) = find_text_under(b, "accessibilityText", |s| s.contains("along with")) {
        let n = a.split("along with ").nth(1)?;
        return parse_count(n);
    }
    find_text_under(b, "title", |s| s.starts_with(|c: char| c.is_ascii_digit())).and_then(|s| parse_count(&s))
}

fn comments_section_token(data: &Value) -> Option<String> {
    let mut sections = Vec::new();
    collect(&data["contents"], "itemSectionRenderer", &mut sections);
    let s = sections.into_iter().find(|s| s["sectionIdentifier"].as_str() == Some("comment-item-section"))?;
    yt::next_token(s)
}

fn comment_count(data: &Value) -> Option<String> {
    data["engagementPanels"].as_array()?.iter().find_map(|p| {
        let p = &p["engagementPanelSectionListRenderer"];
        (p["targetId"].as_str() == Some("engagement-panel-comments-section"))
            .then(|| text(&p["header"]["engagementPanelTitleHeaderRenderer"]["contextualInfo"]))
            .flatten()
    })
}

/// The video's chapters, and whether YouTube made them up itself.
///
/// In order of preference: the uploader's chapters as YouTube shows them in
/// the player, the timestamps in the description (YouTube sometimes ignores
/// them over a small mistake, like a correction note further down), and
/// YouTube's automatic chapters.
fn chapters(data: &Value, description: &str) -> (Vec<Chapter>, bool) {
    let mut maps = Vec::new();
    collect(&data["playerOverlays"], "markersMap", &mut maps);
    let mut by_key: Vec<(String, Vec<Chapter>)> = Vec::new();
    for entry in maps.iter().filter_map(|m| m.as_array()).flatten() {
        let key = entry["key"].as_str().unwrap_or_default();
        if !key.contains("CHAPTERS") {
            continue;
        }
        let mut list = Vec::new();
        collect(&entry["value"], "chapterRenderer", &mut list);
        let chapters: Vec<Chapter> = list
            .iter()
            .filter_map(|c| {
                Some(Chapter { start: c["timeRangeStartMillis"].as_f64()? / 1000.0, title: text(&c["title"]).unwrap_or_default() })
            })
            .collect();
        if !chapters.is_empty() {
            by_key.push((key.to_owned(), chapters));
        }
    }
    if let Some((_, c)) = by_key.iter().find(|(k, _)| k == "DESCRIPTION_CHAPTERS") {
        return (c.clone(), false);
    }
    let from_text = description_chapters(description);
    if !from_text.is_empty() {
        return (from_text, false);
    }
    if let Some((_, c)) = by_key.into_iter().next() {
        return (c, true);
    }
    // Last resort: the "Chapters" side panel.
    for p in data["engagementPanels"].as_array().into_iter().flatten() {
        let p = &p["engagementPanelSectionListRenderer"];
        let target = p["targetId"].as_str().unwrap_or_default();
        if !target.contains("chapters") {
            continue;
        }
        let mut items = Vec::new();
        collect(&p["content"], "macroMarkersListItemRenderer", &mut items);
        let list: Vec<Chapter> = items
            .iter()
            .filter_map(|i| {
                Some(Chapter {
                    start: i["onTap"]["watchEndpoint"]["startTimeSeconds"]
                        .as_f64()
                        .or_else(|| text(&i["timeDescription"]).and_then(|t| yt::parse_clock(&t)))?,
                    title: text(&i["title"]).unwrap_or_default(),
                })
            })
            .collect();
        if !list.is_empty() {
            return (list, target.contains("auto"));
        }
    }
    (Vec::new(), false)
}

/// Chapters written into a description, one per line, with the time at the
/// start ("00:06:19 Variables", "(1:02) Intro", "0:00 - Start") or at the end
/// ("Intro 0:00"). Like YouTube, it needs at least two, starting at 0:00; it
/// stops at the first time that goes backwards.
pub fn description_chapters(desc: &str) -> Vec<Chapter> {
    const SEP: &[char] = &['-', '–', '—', ':', '|', '•', '·', ')', ']'];
    let clock = |w: &str| yt::parse_clock(w.trim_matches(|c| "()[]".contains(c)));
    let mut out: Vec<Chapter> = Vec::new();
    for line in desc.lines() {
        let line = line.trim();
        let words: Vec<&str> = line.split_whitespace().collect();
        let (start, title) = match words.as_slice() {
            [first, rest @ ..] if clock(first).is_some() => (clock(first), rest.join(" ")),
            [rest @ .., last] if clock(last).is_some() => (clock(last), rest.join(" ")),
            _ => continue,
        };
        let Some(start) = start else { continue };
        let title = title.trim_matches(|c: char| c.is_whitespace() || SEP.contains(&c)).to_owned();
        match out.last() {
            None if start != 0.0 => continue,
            Some(prev) if start <= prev.start => break,
            _ => out.push(Chapter { start, title }),
        }
    }
    if out.len() >= 2 { out } else { Vec::new() }
}

/// "2023-06-08T05:43:32-07:00", "2023-06-08T12:00:00Z" or "2023-06-08" -> unix time.
pub fn iso_to_unix(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let mut t = (era * 146097 + doe - 719468) * 86400;
    if s.as_bytes().get(10) == Some(&b'T') {
        t += num(11, 13)? * 3600 + num(14, 16)? * 60 + num(17, 19).unwrap_or(0);
        // Skip fractional seconds, then apply the zone offset.
        let rest = s.get(19..).unwrap_or("");
        let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let mi: i64 = rest.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
            let off = h * 3600 + mi * 60;
            t += if sign == '+' { -off } else { off };
        }
    }
    Some(t)
}

/// "Jun 8, 2023", "Streamed live on Jun 8, 2023", "Premiered Jun 8, 2023" -> unix
/// time of that day's midnight UTC. Only used when the exact date is missing.
fn date_text_to_unix(t: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let words: Vec<String> = t.split_whitespace().map(|w| w.trim_end_matches(',').to_lowercase()).collect();
    words.windows(3).find_map(|w| {
        let m = MONTHS.iter().position(|m| w[0].starts_with(m))? + 1;
        let d: u32 = w[1].parse().ok()?;
        let y: u32 = w[2].parse().ok()?;
        iso_to_unix(&format!("{y:04}-{m:02}-{d:02}"))
    })
}

// ---------------------------------------------------------------------------
// Comments

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Top,
    Newest,
}

#[derive(Debug, Clone, Default)]
pub struct Comment {
    pub id: String,
    /// "@handle" (or the display name on older comments).
    pub author: String,
    pub author_id: String,
    pub author_avatar: Option<String>,
    /// The comment is by the video's own channel.
    pub author_is_creator: bool,
    pub author_verified: bool,
    pub text: String,
    /// "1.6K", or empty when nobody liked it.
    pub likes_text: String,
    pub likes: i64,
    /// "3 years ago", "2 days ago (edited)"
    pub published: String,
    pub reply_count: i64,
    pub pinned: bool,
    /// The creator gave it a heart.
    pub hearted: bool,
    /// Pass to [`replies`] to read the replies.
    pub replies: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CommentPage {
    pub comments: Vec<Comment>,
    /// Token for the next page: pass it to [`comments`] (or [`replies`] on a reply page).
    pub next: Option<String>,
    /// "827" — the total count, only on the first page.
    pub count: Option<String>,
}

/// The first page of a video's comments (pass a video id), or a later page
/// (pass [`CommentPage::next`] or [`Details::comments`]). `sort` only applies
/// when starting from a video id: a token already knows its order.
pub fn comments(id_or_token: &str, sort: Sort) -> Result<CommentPage> {
    let token = if is_video_id(id_or_token) { comments_token(id_or_token, sort) } else { id_or_token.to_owned() };
    let v = yt::innertube("next", json!({ "continuation": token }))?;
    Ok(parse_comments(&v))
}

/// Replies to a comment, from [`Comment::replies`]; later pages from [`CommentPage::next`].
pub fn replies(token: &str) -> Result<CommentPage> {
    let v = yt::innertube("next", json!({ "continuation": token }))?;
    Ok(parse_comments(&v))
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The token youtube.com uses to open a video's comment section, built
/// directly so we don't need to load the watch page first. Field 6 of the
/// inner message is the sort order (0 top, 1 newest).
pub fn comments_token(video_id: &str, sort: Sort) -> String {
    let inner = Proto::default().string(4, video_id).varint(6, if sort == Sort::Newest { 1 } else { 0 }).varint(15, 2);
    let msg = Proto::default()
        .message(2, Proto::default().string(2, video_id))
        .varint(3, 6)
        .message(6, Proto::default().message(4, inner).string(8, "comments-section"));
    yt::url_encode(&yt::b64(&msg.0))
}

/// Reads a `next` response holding comments or replies.
///
/// Today's YouTube sends thin `commentViewModel`s that only hold keys; the
/// text, author and counts live in `frameworkUpdates.entityBatchUpdate`
/// "entities" under those keys. Older responses had everything in a
/// `commentRenderer`. Both are handled.
pub fn parse_comments(v: &Value) -> CommentPage {
    let mut entities: HashMap<&str, &Value> = HashMap::new();
    for m in v["frameworkUpdates"]["entityBatchUpdate"]["mutations"].as_array().into_iter().flatten() {
        if let (Some(key), Some(payload)) = (m["entityKey"].as_str(), m["payload"].as_object())
            && let Some(inner) = payload.values().next()
        {
            entities.insert(key, inner);
        }
    }
    let mut page = CommentPage::default();
    for list in ["onResponseReceivedEndpoints", "onResponseReceivedActions"] {
        for e in v[list].as_array().into_iter().flatten() {
            let action = if e["reloadContinuationItemsCommand"].is_object() {
                &e["reloadContinuationItemsCommand"]
            } else {
                &e["appendContinuationItemsAction"]
            };
            for item in action["continuationItems"].as_array().into_iter().flatten() {
                if let Some(h) = item.get("commentsHeaderRenderer") {
                    page.count = h["countText"]["runs"][0]["text"].as_str().map(str::to_owned).or_else(|| text(&h["countText"]));
                } else if let Some(t) = item.get("commentThreadRenderer") {
                    let c = if t["commentViewModel"].is_object() {
                        view_model_comment(&t["commentViewModel"], &entities)
                    } else {
                        old_comment(&t["comment"]["commentRenderer"])
                    };
                    if let Some(mut c) = c {
                        c.replies = yt::next_token(&t["replies"]);
                        page.comments.push(c);
                    }
                } else if let Some(vm) = item.get("commentViewModel") {
                    page.comments.extend(view_model_comment(vm, &entities));
                } else if let Some(r) = item.get("commentRenderer") {
                    page.comments.extend(old_comment(r));
                } else if item.get("continuationItemRenderer").is_some() {
                    page.next = yt::next_token(item);
                }
            }
        }
    }
    page
}

fn view_model_comment(vm: &Value, entities: &HashMap<&str, &Value>) -> Option<Comment> {
    // Threads wrap it once more: {commentViewModel: {commentViewModel: {...}}}.
    let vm = if vm["commentViewModel"].is_object() { &vm["commentViewModel"] } else { vm };
    let e = entities.get(vm["commentKey"].as_str()?)?;
    let state = vm["toolbarStateKey"].as_str().and_then(|k| entities.get(k));
    let props = &e["properties"];
    let author = &e["author"];
    let toolbar = &e["toolbar"];
    let likes_text = toolbar["likeCountNotliked"].as_str().unwrap_or("").trim().to_owned();
    Some(Comment {
        id: props["commentId"].as_str().or(vm["commentId"].as_str()).unwrap_or_default().to_owned(),
        author: author["displayName"].as_str().unwrap_or_default().to_owned(),
        author_id: author["channelId"].as_str().unwrap_or_default().to_owned(),
        author_avatar: author["avatarThumbnailUrl"].as_str().map(str::to_owned),
        author_is_creator: author["isCreator"].as_bool().unwrap_or(false),
        author_verified: author["isVerified"].as_bool().unwrap_or(false),
        text: props["content"]["content"].as_str().unwrap_or_default().to_owned(),
        likes: parse_count(&likes_text).unwrap_or(0),
        likes_text,
        published: props["publishedTime"].as_str().unwrap_or_default().to_owned(),
        reply_count: toolbar["replyCount"].as_str().and_then(parse_count).unwrap_or(0),
        pinned: vm["pinnedText"].as_str().is_some_and(|s| !s.is_empty()),
        hearted: state.is_some_and(|s| s["heartState"].as_str() == Some("TOOLBAR_HEART_STATE_HEARTED")),
        replies: None,
    })
}

fn old_comment(r: &Value) -> Option<Comment> {
    let likes_text = text(&r["voteCount"]).unwrap_or_default().trim().to_owned();
    Some(Comment {
        id: r["commentId"].as_str()?.to_owned(),
        author: text(&r["authorText"]).unwrap_or_default(),
        author_id: r["authorEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or_default().to_owned(),
        author_avatar: best_image(&r["authorThumbnail"]),
        author_is_creator: r["authorIsChannelOwner"].as_bool().unwrap_or(false),
        author_verified: r["authorCommentBadge"].is_object(),
        text: text(&r["contentText"]).unwrap_or_default(),
        likes: parse_count(&likes_text).unwrap_or(0),
        likes_text,
        published: text(&r["publishedTimeText"]).unwrap_or_default(),
        reply_count: r["replyCount"].as_i64().unwrap_or(0),
        pinned: r["pinnedCommentBadge"].is_object(),
        hearted: r["actionButtons"]["commentActionButtonsRenderer"]["creatorHeart"]["creatorHeartRenderer"]["isHearted"].as_bool()
            == Some(true),
        replies: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(iso_to_unix("1970-01-01"), Some(0));
        assert_eq!(iso_to_unix("2023-06-08T05:43:32-07:00"), Some(1686228212));
        assert_eq!(iso_to_unix("2023-06-08T12:43:32Z"), Some(1686228212));
        assert_eq!(iso_to_unix("2023-06-08T12:43:32.123+00:00"), Some(1686228212));
        assert_eq!(iso_to_unix("2000-02-29"), Some(951782400));
        assert_eq!(iso_to_unix("nope"), None);
        assert_eq!(date_text_to_unix("Jun 8, 2023"), iso_to_unix("2023-06-08"));
        assert_eq!(date_text_to_unix("Streamed live on Dec 31, 2020"), iso_to_unix("2020-12-31"));
        assert_eq!(date_text_to_unix("3 years ago"), None);
    }

    #[test]
    fn chapters_in_description() {
        let d = "Intro text\n\n⭐️ Contents ⭐️\n00:00:00 Introduction & Learning Resources\n00:06:19 Variables\n(1:02:03) - Numbers\n\nCorrection:\n0:30 a correction note";
        assert_eq!(
            description_chapters(d),
            vec![
                Chapter { start: 0.0, title: "Introduction & Learning Resources".into() },
                Chapter { start: 379.0, title: "Variables".into() },
                Chapter { start: 3723.0, title: "Numbers".into() },
            ]
        );
        assert_eq!(
            description_chapters("Start 0:00\nMiddle part – 4:05"),
            vec![Chapter { start: 0.0, title: "Start".into() }, Chapter { start: 245.0, title: "Middle part".into() },]
        );
        // One timestamp, or none at 0:00, is not a chapter list.
        assert!(description_chapters("Best bit at 3:15").is_empty());
        assert!(description_chapters("1:00 a\n2:00 b").is_empty());
    }

    #[test]
    fn comment_tokens() {
        assert!(is_video_id("BpPEoZW5IiY") && !is_video_id("Eg0SC0JwUEVvWlc1SWlZGAYy"));
        // Same bytes as the token on the real watch page (which is "Top").
        assert_eq!(
            comments_token("BpPEoZW5IiY", Sort::Top),
            "Eg0SC0JwUEVvWlc1SWlZGAYyJSIRIgtCcFBFb1pXNUlpWTAAeAJCEGNvbW1lbnRzLXNlY3Rpb24%3D"
        );
        assert_ne!(comments_token("BpPEoZW5IiY", Sort::Newest), comments_token("BpPEoZW5IiY", Sort::Top));
    }

    /// Hand-made, trimmed copy of today's comment format (view models + entities).
    #[test]
    fn comment_entities_fixture() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/yt/comments-entities.json")).unwrap();
        let p = parse_comments(&v);
        assert_eq!(p.count.as_deref(), Some("827"));
        assert_eq!(p.next.as_deref(), Some("NEXT_PAGE_TOKEN"));
        assert_eq!(p.comments.len(), 2);
        let c = &p.comments[0];
        assert_eq!(c.id, "Ugw4HW6FBuu65NAh2fJ4AaABAg");
        assert_eq!(c.author, "@zubiarfan");
        assert_eq!(c.author_id, "UC_ne_iOUtUPtKiBasvWgz_g");
        assert!(c.text.starts_with("Thank you so much") && c.text.contains('\n'));
        assert_eq!((c.likes_text.as_str(), c.likes), ("1.6K", 1600));
        assert_eq!(c.published, "3 years ago");
        assert_eq!(c.reply_count, 46);
        assert!(c.pinned && c.hearted && !c.author_is_creator);
        assert_eq!(c.replies.as_deref(), Some("REPLIES_TOKEN"));
        let c = &p.comments[1];
        assert!(!c.pinned && !c.hearted && c.author_is_creator);
        assert_eq!((c.likes, c.reply_count, c.replies.as_deref()), (0, 0, None));
    }

    /// Reply pages: bare view models and a "Show more replies" button.
    #[test]
    fn reply_page_fixture() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/yt/comments-replies.json")).unwrap();
        let p = parse_comments(&v);
        assert_eq!(p.comments.len(), 1);
        assert_eq!(p.comments[0].text, "A reply");
        assert_eq!(p.next.as_deref(), Some("MORE_REPLIES_TOKEN"));
    }

    /// The older `commentRenderer` shape.
    #[test]
    fn old_comment_fixture() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/yt/comments-renderer.json")).unwrap();
        let p = parse_comments(&v);
        assert_eq!(p.comments.len(), 1);
        let c = &p.comments[0];
        assert_eq!((c.author.as_str(), c.author_id.as_str()), ("@someone", "UCabc"));
        assert_eq!(c.text, "Old style comment");
        assert_eq!((c.likes, c.reply_count), (12, 3));
        assert!(c.pinned && c.hearted);
        assert_eq!(c.replies.as_deref(), Some("OLD_REPLIES"));
    }

    #[test]
    fn real_watch_page_details() {
        // A long video with chapters in its description and comments turned on.
        let Ok(html) = std::fs::read_to_string("tests/data/watch-chapters.html") else { return };
        let d = parse_details(&html, 2_000_000_000).unwrap();
        assert_eq!(d.id.len(), 11);
        assert!(!d.title.is_empty() && !d.channel.is_empty() && d.channel_id.starts_with("UC"));
        assert!(d.channel_avatar.is_some() && d.subscribers.is_some());
        assert!(d.published.is_some() && d.published_text.is_some());
        assert!(d.views.is_some() && d.likes.is_some() && d.length.is_some());
        assert!(d.description.contains('\n'));
        assert!(d.chapters.len() >= 3, "{:?}", d.chapters);
        assert_eq!(d.chapters[0].start, 0.0);
        assert!(d.chapters.windows(2).all(|w| w[0].start < w[1].start));
        assert!(d.comments.is_some());
        assert!(!d.related.is_empty());
        assert!(!d.live && !d.upcoming && d.unplayable.is_none());
    }

    #[test]
    fn real_watch_page_bot_check() {
        // The same video fetched over Tor: YouTube's "confirm you're not a bot"
        // page, whose player data is empty but whose page data is still there.
        let Ok(html) = std::fs::read_to_string("tests/data/watch-botcheck.html") else { return };
        let d = parse_details(&html, 2_000_000_000).unwrap();
        assert!(d.unplayable.as_deref().is_some_and(|r| r.contains("bot")), "{:?}", d.unplayable);
        assert_eq!(d.id.len(), 11);
        assert!(!d.title.is_empty() && d.channel_id.starts_with("UC") && !d.description.is_empty());
        assert!(d.published.is_some() && d.views.is_some());
        assert!(d.chapters.len() >= 3 && d.comments.is_some() && !d.related.is_empty());
    }

    /// Hits the live site; run with `cargo test -- --ignored --nocapture live_`.
    #[test]
    #[ignore]
    fn live_video_page() {
        let d = details("BpPEoZW5IiY").unwrap();
        println!(
            "{} | {} ({}) {:?} | {:?} {:?} | views {:?} likes {:?} | len {:?}",
            d.title, d.channel, d.channel_id, d.subscribers, d.published_text, d.published, d.views, d.likes, d.length
        );
        println!("chapters ({}, auto {}): {:?}", d.chapters.len(), d.chapters_auto, &d.chapters[..d.chapters.len().min(4)]);
        println!("keywords {:?} category {:?} comment count {:?} related {}", d.keywords, d.category, d.comment_count, d.related.len());
        assert!(d.chapters.len() >= 3);
        let token = d.comments.clone().unwrap();
        let p = comments(&token, Sort::Top).unwrap();
        println!("page 1: {} comments, count {:?}, next {}", p.comments.len(), p.count, p.next.is_some());
        for c in &p.comments {
            println!(
                "  {} [{}] likes {} replies {} pinned {} hearted {} creator {}: {:.60}",
                c.author,
                c.published,
                c.likes_text,
                c.reply_count,
                c.pinned,
                c.hearted,
                c.author_is_creator,
                c.text.replace('\n', " ")
            );
        }
        assert!(p.comments.len() >= 20);
        let p2 = comments(p.next.as_deref().unwrap(), Sort::Top).unwrap();
        println!("page 2: {} comments, next {}", p2.comments.len(), p2.next.is_some());
        assert!(!p2.comments.is_empty());
        assert!(p2.comments.iter().all(|c| !p.comments.iter().any(|o| o.id == c.id)));
        // Built token, newest first.
        let n = comments("BpPEoZW5IiY", Sort::Newest).unwrap();
        println!("newest: {} comments, first {:?}", n.comments.len(), n.comments.iter().take(3).map(|c| &c.published).collect::<Vec<_>>());
        assert!(!n.comments.is_empty());
        let with_replies = p.comments.iter().find(|c| c.replies.is_some()).unwrap();
        let r = replies(with_replies.replies.as_deref().unwrap()).unwrap();
        println!("replies to {}: {} (of {}), more {}", with_replies.author, r.comments.len(), with_replies.reply_count, r.next.is_some());
        assert!(!r.comments.is_empty());
    }

    #[test]
    #[ignore]
    fn live_stream_details() {
        let f = yt::Filters { live: true, ..yt::Filters::default() };
        let r = yt::search_with("news", &f).unwrap();
        let id = r.live.first().expect("a live stream");
        let d = details(id).unwrap();
        println!(
            "{} | live {} upcoming {} was_live {} views {:?} len {:?} unplayable {:?}",
            d.title, d.live, d.upcoming, d.was_live, d.views, d.length, d.unplayable
        );
        assert!(d.live && d.was_live && !d.upcoming);
    }
}
