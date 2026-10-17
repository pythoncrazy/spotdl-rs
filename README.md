# spotDL-RS

Use this program to get audio for Spotify songs, albums, playlists, and artists.

The program needs `yt-dlp` and FFmpeg. Put both tools in `PATH`, or use the local installers below.

```sh
spotdl <Spotify URL>
spotdl save <Spotify URL> --save-file songs.spotdl
spotdl sync songs.spotdl
spotdl url "Daft Punk - Get Lucky"
spotdl meta ~/Music
```

Use `download`, `save`, `sync`, `url`, or `meta` before a query to select an action.

Use `--config file.json` to load a config file. Use `--generate-config` to write the default file to `~/.spotdl/config.json`.

Use `--download-ffmpeg`, `--download-deno`, or `--download-yt-dlp` to install a local tool in `~/.spotdl`. The yt-dlp installer uses the platform's onedir release in `~/.spotdl/yt-dlp_dist`; existing `~/.spotdl/yt-dlp` installs are still detected. The standalone yt-dlp build includes a YouTube challenge solver. For reliable YouTube downloads, have Deno, Node.js, or Bun in `PATH`, or run `--download-deno`. The program uses local FFmpeg and yt-dlp files when you do not set `--ffmpeg` or `--yt-dlp`.

Use `--key=value` when a value starts with `--`. For example, use `--yt-dlp-args="--js-runtimes node"`.

Use `--check-for-updates` to check the latest release. Use `--max-retries 3` to set the number of retries after a failed request.

Use `--user-auth` to sign in to Spotify. Set `client_id` and `client_secret` in the config file. Use `--headless` to print the sign-in URL instead of opening a browser. The callback uses `127.0.0.1:9900`.

Use `--use-official-api` to use Spotify's Web API. Use `--use-cache-file` to save track data between runs. Use `--no-cache` to turn off local caches.

Use `--meta-threads N` to set metadata worker count. Save uses these workers for lyrics requests. Each lyrics request has a 10-second timeout.

Use `--scan-for-songs` to find songs in the output folder. Use `--fetch-albums` to add all songs from each album. Use `--ytm-data` to use the title and artist from a YouTube Music page.

Use `--playlist-numbering` to use the playlist name as the album name and the playlist owner as the album artist. Use `--playlist-retain-track-cover` to keep each track's album cover.

Use `--print-errors` to print all errors at the end. Use `--save-errors errors.txt` to save them to a file. Set `--log-level` and `--log-format` to change log output.

You can use `--redownload` with `meta` to save the local songs again in the selected format. Use `--sync-remove-lrc` to remove or rename lyric files when `sync` changes files. Use `--add-unavailable` to add songs without a download to the archive and playlists.
