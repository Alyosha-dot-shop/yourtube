//! Optional AI help for For You. Off unless you turn it on.
//!
//! Works with any server that speaks the OpenAI-style `/chat/completions` API:
//! a local model (Ollama: `http://127.0.0.1:11434/v1`, LM Studio, llama.cpp's
//! server) or a hosted one (OpenAI, OpenRouter, …). You pick the address, the
//! model, and whether a key is needed.
//!
//! What is sent, and only when For You refreshes:
//! - your recent YouTube searches (up to 20),
//! - titles and channels of videos you recently watched (up to 30),
//! - titles you marked "not interested" (up to 10),
//! - for re-ranking, the titles and channels of up to 40 candidate videos.
//!
//! The model does two things: suggests new searches to explore beyond what you
//! already watch, and re-orders the best candidates with a one-line reason each.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct AiConfig {
    /// Base URL ending in /v1 (or wherever /chat/completions lives).
    pub url: String,
    pub model: String,
    pub api_key: Option<String>,
}

fn chat(cfg: &AiConfig, system: &str, user: &str) -> Result<String> {
    let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(120))).http_status_as_error(true).build().new_agent();
    let mut req = agent.post(&format!("{}/chat/completions", cfg.url.trim_end_matches('/'))).header("Content-Type", "application/json");
    if let Some(k) = cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.header("Authorization", &format!("Bearer {k}"));
    }
    let body = json!({
        "model": cfg.model,
        "temperature": 0.4,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
    });
    let text = req.send(body.to_string())?.body_mut().read_to_string()?;
    let v: Value = serde_json::from_str(&text)?;
    v["choices"][0]["message"]["content"].as_str().map(str::to_owned).ok_or_else(|| anyhow!("the model's answer had no text"))
}

/// The first JSON object in a model's answer (models like to wrap it in prose or ``` fences).
pub fn extract_json(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s[start..].char_indices() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&s[start..start + i + 1]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

const SYSTEM: &str = "You help a person find YouTube videos they will enjoy. You only see what they searched and watched recently. \
Answer with JSON only, no other text.";

/// New searches worth running, each with a short reason.
pub fn explore(cfg: &AiConfig, searches: &[String], watched: &[(String, String)], disliked: &[String]) -> Result<Vec<(String, String)>> {
    let user = format!(
        "Recent searches (newest first):\n{}\n\nRecently watched (title — channel):\n{}\n\nNot interested in:\n{}\n\n\
         Suggest 6 YouTube search queries. Four should follow their current interests more deeply; two should be \
         adjacent topics they would probably like but haven't searched. Keep queries short (2–6 words), no quotes.\n\
         Reply as {{\"queries\": [{{\"query\": \"...\", \"why\": \"under 12 words\"}}]}}",
        list(searches.iter().take(20)),
        list(watched.iter().take(30).map(|(t, c)| format!("{t} — {c}"))),
        list(disliked.iter().take(10)),
    );
    let v = extract_json(&chat(cfg, SYSTEM, &user)?).ok_or_else(|| anyhow!("the model didn't answer in JSON"))?;
    Ok(v["queries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|q| Some((q["query"].as_str()?.trim().to_owned(), q["why"].as_str().unwrap_or("").trim().to_owned())))
                .filter(|(q, _)| !q.is_empty() && q.len() <= 80)
                .take(8)
                .collect()
        })
        .unwrap_or_default())
}

/// Re-order candidates: returns (index into `cands`, reason), best first.
pub fn rerank(cfg: &AiConfig, interests: &[String], cands: &[(String, String)]) -> Result<Vec<(usize, String)>> {
    let numbered: Vec<String> = cands.iter().enumerate().map(|(i, (t, c))| format!("{i}. {t} — {c}")).collect();
    let user = format!(
        "Their interests lately: {}\n\nCandidate videos:\n{}\n\n\
         Pick the 20 they would most want to watch next, best first. Prefer variety over many similar videos. \
         Reply as {{\"picks\": [{{\"i\": <number>, \"why\": \"under 12 words, addressed to them\"}}]}}",
        interests.join(", "),
        numbered.join("\n"),
    );
    let v = extract_json(&chat(cfg, SYSTEM, &user)?).ok_or_else(|| anyhow!("the model didn't answer in JSON"))?;
    let mut seen = std::collections::HashSet::new();
    Ok(v["picks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p["i"].as_u64()? as usize, p["why"].as_str().unwrap_or("").trim().to_owned())))
                .filter(|(i, _)| *i < cands.len() && seen.insert(*i))
                .collect()
        })
        .unwrap_or_default())
}

/// Check the settings work: one tiny request.
pub fn test(cfg: &AiConfig) -> Result<String> {
    let answer = chat(cfg, "Reply with JSON only.", "Reply with {\"ok\": true}")?;
    match extract_json(&answer) {
        Some(_) => Ok(format!("{} answered", cfg.model)),
        None => Err(anyhow!("{} answered, but not in JSON: {}", cfg.model, answer.chars().take(80).collect::<String>())),
    }
}

fn list(items: impl Iterator<Item = impl AsRef<str>>) -> String {
    let v: Vec<String> = items.map(|s| format!("- {}", s.as_ref())).collect();
    if v.is_empty() { "(none)".into() } else { v.join("\n") }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// A one-request fake chat server. Returns its base URL and a handle yielding the request body.
    pub fn fake_server(reply_content: &str) -> (String, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", l.local_addr().unwrap());
        let body = json!({"choices": [{"message": {"role": "assistant", "content": reply_content}}]}).to_string();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0;
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let lower = line.to_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if lower.starts_with("authorization:") {
                    auth = line.trim().to_owned();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut req = vec![0; len];
            r.read_exact(&mut req).unwrap();
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            format!("{auth}\n{}", String::from_utf8(req).unwrap())
        });
        (url, h)
    }

    #[test]
    fn json_in_prose_and_fences() {
        assert_eq!(extract_json("Sure!\n```json\n{\"a\": {\"b\": \"}\"}}\n```").unwrap()["a"]["b"].as_str(), Some("}"));
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn explore_round_trip() {
        let (url, h) = fake_server(
            "```json\n{\"queries\": [{\"query\": \"e30 turbo manifold\", \"why\": \"you searched e30 turbo\"}, {\"query\": \"\", \"why\": \"x\"}]}\n```",
        );
        let cfg = AiConfig { url, model: "m".into(), api_key: Some("k123".into()) };
        let q = explore(&cfg, &["e30 turbo".into()], &[("E30 build".into(), "Garage".into())], &[]).unwrap();
        assert_eq!(q, vec![("e30 turbo manifold".to_owned(), "you searched e30 turbo".to_owned())]);
        let sent = h.join().unwrap();
        assert!(sent.to_lowercase().starts_with("authorization: bearer k123"), "{sent}");
        assert!(sent.contains("e30 turbo") && sent.contains("E30 build — Garage") && sent.contains("\"model\":\"m\""));
    }

    #[test]
    fn rerank_drops_bad_indexes_and_repeats() {
        let (url, _h) = fake_server(
            "{\"picks\": [{\"i\": 1, \"why\": \"a\"}, {\"i\": 9, \"why\": \"b\"}, {\"i\": 1, \"why\": \"c\"}, {\"i\": 0, \"why\": \"d\"}]}",
        );
        let cfg = AiConfig { url, model: "m".into(), api_key: None };
        let r = rerank(&cfg, &["e30".into()], &[("A".into(), "x".into()), ("B".into(), "y".into())]).unwrap();
        assert_eq!(r, vec![(1, "a".to_owned()), (0, "d".to_owned())]);
    }
}
