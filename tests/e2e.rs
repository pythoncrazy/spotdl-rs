//! End-to-end tests: each runs the built binary against the live Spotify, YouTube Music and Deezer services.
//! The download test also needs yt-dlp, FFmpeg and a network YouTube serves audio to; enable it with
//! `SPOTDL_E2E_DOWNLOAD=1` (and `SPOTDL_E2E_YT_DLP=/path/to/yt-dlp` to pick the yt-dlp binary).
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::*,
    tag::{ItemKey, ItemValue, Tag, TagItem},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const DONDA: &str = "https://open.spotify.com/album/5CnpZV3q5BcESefcB3WJmz";
const VIDEO: &str = "https://music.youtube.com/watch?v=Uq0aeEYLkIE";
const ELEANOR: &str = "https://open.spotify.com/track/4KRgqwb4kvBuTz1utbYxfb";
const TAXMAN: &str = "https://open.spotify.com/track/1ob06Ol8FMIPQgjpg7bMyk";
const FORTUNE_DAYS: &str = "https://open.spotify.com/track/5VWVVSABdgccboSdrKGiZH";

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spotdl-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn spotdl(dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spotdl"));
    command.current_dir(dir).env("HOME", dir).args(args).args(["--lyrics-providers="]);
    if let Ok(path) = std::env::var("SPOTDL_E2E_YT_DLP") {
        command.args(["--yt-dlp", &path]);
    }
    command.output().unwrap()
}

fn ok(output: Output) -> String {
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{text}");
    text
}

fn fails_with(output: Output, message: &str) {
    let text = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(!output.status.success() && text.contains(message), "{text}");
}

fn skip(file: &Path) -> PathBuf {
    file.with_extension("opus.skip")
}

fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn save_keeps_pins_and_lists_every_song() {
    let dir = dir("save");
    let file = dir.join("donda.spotdl");
    ok(spotdl(&dir, &["save", DONDA, "--save-file", file.to_str().unwrap()]));
    let songs = json_file(&file);
    let songs = songs.as_array().unwrap();
    assert_eq!(songs.len(), 27);
    let numbers: Vec<u64> = songs.iter().map(|song| song["track_number"].as_u64().unwrap()).collect();
    assert_eq!(numbers, (1..=27).collect::<Vec<_>>());
    for song in songs {
        assert_eq!(song["album_name"], "Donda");
        assert!(!song["name"].as_str().unwrap().is_empty() && song["duration"].as_f64().unwrap() > 0.0);
        assert!(!song["artists"].as_array().unwrap().is_empty() && song["artists"][0] == song["artist"]);
    }

    // The same song pinned to a video after the album: one entry, with the pin, but listed twice in the m3u.
    let url = songs[4]["url"].as_str().unwrap();
    let pinned = format!("{VIDEO}|{url}");
    ok(spotdl(&dir, &["save", DONDA, &pinned, "--save-file", file.to_str().unwrap(), "--m3u"]));
    let songs = json_file(&file);
    let songs = songs.as_array().unwrap();
    assert_eq!(songs.len(), 27);
    let song = songs.iter().find(|song| song["url"] == url).unwrap();
    assert_eq!(song["download_url"], VIDEO);
    let list = fs::read_to_string(dir.join("Donda.m3u8")).unwrap();
    assert_eq!(list.lines().filter(|line| line.starts_with("#EXTINF")).count(), 28);
}

