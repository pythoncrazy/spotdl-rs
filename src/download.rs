use crate::{search, Song, R};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{mpsc::Sender, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub type LyricsJob = (usize, Song, PathBuf);

const ENRICHED: [&str; 7] = ["genre", "isrc", "publisher", "upc", "bpm", "gain", "original-date"];

pub fn uses_enriched_fields(template: &str) -> bool {
    ENRICHED.iter().any(|name| template.contains(&format!("{{{name}}}")))
}

pub fn fill(tpl: &str, s: &Song, cfg: &Value) -> String {
    let clean = |value: &str| {
        let kept: String = value.chars().filter(|c| !"/\\?%*:|\"<>".contains(*c) && !c.is_control()).collect();
        kept.trim().trim_end_matches('.').to_string()
    };
    let extra = [
        ("title", s.name.clone()),
        ("artists", s.artists.join(", ")),
        ("album", s.album_name.clone()),
        ("list", s.list_name.clone().unwrap_or_else(|| s.album_name.clone())),
        ("genre", s.genres.first().cloned().unwrap_or_default()),
        ("isrc", s.isrc.clone().unwrap_or_default()),
        ("publisher", s.publisher.clone()),
        ("upc", s.upc.clone()),
        ("bpm", s.bpm.to_string()),
        ("gain", s.gain.to_string()),
        ("original-date", if s.original_date.is_empty() { s.date.clone() } else { s.original_date.clone() }),
        ("track-id", s.song_id.clone()),
        ("track-number", format!("{:02}", s.track_number)),
        ("duration", (s.duration as u64).to_string()),
        ("output-ext", cfg["format"].as_str().unwrap_or("mp3").into()),
    ];
    let json = serde_json::to_value(s).unwrap_or_default();
    let text = |v: &Value| {
        if v.is_null() {
            String::new()
        } else {
            v.as_str().map_or(v.to_string(), String::from)
        }
    };
    let fields = json.as_object().into_iter().flatten().map(|(k, v)| (k.replace('_', "-"), text(v)));
    let vars = extra.into_iter().map(|(k, v)| (k.to_string(), v)).chain(fields);
    vars.fold(tpl.to_string(), |p, (k, v)| p.replace(&format!("{{{k}}}"), &clean(&v)))
}

pub fn path(s: &Song, cfg: &Value) -> PathBuf {
    let output = cfg["output"].as_str().unwrap_or_default();
    let tpl = if output.contains('{') { output.to_string() } else { format!("{output}/{{artists}} - {{title}}") };
    let max = cfg["max_filename_length"].as_u64().filter(|max| *max > 0).unwrap_or(u64::MAX) as usize;
    let part = |part: &str| {
        let value = fill(part, s, cfg);
        let value = match cfg["restrict"].as_str() {
            Some("strict") => search::slug(&value, "_"),
            Some("ascii") => deunicode::deunicode(&value),
            _ => value,
        };
        if value == "." || value == ".." {
            "_".to_string()
        } else {
            let mut component = String::new();
            for character in value.chars().take(max) {
                if component.len() + character.len_utf8() > 200 {
                    break;
                }
                component.push(character);
            }
            component
        }
    };
    let parts = tpl.trim_start_matches('/').trim_end_matches(".{output-ext}").split('/').map(part);
    let mut out = Path::new(if tpl.starts_with('/') { "/" } else { "" }).join(parts.collect::<PathBuf>());
    out.as_mut_os_string().push(format!(".{}", cfg["format"].as_str().unwrap_or("mp3")));
    out
}

fn skip_path(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_os_string();
    path.push(".skip");
    path.into()
}

fn html(page: &str, start: &str, end: &str) -> Option<String> {
    let region = page.split_once(start)?.1.split(end).next()?;
    let (_, text) = region.split('<').enumerate().fold((0, String::new()), |(skip, mut out), (i, p)| {
        let (tag, text) =
            if i == 0 { ("", p.split_once('>').map_or(p, |x| x.1)) } else { p.split_once('>').unwrap_or((p, "")) };
        let skip = match tag {
            t if t.contains("data-exclude-from-selection") => skip + 1,
            t if skip > 0 && t.starts_with("div") => skip + 1,
            "/div" if skip > 0 => skip - 1,
            _ => skip,
        };
        out.push_str(if skip == 0 && tag.starts_with("br") { "\n" } else { "" });
        out.push_str(if skip == 0 { text } else { "" });
        (skip, out)
    });
    Some(text.replace("&#x27;", "'").replace("&quot;", "\"").replace("&amp;", "&"))
}

const LRCLIB_INTERVAL: Duration = Duration::from_millis(750);
static LRCLIB_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
const GENIUS_INTERVAL: Duration = Duration::from_millis(1000);
static GENIUS_NEXT: Mutex<Option<Instant>> = Mutex::new(None);

fn wait_turn(next: &Mutex<Option<Instant>>, interval: Duration) {
    let now = Instant::now();
    let slot = {
        let mut next = next.lock().unwrap_or_else(|error| error.into_inner());
        let slot = next.map_or(now, |next| now.max(next));
        *next = Some(slot + interval);
        slot
    };
    std::thread::sleep(slot.saturating_duration_since(Instant::now()));
}

fn lrclib_request(url: &str, cfg: &Value) -> Option<Vec<u8>> {
    let mut request_cfg = cfg.clone();
    request_cfg["max_retries"] = json!(0);
    for attempt in 0..5 {
        wait_turn(&LRCLIB_NEXT, LRCLIB_INTERVAL);
        let response = crate::http("GET", url, &[], None, &request_cfg);
        match response {
            Ok(bytes) => return Some(bytes),
            Err(error) => {
                let status = error.to_string().split_whitespace().nth(1).and_then(|status| status.parse::<u16>().ok());
                let status = status.filter(|status| *status == 429 || (500..600).contains(status))?;
                crate::log(cfg, "DEBUG", &format!("LRCLIB HTTP {status} on attempt {}/5", attempt + 1));
                if attempt == 4 {
                    return None;
                }
                std::thread::sleep(Duration::from_secs(1 << (attempt + 1)));
            }
        }
    }
    None
}

pub fn lyrics(song: &Song, cfg: &Value) -> Option<String> {
    let mut lyrics_cfg = cfg.clone();
    lyrics_cfg["max_retries"] = json!(1);
    lyrics_cfg["timeout"] = json!(10);
    let page = |url: &str, headers: &[(&str, &str)]| {
        let bytes = crate::http("GET", url, headers, None, &lyrics_cfg).ok()?;
        String::from_utf8(bytes).ok()
    };
    let encode = crate::url_encode;
    let provider = |name: &str| {
        let text = match name {
            "synced" => {
                let url = format!(
                    "https://lrclib.net/api/search?artist_name={}&track_name={}",
                    encode(&song.artist),
                    encode(search::versionless(&song.name))
                );
                let bytes = lrclib_request(&url, &lyrics_cfg)?;
                let tracks: Vec<Value> = serde_json::from_slice(&bytes).ok()?;
                let hit = tracks
                    .iter()
                    .find(|track| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0)?;
                hit["syncedLyrics"].as_str().or(hit["plainLyrics"].as_str())?.to_string()
            }
            "genius" => {
                let url = format!(
                    "https://genius.com/api/search/song?q={}",
                    encode(&format!("{} {}", song.artist, search::versionless(&song.name)))
                );
                let auth = format!("Bearer {}", cfg["genius_token"].as_str().unwrap_or_default());
                let headers: &[(&str, &str)] = if auth.len() > 7 { &[("Authorization", &auth)] } else { &[] };
                wait_turn(&GENIUS_NEXT, GENIUS_INTERVAL);
                let reply: Value = crate::http_json("GET", &url, headers, None, &lyrics_cfg).ok()?;
                let artist = search::slug(&song.artist, "");
                let hits = reply["response"]["sections"][0]["hits"].as_array()?;
                let url = hits.iter().find(|hit| {
                    let primary =
                        search::slug(hit["result"]["primary_artist"]["name"].as_str().unwrap_or_default(), "");
                    !artist.is_empty()
                        && !primary.is_empty()
                        && (primary == artist || primary.contains(&artist) || artist.contains(&primary))
                })?["result"]["url"]
                    .as_str()?;
                wait_turn(&GENIUS_NEXT, GENIUS_INTERVAL);
                html(&page(url, &[])?, "data-lyrics-container=\"true\"", "LyricsFooter")?
            }
            "azlyrics" => {
                let artist = search::slug(&song.artist, "");
                let title = search::slug(search::versionless(&song.name), "");
                html(
                    &page(&format!("https://www.azlyrics.com/lyrics/{artist}/{title}.html"), &[])?,
                    "<!-- Usage of azlyrics.com",
                    "</div>",
                )?
            }
            _ => {
                let url = format!(
                    "https://www.musixmatch.com/lyrics/{}/{}",
                    search::slug(&song.artist, "-"),
                    search::slug(search::versionless(&song.name), "-")
                );
                let body = page(&url, &[])?.split_once("\"body\":")?.1.to_string();
                serde_json::Deserializer::from_str(&body).into_iter::<String>().next()?.ok()?
            }
        };
        Some(text.trim().to_string()).filter(|text| !text.is_empty())
    };
    cfg["lyrics_providers"].as_array().into_iter().flatten().filter_map(Value::as_str).find_map(provider)
}

pub fn add_lyrics(song: &Song, file: &Path, cfg: &Value) -> Option<String> {
    let text = lyrics(song, cfg)?;
    if let Err(error) = set_lyrics(file, &text).and_then(|_| write_lrc(file, Some(&text), cfg)) {
        crate::log(cfg, "ERROR", &format!("{} - {}: {error}", song.artists.join(", "), song.name));
        return None;
    }
    Some(text)
}

fn set_lyrics(file: &Path, lyrics: &str) -> R<()> {
    let mut audio = lofty::read_from_path(file)?;
    let format = audio.primary_tag_type();
    if audio.primary_tag().is_none() {
        audio.insert_tag(Tag::new(format));
    }
    let tag = audio.primary_tag_mut().ok_or("no primary tag")?;
    if !tag.insert_text(ItemKey::Lyrics, lyrics.into()) {
        return Err("lyrics are not supported by the primary tag".into());
    }
    audio.save_to_path(file, WriteOptions::default())?;
    Ok(())
}

fn tag(file: &Path, s: &Song, cfg: &Value) -> R<()> {
    let mut f = lofty::read_from_path(file)?;
    let format = f.primary_tag_type();
    let url_key = audio_url_key(format);
    let mut t = Tag::new(format);
    t.set_title(s.name.clone());
    t.set_artist(s.artists.join(cfg["id3_separator"].as_str().unwrap_or("/")));
    t.set_album(s.album_name.clone());
    t.set_track(s.track_number);
    t.set_track_total(s.tracks_count);
    t.set_disk(s.disc_number);
    t.set_disk_total(s.disc_count);
    t.set_comment(s.download_url.clone().unwrap_or_default());
    let id3 = format == TagType::Id3v2;
    let year = if s.year > 0 { s.year.to_string() } else { String::new() };
    let bpm = if id3 { s.bpm.round() } else { s.bpm };
    let bpm = if s.bpm > 0.0 { bpm.to_string() } else { String::new() };
    let bpm_key = if id3 { ItemKey::IntegerBpm } else { ItemKey::Bpm };
    let gain = if s.gain != 0.0 { format!("{:.2} dB", -18.0 - s.gain) } else { String::new() };
    let compilation = if s.album_type == "compilation" { "1".into() } else { String::new() };
    let artists = if id3 { vec![s.artists.join("\0")] } else { s.artists.clone() };
    let mut tags = vec![
        (ItemKey::AlbumArtist, vec![s.album_artist.clone()]),
        (ItemKey::RecordingDate, vec![s.date.clone()]),
        (ItemKey::ReleaseDate, vec![s.date.clone()]),
        (ItemKey::OriginalReleaseDate, vec![s.original_date.clone()]),
        (ItemKey::Year, vec![year]),
        (ItemKey::Isrc, vec![s.isrc.clone().unwrap_or_default()]),
        (ItemKey::Label, vec![s.publisher.clone()]),
        (ItemKey::CopyrightMessage, vec![s.copyright_text.clone().unwrap_or_default()]),
        (ItemKey::Lyrics, vec![s.lyrics.clone().unwrap_or_default()]),
        (bpm_key, vec![bpm]),
        (ItemKey::Barcode, vec![s.upc.clone()]),
        (ItemKey::ReplayGainTrackGain, vec![gain]),
        (ItemKey::FlagCompilation, vec![compilation]),
        (ItemKey::ParentalAdvisory, vec![if s.explicit { "1" } else { "0" }.into()]),
        (ItemKey::TrackArtists, artists),
        (ItemKey::Genre, s.genres.clone()),
        (ItemKey::Unknown("RELEASETYPE".into()), vec![s.album_type.clone()]),
    ];
    if id3 {
        tags.push((ItemKey::AudioSourceUrl, vec![s.url.clone()]));
    }
    tags.push((url_key, vec![s.url.clone()]));
    tags.into_iter().for_each(|(key, values)| {
        values.into_iter().filter(|value| !value.is_empty()).for_each(|value| {
            let item_value = if key == ItemKey::AudioFileUrl || key == ItemKey::AudioSourceUrl {
                ItemValue::Locator(value)
            } else {
                ItemValue::Text(value)
            };
            let item = TagItem::new(key.clone(), item_value);
            if matches!(key, ItemKey::Unknown(_)) {
                t.insert_unchecked(item);
            } else {
                t.push(item);
            }
        });
    });
    let art = s
        .cover_url
        .as_ref()
        .filter(|_| cfg["skip_album_art"] != true)
        .and_then(|u| crate::http("GET", u, &[], None, cfg).ok());
    art.into_iter()
        .for_each(|a| t.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Jpeg), None, a)));
    f.clear();
    if file.extension().is_some_and(|e| e == "wav") {
        f.insert_tag(lofty::iff::wav::RiffInfoList::from(t.clone()).into());
    }
    f.insert_tag(t);
    f.save_to_path(file, WriteOptions::default()).map_err(Into::into)
}

