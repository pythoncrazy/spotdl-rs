# spotDL-RS

Use this program to get audio for Spotify songs, albums, playlists, and artists. It finds each song on YouTube Music, prefers the label's official audio and the same clean or explicit edit, and tags the file with Spotify and Deezer metadata and lyrics.

The program needs `yt-dlp` and FFmpeg in `PATH` (or set `--yt-dlp` and `--ffmpeg`). For reliable YouTube downloads, also have Deno, Node.js, or Bun in `PATH`.

```sh
spotdl <Spotify URL>
spotdl save <Spotify URL> --save-file songs.spotdl
spotdl sync songs.spotdl
spotdl url "Daft Punk - Get Lucky"
spotdl meta ~/Music
```

Use `download`, `save`, `sync`, `url`, or `meta` before a query to select an action. A query is a Spotify URL or URI, a `.spotdl` file, a YouTube, YouTube Music or SoundCloud URL, or search text (`album:`, `playlist:` and `artist:` pick the result type). Use `youtube-url|spotify-url` to pin the video for a song; a pinned video wins over the search, also when the song appears in another query. An artist link gets every release on the artist page. Each song is downloaded once, from the first album that has it; reissues and singles add only new songs. A bigger edition of an album (deluxe, super deluxe) that has at least 90% of a smaller edition's songs replaces the smaller edition and is downloaded whole; an edition's title is the album's title or starts with it ("OK Computer OKNOTOK 1997 2017"). On albums and EPs, two songs with the same title are the same song only when their lengths differ by 5 seconds or less, so each album keeps its own "Intro".

In `--output`, `{disc-track-number}` is the track number (`07`), with the disc in front on albums that have more than one disc (`2-07`).

Use `--config file.json` to load a config file. Use `--generate-config` to write the default file to `~/.spotdl/config.json`. Use `--key=value` when a value starts with `--`, for example `--yt-dlp-args="--js-runtimes node"`.

Use `--max-retries 3` to set the number of retries after a failed request. Use `--use-cache-file` to keep track data between runs. Use `--no-cache` to turn off local caches.

Use `--meta-threads N` to set the number of metadata workers; `save` uses them for searches and lyrics. Lyrics are fetched while the audio downloads.

Use `--generate-lrc` to write a synced `.lrc` file next to each song. Synced lyrics come from LRCLIB, a crowd's uploads: same titles and lengths do not make the same song, and LRCLIB has, for example, the words of "KING" from VULTURES 1 under BULLY's "KING". So synced lyrics are used only when they were timed to a recording as long as the file and their words are the song's Genius lyrics (at least 30% of the word pairs are the same; LRCLIB and Genius share about 80% for one song and at most 8% for two). Genius has a page for each song, with its artists and release date: of two songs of an artist with one title, the one of the release's year is taken. Without synced lyrics that agree, the song has its Genius lyrics in the tags and no `.lrc` file. `--overwrite metadata` or `force` checks the lyrics of files again: lyrics in the tags and `.lrc` files that are another song's are replaced or removed.

Python spotDL's `synced` provider asks Musixmatch first; Musixmatch now gives programs without an account made-up lyrics, so spotdl-rs does not use its API.

`--lyrics-aligner CMD` checks lyrics against the recording instead. The program runs `CMD AUDIO LYRICS OUT.lrc` for each candidate, in this order, until one fits: the old `.lrc` file, the song's own synced lyrics, LRCLIB's versions as long as the file (same album first, synced first), the song's own plain lyrics, and for a song that is not on Spotify, the Genius pages of each of its artists. LRCLIB is asked only when the lyrics before it do not fit. LYRICS is synced or plain text. The command writes the `.lrc` file, or exits with status 3 when the lyrics are another song's, or with status 4 when it cannot tell (an old `.lrc` file then stays; without one, synced lyrics as long as the file are used). When no candidate fits, an old `.lrc` file is removed if `--overwrite` is `metadata` or `force`. Any other failure writes no `.lrc` file. `--lyrics-aligner-jobs N` runs the command for N songs at a time (default 1).

`scripts/align_lyrics.py` is such a command (`--lyrics-aligner scripts/align_lyrics.py`, with `uv` in `PATH`). It uses Whisper to transcribe the recording, and matches the lyric words to the heard words:

- Synced lyrics whose times agree with the heard words are kept as they are; other lyrics are timed again.
- Lines that the recording does not have are dropped (an intro of another version).
- Verses that the text does not have are added from the transcript, and choruses that the text has once are repeated.
- Lyrics are another song's (exit 3) when less than 40% of the lines are heard, no 8 lines in a row are, and less than 15% of the words Whisper hears are words of heard lines. On 400 songs of a library, right lyrics had at least 18% and other songs' lyrics at most 9%.
- It cannot tell (exit 4) when Whisper hears fewer than 20 different words: shouted vocals, or an instrumental.
- Whisper finds the language of the song, so lyrics in any language are checked.

It keeps each transcript in `$ALIGN_CACHE` (default `~/.cache/align-lyrics`), by the audio's SHA-1, and `--cache AUDIO...` only fills that cache. OUT can be `-` for stdout.

`meta` works on `--threads` files at a time. With `meta`, a song without a Spotify URL takes the tags of a Spotify search result only when that result has the same title, artist, and length. Otherwise the song keeps its tags, and only its lyrics change. Such a song (for example, an unreleased one) has many versions of about the same length, so its lyrics are always timed by the aligner when one is set. Without other lyrics, the aligner times the words of its old `.lrc` file again.

Use `--scan-for-songs` to find songs in the output folder. Use `--fetch-albums` to add all songs from each album. Use `--ytm-data` to use the title and artist from a YouTube Music page.

Use `--playlist-numbering` to use the playlist name as the album name and the playlist owner as the album artist. Use `--playlist-retain-track-cover` to keep each track's album cover. Use `--m3u` to write one playlist file per list; it includes every song of the list, also those that another query already downloaded.

Use `--print-errors` to print all errors at the end. Use `--save-errors errors.txt` to save them, with the Spotify URL of each song. Set `--log-level` and `--log-format` to change log output.

You can use `--redownload` with `meta` to save the local songs again in the selected format. Use `--sync-remove-lrc` to remove or rename lyric files when `sync` changes files. Use `--add-unavailable` to add songs without a download to the archive and playlists.

The Spotify URL is stored in the `WOAS` tag (`----:spotdl:WOAS` in M4A), as Python spotDL does, so either program can scan the other's files.

Run `cargo test` for the end-to-end tests; they use the live services. Set `SPOTDL_E2E_DOWNLOAD=1` to also test downloads, and `SPOTDL_E2E_YT_DLP` to select yt-dlp.