#[test]
fn artist_keeps_albums_whole_and_prefers_deluxe_editions() {
    let dir = dir("artist");
    let file = dir.join("kendrick.spotdl");
    let kendrick = "https://open.spotify.com/artist/2YZyLoL8N0Wb9xBt1NhZWg";
    ok(spotdl(&dir, &["save", kendrick, "--save-file", file.to_str().unwrap()]));
    let songs = json_file(&file);
    let album = |name: &str| songs.as_array().unwrap().iter().filter(|song| song["album_name"] == name).count();
    // An album keeps all its songs; a reissue under another title adds only the songs that are new.
    assert_eq!(album("DAMN."), 14);
    assert_eq!(album("DAMN. COLLECTORS EDITION."), 0);
    // A deluxe edition that has the standard edition's songs replaces it and is kept whole.
    assert_eq!(album("good kid, m.A.A.d city (Deluxe)"), 17);
    assert_eq!(album("good kid, m.A.A.d city"), 0);

    // The anniversary edition is kept whole, with its "Intro": an "Intro" on another album is another song.
    let file = dir.join("cole.spotdl");
    let cole = "https://open.spotify.com/artist/6l3HvQ5sa6mXTsMTB19rO5";
    ok(spotdl(&dir, &["save", cole, "--save-file", file.to_str().unwrap(), "--audio-providers="]));
    let songs = json_file(&file);
    let album = |name: &str| songs.as_array().unwrap().iter().filter(|song| song["album_name"] == name).count();
    assert_eq!(album("2014 Forest Hills Drive (10 Year Anniversary Edition)"), 21);
    assert_eq!(album("2014 Forest Hills Drive"), 0);
}

#[test]
fn url_finds_official_audio() {
    let dir = dir("url");
    for track in [
        ELEANOR,
        "https://open.spotify.com/track/3AfT3nmctE9nlndrFgshXH",
        "https://open.spotify.com/track/0u2P5u6lvoDfwTYjAADbn4",
        // "Lil' Homies - Album Version (Edited)": the suffix is a version note, not part of the title.
        "https://open.spotify.com/track/1YTfq6avUGOxlJ1GluNIqj",
    ] {
        let url = ok(spotdl(&dir, &["url", track]));
        let url = url.lines().next().unwrap();
        assert!(url.starts_with("https://music.youtube.com/watch?v="), "{track}: {url}");
        let oembed = format!("https://www.youtube.com/oembed?format=json&url={url}");
        let video: Value = ureq::get(&oembed).call().unwrap().into_body().read_json().unwrap();
        let title = video["title"].as_str().unwrap().to_lowercase();
        let altered = ["karaoke", "cover", "live", "nightcore", "slowed", "sped up", "8d", "instrumental"];
        assert!(!altered.iter().any(|word| title.contains(word)), "{track}: {title}");
    }
}

#[test]
fn sync_moves_and_removes_files() {
    let dir = dir("sync");
    let output = format!("{}/{{track-id}}.{{output-ext}}", dir.display());
    let file = |url: &str| dir.join(format!("{}.opus", url.rsplit('/').next().unwrap()));
    // Files that already exist are kept, so this needs no download.
    for url in [ELEANOR, TAXMAN] {
        fs::write(file(url), "").unwrap();
        fs::write(file(url).with_extension("opus.skip"), "").unwrap();
    }
    let state = dir.join("s.spotdl");
    let state_path = state.to_str().unwrap();
    ok(spotdl(&dir, &["sync", ELEANOR, TAXMAN, "--save-file", state_path, "--output", &output]));
    let mut saved = json_file(&state);
    assert_eq!(saved["songs"].as_array().unwrap().len(), 2);
    saved["query"] = json!([ELEANOR]);
    fs::write(&state, saved.to_string()).unwrap();
    ok(spotdl(&dir, &["sync", state_path, "--output", &output]));
    assert!(file(ELEANOR).exists() && file(ELEANOR).with_extension("opus.skip").exists());
    assert!(!file(TAXMAN).exists() && !file(TAXMAN).with_extension("opus.skip").exists());
}

