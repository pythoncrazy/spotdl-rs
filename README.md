# spotdl-rs

Use spotdl to download audio for Spotify tracks, albums, playlists, and artists.

Install Python 3.10 or later. Python 3.11 or later is recommended.

Install or update yt-dlp with `python -m pip install -U "yt-dlp[default]"`.
Install Deno from https://deno.land. yt-dlp can use Deno to solve YouTube JavaScript checks.
Keep yt-dlp and Deno up to date.

Install ffmpeg. Add yt-dlp and ffmpeg to `PATH`.

If YouTube asks you to sign in, pass a cookies file with `--cookie-file`.

```sh
spotdl <Spotify URL>
spotdl save <Spotify URL> --save-file songs.spotdl
spotdl sync songs.spotdl
spotdl url "Daft Punk - Get Lucky"
spotdl meta ~/Music
```

Use `download`, `save`, `sync`, `url`, or `meta` before a query to select a command.

The program does not include a web interface, Piped, Slider.kz, Bandcamp, profiles, or update checks.