fn audio_url_key(format: TagType) -> ItemKey {
    match format {
        TagType::Id3v2 => ItemKey::AudioFileUrl,
        TagType::Mp4Ilst => ItemKey::Unknown("----:com.apple.iTunes:WWWAUDIOFILE".into()),
        _ => ItemKey::Unknown("WWWAUDIOFILE".into()),
    }
}

fn tag_value(tag: &Tag, key: &ItemKey) -> Option<String> {
    tag.get(key).and_then(|item| item.value().text().or(item.value().locator())).map(String::from)
}

fn audio_url(tag: &Tag, format: TagType) -> Option<String> {
    [audio_url_key(format), ItemKey::AudioFileUrl, ItemKey::AudioSourceUrl]
        .iter()
        .find_map(|key| tag_value(tag, key).filter(|value| !value.is_empty()))
}

fn yt_dlp(cfg: &Value) -> Command {
    let mut command = Command::new(cfg["yt_dlp"].as_str().unwrap_or("yt-dlp"));
    for (key, flag) in [("cookie_file", "--cookies"), ("proxy", "--proxy")] {
        if let Some(value) = cfg[key].as_str() {
            command.args([flag, value]);
        }
    }
    command.args(cfg["yt_dlp_args"].as_str().unwrap_or_default().split_whitespace());
    search::js_runtime_args(&mut command);
    command
}

