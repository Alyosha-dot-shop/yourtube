//! Starting yt-dlp and mpv.
//!
//! Both run from YourTube's own private folder, never from wherever YourTube was started:
//! yt-dlp reads a `yt-dlp.conf` from its working directory, and such a file can hold
//! `--exec`, which runs any command. yt-dlp also gets flags that keep a config file from
//! widening what it may do: no `--exec`, no downloading of extra JavaScript, and Deno
//! (sandboxed: no file or network access) as the only JavaScript runtime.

use std::path::PathBuf;
use std::process::Command;

/// yt-dlp flags that hold whatever a config file says.
pub const YTDLP_SAFE: [&str; 5] = ["--no-exec", "--no-remote-components", "--no-js-runtimes", "--js-runtimes", "deno"];

/// The same for yt-dlp started by mpv's ytdl hook, as `--ytdl-raw-options-append` values.
/// mpv passes these in no fixed order, so "--no-js-runtimes" (which must come first) is left out.
pub const MPV_YTDL_SAFE: [&str; 3] = ["no-exec=", "no-remote-components=", "js-runtimes=deno"];

/// The private working folder (created 0700 on Unix).
pub fn run_dir() -> PathBuf {
    let dir = dirs::state_dir().or_else(dirs::data_local_dir).unwrap_or_else(std::env::temp_dir).join("yourtube").join("run");
    if std::fs::create_dir_all(&dir).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    dir
}

/// `yt-dlp` with the safe flags, started in the private folder.
pub fn ytdlp() -> Command {
    let mut c = Command::new("yt-dlp");
    c.current_dir(run_dir()).args(YTDLP_SAFE);
    c
}

/// `mpv`, started in the private folder (its yt-dlp inherits it).
pub fn mpv() -> Command {
    let mut c = Command::new("mpv");
    c.current_dir(run_dir());
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_start_in_the_private_folder() {
        let y = ytdlp();
        assert_eq!(y.get_current_dir(), Some(run_dir().as_path()));
        let args: Vec<_> = y.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, YTDLP_SAFE);
        assert_eq!(mpv().get_current_dir(), Some(run_dir().as_path()));
        assert!(run_dir().ends_with("yourtube/run"));
    }
}
