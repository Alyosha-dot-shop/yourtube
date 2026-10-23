<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="data/icons/yourtube-wordmark-dark.svg">
    <img src="data/icons/yourtube-wordmark.svg" alt="YourTube" width="420">
  </picture>
</p>

<p align="center">A YouTube app that keeps your history and recommendations on your own computer.</p>

---

Hi, I'm Alyosha. I didn't want my watch history and recommendations living on someone else's
servers, and I didn't want a browser-sized app just to watch a video. So I built YourTube. It
runs on your computer, keeps everything in one SQLite file, and tells you why it recommends
each video.

I made it in the same spirit as [degoog](https://github.com/degoog-org/degoog) and the other
de-Google projects that have been showing up lately: your own copy, running on your own
machine, instead of a Google service.

No account, no subscription, no tracking. It's free and GPL-licensed, so change it however
you like.

## What it does

- **For You**: Recommendations from all of YouTube, worked out on your computer from what you
  search and watch. Each pick says why it's there ("Because you searched…"). *More like this*
  and *Not interested* change what comes next. An optional small model matches topics by
  meaning, and `yourtube eval` measures how well it all predicts what you watch.
- **Optional AI**: A small local model can suggest searches and sort the picks. Cloud models
  work too.
- **Subscriptions**, newest first, without Shorts.
- **Resume**: Your position is saved every few seconds, so a crash doesn't send you back to 0:00.
- **Search** with filters, channels with tabs, playlists, Watch later and history.
- **Tabs**: Middle-click a video (or press T) to open it in a new tab.
- **Hide a channel** everywhere with one key.
- **Profiles** for each person on the computer, and **incognito**, which saves nothing.
- **Themes**, accent colour, card size, your own CSS, and shortcuts you can rebind.
- **From FreeTube**: SponsorBlock, downloads that play offline, sound-only mode, chapters,
  comments, Return YouTube Dislike and DeArrow.
- **Tor**: Searches, browsing and even playback can go through Tor, each kind on its own
  circuit (see below).
- **Web version** for the rest of the house (see below).
- **Export** everything to JSON, CSV and OPML.
- **Bug reports**: Ctrl+Shift+B takes a screenshot and opens a filled-in GitHub issue.

It's one GTK 4 window, and videos play in [mpv](https://mpv.io).

## Download

Ready-made builds are on the
[Releases page](https://github.com/Alyosha-dot-shop/yourtube/releases). Every platform also
needs [mpv](https://mpv.io) (plays the videos), [yt-dlp](https://github.com/yt-dlp/yt-dlp)
(finds them) and [Deno](https://deno.com) (yt-dlp needs it for YouTube).

- **Windows**: unzip `yourtube-windows-x86_64.zip` and run `YourTube\yourtube.exe`. GTK comes
  inside the zip; install mpv, yt-dlp and Deno so they're on your PATH.
- **Linux**: `yourtube-linux-x86_64` needs GTK 4.20+ and libadwaita 1.8+, which recent
  distributions have. On Arch: `sudo pacman -S gtk4 libadwaita mpv yt-dlp deno`, then
  `chmod +x yourtube-linux-x86_64` and run it.
- **macOS** (Apple Silicon): `brew install gtk4 libadwaita mpv yt-dlp deno`, then
  `chmod +x yourtube-macos` and `xattr -d com.apple.quarantine yourtube-macos` (it isn't
  signed by Apple), and run it.

I use it daily on Linux. The Windows and macOS builds pass the same tests but get far less real
use, so please tell me what breaks. To check a download, compare it with `SHA256SUMS` on the
release page, or run
`gh attestation verify <file> --repo Alyosha-dot-shop/yourtube`.

## Build it yourself

You need GTK 4.20+, libadwaita 1.8+, mpv, a recent yt-dlp, Deno and Rust.

```sh
git clone https://github.com/Alyosha-dot-shop/yourtube && cd yourtube
cargo build --release
./target/release/yourtube
```

For the app menu entry and `yourtube://` links, copy `data/io.github.yourtube.YourTube.desktop`
to `~/.local/share/applications/`, the icons in `data/icons/` to
`~/.local/share/icons/hicolor/`, and run
`xdg-mime default io.github.yourtube.YourTube.desktop x-scheme-handler/yourtube`.

## Importing

Menu → Import (Ctrl+I) reads:

- a **Google Takeout** zip (pick only *YouTube and YouTube Music* at
  [takeout.google.com](https://takeout.google.com)): subscriptions, watch and search history,
  playlists;
- a **NewPipe** subscriptions export or backup;
- a **FreeTube** folder or export.

Importing twice doesn't create duplicates.

## How For You works

The code is in [`src/foryou/`](src/foryou).

1. It builds a profile from your searches (weighted most, half-life 10 days), what you watched
   and how much of it (half-life 21 days), your feedback and your subscriptions.
2. It searches YouTube for your recent searches, reads the related videos next to what you
   watched, and checks your subscriptions for new uploads. That's about 20 requests.
3. It scores each video: topic match 45%, how it was found 15%, how much you watch the channel
   15%, how new 15%, how popular 10%. Then it allows at most two videos per channel and four
   per reason, and makes at least a third come from channels you don't follow.

Half-watched videos come first.

### Matching by meaning (optional)

Word matching misses that "home server build" and "self-hosting on an old PC" are the same
topic. Preferences → For You → *Match topics by meaning* downloads a small model once
([potion-base-8M](https://huggingface.co/minishlab/potion-base-8M), 30 MB, MIT, pinned and
checksum-checked) and runs it on your computer. Each video is then also scored by how close it
is to the nearest of your 300 strongest searches and watched titles.

### Measuring it

`yourtube eval` checks how well the scoring predicts what you actually watched: for each of the
last 8 weeks it builds your profile from what came before, then scores the videos you watched
that week against 1,000 random ones you didn't. On my own history (about 17,900 watches):

| Scoring | AUC | Recall@100 |
|---|---|---|
| Random | 0.50 | 0.10 |
| Words and channels (default) | 0.73 | 0.57 |
| With the meaning model | 0.82 | 0.54 |

AUC is the chance a real watch scores above a random video. Every change to the scoring gets
checked against this first.

### AI

In Preferences, AI can suggest new searches and sort the top 40 videos. Anything with an
OpenAI-style `/chat/completions` API works. I recommend a local model, because then nothing
about what you watch leaves your computer, and a small one is enough:

| Size | Examples | Memory | Runs on |
|---|---|---|---|
| 1–4B | Qwen3 4B, Gemma 3 4B, Llama 3.2 3B | ~3 GB | any recent laptop, CPU is fine |
| 7–8B | Qwen3 8B, Llama 3.1 8B | ~6 GB | 16 GB RAM or a 6 GB+ graphics card |

With [Ollama](https://ollama.com), run `ollama pull qwen3:4b`, then choose *Ollama* in Preferences
and type `qwen3:4b`. LM Studio and llama.cpp's server work too.

A cloud model receives, on each refresh, up to 20 recent searches, 30 recently watched titles
and channels, 10 titles you rejected and 40 candidate titles. If the AI is down, For You is
made without it.

You can also point Preferences at your own recommender; the API is in
[`src/foryou/remote.rs`](src/foryou/remote.rs).

## Web version

`yourtube serve` serves the same subscriptions, For You, history and playlists to any browser,
and streams the videos through itself. A Raspberry Pi 4 or an old laptop is enough.

1. Set an address and password in `~/.config/yourtube/config`:
   ```text
   web_address = 0.0.0.0:8723
   web_password = something long
   ```
2. Run `yourtube serve`, or make it a systemd service.
3. Open `http://<that machine>:8723`. If your router or Pi-hole does local DNS, give it a name
   like `yourtube.home`.

Without an address it only listens on the machine itself. Each browser picks its own profile
and can turn on incognito. Other websites can't embed or call it, so a page someone opens can't
search or play things on it behind their back.

The password is stored only as a hash (argon2id), and a device that guesses wrong ten times
waits ten minutes. Slow or oversized requests are cut off, and one device can't hold more than
16 connections, so a misbehaving gadget on the network can't tie it up. It speaks plain HTTP,
though: to reach it from outside your home, put [Caddy](https://caddyserver.com) or
`tailscale serve` in front of it for HTTPS.

## Tor

YourTube never signs in to Google and sends no tracking cookies, so YouTube can't attach what
you do to a Google account or ad profile. It still sees your IP address, which is enough to
link your searches to your home over time. Routing through Tor hides that too.

With [Tor](https://www.torproject.org) running (on Arch: `sudo pacman -S tor` and
`sudo systemctl enable --now tor`), go to Preferences → Network and set the proxy to
`socks5h://127.0.0.1:9050`. Then searches, channel and video pages, subscription feeds,
thumbnails, comments and SponsorBlock lookups go through Tor, and YouTube sees a Tor exit
instead of you.

Each kind of request gets its own Tor circuit: your searches, the pages you browse, your
subscription feed, thumbnails, For You's lookups and the extras all leave from different exits,
so YouTube can't join them up. YourTube also doesn't send back the visitor id YouTube hands
out, which would otherwise link them. For You spreads its lookups over half a minute instead
of sending them all at once.

Playback and downloads still come straight from YouTube by default, because YouTube blocks
most video streams over Tor with "Sign in to confirm you're not a bot". So YouTube can see
which videos your IP plays, but not what you searched for or browsed to find them. The switch
under the proxy sends playback and downloads through Tor as well. Some videos will play, many
won't; when one fails, YourTube doesn't fall back to your own address. Preferences → Network
shows what is covered right now.

Tor makes pages a few seconds slower, and some requests fail when YouTube blocks an exit node.
Trying again usually works.

## Keyboard

Change any of these in Preferences → Keyboard.

| | |
|---|---|
| Ctrl+F, or start typing | filter this page; Enter searches YouTube |
| Ctrl+R / F5 | refresh |
| Alt+1 … Alt+4 | For You, Subscriptions, Library, History |
| Ctrl+T / Ctrl+W | new tab / close tab |
| Ctrl+Tab, Ctrl+Shift+Tab | next / previous tab (or page, with no tabs open) |
| Escape / Alt+Left | back |
| Ctrl+H | hide watched videos |
| Ctrl+Shift+N / Ctrl+Shift+P | incognito / switch profile |
| Ctrl+I / Ctrl+, | import / preferences |
| Ctrl+Shift+B | report a problem |
| Ctrl+? or F1 | all shortcuts |

On a video (arrow keys to move, Enter to play): **I** details · **T** details in a new tab ·
**A** listen · **D** download · **W** Watch later · **P** playlist · **X** watched ·
**M** more like this · **N** not interested · **B** hide channel · **C** channel ·
**L** copy link · **Menu** everything.

## Your data

| Where | What |
|---|---|
| `~/.local/share/yourtube/yourtube.db` | subscriptions, history, playlists, searches, For You |
| `~/.local/share/yourtube/profiles/<name>/` | the same, per extra profile |
| `~/.config/yourtube/config` | settings (may hold an API key) |
| `~/.config/yourtube/style.css` | your CSS, reloaded when you save |
| `~/.cache/yourtube/` | thumbnails, safe to delete |
| `~/.local/state/yourtube/player.log` | one line per mpv exit |

Library → Your data → Export everything writes `yourtube-data.json` (which YourTube can import
again), `subscriptions.csv` for NewPipe and FreeTube, `subscriptions.opml` for RSS readers, and
CSV files of your history, searches and playlists. You can also delete searches and history
there.

YourTube connects to YouTube, SponsorBlock, Return YouTube Dislike and DeArrow (the last three
can be turned off), and to your AI or recommender only if you set one up. Ctrl+Shift+B bug
reports leave out your history, searches, keys and passwords, and you see the report before
it's sent.

Deleting history overwrites it in the database instead of leaving it in free space, and
"Clear history" also empties the thumbnail cache, whose file names are video ids. In incognito,
thumbnails are kept in memory (your session's runtime folder) and dropped when you leave it.

## Command line

```text
yourtube                     open the window (--profile NAME, --incognito)
yourtube <link or video id>  play it
yourtube import <file>       import a Takeout zip, NewPipe or FreeTube export
yourtube refresh             update subscriptions
yourtube foryou              rebuild For You
yourtube eval [WEEKS]        how well For You predicts what you watch
yourtube play VIDEO_ID       play one video from where you left off
yourtube serve [ADDRESS]     run the web version
```

## Compared with FreeTube

Same laptop (Linux, Intel graphics), both measured with [`tools/bench.py`](tools/bench.py):
three starts each on the main page, then 30 seconds of the same 1080p60 video.

| | YourTube | FreeTube |
|---|---|---|
| Window appears after | ~0.3 s | ~0.9 s |
| Memory, main page | ~75 MB | ~410 MB (peaks up to ~660 MB) |
| Memory, playing | ~150 MB (with mpv) | ~530 MB (peaks up to ~740 MB) |
| CPU, playing | ~10% | ~26% |
| CPU time to start | ~0.3 s | ~9 s |
| Processes | 1 (+ mpv while playing) | 8 |
| On disk | 8 MB (+ mpv) | 333 MB |

CPU is per core, so 100% is one full core. The first YourTube start after a reboot takes
about 0.6 s while the system loads GTK.

## Contributing

Issues and pull requests are welcome. My one rule is local first: no accounts, no telemetry,
and nothing leaves the computer unless the user turned it on.

Run `cargo test`. Some parser tests also use saved YouTube pages in `tests/data/` (see the
README there).

## License

[GPL-3.0-or-later](LICENSE). If you share a changed version, keep it under the same license.

YourTube isn't affiliated with YouTube or Google. "YouTube" is their trademark.

<p align="center">Made with love by Alyosha :)</p>