fn download<'a>(command: &'a mut Command, selector: &str, template: &Path, cfg: &Value) -> &'a mut Command {
    command
        .args(["-f", selector])
        .args("--no-playlist --no-warnings --force-overwrites -q --print after_move:filepath -o".split_whitespace())
        .arg(template);
    if cfg["sponsor_block"] == true {
        command.args(["--sponsorblock-remove", "all"]);
    }
    command
}

fn downloaded(output: &Output) -> Option<PathBuf> {
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (output.status.success() && path.is_file()).then_some(path)
}

fn skip_ad_wait(info: &mut Value) {
    match info {
        Value::Object(object) => {
            object.remove("available_at");
            object.values_mut().for_each(skip_ad_wait);
        }
        Value::Array(values) => values.iter_mut().for_each(skip_ad_wait),
        _ => {}
    }
}

fn remove_partial_download(out: &Path, id: &str) {
    let Some(stem) = out.file_stem() else { return };
    let prefix = format!("{}.{id}.", stem.to_string_lossy());
    let directory = out.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn fetch(url: &str, out: &Path, id: &str, cfg: &Value) -> R<()> {
    let format = cfg["format"].as_str().unwrap_or("mp3");
    let bitrate = cfg["bitrate"].as_str().unwrap_or("auto");
    let selectors = [("m4a", "bestaudio[ext=m4a]/bestaudio/best"), ("opus", "bestaudio[ext=webm]/bestaudio/best")];
    let selector = selectors.iter().find(|entry| entry.0 == format).map_or("bestaudio/best", |entry| entry.1);
    let template = out.with_extension(format!("{id}.%(ext)s"));
    let extracted = search::yt(
        yt_dlp(cfg).args(["-f", selector, "--no-playlist", "--no-warnings", "-q", "-j"]).arg(url),
        cfg,
        &search::DOWNLOAD_GATE,
    )?;
    if !extracted.status.success() {
        return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&extracted.stderr).trim()).into());
    }
    let mut info: Value = serde_json::from_slice(&extracted.stdout)?;
    skip_ad_wait(&mut info);
    let info_file = out.with_extension(format!("{id}.info.json"));
    if let Err(error) = fs::write(&info_file, serde_json::to_vec(&info)?) {
        let _ = fs::remove_file(&info_file);
        return Err(error.into());
    }
    let mut y = yt_dlp(cfg);
    y.args(["--load-info-json"]).arg(&info_file);
    let result = download(&mut y, selector, &template, cfg).output();
    let _ = fs::remove_file(&info_file);
    let got = match result.ok().and_then(|output| downloaded(&output)) {
        Some(got) => got,
        None => {
            remove_partial_download(out, id);
            let mut y = yt_dlp(cfg);
            let res = search::yt(download(&mut y, selector, &template, cfg).arg(url), cfg, &search::DOWNLOAD_GATE)?;
            match downloaded(&res) {
                Some(got) => got,
                None => {
                    let got = PathBuf::from(String::from_utf8_lossy(&res.stdout).trim());
                    let _ = fs::remove_file(got);
                    return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&res.stderr).trim()).into());
                }
            }
        }
    };
    let ext = got.extension().unwrap_or_default().to_string_lossy().to_string();
    let keep = ["auto", "disable"].contains(&bitrate);
    let temp = out.with_extension(format!("{id}.converted.{format}"));
    let converted = if ext == format && keep {
        fs::rename(&got, &temp).map_err(Into::into)
    } else {
        let codecs =
            [("mp3", "libmp3lame"), ("flac", "flac"), ("ogg", "libvorbis"), ("opus", "libopus"), ("m4a", "aac")];
        let codec = codecs.iter().find(|entry| entry.0 == format).map_or("pcm_s16le", |entry| entry.1);
        let codec = if keep && ext == "webm" && format == "opus" { "copy" } else { codec };
        let mut f = Command::new(cfg["ffmpeg"].as_str().unwrap_or("ffmpeg"));
        f.args("-nostdin -y -loglevel error -i".split_whitespace())
            .arg(&got)
            .args("-vn -map_metadata -1 -c:a".split_whitespace())
            .arg(codec);
        if !keep {
            f.args(["-b:a", bitrate]);
        }
        let res = f.args(cfg["ffmpeg_args"].as_str().unwrap_or_default().split_whitespace()).arg(&temp).output();
        let _ = fs::remove_file(&got);
        match res {
            Ok(res) if res.status.success() => Ok(()),
            Ok(res) => Err(format!("ffmpeg: {}", String::from_utf8_lossy(&res.stderr).trim()).into()),
            Err(error) => Err(error.into()),
        }
    };
    let result: R<()> = converted.and_then(|_| Ok(fs::rename(&temp, out)?));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn append(path: Option<&str>, line: &str) -> R<()> {
    if let Some(path) = path {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())?;
    }
    Ok(())
}

