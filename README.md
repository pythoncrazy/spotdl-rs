# spotdl-rs

Rust Spotify downloader. Requires `yt-dlp` and `ffmpeg` on `PATH`.

```sh
cargo build --release
spotdl download "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"
spotdl save "https://open.spotify.com/playlist/PLAYLIST_ID" --save-file list.spotdl
spotdl sync list.spotdl --threads 16
spotdl meta "song.mp3"
```

Spotify public URLs use the anonymous web-player API. Text search requires Spotify API credentials; user-library queries require `--auth-token`. The rewrite uses rayon pools and supports YouTube Music, YouTube, and SoundCloud searches. The web UI, Piped, Slider.kz, Bandcamp, profiles, and update checks are not included.
