//! YourTube: a lightweight YouTube subscriptions app. GTK4 window, mpv for playback.
//!
//! Usage:
//!   yourtube                 open the window (add --profile NAME or --incognito)
//!   yourtube import [PATH]   import a Google Takeout zip, NewPipe or FreeTube export
//!   yourtube refresh         refresh the subscription feed from the command line
//!   yourtube foryou          rebuild the For You page from the command line
//!   yourtube eval [WEEKS]    how well For You's scoring predicts what you watched
//!   yourtube serve           run the web version for your home network
//!   yourtube web-password    set the web version's password (asked twice, stored as a hash)
//!   yourtube sync-key        on your server: make the key devices sync with (shown once)
//!   yourtube sync            sync history, subscriptions and playlists with your server now
//!   yourtube play VIDEO_ID   play one video in mpv, resuming from saved progress

// A server build leaves out the window, so code only the window calls looks unused there.
// The normal build still warns about real dead code.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

mod bridge;
mod db;
mod feed;
mod import;
mod player;
mod proc;
mod report;
mod thumbs;
#[cfg(feature = "gui")]
mod ui;
mod yt;

mod config;
mod download;
mod export;
mod extras;
mod fmt;
mod foryou;
mod links;
mod sponsorblock;
mod sync;
mod video;
mod web;

use anyhow::Result;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Options that work with every command.
    let cfg = config::load();
    db::set_profile(take_value(&mut args, "--profile").or(cfg.profile).filter(|p| db::profiles().contains(p)));
    db::set_incognito(take_flag(&mut args, "--incognito"));
    match args.first().map(String::as_str) {
        Some("import") => {
            let path = args.get(1).map(Into::into).unwrap_or_else(import::freetube_dir);
            let mut conn = db::open()?;
            let n = import::import_any(&mut conn, &path)?;
            println!(
                "Imported {} subscriptions, {} watched videos, {} searches, {} playlists ({} videos); skipped {}.",
                n.subscriptions, n.history, n.searches, n.playlists, n.playlist_items, n.skipped
            );
        }
        Some("refresh") => feed::refresh(|p| eprintln!("{p:?}"))?,
        Some("serve") => web::serve(&config::load())?,
        Some("web-password") => set_web_password()?,
        Some("sync-key") => {
            let (key, hash) = sync::new_key()?;
            let mut c = config::load();
            c.sync_key_hash = hash;
            config::save(&c)?;
            println!("Sync key (shown only now; the server keeps just its hash):\n\n    {key}\n");
            println!("Put it in YourTube on your computer: Preferences → Sync. Restart `yourtube serve` to use it.");
            println!("Running this again makes a new key, and devices with the old one stop syncing.");
        }
        Some("sync") => {
            let s = sync::sync_now(&config::load())?;
            println!(
                "Sent {} changes ({} taken by the server), got {} ({} applied here).",
                s.sent, s.taken_by_server, s.received, s.applied
            );
        }
        Some("foryou") => {
            let s = foryou::refresh(&config::load(), |s| eprintln!("{s}"))?;
            println!("{} picks from {} candidates ({} requests failed).", s.picks, s.candidates, s.failed_requests);
            println!("Candidates by source: {}", s.by_source.iter().map(|(n, c)| format!("{n} {c}")).collect::<Vec<_>>().join(", "));
            if let Some(e) = s.ai_error {
                println!("AI wasn't used: {e}");
            }
            if let Some(e) = s.model_error {
                println!("The topic model wasn't used: {e}");
            }
        }
        Some("eval") => {
            let weeks = args.get(1).and_then(|w| w.parse().ok()).unwrap_or(8);
            // The installed topic model, or another Model2Vec model folder in YOURTUBE_MODEL to compare.
            let model = match std::env::var_os("YOURTUBE_MODEL") {
                Some(d) => Some(foryou::embed::Model::load(std::path::Path::new(&d))?),
                None => foryou::embed::load_installed(),
            };
            if model.is_none() {
                println!("(The topic model isn't installed, so only the word-based scorers run. Turn it on in Preferences → For You.)\n");
            }
            let r = foryou::eval::run(&db::open()?, weeks, model.as_ref())?;
            println!("{} weeks, {} watched videos, each week against 1000 random unwatched ones.\n", r.weeks, r.positives);
            println!("{:<28} {:>6} {:>11}", "scorer", "AUC", "recall@100");
            for row in r.rows {
                println!("{:<28} {:>6.3} {:>11.3}", row.name, row.auc, row.recall_100);
            }
            println!("\nAUC 0.5 is a coin flip. Compare scorers with each other: see src/foryou/eval.rs for the limits.");
        }
        Some("play") => {
            let id = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: yourtube play VIDEO_ID"))?;
            player::play(id, id, |e| {
                if let player::Event::Exited { summary, .. } = e {
                    eprintln!("{summary}");
                }
            })?;
        }
        Some("-h" | "--help" | "help") => println!(
            "{}",
            include_str!("main.rs").lines().skip(2).take(11).map(|l| l.trim_start_matches("//!")).collect::<Vec<_>>().join("\n")
        ),
        Some(other) if links::video_id_from(other).is_none() => anyhow::bail!("unknown command {other:?}; try yourtube --help"),
        #[cfg(feature = "gui")]
        _ => {
            first_run_import()?;
            ui::run(&args);
        }
        // A server build (--no-default-features) has no window; `serve` and the other commands still work.
        #[cfg(not(feature = "gui"))]
        _ => anyhow::bail!("this build has no window (built without the \"gui\" feature); try yourtube serve"),
    }
    Ok(())
}

/// On the very first start, pull in FreeTube's data if it exists.
#[cfg(feature = "gui")]
fn first_run_import() -> Result<()> {
    let mut conn = db::open()?;
    let empty: bool = conn.query_row("SELECT COUNT(*) = 0 FROM channels", [], |r| r.get(0))?;
    let dir = import::freetube_dir();
    if empty && dir.join("profiles.db").exists() {
        import::import(&mut conn, &dir)?;
    }
    Ok(())
}

/// Ask for the web version's password (twice, not shown, when typed in a terminal) and save its hash.
/// Without a terminal it reads one line, so `printf '%s\n' "$pw" | yourtube web-password` works too.
fn set_web_password() -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let tty = std::io::stdin().is_terminal();
    let echo = |on: bool| {
        if tty {
            let _ =
                std::process::Command::new("stty").arg(if on { "echo" } else { "-echo" }).stdin(std::process::Stdio::inherit()).status();
        }
    };
    let ask = |prompt: &str| -> Result<String> {
        if tty {
            eprint!("{prompt}");
            std::io::stderr().flush()?;
        }
        echo(false);
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        echo(true);
        if tty {
            eprintln!();
        }
        read?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    };
    let pw = ask("New password for the web version: ")?;
    if pw.chars().count() < 8 {
        anyhow::bail!("use at least 8 characters; nothing was changed");
    }
    if tty && ask("Once more: ")? != pw {
        anyhow::bail!("the two didn't match; nothing was changed");
    }
    let mut c = config::load();
    c.web_password = pw;
    config::save(&c)?; // stores it as an argon2id hash
    println!("Saved (only a hash is stored). Restart `yourtube serve` to use it.");
    Ok(())
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}
