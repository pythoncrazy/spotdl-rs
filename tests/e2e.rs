use lofty::{config::WriteOptions, prelude::*, tag::ItemKey};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

const DONDA: &str = "https://open.spotify.com/album/5CnpZV3q5BcESefcB3WJmz";
const REVOLVER: [(&str, &str); 2] = [
    ("https://music.youtube.com/watch?v=Uq0aeEYLkIE", "https://open.spotify.com/track/4KRgqwb4kvBuTz1utbYxfb"),
    ("https://music.youtube.com/watch?v=Tb9L3iAUhc0", "https://open.spotify.com/track/1ob06Ol8FMIPQgjpg7bMyk"),
];

fn fresh_dir(name: &str) -> PathBuf {
    let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = env::temp_dir().join(format!("spotdl-e2e-{name}-{}-{id}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn spotdl(dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spotdl"));
    command.current_dir(dir).env("HOME", dir).args(args);
    if let Ok(path) = env::var("SPOTDL_E2E_YT_DLP") {
        command.arg("--yt-dlp").arg(path);
    }
    if let Ok(args) = env::var("SPOTDL_E2E_YT_DLP_ARGS") {
        command.arg(format!("--yt-dlp-args={args}"));
    }
    command.output().expect("could not start spotdl")
}

fn output_text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
}

fn files_with_extension(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(files_with_extension(&path, extension));
        } else if path.extension().and_then(|value| value.to_str()) == Some(extension) {
            files.push(path);
        }
    }
    files
}

fn skip_file(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_os_string();
    path.push(".skip");
    path.into()
}

fn audio_title(file: &Path) -> String {
    lofty::read_from_path(file).unwrap().primary_tag().unwrap().title().unwrap().into()
}

fn saved_song<'a>(songs: &'a [Value], url: &str) -> &'a Value {
    songs.iter().find(|song| song["url"].as_str() == Some(url)).expect("Spotify track missing from save file")
}

#[test]
fn saves_donda_album_metadata() {
    let dir = fresh_dir("save");
    let save_file = dir.join("donda.spotdl").display().to_string();
    let output = spotdl(&dir, &["save", DONDA, "--save-file", &save_file, "--lyrics-providers="]);
    assert_success(&output);

    let songs: Vec<Value> = serde_json::from_slice(&fs::read(save_file).unwrap()).unwrap();
    assert_eq!(songs.len(), 27);
    let mut tracks = BTreeSet::new();
    for song in &songs {
        assert!(!song["name"].as_str().unwrap_or_default().trim().is_empty());
        assert!(!song["artists"].as_array().unwrap().is_empty());
        assert!(song["duration"].as_f64().unwrap_or_default() > 0.0);
        assert_eq!(song["album_name"], "Donda");
        tracks.insert(song["track_number"].as_u64().unwrap());
    }
    assert_eq!(tracks, (1..=27).collect());
}

fn watch_url(output: &Output) -> String {
    assert_success(output);
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        url.starts_with("https://music.youtube.com/watch?v=") || url.starts_with("https://www.youtube.com/watch?v="),
        "unexpected watch URL: {url}"
    );
    url
}

fn oembed_title(watch: &str) -> String {
    let url =
        format!("https://www.youtube.com/oembed?format=json&url={}", utf8_percent_encode(watch, NON_ALPHANUMERIC));
    let response: Value =
        ureq::get(&url).call().expect("YouTube oEmbed request failed").into_body().read_json().unwrap();
    response["title"].as_str().expect("YouTube oEmbed response has no title").into()
}

