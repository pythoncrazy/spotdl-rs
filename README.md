# spotdl-rs

Download audio for Spotify songs, albums, playlists, and artists.

Install `yt-dlp` and `ffmpeg`. Add both tools to `PATH`.

```sh
spotdl <Spotify URL>
spotdl save <Spotify URL> --save-file songs.spotdl
spotdl sync songs.spotdl
spotdl url "Daft Punk - Get Lucky"
spotdl meta ~/Music
```

Use `download`, `save`, `sync`, `url`, or `meta` before a query to select a command.

The program does not include a web interface, Piped, Slider.kz, Bandcamp, profiles, or update checks.