fn audio_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let extension = path.extension().map(|value| value.to_string_lossy().to_lowercase()).unwrap_or_default();
        if file_type.is_dir() {
            audio_files(&path, files);
        } else if file_type.is_file() && ["mp3", "flac", "ogg", "opus", "m4a", "wav"].contains(&extension.as_str()) {
            files.push(path);
        }
    }
}

static SCAN: std::sync::OnceLock<std::collections::HashMap<String, PathBuf>> = std::sync::OnceLock::new();
static ARCHIVED: OnceLock<HashSet<String>> = OnceLock::new();

fn write_lrc(file: &Path, lyrics: Option<&str>, cfg: &Value) -> R<()> {
    let sidecar = file.with_extension("lrc");
    let replace = matches!(cfg["overwrite"].as_str(), Some("force" | "metadata"));
    lyrics
        .filter(|text| cfg["generate_lrc"] == true && (replace || !sidecar.exists()) && is_synced(text))
        .map(|text| fs::write(sidecar, text))
        .transpose()?;
    Ok(())
}

fn is_synced(text: &str) -> bool {
    text.lines().any(|line| {
        let minutes = line.bytes().skip(1).take_while(u8::is_ascii_digit).count();
        line.starts_with('[') && minutes > 0 && line.as_bytes().get(minutes + 1) == Some(&b':')
    })
}