#[test]
fn resolves_spotify_tracks_to_youtube_urls() {
    let dir = fresh_dir("url");
    for (name, spotify, check_title) in [
        ("lovely", "https://open.spotify.com/track/0u2P5u6lvoDfwTYjAADbn4", false),
        ("Dior", "https://open.spotify.com/track/79s5XnCN4TJKTVMSmOx8Ep", false),
        ("Fly", "https://open.spotify.com/track/6mjEABpi5cQ5gqFFOkR1Cc", false),
        ("Kun tää loppuu", "https://open.spotify.com/track/10B96serXW72hB0N65H5Lt", false),
        ("Hurt", "https://open.spotify.com/track/3AfT3nmctE9nlndrFgshXH", true),
        ("Ghost", "https://open.spotify.com/track/4XPWKmy05Rcff6TLHNoNF8", true),
        ("Stuck with U", "https://open.spotify.com/track/4HBZA5flZLE435QTztThqH", true),
        ("Tornado", "https://open.spotify.com/track/3seNictp7GWitSNKlujwnY", true),
        ("Hey, Soul Sister", "https://open.spotify.com/track/0KpfYajJVVGgQ32Dby7e9i", true),
        ("It Wasn't Me", "https://open.spotify.com/track/1lzr43nnXAijIGYnCT8M8H", true),
    ] {
        let watch = watch_url(&spotdl(&dir, &["url", spotify]));
        if check_title {
            let title = oembed_title(&watch);
            let normalized = title.to_lowercase();
            assert!(
                !["karaoke", "bass boost", "bassboost", "tribute", "nightcore", "slowed", "sped up"]
                    .iter()
                    .any(|version| normalized.contains(version)),
                "{name} resolved to {title}"
            );
            assert!(
                !normalized.split(|c: char| !c.is_alphanumeric()).any(|word| word == "8d"),
                "{name} resolved to {title}"
            );
        }
    }
}

#[test]
fn downloads_revolver_tracks_and_updates_metadata() {
    let dir = fresh_dir("download");
    let save_file = dir.join("revolver.spotdl").display().to_string();
    let mut save_args = vec!["save"];
    save_args.extend(REVOLVER.iter().map(|(_, spotify)| *spotify));
    save_args.extend(["--save-file", &save_file, "--lyrics-providers="]);
    assert_success(&spotdl(&dir, &save_args));
    let songs: Vec<Value> = serde_json::from_slice(&fs::read(&save_file).unwrap()).unwrap();

    let output = format!("{}/{{album-artist}}/{{album}}/{{track-number}} - {{title}}.{{output-ext}}", dir.display());
    let archive = dir.join("archive.txt").display().to_string();
    let m3u = dir.join("list.m3u").display().to_string();
    let downloads = REVOLVER.iter().map(|(youtube, spotify)| format!("{youtube}|{spotify}")).collect::<Vec<_>>();
    let args = [
        "download",
        downloads[0].as_str(),
        downloads[1].as_str(),
        "--format",
        "opus",
        "--bitrate",
        "disable",
        "--output",
        &output,
        "--archive",
        &archive,
        "--m3u",
        &m3u,
        "--generate-lrc",
        "--create-skip-file",
        "--max-retries",
        "2",
    ];
    assert_success(&spotdl(&dir, &args));

    let files = files_with_extension(&dir, "opus");
    assert_eq!(files.len(), 2);
    for file in &files {
        let audio = lofty::read_from_path(file).unwrap();
        let tag = audio.primary_tag().unwrap();
        let title = tag.title().expect("audio has no title");
        let expected =
            songs.iter().find(|song| song["name"].as_str() == Some(title.as_ref())).expect("unexpected track title");
        let spotify = expected["url"].as_str().unwrap();
        let expected_video = REVOLVER.iter().find(|(_, url)| *url == spotify).expect("unexpected Spotify URL").0;
        let artist = tag.artist().unwrap_or_default();
        assert!(expected["artists"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|name| artist.contains(name)));
        assert_eq!(tag.album().as_deref(), expected["album_name"].as_str());
        assert_eq!(tag.track(), expected["track_number"].as_u64().map(|number| number as u32));
        assert!(!tag.pictures().is_empty());
        assert!((audio.properties().duration().as_secs_f64() - expected["duration"].as_f64().unwrap()).abs() < 5.0);
        let source = tag.get_string(&ItemKey::Unknown("WWWAUDIOFILE".into())).unwrap_or_default();
        assert_eq!(source, spotify);
        assert_eq!(tag.comment().as_deref(), Some(expected_video));
        assert!(skip_file(file).is_file());
    }

    let archived = fs::read_to_string(&archive).unwrap();
    assert!(REVOLVER.iter().all(|(_, url)| archived.lines().any(|line| line == *url)));
    let playlist = fs::read_to_string(&m3u).unwrap();
    assert_eq!(playlist.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count(), 2);
    assert!(files.iter().all(|file| playlist.contains(file.file_name().unwrap().to_str().unwrap())));

    let modified =
        files.iter().map(|file| (file.clone(), fs::metadata(file).unwrap().modified().unwrap())).collect::<Vec<_>>();
    assert_success(&spotdl(&dir, &args));
    for (file, time) in modified {
        assert_eq!(fs::metadata(file).unwrap().modified().unwrap(), time);
    }

    let file = &files[0];
    let title = audio_title(file);
    let mut audio = lofty::read_from_path(file).unwrap();
    audio.primary_tag_mut().unwrap().remove_key(&ItemKey::TrackTitle);
    audio.save_to_path(file, WriteOptions::default()).unwrap();
    let file = file.display().to_string();
    assert_success(&spotdl(&dir, &["meta", &file]));
    assert_eq!(audio_title(Path::new(&file)), title);
}

