# spotDL-RS

Use this program to get audio for Spotify songs, albums, playlists, and artists. It finds each song on YouTube Music, prefers the label's official audio and the same clean or explicit edit, and tags the file with Spotify and Deezer metadata and lyrics.

The program needs `yt-dlp` and FFmpeg in `PATH` (or set `--yt-dlp` and `--ffmpeg`). For reliable YouTube downloads, also have Deno, Node.js, or Bun in `PATH`. A small yt-dlp plugin in `~/.spotdl/yt-dlp-plugins` skips the pre-roll ad wait that logged-out YouTube imposes.

```sh
spotdl <Spotify URL>
spotdl save <Spotify URL> --save-file songs.spotdl
spotdl sync songs.spotdl
spotdl url "Daft Punk - Get Lucky"
spotdl meta ~/Music
```

Use `download`, `save`, `sync`, `url`, or `meta` before a query to select an action. A query is a Spotify URL or URI, a `.spotdl` file, a YouTube, YouTube Music or SoundCloud URL, or search text (`album:`, `playlist:` and `artist:` pick the result type). Use `youtube-url|spotify-url` to pin the video for a song; a pinned video wins over the search, also when the song appears in another query.

Use `--config file.json` to load a config file. Use `--generate-config` to write the default file to `~/.spotdl/config.json`. Use `--key=value` when a value starts with `--`, for example `--yt-dlp-args="--js-runtimes node"`.

Use `--max-retries 3` to set the number of retries after a failed request. Use `--use-cache-file` to keep track data between runs. Use `--no-cache` to turn off local caches.

Use `--meta-threads N` to set the number of metadata workers; `save` uses them for searches and lyrics. Lyrics are fetched while the audio downloads.

Use `--scan-for-songs` to find songs in the output folder. Use `--fetch-albums` to add all songs from each album. Use `--ytm-data` to use the title and artist from a YouTube Music page.

Use `--playlist-numbering` to use the playlist name as the album name and the playlist owner as the album artist. Use `--playlist-retain-track-cover` to keep each track's album cover. Use `--m3u` to write one playlist file per list; it includes every song of the list, also those that another query already downloaded.

Use `--print-errors` to print all errors at the end. Use `--save-errors errors.txt` to save them, with the Spotify URL of each song. Set `--log-level` and `--log-format` to change log output.

You can use `--redownload` with `meta` to save the local songs again in the selected format. Use `--sync-remove-lrc` to remove or rename lyric files when `sync` changes files. Use `--add-unavailable` to add songs without a download to the archive and playlists.

The Spotify URL is stored in the `WOAS` tag (`----:spotdl:WOAS` in M4A), as Python spotDL does, so either program can scan the other's files.

Run `cargo test` for the end-to-end tests; they use the live services. Set `SPOTDL_E2E_DOWNLOAD=1` to also test downloads, and `SPOTDL_E2E_YT_DLP` to select yt-dlp.
