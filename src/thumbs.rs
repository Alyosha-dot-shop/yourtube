//! Thumbnail cache: one JPEG per video in ~/.cache/yourtube/thumbs. In incognito they go to a
//! folder in RAM instead (the session's runtime folder), so nothing you look at lands on disk.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

pub fn path(video_id: &str) -> PathBuf {
    dir().join(format!("{video_id}.jpg"))
}

fn dir() -> PathBuf {
    if crate::db::incognito() { incognito_dir() } else { crate::db::cache_dir().join("thumbs") }
}

fn incognito_dir() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("yourtube-incognito").join("thumbs")
}

/// Drop incognito's thumbnails (when it's turned off, and at start-up after a crash).
pub fn forget_incognito() {
    let _ = std::fs::remove_dir_all(incognito_dir().parent().unwrap_or(&incognito_dir()));
}

/// Empty the whole thumbnail cache (with "Clear history": file names are video ids).
pub fn clear() {
    let d = crate::db::cache_dir().join("thumbs");
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::create_dir_all(&d);
}

/// Downloads thumbnails on a few background threads, one request per video at a time.
pub struct Loader {
    tx: mpsc::Sender<String>,
    pending: Arc<Mutex<HashSet<String>>>,
}

impl Loader {
    /// `done` is called on a worker thread with the video id once its file exists.
    pub fn new(workers: usize, done: impl Fn(String) + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(done);
        let pending = Arc::new(Mutex::new(HashSet::new()));
        let _ = std::fs::create_dir_all(crate::db::cache_dir().join("thumbs"));
        for _ in 0..workers {
            let (rx, done, pending) = (rx.clone(), done.clone(), pending.clone());
            std::thread::spawn(move || {
                loop {
                    let Ok(id) = rx.lock().unwrap().recv() else { break };
                    if download(&id) {
                        done(id.clone());
                    }
                    pending.lock().unwrap().remove(&id);
                }
            });
        }
        Loader { tx, pending }
    }

    pub fn request(&self, video_id: &str) {
        if self.pending.lock().unwrap().insert(video_id.to_owned()) {
            let _ = self.tx.send(video_id.to_owned());
        }
    }
}

fn download(id: &str) -> bool {
    // 480x360 looks sharp on HiDPI (its letterbox bars are cropped by the card); 320x180 is the fallback.
    let fetch = |name: &str| -> Option<Vec<u8>> {
        let url = format!("https://i.ytimg.com/vi/{id}/{name}.jpg");
        crate::yt::agent_for_url(&url).get(&url).call().ok()?.body_mut().read_to_vec().ok()
    };
    let Some(bytes) = fetch("hqdefault").or_else(|| fetch("mqdefault")) else { return false };
    let dest = path(id);
    if let Some(d) = dest.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &dest).is_ok()
}
