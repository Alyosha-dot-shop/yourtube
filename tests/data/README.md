# Test pages

The parser tests in `src/yt.rs` and `src/video.rs` also run against saved copies of
real YouTube pages when they are here. They are not checked in (YouTube's pages are
theirs, and large). Without them those tests pass trivially; everything else needs
no network. Hand-made, committed fixtures live in `tests/fixtures/yt/` instead.

To refresh them:

```sh
UA='Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0'
get() { curl -s --compressed -A "$UA" -H 'Accept-Language: en-US' -H 'Cookie: SOCS=CAI' -o "tests/data/$1" "$2"; }
get channel.html 'https://www.youtube.com/channel/UCBJycsmduvYEL83R_U4JriQ/videos'
get search.html 'https://www.youtube.com/results?search_query=e30+turbo+build'
get watch.html 'https://www.youtube.com/watch?v=F3BGWNuVMUE'
# A long video with chapters in its description, and comments.
get watch-chapters.html 'https://www.youtube.com/watch?v=BpPEoZW5IiY'
# A playlist with more than 100 videos (so it has a continuation).
get playlist.html 'https://www.youtube.com/playlist?list=PLWKjhJtqVAbm04DK8TSUCRheRjW2P9TR7'
get channel-streams.html 'https://www.youtube.com/channel/UC8butISFwT-Wl7EV0hUK0BQ/streams'
get channel-shorts.html 'https://www.youtube.com/channel/UC8butISFwT-Wl7EV0hUK0BQ/shorts'
get channel-playlists.html 'https://www.youtube.com/channel/UC8butISFwT-Wl7EV0hUK0BQ/playlists'
# "rust tutorial": sorted by views, this month, over 20 minutes, videos only.
get search-filtered.html 'https://www.youtube.com/results?search_query=rust+tutorial&sp=CAMSBggEEAEYAg%3D%3D'
# Plain "rust tutorial", which lists a few courses/playlists among the videos.
get search-playlists.html 'https://www.youtube.com/results?search_query=rust+tutorial'
```

`watch-botcheck.html` is `watch-chapters.html`'s video fetched through Tor, where YouTube
answers with its "Sign in to confirm you're not a bot" page:

```sh
curl -s --socks5-hostname 127.0.0.1:9050 --compressed -A "$UA" -H 'Accept-Language: en-US' \
  -H 'Cookie: SOCS=CAI' -o tests/data/watch-botcheck.html 'https://www.youtube.com/watch?v=BpPEoZW5IiY&hl=en'
```
