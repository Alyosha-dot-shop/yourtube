//! Optional "For You" recommendations from an external recommender service.
//!
//! YourTube doesn't rank videos itself. If a recommender is configured
//! (`recommender = http://…` in the config file), YourTube shows its picks and
//! sends back what you clicked and any feedback ("more like this", "not
//! interested", "fewer from this channel"). The API is the one Argus serves:
//!
//! - `GET  /api/recommendations?limit=N` → `{run_id, picks: [{youtube_id, title, channel, channel_id, reason, duration, published, views}]}`
//! - `POST /api/rec/feedback`    `{youtube_id, kind}` → `{id}`
//! - `POST /api/rec/impressions` `{run_id, youtube_ids}`
//! - `POST /api/rec/click`       `{run_id, youtube_id}`
//!
//! All requests are JSON and go only to the configured address.

use crate::db::Item;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    More,
    NotInterested,
}

impl Feedback {
    pub fn kind(self) -> &'static str {
        match self {
            Feedback::More => "more",
            Feedback::NotInterested => "not_interested",
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).http_status_as_error(true).build().new_agent()
}

pub fn picks(base: &str, limit: usize) -> Result<Vec<Item>> {
    let v: Value = serde_json::from_str(
        &agent().get(&format!("{}/api/recommendations?limit={limit}", base.trim_end_matches('/'))).call()?.body_mut().read_to_string()?,
    )?;
    parse_picks(&v)
}

pub fn parse_picks(v: &Value) -> Result<Vec<Item>> {
    let run_id = v["run_id"].as_i64();
    let picks = v["picks"].as_array().ok_or_else(|| anyhow!("no picks in the recommender's answer"))?;
    Ok(picks
        .iter()
        .filter_map(|p| {
            // Older servers only send the watch URL.
            let id = p["youtube_id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| p["url"].as_str()?.split("v=").nth(1).map(|s| s.split('&').next().unwrap_or(s).to_owned()))?;
            Some(Item {
                id,
                title: p["title"].as_str().unwrap_or_default().to_owned(),
                channel: p["channel"].as_str().unwrap_or_default().to_owned(),
                channel_id: p["channel_id"].as_str().unwrap_or_default().to_owned(),
                published: p["published"].as_str().and_then(crate::feed::parse_time),
                views: p["views"].as_i64(),
                length: p["duration"].as_f64().filter(|d| *d > 0.0),
                note: p["reason"].as_str().map(str::to_owned),
                run_id,
                ..Default::default()
            })
        })
        .collect())
}

fn post(base: &str, path: &str, body: Value) -> Result<Value> {
    let text = agent()
        .post(&format!("{}{path}", base.trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .send(body.to_string())?
        .body_mut()
        .read_to_string()?;
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

pub fn feedback(base: &str, youtube_id: &str, kind: Feedback) -> Result<()> {
    post(base, "/api/rec/feedback", json!({"youtube_id": youtube_id, "kind": kind.kind()}))?;
    Ok(())
}

pub fn impressions(base: &str, run_id: i64, ids: &[String]) -> Result<()> {
    post(base, "/api/rec/impressions", json!({"run_id": run_id, "youtube_ids": ids}))?;
    Ok(())
}

pub fn click(base: &str, run_id: i64, youtube_id: &str) -> Result<()> {
    post(base, "/api/rec/click", json!({"run_id": run_id, "youtube_id": youtube_id}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to a real recommender: `YOURTUBE_TEST_RECOMMENDER=http://127.0.0.1:8899 cargo test -- --ignored`.
    /// Point it at a test copy; it sends real feedback ("not interested" on the last pick).
    #[test]
    #[ignore]
    fn live_round_trip() {
        let base = std::env::var("YOURTUBE_TEST_RECOMMENDER").expect("set YOURTUBE_TEST_RECOMMENDER");
        let p = picks(&base, 200).unwrap();
        assert!(p.len() > 5, "{}", p.len());
        let run = p[0].run_id.expect("server sends run_id");
        let ids: Vec<String> = p.iter().take(10).map(|i| i.id.clone()).collect();
        impressions(&base, run, &ids).unwrap();
        click(&base, run, &p[0].id).unwrap();
        let last = p.last().unwrap().id.clone();
        feedback(&base, &last, Feedback::NotInterested).unwrap();
        assert!(picks(&base, 200).unwrap().iter().all(|i| i.id != last), "not-interested pick still listed");
        assert!(feedback(&base, &last, Feedback::More).is_ok());
        assert!(feedback(&base, "not a video id!", Feedback::More).is_err());
    }

    #[test]
    fn reads_new_and_old_pick_shapes() {
        let v = json!({"run_id": 7, "picks": [
            {"youtube_id": "abc", "title": "T", "channel": "C", "channel_id": "UC1", "reason": "New from C",
             "duration": 600, "published": "2026-10-21T14:03:11Z", "views": 5},
            {"title": "Old", "url": "https://www.youtube.com/watch?v=def&t=1", "duration": 0, "published": null}
        ]});
        let p = parse_picks(&v).unwrap();
        assert_eq!(p[0].id, "abc");
        assert_eq!(p[0].run_id, Some(7));
        assert_eq!(p[0].published, Some(1792591391));
        assert_eq!(p[0].note.as_deref(), Some("New from C"));
        assert_eq!(p[1].id, "def");
        assert_eq!(p[1].length, None);
    }
}