fn finish(s: &mut Song, op: &str, cfg: &Value, index: usize, lyrics_tx: Option<&Sender<LyricsJob>>) -> R<PathBuf> {
    let should_enrich = op != "save" && op != "url";
    let enriched_for_output = should_enrich && cfg["output"].as_str().is_some_and(uses_enriched_fields);
    if enriched_for_output {
        crate::spotify::enrich(s, cfg);
    }
    let out = path(s, cfg);
    let formats = cfg["detect_formats"].as_array().into_iter().flatten().filter_map(Value::as_str);
    let detected = formats.map(|format| out.with_extension(format)).find(|path| path.is_file());
    let scan = || {
        let mut files = Vec::new();
        let output = Path::new(cfg["output"].as_str().unwrap_or_default());
        let root: PathBuf =
            output.components().take_while(|part| !part.as_os_str().to_string_lossy().contains('{')).collect();
        audio_files(if root.as_os_str().is_empty() { Path::new(".") } else { &root }, &mut files);
        files
            .into_iter()
            .filter_map(|file| {
                let tags = lofty::read_from_path(&file).ok()?;
                audio_url(tags.primary_tag()?, tags.primary_tag_type()).map(|url| (url, file))
            })
            .collect()
    };
    let scanned = (cfg["scan_for_songs"] == true && op != "save").then(|| SCAN.get_or_init(scan).get(&s.url).cloned());
    let mut target =
        scanned.flatten().or(Some(out.clone()).filter(|out| out.is_file())).or(detected).unwrap_or(out.clone());
    if op == "save" {
        if cfg["preload"] == true {
            s.download_url = search::first_hit(s, cfg).ok();
        }
        s.lyrics = lyrics(s, cfg);
        return Ok(target);
    }
    let archive = cfg["archive"].as_str();
    let archived = archive.is_some_and(|file| {
        ARCHIVED
            .get_or_init(|| fs::read_to_string(file).unwrap_or_default().lines().map(String::from).collect())
            .contains(&s.url)
    });
    let skip = cfg["respect_skip_file"] == true && skip_path(&target).exists();
    let exists = target.is_file();
    let overwrite = cfg["overwrite"].as_str().unwrap_or("skip");
    if op != "url" && (archived || skip || exists && overwrite == "skip" && cfg["force_update_metadata"] != true) {
        if exists && !archived {
            append(archive, &s.url)?;
        }
        return Ok(target);
    }
    if should_enrich && !enriched_for_output {
        crate::spotify::enrich(s, cfg);
    }
    let will_download = op != "url" && (!exists || overwrite == "force");
    if op == "url" {
        if s.download_url.is_none() {
            s.download_url = Some(search::first_hit(s, cfg)?);
        }
        return Ok(target);
    }
    if will_download {
        fs::create_dir_all(out.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
        let id = if s.song_id.is_empty() { format!("{:x}", Sha1::digest(s.url.as_bytes())) } else { s.song_id.clone() };
        let mut error = String::from("no match");
        let mut tried = HashSet::new();
        let mut fetch_first = |urls: Vec<String>| {
            urls.into_iter().find_map(|url| {
                if !tried.insert(url.clone()) {
                    return None;
                }
                fetch(&url, &out, &id, cfg).map(|()| Some(url)).unwrap_or_else(|fetch_error| {
                    error = fetch_error.to_string();
                    None
                })
            })
        };
        let mut downloaded = fetch_first(s.download_url.clone().into_iter().collect());
        let isrc = if downloaded.is_none() { search::isrc_hit(s, cfg) } else { Ok(None) };
        if let Ok(Some(url)) = isrc.as_ref() {
            downloaded = fetch_first(vec![url.clone()]);
        }
        if downloaded.is_none() {
            let candidates = match search::find(s, cfg, isrc.err().map(|error| error.to_string())) {
                Ok(candidates) => candidates,
                Err(search_error) if search_error.to_string().starts_with("no match for ") => {
                    drop(fetch_first);
                    return Err(if error == "no match" { search_error } else { error.into() });
                }
                Err(search_error) => return Err(search_error),
            };
            downloaded = fetch_first(candidates);
        }
        drop(fetch_first);
        s.download_url = Some(downloaded.ok_or(error)?);
        if target != out && target.is_file() {
            fs::remove_file(&target)?;
        }
        target = out.clone();
    }
    if lyrics_tx.is_none() && s.lyrics.is_none() {
        s.lyrics = lyrics(s, cfg);
    }
    tag(&target, s, cfg).and_then(|_| write_lrc(&target, s.lyrics.as_deref(), cfg))?;
    if cfg["create_skip_file"] == true {
        fs::write(skip_path(&target), "")?;
    }
    append(archive, &s.url)?;
    if let Some(sender) = lyrics_tx.filter(|_| s.lyrics.is_none()) {
        sender.send((index, s.clone(), target.clone()))?;
    }
    Ok(target)
}

pub fn process(
    mut s: Song,
    op: &str,
    cfg: &Value,
    index: usize,
    lyrics_tx: Option<&Sender<LyricsJob>>,
) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", s.artists.join(", "), s.name);
    let result = finish(&mut s, op, cfg, index, lyrics_tx);
    match &result {
        Ok(_) if op == "url" => println!("{}", s.download_url.as_deref().unwrap_or_default()),
        Ok(path) if cfg["simple_tui"] == true => println!("{label}: {}", path.display()),
        Ok(path) => crate::log(cfg, "INFO", &format!("{label}: {}", path.display())),
        Err(error) => {
            crate::log(cfg, "ERROR", &format!("{label}: {error}"));
            if cfg["add_unavailable"] == true && error.to_string().starts_with("no match for ") {
                let _ = append(cfg["archive"].as_str(), &s.url);
            }
        }
    }
    (s, result)
}