#[test]
fn sync_prunes_removed_tracks_and_skip_files() {
    let dir = fresh_dir("sync");
    let output = format!("{}/{{album-artist}}/{{album}}/{{track-number}} - {{title}}.{{output-ext}}", dir.display());
    let sync_file = dir.join("s.sync.spotdl").display().to_string();
    let options = [
        "--output",
        &output,
        "--format",
        "opus",
        "--bitrate",
        "disable",
        "--create-skip-file",
        "--lyrics-providers=",
        "--max-retries",
        "0",
    ];
    let mut first_args = vec!["sync", REVOLVER[0].1, REVOLVER[1].1, "--save-file", &sync_file];
    first_args.extend(options);
    assert_success(&spotdl(&dir, &first_args));

    let mut state: Value = serde_json::from_slice(&fs::read(&sync_file).unwrap()).unwrap();
    let old_songs = state["songs"].as_array().unwrap();
    let title_a = saved_song(old_songs, REVOLVER[0].1)["name"].as_str().unwrap();
    let title_b = saved_song(old_songs, REVOLVER[1].1)["name"].as_str().unwrap();
    let files = files_with_extension(&dir, "opus");
    let file_a = files.iter().find(|file| audio_title(file) == title_a).unwrap().clone();
    let file_b = files.iter().find(|file| audio_title(file) == title_b).unwrap().clone();

    state["query"] = json!([REVOLVER[0].1]);
    fs::write(&sync_file, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let mut next_args = vec!["sync", &sync_file];
    next_args.extend(options);
    assert_success(&spotdl(&dir, &next_args));
    assert!(file_a.is_file() && skip_file(&file_a).is_file());
    assert!(!file_b.exists() && !skip_file(&file_b).exists());
}

#[test]
fn reports_cli_option_and_config_errors() {
    let dir = fresh_dir("errors");
    let unknown = spotdl(&dir, &["--not-a-spotdl-option"]);
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8_lossy(&unknown.stderr).to_lowercase().contains("unknown option"),
        "{}",
        output_text(&unknown)
    );

    let config = spotdl(&dir, &["--config", "/nonexistent"]);
    assert!(!config.status.success());
    assert!(
        String::from_utf8_lossy(&config.stderr).to_lowercase().contains("config file not found"),
        "{}",
        output_text(&config)
    );
}
