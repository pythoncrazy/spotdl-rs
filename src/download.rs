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
    process::Command,
    sync::OnceLock,
};

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
        ("original-date", s.date.clone()),
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
                    encode(&song.name)
                );
                let tracks: Vec<Value> = serde_json::from_str(&page(&url, &[])?).ok()?;
                let hit = tracks
                    .iter()
                    .find(|track| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0)?;
                hit["syncedLyrics"].as_str().or(hit["plainLyrics"].as_str())?.to_string()
            }
            "genius" => {
                let url = format!(
                    "https://genius.com/api/search/song?q={}",
                    encode(&format!("{} {}", song.artist, song.name))
                );
                let auth = format!("Bearer {}", cfg["genius_token"].as_str().unwrap_or_default());
                let headers: &[(&str, &str)] = if auth.len() > 7 { &[("Authorization", &auth)] } else { &[] };
                let reply: Value = serde_json::from_str(&page(&url, headers)?).ok()?;
                let url = reply["response"]["sections"][0]["hits"][0]["result"]["url"].as_str()?;
                html(&page(url, &[])?, "data-lyrics-container=\"true\"", "LyricsFooter")?
            }
            "azlyrics" => {
                let artist = search::slug(&song.artist, "");
                let title = search::slug(&song.name, "");
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
                    search::slug(&song.name, "-")
                );
                let body = page(&url, &[])?.split_once("\"body\":")?.1.to_string();
                serde_json::Deserializer::from_str(&body).into_iter::<String>().next()?.ok()?
            }
        };
        Some(text.trim().to_string()).filter(|text| !text.is_empty())
    };
    cfg["lyrics_providers"].as_array().into_iter().flatten().filter_map(Value::as_str).find_map(provider)
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

fn fetch(url: &str, out: &Path, id: &str, cfg: &Value) -> R<()> {
    let text = |key: &str| cfg[key].as_str().unwrap_or_default().split_whitespace().collect::<Vec<_>>();
    let format = cfg["format"].as_str().unwrap_or("mp3");
    let bitrate = cfg["bitrate"].as_str().unwrap_or("auto");
    let selectors = [("m4a", "bestaudio[ext=m4a]/bestaudio/best"), ("opus", "bestaudio[ext=webm]/bestaudio/best")];
    let selector = selectors.iter().find(|entry| entry.0 == format).map_or("bestaudio/best", |entry| entry.1);
    let mut y = Command::new(cfg["yt_dlp"].as_str().unwrap_or("yt-dlp"));
    y.args(["-f", selector])
        .args("--no-playlist --no-warnings --force-overwrites -q --print after_move:filepath -o".split(' '));
    y.arg(out.with_extension(format!("{id}.%(ext)s")));
    for (key, flag) in [("cookie_file", "--cookies"), ("proxy", "--proxy")] {
        if let Some(value) = cfg[key].as_str() {
            y.args([flag, value]);
        }
    }
    if cfg["sponsor_block"] == true {
        y.args(["--sponsorblock-remove", "all"]);
    }
    y.args(text("yt_dlp_args"));
    search::js_runtime_args(&mut y);
    let res = search::yt(y.arg(url), cfg)?;
    let got = PathBuf::from(String::from_utf8_lossy(&res.stdout).trim());
    if !res.status.success() || !got.is_file() {
        let _ = fs::remove_file(&got);
        return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&res.stderr).trim()).into());
    }
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
        f.args("-nostdin -y -loglevel error -i".split(' '))
            .arg(&got)
            .args("-vn -map_metadata -1 -c:a".split(' '))
            .arg(codec);
        if !keep {
            f.args(["-b:a", bitrate]);
        }
        let res = f.args(text("ffmpeg_args")).arg(&temp).output();
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
    for path in fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()) {
        let extension = path.extension().map(|value| value.to_string_lossy().to_lowercase()).unwrap_or_default();
        if path.is_dir() {
            audio_files(&path, files);
        } else if ["mp3", "flac", "ogg", "opus", "m4a", "wav"].contains(&extension.as_str()) {
            files.push(path);
        }
    }
}

