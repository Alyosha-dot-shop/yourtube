<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="data/icons/yourtube-wordmark-dark.svg">
    <img src="data/icons/yourtube-wordmark.svg" alt="YourTube" width="420">
  </picture>
</p>

<p align="center"><b>YouTube, owned by the people who watch it.</b></p>

---

Hi, I'm Alyosha. I built YourTube because I was tired of my watch history, my habits and
my "recommended for you" living on someone else's servers, and tired of a browser-sized app
just to watch a video. So this is the YouTube app I wanted: it runs on your computer, keeps
everything in a file you own, and shows you why it recommends what it recommends.

No account. No subscription. No tracking. It's free, it's open, and you're welcome to change
it into whatever you want.

## What it does

- **For You that answers to you.** Recommendations from all of YouTube, built on your own
  computer from what *you* search and watch. Every pick says why it's there ("Because you
  searched…", "Related to…"). Tell it *more like this* or *not interested* and it listens.
- **Optional AI, local first.** Let a small model on your own machine suggest new things to
  explore. Cloud models work too, but you don't need them.
- **Your subscriptions**, newest first, without Shorts.
- **Never lose your place.** Your position is saved every few seconds, so a crash or a closed
  window never sends you back to 0:00.
- **Search, channels, playlists, Watch later, history**: the usual, minus the noise.
- **Tabs**, like a browser: middle-click a video (or press **T**) to open it in a new tab,
  Ctrl+T for a new search, Ctrl+W to close one.
- **Don't want to see a channel again?** Hide it everywhere with one key.
- **Profiles** for everyone who shares the computer, and **incognito** when you'd rather it
  didn't remember.
- **Make it yours.** Themes, accent colour, rounding, card size, or your own CSS on top.
- **Keyboard-friendly** from top to bottom, and every shortcut is yours to change.
- **Everything I loved in FreeTube**: SponsorBlock skipping, downloads that play offline,
  sound-only listening, chapters, comments, Return YouTube Dislike, DeArrow titles,
  search filters, channel tabs and Play all.
- **On every screen in the house.** Run it once on a Raspberry Pi or an old laptop and use it
  from any phone, tablet or computer in a browser.
- **Your data, out whenever you want.** Export everything to JSON, CSV and OPML in one click.
- **Something broke? One key.** Ctrl+Shift+B takes a screenshot, writes down what happened
  and opens a ready-to-send issue.