pub fn prune(old: &[Song], new: &[Song], cfg: &Value) {
    let lrc = cfg["sync_remove_lrc"] == true;
    let count = 2 + usize::from(lrc);
    let mut by_url = HashMap::new();
    for song in new {
        by_url.entry(song.url.as_str()).or_insert(song);
    }
    for song in old {
        let from = path(song, cfg);
        let to = by_url.get(song.url.as_str()).map(|next| path(next, cfg));
        if to.as_ref().is_some_and(|to| *to == from) {
            continue;
        }
        let targets = to
            .as_ref()
            .filter(|to| **to != from && !to.exists())
            .map(|to| [to.clone(), skip_path(to), to.with_extension("lrc")]);
        let sources = [from.clone(), skip_path(&from), from.with_extension("lrc")];
        for (index, source) in sources.iter().enumerate().take(count) {
            let (result, description) = if let Some(targets) = &targets {
                let target = &targets[index];
                (fs::rename(source, target), format!("could not move {} to {}", source.display(), target.display()))
            } else {
                (fs::remove_file(source), format!("could not remove {}", source.display()))
            };
            prune_result(result, &description, cfg);
        }
    }
}

fn prune_result(result: std::io::Result<()>, description: &str, cfg: &Value) {
    let Err(error) = result else { return };
    if error.kind() != std::io::ErrorKind::NotFound {
        crate::log(cfg, "ERROR", &format!("{description}: {error}"));
    }
}