static SCAN: std::sync::OnceLock<std::collections::HashMap<String, PathBuf>> = std::sync::OnceLock::new();
static ARCHIVED: OnceLock<HashSet<String>> = OnceLock::new();

fn write_lrc(file: &Path, song: &Song, cfg: &Value) -> R<()> {
    let sidecar = file.with_extension("lrc");
    if let Some(lyrics) =
        song.lyrics.as_deref().filter(|text| cfg["generate_lrc"] == true && !sidecar.exists() && text.starts_with('['))
    {
        fs::write(sidecar, lyrics)?;
    }
    Ok(())
}

fn finish(s: &mut Song, op: &str, cfg: &Value) -> R<PathBuf> {
    let should_enrich = op != "save" && op != "url";
    let enriched_for_output = should_enrich
        && cfg["output"].as_str().is_some_and(|output| {
            ["{genre", "{isrc}", "{publisher}", "{upc}", "{bpm}", "{gain}", "{original_date}"]
                .iter()
                .any(|field| output.contains(field))
        });
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
                let item = tags.primary_tag()?.get(&ItemKey::AudioFileUrl)?;
                Some((item.value().locator()?.to_string(), file))
            })
            .collect()
    };
    let scanned = (cfg["scan_for_songs"] == true && op != "save").then(|| SCAN.get_or_init(scan).get(&s.url).cloned());
    let mut target =
        scanned.flatten().or(Some(out.clone()).filter(|out| out.is_file())).or(detected).unwrap_or(out.clone());
    if op == "save" {
        if cfg["preload"] == true {
            s.download_url = search::find(s, cfg).ok().and_then(|urls| urls.into_iter().next());
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
    let skip = cfg["respect_skip_file"] == true && Path::new(&format!("{}.skip", target.display())).exists();
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
    std::thread::scope(|scope| -> R<PathBuf> {
        let pending = (will_download && s.lyrics.is_none()).then(|| {
            let song = s.clone();
            scope.spawn(move || lyrics(&song, cfg))
        });
        let candidates = match &s.download_url {
            Some(url) => vec![url.clone()],
            None if exists && overwrite == "metadata" && op != "url" => Vec::new(),
            None => search::find(s, cfg)?,
        };
        if op == "url" {
            s.download_url = candidates.first().cloned();
            return Ok(target);
        }
        if will_download {
            fs::create_dir_all(out.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
            let id =
                if s.song_id.is_empty() { format!("{:x}", Sha1::digest(s.url.as_bytes())) } else { s.song_id.clone() };
            let mut error = String::from("no match");
            let mut fetched =
                candidates.iter().filter(|url| fetch(url, &out, &id, cfg).map_err(|e| error = e.to_string()).is_ok());
            s.download_url = Some(fetched.next().ok_or(error)?.clone());
            if target != out && target.is_file() {
                fs::remove_file(&target)?;
            }
            target = out.clone();
        }
        if let Some(task) = pending {
            s.lyrics = task.join().ok().flatten();
        } else if s.lyrics.is_none() {
            s.lyrics = lyrics(s, cfg);
        }
        tag(&target, s, cfg)?;
        write_lrc(&target, s, cfg)?;
        if cfg["create_skip_file"] == true {
            fs::write(format!("{}.skip", target.display()), "")?;
        }
        append(archive, &s.url)?;
        Ok(target)
    })
}

pub fn process(mut s: Song, op: &str, cfg: &Value) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", s.artists.join(", "), s.name);
    let result = finish(&mut s, op, cfg);
    match &result {
        Ok(_) if op == "url" => println!("{}", s.download_url.as_deref().unwrap_or_default()),
        Ok(path) if cfg["simple_tui"] == true => println!("{label}: {}", path.display()),
        Ok(path) => crate::log(cfg, "INFO", &format!("{label}: {}", path.display())),
        Err(error) => {
            crate::log(cfg, "ERROR", &format!("{label}: {error}"));
            if cfg["add_unavailable"] == true {
                let _ = append(cfg["archive"].as_str(), &s.url);
            }
        }
    }
    (s, result)
}

pub fn prune(old: &[Song], new: &[Song], cfg: &Value) {
    let lrc = cfg["sync_remove_lrc"] == true;
    let mut by_url = HashMap::new();
    for song in new {
        by_url.entry(song.url.as_str()).or_insert(song);
    }
    for song in old {
        let from = path(song, cfg);
        let to = by_url.get(song.url.as_str()).map(|next| path(next, cfg));
        let moved = to.as_ref().filter(|to| **to != from && !to.exists());
        match (&to, moved) {
            (_, Some(to)) => {
                let _ = fs::rename(&from, to);
                let _ = lrc && fs::rename(from.with_extension("lrc"), to.with_extension("lrc")).is_ok();
            }
            (Some(to), None) if *to == from => {}
            _ => {
                let _ = fs::remove_file(&from);
                let _ = lrc && fs::remove_file(from.with_extension("lrc")).is_ok();
            }
        }
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
            write_lrc(file, &song, cfg)?;
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
    write_lrc(file, &song, cfg)?;
    if redownload {
        let mut cfg = cfg.clone();
        cfg["overwrite"] = json!("force");
        let songs = if cfg["fetch_albums"] == true {
            crate::spotify::resolve(&format!("https://open.spotify.com/album/{}", song.album_id), &cfg)?
        } else {
            vec![song]
        };
        for song in songs {
            process(song, "download", &cfg).1?;
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
    for file in listed {
        let result = meta_file(&file, cfg).map_or_else(|error| error.to_string(), |_| "updated".into());
        println!("{}: {result}", file.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_limits_components_to_200_utf8_bytes_and_honors_character_limit() {
        let song = Song { name: "é".repeat(300), ..Song::default() };
        let cfg = json!({"output": "{title}", "format": "mp3"});
        let name = path(&song, &cfg).file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(name.len(), 200);
        assert!(std::str::from_utf8(name.as_bytes()).is_ok());

        let cfg = json!({"output": "{title}", "format": "mp3", "max_filename_length": 25});
        let name = path(&song, &cfg).file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(name.chars().count(), 25);
        assert_eq!(name.len(), 50);
    }

    #[test]
    fn spotify_url_round_trips_for_supported_audio_formats() {
        let Ok(version) = Command::new("ffmpeg").arg("-version").output() else {
            return;
        };
        if !version.status.success() {
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "spotdl-url-tags-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let song = Song {
            name: "Test".into(),
            artists: vec!["Test Artist".into()],
            url: "https://open.spotify.com/track/test".into(),
            ..Song::default()
        };
        let cfg = json!({"skip_album_art": true});

        for (extension, codec) in
            [("mp3", "libmp3lame"), ("m4a", "aac"), ("opus", "libopus"), ("ogg", "libvorbis"), ("flac", "flac")]
        {
            let file = dir.join(format!("test.{extension}"));
            let result = Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "anullsrc=r=44100:cl=stereo",
                    "-t",
                    "0.1",
                    "-c:a",
                    codec,
                ])
                .arg(&file)
                .output()
                .unwrap();
            assert!(result.status.success(), "{extension}: {}", String::from_utf8_lossy(&result.stderr));
            tag(&file, &song, &cfg).unwrap();
            let tags = lofty::read_from_path(&file).unwrap();
            let format = tags.primary_tag_type();
            let primary = tags.primary_tag().unwrap();
            assert_eq!(audio_url(primary, format).as_deref(), Some(song.url.as_str()), "{extension}");
        }

        fs::remove_dir_all(dir).unwrap();
    }
}