/// A song whose Spotify URL changed (a re-released album gives its songs new IDs) but whose file did not keeps
/// its file: it is not deleted before a download that can fail. Needs no network: the songs come from files.
#[test]
fn sync_keeps_the_files_of_songs_whose_url_changed() {
    let dir = dir("relink");
    let songs = |id: &str| {
        let song = |name: &str, n: u32| {
            json!({"name": name, "artists": ["Art"], "artist": "Art", "album_name": "Alb", "album_artist": "Art",
                "duration": 30, "year": 2020, "date": "2020", "track_number": n, "tracks_count": 2,
                "url": format!("https://open.spotify.com/track/{name}{id}"),
                "download_url": "https://music.youtube.com/watch?v=Uq0aeEYLkIE"})
        };
        json!([song("A", 1), song("B", 2)])
    };
    fs::write(dir.join("new.spotdl"), songs("2").to_string()).unwrap();
    let state = dir.join("s.spotdl");
    fs::write(&state, json!({"type": "sync", "query": ["new.spotdl"], "songs": songs("1")}).to_string()).unwrap();
    for name in ["A", "B"] {
        fs::write(dir.join(format!("Art - {name}.opus")), "").unwrap();
    }
    // A download would fail.
    ok(spotdl(&dir, &["sync", state.to_str().unwrap(), "--yt-dlp", "false"]));
    assert!(dir.join("Art - A.opus").exists() && dir.join("Art - B.opus").exists());
}

#[test]
fn rejects_bad_options_and_unsafe_paths() {
    let dir = dir("errors");
    fails_with(spotdl(&dir, &["--not-an-option"]), "unknown option --not-an-option");
    fails_with(spotdl(&dir, &["x", "--config", "/nonexistent"]), "config file not found");
    fails_with(spotdl(&dir, &["x", "--format"]), "missing value for --format");
    fails_with(spotdl(&dir, &["x", "--audio", "napster"]), "unknown audio provider");

    let songs = json!([{"name": "．．／．．／etc", "artist": "A", "artists": ["A"], "album_name": "Album", "url": "https://open.spotify.com/track/0123456789012345678901"}]);
    fs::write(dir.join("local.spotdl"), songs.to_string()).unwrap();
    let output = format!("{}/safe/{{title}}.{{output-ext}}", dir.display());
    let text = ok(spotdl(
        &dir,
        &["save", "local.spotdl", "--save-file", "-", "--restrict", "ascii", "--output", &output, "--m3u"],
    ));
    assert!(text.contains(&format!("{}/safe/....etc.opus", dir.display())), "{text}");
    let list = fs::read_to_string(dir.join("Album.m3u8")).unwrap();
    assert!(list.contains("\nsafe/....etc.opus\n"), "{list}");
    let text = ok(spotdl(&dir, &["save", "local.spotdl", "--save-file", "-", "--output", ""]));
    assert!(text.contains("A - ．．／．．／etc: A - ．．／．．／etc.opus"), "{text}");

    // Multi-disc albums number tracks "2-07", so two discs' track 7 do not share a path.
    let song = |disc: u32, count: u32| {
        json!({"name": format!("S{disc}{count}"), "artist": "A", "artists": ["A"],
        "album_name": "Album", "track_number": 7, "disc_number": disc, "disc_count": count,
        "url": format!("https://open.spotify.com/track/01234567890123456789{disc}{count}")})
    };
    fs::write(dir.join("discs.spotdl"), json!([song(1, 2), song(2, 2), song(1, 1)]).to_string()).unwrap();
    let text = ok(spotdl(
        &dir,
        &["save", "discs.spotdl", "--save-file", "-", "--output", "{title} {disc-track-number}.{output-ext}"],
    ));
    for line in ["S12 1-07.opus", "S22 2-07.opus", "S11 07.opus"] {
        assert!(text.contains(line), "{line} not in {text}");
    }
}