- **Light.** One small native window (GTK 4). Videos play in [mpv](https://mpv.io). See
  [how light](#how-light-is-it) next to FreeTube.

## Getting started

You'll need GTK 4.20+, libadwaita 1.8+, [mpv](https://mpv.io), an up-to-date
[yt-dlp](https://github.com/yt-dlp/yt-dlp) (mpv uses it to play YouTube) and Rust.

I use it every day on Linux. It also builds for macOS (`brew install gtk4 libadwaita mpv
yt-dlp`) and Windows (MSYS2), and every change is built and tested on all three, but those
two get less real-world use, so tell me what breaks.

```sh
git clone https://github.com/Alyosha-dot-shop/yourtube && cd yourtube
cargo build --release
./target/release/yourtube
```

To have `yourtube://` links (and the app menu entry) work, copy
`data/io.github.yourtube.YourTube.desktop` to `~/.local/share/applications/`, the icons in
`data/icons/` into `~/.local/share/icons/hicolor/`, and run
`xdg-mime default io.github.yourtube.YourTube.desktop x-scheme-handler/yourtube`.

### Bring your stuff with you

Menu → **Import subscriptions & history** (Ctrl+I) takes:

- **Google Takeout**: go to [takeout.google.com](https://takeout.google.com), pick only
  *YouTube and YouTube Music*, and download. Hand YourTube the `.zip` as is. It picks up your
  subscriptions, watch history, search history and playlists.
- **NewPipe**: a subscriptions export or a full backup.
- **FreeTube**: your FreeTube folder or its export files.

Importing twice is fine; nothing gets doubled.

## The FreeTube things

If you're coming from FreeTube, most of what you rely on is here:

- **SponsorBlock.** Sponsors, intros, "like and subscribe" and friends are skipped, offered as
  a skip, or left alone; you choose per category in Preferences. YourTube never sends the video
  id: it asks for the first few characters of its hash and picks your video out of the answer.
- **Downloads** (**D** on a video). Downloaded videos play from disk, even offline, and can
  have sponsor segments cut out. Library → Downloads lists them.
- **Listen only** (**A**): sound without the picture, nice for talks and music.
- **About this video** (**I**): description, chapters you can jump to, comments, likes and
  Return YouTube Dislike's estimate.
- **DeArrow** swaps clickbait titles and thumbnails for community-made ones (optional).
- **Search filters** (date, length, type), channel tabs (videos, live, playlists), YouTube
  playlists with **Play all**, playback speed, subtitles, autoplay and a proxy setting.
- **Privacy switches**: don't keep history, don't keep searches, hide view counts, hide live
  streams.

## On every screen in the house

`yourtube serve` runs the web version: the same subscriptions, For You, history and playlists
in any browser, with videos streamed through it. It's light enough for a Raspberry Pi 4 or 5,
or that old laptop in a drawer.

1. On the always-on machine, set an address and a password in `~/.config/yourtube/config`:
   ```text
   web_address = 0.0.0.0:8723
   web_password = something long
   ```
2. Run `yourtube serve` (or make it a systemd service so it starts on boot).
3. Open `http://<that machine>:8723` from any device. If your router or Pi-hole does local DNS,
   give it a name like `yourtube.home` and the whole house can just type that.

By default it only listens on the machine itself. Each browser can pick its own profile and
turn incognito on for itself. Please keep the password if anything outside your home can
reach it.

## How For You works

There's no secret sauce, and that's the point. It all lives in [`src/foryou/`](src/foryou)
and you can read it in one sitting.

1. **What you're into lately.** Your searches count the most, then what you watched (and how
   much of it), then your feedback and subscriptions. Old activity fades out: searches lose
   half their weight every 10 days, watches every 21.
2. **Looking around YouTube.** It searches for what you've been searching, looks at what
   YouTube lists next to the videos you watched, and checks your subscriptions for new
   uploads. That's about 20 requests and a few seconds.
3. **Sorting.** A score you can read: how well a title matches your interests (45%), why it
   was found (15%), how much you watch that channel (15%), how new it is (15%) and how popular
   (10%). Then it keeps things varied: no more than two videos per channel or four per reason,
   no near-duplicates, and at least a third from channels you don't follow yet.
4. **Listening.** *More like this* boosts those topics and that channel. *Not interested*
   hides the video and quiets its topics. *Don't show this channel* hides it everywhere.

Half-watched videos show up first, so you can finish them.

### AI (optional, and we suggest local)

In Preferences you can let an AI suggest new searches and pick the best videos, with a short
reason for each. Anything that speaks the OpenAI-style `/chat/completions` API works.

**I'd really recommend a model on your own computer.** Then nothing about what you watch
leaves your machine. And you don't need a big one, since the job is just picking a few
searches and sorting about 40 titles:

| Model size | Examples | Memory | Runs well on |
|---|---|---|---|
| 1–4B | Qwen3 4B, Gemma 3 4B, Llama 3.2 3B | ~3 GB | any recent laptop, even on the CPU; a refresh takes under a minute |
| 7–8B | Qwen3 8B, Llama 3.1 8B | ~6 GB | 16 GB of RAM, or a graphics card with 6 GB+; picks get a bit sharper |

The easiest way in: install [Ollama](https://ollama.com), run `ollama pull qwen3:4b`, then
in Preferences choose *Ollama* and type `qwen3:4b`. LM Studio and llama.cpp's server work too.

Cloud models (OpenAI, Anthropic, OpenRouter, …) work if you add a key. Just know that, when
For You refreshes, they receive your recent searches (up to 20), titles and channels of videos
you watched lately (up to 30), titles you said no to (up to 10), and up to 40 candidate titles.
If the AI is down, For You is made without it.

### Bring your own recommender

If you'd rather run your own, point Preferences at it. It only needs to speak the small JSON
API in [`src/foryou/remote.rs`](src/foryou/remote.rs), and it can read YourTube's database.

## Profiles and incognito

Each **profile** has its own subscriptions, history, playlists and For You. Switch from the
little avatar in the corner (Ctrl+Shift+P), or start with `yourtube --profile Name`.

**Incognito** (Ctrl+Shift+N, the mask button, or `yourtube --incognito`) remembers nothing:
no history, no resume points, no searches, no For You signals. The header turns violet so you
always know it's on.

## Make it look how you like

Preferences → Appearance has light/dark, themes (System, YourTube, Midnight, Paper, Forest,
Grape, Ocean, Mono), any accent colour, corner rounding, video size and animations on or off.

For everything else there's `~/.config/yourtube/style.css`. It loads on top of everything and
reloads the moment you save. Preferences has a button that opens it with a few examples inside.

## Keyboard

Every shortcut can be changed: Preferences → Keyboard, click one, press the keys you want.
These are the defaults.

| | |
|---|---|
| Ctrl+F or just start typing | filter this page; Enter searches YouTube |
| Ctrl+R / F5 | refresh |
| Alt+1 … Alt+4 | For You, Subscriptions, Library, History |
| Ctrl+T / Ctrl+W | new tab (with a search box) / close tab |
| Ctrl+Tab, Ctrl+Shift+Tab | next / previous tab (or page, when no tabs are open) |
| Escape / Alt+Left | back |
| Ctrl+H | hide watched videos |
| Ctrl+Shift+N / Ctrl+Shift+P | incognito / switch profile |
| Ctrl+I | import |
| Ctrl+, | preferences |
| Ctrl+? or F1 | every shortcut |

On a video (arrow keys to move, Enter to play): **I** about this video · **T** the same in a
new tab (or middle-click) · **A** listen · **D** download · **W** Watch later · **P** playlist ·
**X** watched · **M** more like this · **N** not interested · **B** don't show this channel ·
**C** go to channel · **L** copy link · **Menu** all of it.

## Your data, in plain sight

| Where | What |
|---|---|
| `~/.local/share/yourtube/yourtube.db` | subscriptions, history, playlists, searches, For You (SQLite) |
| `~/.local/share/yourtube/profiles/<name>/` | the same for each extra profile |
| `~/.config/yourtube/config` | settings (private to you; may hold an API key) |
| `~/.config/yourtube/style.css` | your own style |
| `~/.cache/yourtube/` | thumbnails (safe to delete) |
| `~/.local/state/yourtube/player.log` | one line each time mpv closes, for when something goes wrong |

**Take it with you:** Library → Your data → **Export everything** writes
`yourtube-data.json` (everything, and YourTube imports it back), `subscriptions.csv`
(NewPipe and FreeTube read it), `subscriptions.opml` for RSS readers, and `history.csv`,
`searches.csv` and `playlists.csv` for any spreadsheet. The same place shows your recent
searches, lets you forget one or all of them, and clears your history.

YourTube talks to YouTube itself (pages, thumbnails, feeds) and to mpv and yt-dlp on your
computer. It also asks SponsorBlock, Return YouTube Dislike and DeArrow, each of which you can
turn off. Your AI and your own recommender are only used if you set them up.

## Something broke?

Press **Ctrl+Shift+B** (or Menu → Report a problem). YourTube takes a screenshot, adds the
error codes it showed you (like `YT-NET-01`), the page you were on and your versions of
YourTube, mpv and yt-dlp, and opens a GitHub issue that's ready to send. Your history,
searches, keys and passwords never go in, and you see all of it before anything is sent.

## Command line

```text
yourtube                     open the window (--profile NAME, --incognito)
yourtube <link or video id>  play it
yourtube import <file>       import a Takeout zip, NewPipe or FreeTube export
yourtube refresh             update subscriptions
yourtube foryou              rebuild For You
yourtube play VIDEO_ID       play one video, picking up where you left off
yourtube serve [ADDRESS]     the web version for your home network
```

## How light is it?

I measured both on the same laptop (Linux, Intel graphics), mostly with
[`tools/bench.py`](tools/bench.py); YourTube's playing numbers come from 30 seconds of a
real evening on the couch:

| | YourTube | FreeTube |
|---|---|---|
| Memory, on the main page | ~110 MB | ~510 MB (up to ~690 MB) |
| Memory, playing 1080p60 | ~590 MB (mpv holds ~6 min of video ahead) | ~690 MB (up to ~760 MB) |
| CPU, playing 1080p60 | ~40% | ~140% |
| Processes | 1 (+ mpv while playing) | 8 |
| On disk | 6 MB (+ mpv) | 333 MB |
| Window shows up after | ~3 s | ~4.4 s |

CPU is per core, so 100% is one core fully busy. YourTube's mpv decodes on the graphics card.

Run `python3 tools/bench.py --help` to measure it on your own machine.

## Helping out

Issues and pull requests are very welcome, from typo fixes to big ideas. There's one rule I
care about: **local first**. No accounts, no telemetry, and nothing leaves the computer unless
the person using it turned that on themselves.

```sh
cargo test
```

Some parser tests also run against saved YouTube pages if you drop them in `tests/data/`
(see the README there).

## License

[GPL-3.0-or-later](LICENSE). Use it, study it, share it, change it. If you share a changed
version, keep it free for the next person too.

YourTube isn't affiliated with YouTube or Google; "YouTube" is their trademark. The logo is
a little house-shaped play button: the tube lives in your house now.

<p align="center">Made with love by Alyosha :)</p>