fn meta_file(file: &Path, cfg: &Value) -> R<()> {
    let tags = lofty::read_from_path(file)?;
    let format = tags.primary_tag_type();
    let primary = tags.primary_tag().ok_or("no tags")?;
    let value = |key: &ItemKey| tag_value(primary, key);
    let filled = |key: &ItemKey| value(key).is_some_and(|value| !value.is_empty());
    let complete = primary.title().is_some_and(|title| !title.is_empty())
        && primary.artist().is_some_and(|artist| !artist.is_empty())
        && filled(&ItemKey::TrackArtists)
        && filled(&ItemKey::Lyrics)
        && !primary.pictures().is_empty();
    let force = cfg["force_update_metadata"] == true;
    let redownload = cfg["redownload"] == true;
    let needs_lrc = cfg["generate_lrc"] == true && !file.with_extension("lrc").exists();
    if complete && !force && !redownload {
        if needs_lrc {
            let song = Song { lyrics: value(&ItemKey::Lyrics), ..Song::default() };
            write_lrc(file, song.lyrics.as_deref(), cfg)?;
        }
        return Ok(());
    }
    let url = audio_url(primary, format).filter(|url| url.contains("spotify"));
    let query = url.unwrap_or_else(|| {
        format!("{} - {}", primary.artist().unwrap_or_default(), primary.title().unwrap_or_default())
    });
    let mut song = crate::spotify::resolve(&query, cfg)?.into_iter().next().ok_or("no match")?;
    crate::spotify::enrich(&mut song, cfg);
    song.lyrics = value(&ItemKey::Lyrics).or_else(|| lyrics(&song, cfg));
    if !complete || force {
        tag(file, &song, cfg)?;
    }
    write_lrc(file, song.lyrics.as_deref(), cfg)?;
    if redownload {
        let mut cfg = cfg.clone();
        cfg["overwrite"] = json!("force");
        let songs = if cfg["fetch_albums"] == true {
            crate::spotify::resolve(&format!("https://open.spotify.com/album/{}", song.album_id), &cfg)?
        } else {
            vec![song]
        };
        for song in songs {
            process(song, "download", &cfg, 0, None).1?;
        }
    }
    Ok(())
}