#[test]
fn downloads_tags_and_repairs_metadata() {
    if std::env::var("SPOTDL_E2E_DOWNLOAD").is_err() {
        return eprintln!("skipped: set SPOTDL_E2E_DOWNLOAD=1 to download from YouTube");
    }
    let dir = dir("download");
    let output = format!("{}/{{album}}/{{track-number}} - {{title}}.{{output-ext}}", dir.display());
    let (archive, list) = (dir.join("archive.txt"), dir.join("list.m3u8"));
    let pinned = format!("{VIDEO}|{ELEANOR}");
    let args = ["download", &pinned, TAXMAN, "--output", &output, "--archive", archive.to_str().unwrap()];
    let args = [&args[..], &["--m3u", list.to_str().unwrap(), "--create-skip-file"]].concat();
    ok(spotdl(&dir, &args));
    let albums = fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().path()).filter(|path| path.is_dir());
    let files: Vec<PathBuf> =
        albums.flat_map(|album| fs::read_dir(album).unwrap().map(|entry| entry.unwrap().path())).collect();
    let audio: Vec<&PathBuf> =
        files.iter().filter(|file| file.extension().is_some_and(|extension| extension == "opus")).collect();
    assert_eq!(audio.len(), 2, "{files:?}");
    for file in &audio {
        let tagged = lofty::read_from_path(file).unwrap();
        let tag = tagged.primary_tag().unwrap();
        let url = tag.get_string(&ItemKey::Unknown("WOAS".into())).unwrap();
        assert!(url == ELEANOR || url == TAXMAN);
        if url == ELEANOR {
            assert_eq!(tag.comment().as_deref(), Some(VIDEO));
        }
        assert!(tag.title().is_some() && tag.album().is_some_and(|album| album.contains("Revolver")));
        assert!(!tag.pictures().is_empty() && skip(file).exists());
        assert!(tag.genre().is_some_and(|genre| !genre.is_empty()), "{file:?} has no genre");
        assert!(tagged.properties().duration().as_secs() > 100);
    }
    assert_eq!(fs::read_to_string(&archive).unwrap().lines().count(), 2);
    assert_eq!(fs::read_to_string(&list).unwrap().lines().filter(|line| line.ends_with(".opus")).count(), 2);

    let modified = |file: &Path| fs::metadata(file).unwrap().modified().unwrap();
    let before = modified(audio[0]);
    ok(spotdl(&dir, &args));
    assert_eq!(modified(audio[0]), before);

    let mut tagged = lofty::read_from_path(audio[0]).unwrap();
    let title = tagged.primary_tag().unwrap().title().unwrap().to_string();
    tagged.primary_tag_mut().unwrap().remove_key(&ItemKey::TrackTitle);
    tagged.save_to_path(audio[0], WriteOptions::default()).unwrap();
    ok(spotdl(&dir, &["meta", audio[0].to_str().unwrap()]));
    assert_eq!(lofty::read_from_path(audio[0]).unwrap().primary_tag().unwrap().title().unwrap(), title);
}

/// A 10-second silent song with only a title, an artist and an album: no Spotify song is that short.
fn local_song(dir: &Path) -> PathBuf {
    let file = dir.join("song.opus");
    let ffmpeg = ["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono", "-t", "10"];
    let status = Command::new("ffmpeg").args(ffmpeg).args(["-c:a", "libopus"]).arg(&file).status().unwrap();
    assert!(status.success());
    let mut tagged = lofty::read_from_path(&file).unwrap();
    let mut tag = Tag::new(tagged.primary_tag_type());
    tag.set_title("That's Where You're Wrong".into());
    tag.set_artist("Arctic Monkeys".into());
    tag.set_album("Demos".into());
    tagged.insert_tag(tag);
    tagged.save_to_path(&file, WriteOptions::default()).unwrap();
    file
}

fn meta(dir: &Path, file: &Path, args: &[&str]) -> String {
    let mut meta = Command::new(env!("CARGO_BIN_EXE_spotdl"));
    let args = [&["meta", file.to_str().unwrap(), "--lyrics-providers", "genius"], args].concat();
    ok(meta.current_dir(dir).env("HOME", dir).args(args).output().unwrap())
}

