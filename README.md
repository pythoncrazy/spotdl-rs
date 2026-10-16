# spotdl-rs

Compact Rust rewrite of [spotDL](https://github.com/spotDL/spotify-downloader). Needs `yt-dlp` and `ffmpeg` on `PATH`.

```sh
spotdl https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT
spotdl download https://open.spotify.com/artist/0gxyHStUsqpMadRV0Di1Qt --threads 32
spotdl save "playlist:top 1000 songs" --save-file list.spotdl
spotdl sync https://open.spotify.com/playlist/ID --save-file list.spotdl
spotdl sync list.spotdl
spotdl url "Daft Punk - Get Lucky"
spotdl meta ~/Music
```

- Public tracks, albums, playlists, artists and text/`album:`/`playlist:`/`artist:` searches use Spotify's anonymous web-player API; no credentials needed.
- `saved`, `all-user-playlists`, `all-saved-playlists`, `all-user-saved-albums`, `all-user-followed-artists` need `--auth-token` (or `--client-id`/`--client-secret` for public data).
- Pages of a playlist, album or discography are fetched in parallel, each album's metadata is fetched once, tracks repeated across queries download once.
- `--meta-threads` (default 32) bounds metadata work, `--threads` (default 16, no cap) bounds downloads.
- Same `config.json` keys and `.spotdl` format as spotDL (`~/.config/spotdl` or `~/.spotdl`). `--generate-config` writes defaults.
- Not included: web UI, Piped, Slider.kz, Bandcamp, profiles, update checks.