pub fn meta(files: &[String], cfg: &Value) -> R<()> {
    let mut listed = Vec::new();
    for file in files.iter().map(PathBuf::from) {
        if file.is_file() {
            listed.push(file)
        } else {
            audio_files(&file, &mut listed)
        }
    }
    let total = listed.len();
    let mut failed = 0;
    for file in listed {
        let error = meta_file(&file, cfg).err();
        failed += error.is_some() as usize;
        println!("{}: {}", file.display(), error.unwrap_or_else(|| "updated".into()));
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} of {total} files failed").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_turn_spaces_requests() {
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        let start = Instant::now();
        for _ in 0..3 {
            wait_turn(&NEXT, Duration::from_millis(50));
        }
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn skip_ad_wait_removes_nested_available_at_fields() {
        let mut info = json!({
            "id": "track",
            "available_at": 42,
            "title": "title",
            "formats": [{"format_id": "251", "available_at": 12}],
            "requested_formats": [{
                "format_id": "251",
                "available_at": 13,
                "fragments": [{"url": "audio", "available_at": 14}]
            }]
        });
        skip_ad_wait(&mut info);
        assert_eq!(
            info,
            json!({
                "id": "track",
                "title": "title",
                "formats": [{"format_id": "251"}],
                "requested_formats": [{
                    "format_id": "251",
                    "fragments": [{"url": "audio"}]
                }]
            })
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spotdl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_limits_components_and_fills_enriched_template_fields() {
        let song = Song {
            name: "é".repeat(300),
            upc: "012345678905".into(),
            bpm: 128.5,
            date: "1987-06-02".into(),
            ..Song::default()
        };
        let template = "{upc} {bpm} {original-date}";
        assert_eq!(fill(template, &song, &json!({})), "012345678905 128.5 1987-06-02");
        assert!(uses_enriched_fields(template));

        let cfg = json!({"output": "{title}", "format": "mp3"});
        let name = path(&song, &cfg).file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(name.len(), 200);

        let cfg = json!({"output": "{title}", "format": "mp3", "max_filename_length": 25});
        let name = path(&song, &cfg).file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(name.chars().count(), 25);
        assert_eq!(name.len(), 50);
    }

    #[test]
    fn prune_moves_and_removes_marker_only_skip_files() {
        let dir = temp_dir("prune-skip");
        let cfg = json!({"output": dir.join("{title}").to_string_lossy(), "format": "mp3"});
        let old = Song { name: "Old".into(), url: "track".into(), ..Song::default() };
        let new = Song { name: "New".into(), ..old.clone() };
        let old_marker = skip_path(&path(&old, &cfg));
        let new_marker = skip_path(&path(&new, &cfg));
        fs::write(&old_marker, "").unwrap();

        prune(&[old], std::slice::from_ref(&new), &cfg);
        assert!(!old_marker.exists());
        assert!(new_marker.exists());
        prune(std::slice::from_ref(&new), &[], &cfg);
        assert!(!new_marker.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overwrite_refreshes_existing_lrc_sidecars() {
        assert!(is_synced("[00:01.00] a\n[00:02.00] b"));
        assert!(!is_synced("[Intro: X]\nline"));
        assert!(is_synced("[ar:Artist]\n[00:01.00] a"));
        let dir = temp_dir("lrc-overwrite");
        let audio = dir.join("track.mp3");
        for overwrite in ["force", "metadata"] {
            fs::write(audio.with_extension("lrc"), "[00:00.00] Old lyrics").unwrap();
            let cfg = json!({"generate_lrc": true, "overwrite": overwrite});
            write_lrc(&audio, Some("[00:01.00] New lyrics"), &cfg).unwrap();
            assert_eq!(fs::read_to_string(audio.with_extension("lrc")).unwrap(), "[00:01.00] New lyrics");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn meta_returns_the_failed_file_count() {
        let dir = temp_dir("meta-errors");
        let file = dir.join("invalid.mp3");
        fs::write(&file, "not an audio file").unwrap();
        let error = meta(&[file.display().to_string()], &json!({})).unwrap_err().to_string();
        assert_eq!(error, "1 of 1 files failed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn spotify_url_round_trips_for_supported_audio_formats() {
        let Ok(version) = Command::new("ffmpeg").arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }

        let dir = temp_dir("url-tags");
        let song = Song {
            name: "Test".into(),
            artists: vec!["Test Artist".into()],
            album_name: "Test Album".into(),
            track_number: 7,
            url: "https://open.spotify.com/track/test".into(),
            ..Song::default()
        };
        let cfg = json!({"skip_album_art": true});

        for (extension, codec) in
            [("mp3", "libmp3lame"), ("m4a", "aac"), ("opus", "libopus"), ("ogg", "libvorbis"), ("flac", "flac")]
        {
            let file = dir.join(format!("test.{extension}"));
            let result = Command::new("ffmpeg")
                .args(
                    "-nostdin -y -loglevel error -f lavfi -i anullsrc=r=44100:cl=stereo -t 0.1 -c:a".split_whitespace(),
                )
                .arg(codec)
                .arg(&file)
                .output()
                .unwrap();
            assert!(result.status.success(), "{extension}: {}", String::from_utf8_lossy(&result.stderr));
            tag(&file, &song, &cfg).unwrap();
            let cover = [0xff, 0xd8, 0xff, 0xd9];
            {
                let mut tagged = lofty::read_from_path(&file).unwrap();
                tagged.primary_tag_mut().unwrap().push_picture(Picture::new_unchecked(
                    PictureType::CoverFront,
                    Some(MimeType::Jpeg),
                    None,
                    cover.to_vec(),
                ));
                tagged.save_to_path(&file, WriteOptions::default()).unwrap();
            }
            let lyrics = "[00:00.00] Local lyrics";
            set_lyrics(&file, lyrics).unwrap();
            let tags = lofty::read_from_path(&file).unwrap();
            let format = tags.primary_tag_type();
            let primary = tags.primary_tag().unwrap();
            assert_eq!(audio_url(primary, format).as_deref(), Some(song.url.as_str()), "{extension}");
            assert_eq!(primary.title().as_deref(), Some(song.name.as_str()), "{extension}");
            assert_eq!(primary.artist().as_deref(), Some("Test Artist"), "{extension}");
            assert_eq!(primary.album().as_deref(), Some("Test Album"), "{extension}");
            assert_eq!(primary.get_string(&ItemKey::Lyrics), Some(lyrics), "{extension}");
            assert_eq!(primary.pictures().len(), 1, "{extension}");
            assert_eq!(primary.pictures()[0].data(), cover.as_slice(), "{extension}");
        }

        fs::remove_dir_all(dir).unwrap();
    }
}