fn lyrics(file: &Path) -> Option<String> {
    lofty::read_from_path(file).unwrap().primary_tag().unwrap().get_string(&ItemKey::Lyrics).map(String::from)
}

#[test]
fn meta_keeps_tags_of_songs_not_on_spotify_and_reads_only_genius_lyrics() {
    let dir = dir("meta");
    let file = local_song(&dir);
    meta(&dir, &file, &[]);
    let tagged = lofty::read_from_path(&file).unwrap();
    let tag = tagged.primary_tag().unwrap();
    assert_eq!(tag.album().as_deref(), Some("Demos"));
    assert!(tag.pictures().is_empty());
    let lyrics = lyrics(&file).unwrap();
    assert!(lyrics.contains("[Verse 1]") && lyrics.lines().count() > 20, "{lyrics}");
    for junk in ["You might also like", "Contributors", "Embed", "Translations"] {
        assert!(!lyrics.contains(junk), "{junk}: {lyrics}");
    }
}

/// A 3-minute Exodus song as another tool tags it: Spotify's track search finds the song on Legend, a compilation.
#[test]
fn meta_tags_songs_without_a_spotify_url_from_their_own_album_and_keeps_other_tags() {
    let dir = dir("meta-album");
    let tagged = |album: &str| {
        let file = dir.join(format!("{album}.opus"));
        let ffmpeg = ["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono", "-t", "180"];
        let status = Command::new("ffmpeg").args(ffmpeg).args(["-c:a", "libopus"]).arg(&file).status().unwrap();
        assert!(status.success());
        let mut tagged = lofty::read_from_path(&file).unwrap();
        let mut tag = Tag::new(tagged.primary_tag_type());
        tag.set_title("Three Little Birds".into());
        tag.set_artist("Bob Marley & The Wailers".into());
        tag.set_album(album.into());
        tag.set_track(9);
        tag.insert_text(ItemKey::MusicBrainzRecordingId, "abc-123".into());
        tag.insert_text(ItemKey::Composer, "Bob Marley".into());
        tag.insert_text(ItemKey::ReplayGainTrackGain, "-2.00 dB".into());
        tag.insert_text(ItemKey::ReplayGainTrackPeak, "0.900000".into());
        tagged.insert_tag(tag);
        tagged.save_to_path(&file, WriteOptions::default()).unwrap();
        file
    };
    let (exodus, rip) = (tagged("Exodus"), tagged("Live at Somewhere"));
    for file in [&exodus, &rip] {
        ok(spotdl(&dir, &["meta", file.to_str().unwrap()]));
    }
    let read = |file: &Path| lofty::read_from_path(file).unwrap().primary_tag().unwrap().clone();
    let (exodus, rip) = (read(&exodus), read(&rip));
    assert!(exodus.get_string(&ItemKey::Unknown("WOAS".into())).is_some_and(|url| url.contains("spotify")));
    assert!(!exodus.pictures().is_empty());
    // A song on no Spotify release of that title keeps its tags.
    assert!(rip.get_string(&ItemKey::Unknown("WOAS".into())).is_none());
    for (tag, album) in [(exodus, "Exodus"), (rip, "Live at Somewhere")] {
        assert_eq!((tag.album().as_deref(), tag.track()), (Some(album), Some(9)));
        assert_eq!(tag.get_string(&ItemKey::MusicBrainzRecordingId), Some("abc-123"));
        assert_eq!(tag.get_string(&ItemKey::Composer), Some("Bob Marley"));
        // The measured gain stays: Deezer's is only an estimate.
        assert_eq!(tag.get_string(&ItemKey::ReplayGainTrackGain), Some("-2.00 dB"));
        assert_eq!(tag.get_string(&ItemKey::ReplayGainTrackPeak), Some("0.900000"));
    }
}

