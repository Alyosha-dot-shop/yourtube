//! Reading a video id out of the links and text people paste or open.

/// The video id in a tube://, youtube.com or youtu.be link, or a bare 11-character id.
pub fn video_id_from(s: &str) -> Option<String> {
    let valid = |id: &str| id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let s = s.trim();
    if valid(s) {
        return Some(s.to_owned());
    }
    let after = |marker: &str| {
        s.split(marker)
            .nth(1)
            .map(|rest| rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>())
    };
    ["v=", "youtu.be/", "/shorts/", "/live/", "/embed/"].iter().filter(|m| s.contains(*m)).find_map(|m| after(m)).filter(|id| valid(id))
}

#[cfg(test)]
mod tests {
    use super::video_id_from;

    #[test]
    fn links() {
        let id = Some("dQw4w9WgXcQ".to_owned());
        assert_eq!(video_id_from("yourtube://watch?v=dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("tube://watch?v=dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42"), id);
        assert_eq!(video_id_from("https://youtu.be/dQw4w9WgXcQ?si=x"), id);
        assert_eq!(video_id_from("https://www.youtube.com/shorts/dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("dQw4w9WgXcQ"), id);
        assert_eq!(video_id_from("cooking"), None);
        assert_eq!(video_id_from("tube://watch?v=short"), None);
    }
}