/// An instrumental, tagged by spotdl but without lyrics. Its title is not Spotify's, so that tags written again show.
#[test]
fn meta_writes_only_the_lyrics_of_tagged_songs_and_does_not_look_again_for_lyrics_that_were_not_found() {
    let dir = dir("meta-instrumental");
    let song = |name: &str, looked: bool| {
        let file = dir.join(format!("{name}.opus"));
        let ffmpeg = ["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono", "-t", "10"];
        let status = Command::new("ffmpeg").args(ffmpeg).args(["-c:a", "libopus"]).arg(&file).status().unwrap();
        assert!(status.success());
        let mut tagged = lofty::read_from_path(&file).unwrap();
        let mut tag = Tag::new(tagged.primary_tag_type());
        tag.set_title("Fortune Days (local)".into());
        tag.set_artist("The Glitch Mob".into());
        tag.insert_text(ItemKey::TrackArtists, "The Glitch Mob".into());
        let keys = [("WOAS", FORTUNE_DAYS), ("ITUNESADVISORY", "0")];
        for (key, value) in keys.into_iter().chain(looked.then_some(("SPOTDL_NOLYRICS", "1"))) {
            tag.push_unchecked(TagItem::new(ItemKey::Unknown(key.into()), ItemValue::Text(value.into())));
        }
        tag.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Jpeg), None, vec![0xff, 0xd8]));
        tagged.insert_tag(tag);
        tagged.save_to_path(&file, WriteOptions::default()).unwrap();
        file
    };
    let (looked, new) = (song("looked", true), song("new", false));
    // Lyrics that were looked for and not found are not looked for again, and nothing is written.
    let before = fs::read(&looked).unwrap();
    meta(&dir, &looked, &[]);
    assert!(fs::read(&looked).unwrap() == before, "the tags were written again");
    // Missing lyrics alone write only the lyrics (or, when Genius answers without any, the key that says so).
    meta(&dir, &new, &[]);
    let tag = lofty::read_from_path(&new).unwrap().primary_tag().unwrap().clone();
    assert_eq!(tag.title().as_deref(), Some("Fortune Days (local)"));
    assert_eq!(tag.pictures()[0].data(), [0xff, 0xd8]);
}

#[test]
fn lyrics_aligner_times_lyrics_keeps_them_when_unsure_and_removes_lyrics_of_another_song() {
    let dir = dir("aligner");
    let file = local_song(&dir);
    let lrc = file.with_extension("lrc");
    let aligner = |name: &str, body: &str| {
        let script = dir.join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        script.to_str().unwrap().to_string()
    };
    let args = ["--generate-lrc", "--overwrite", "metadata", "--lyrics-aligner"];

    // The aligner gets the audio, the plain lyrics and the .lrc to write.
    let fits = aligner("fits.sh", r#"[ -f "$1" ] && grep -q "Verse 1" "$2" && echo "[00:01.00]timed" > "$3""#);
    meta(&dir, &file, &[&args[..], &[&fits]].concat());
    assert_eq!(fs::read_to_string(&lrc).unwrap(), "[00:01.00]timed\n");
    assert!(lyrics(&file).is_some());

    // Exit status 4: the aligner cannot hear the words, so the old .lrc stays.
    let unsure = aligner("unsure.sh", "exit 4");
    let output = meta(&dir, &file, &[&args[..], &[&unsure]].concat());
    assert_eq!(fs::read_to_string(&lrc).unwrap(), "[00:01.00]timed\n", "{output}");

    // Exit status 3: the lyrics are another song's, so the old .lrc goes and no lyrics go into the tags.
    let mut tagged = lofty::read_from_path(&file).unwrap();
    tagged.primary_tag_mut().unwrap().remove_key(&ItemKey::Lyrics);
    tagged.save_to_path(&file, WriteOptions::default()).unwrap();
    let other = aligner("other.sh", "exit 3");
    let output = meta(&dir, &file, &[&args[..], &[&other]].concat());
    assert!(!lrc.exists(), "{output}");
    assert!(lyrics(&file).is_none());
}
