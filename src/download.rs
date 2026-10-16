use crate::{search, spotify::HTTP, Settings, Song, R};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::Duration,
};

pub fn clean(s: &str) -> String {
    let kept: String = s.chars().filter(|c| !"/\\?%*:|\"<>".contains(*c) && !c.is_control()).collect();
    kept.trim().trim_end_matches('.').into()
}

pub fn fill(tpl: &str, s: &Song, cfg: &Settings) -> String {
    let extra = [
        ("title", s.name.clone()),
        ("artists", s.artists.join(", ")),
        ("album", s.album_name.clone()),
        ("genre", s.genres.first().cloned().unwrap_or_default()),
        ("original-date", s.date.clone()),
        ("track-id", s.song_id.clone()),
        ("track-number", format!("{:02}", s.track_number)),
        ("duration", (s.duration as u64).to_string()),
        ("output-ext", cfg.format.clone()),
    ];
    let json = serde_json::to_value(s).unwrap_or_default();
    let text = |v: &Value| if v.is_null() { String::new() } else { v.as_str().map_or(v.to_string(), String::from) };
    let fields = json.as_object().into_iter().flatten().map(|(k, v)| (k.replace('_', "-"), text(v)));
    let vars = extra.into_iter().map(|(k, v)| (k.to_string(), v)).chain(fields);
    vars.fold(tpl.to_string(), |p, (k, v)| p.replace(&format!("{{{k}}}"), &clean(&v)))
}

pub fn path(s: &Song, cfg: &Settings) -> PathBuf {
    let tpl =
        if cfg.output.contains('{') { cfg.output.clone() } else { format!("{}/{{artists}} - {{title}}", cfg.output) };
    let max = if cfg.max_filename_length > 0 { cfg.max_filename_length } else { usize::MAX };
    let absolute = tpl.starts_with('/');
    let mut out = Path::new(if absolute { "/" } else { "" }).join(
        tpl.trim_start_matches('/')
            .trim_end_matches(".{output-ext}")
            .split('/')
            .map(|part| {
                let v = fill(part, s, cfg);
                match cfg.restrict.as_deref() {
                    Some("strict") => search::slug(&v).replace('-', "_"),
                    Some("ascii") => deunicode::deunicode(&v),
                    _ => v,
                }
            })
            .map(|v| if v == "." || v == ".." { "_".to_string() } else { v.chars().take(max).collect() })
            .collect::<PathBuf>(),
    );
    out.as_mut_os_string().push(format!(".{}", cfg.format));
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

static LYRICS_HTTP: LazyLock<ureq::Agent> =
    LazyLock::new(|| ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into());

fn lyric_page(url: &str) -> Option<String> {
    let mut response = LYRICS_HTTP.get(url).call().ok()?;
    response.body_mut().read_to_string().ok()
}

fn lyric_json<T: serde::de::DeserializeOwned>(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
) -> Option<T> {
    let mut response = request.call().ok()?;
    response.body_mut().read_json().ok()
}

fn lyrics_from_provider(provider: &str, song: &Song, cfg: &Settings) -> Option<String> {
    let text = match provider {
        "synced" => {
            let request = LYRICS_HTTP
                .get("https://lrclib.net/api/search")
                .query("artist_name", &song.artist)
                .query("track_name", &song.name);
            let tracks: Vec<Value> = lyric_json(request)?;
            let hit = tracks
                .iter()
                .find(|track| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0)?;
            hit["syncedLyrics"].as_str().or(hit["plainLyrics"].as_str())?.to_string()
        }
        "genius" => {
            let request = LYRICS_HTTP
                .get("https://genius.com/api/search/song")
                .query("q", format!("{} {}", song.artist, song.name));
            let request = if cfg.genius_token.is_empty() {
                request
            } else {
                request.header("Authorization", format!("Bearer {}", cfg.genius_token))
            };
            let response: Value = lyric_json(request)?;
            let url = response["response"]["sections"][0]["hits"][0]["result"]["url"].as_str()?;
            html(&lyric_page(url)?, "data-lyrics-container=\"true\"", "LyricsFooter")?
        }
        "azlyrics" => {
            let artist = search::slug(&song.artist).replace('-', "");
            let title = search::slug(&song.name).replace('-', "");
            let url = format!("https://www.azlyrics.com/lyrics/{artist}/{title}.html");
            html(&lyric_page(&url)?, "<!-- Usage of azlyrics.com", "</div>")?
        }
        "musixmatch" => {
            let url = format!(
                "https://www.musixmatch.com/lyrics/{}/{}",
                search::slug(&song.artist),
                search::slug(&song.name)
            );
            let body = lyric_page(&url)?.split_once("\"body\":")?.1.to_string();
            serde_json::Deserializer::from_str(&body).into_iter::<String>().next()?.ok()?
        }
        _ => return None,
    };
    Some(text.trim().to_string()).filter(|text| !text.is_empty())
}

fn lyrics(song: &Song, cfg: &Settings) -> Option<String> {
    for provider in &cfg.lyrics_providers {
        if let Some(text) = lyrics_from_provider(provider, song, cfg) {
            return Some(text);
        }
    }
    None
}

fn tag(file: &Path, s: &Song, cfg: &Settings) -> R<()> {
    let mut f = lofty::read_from_path(file)?;
    let format = f.primary_tag_type();
    let mut t = Tag::new(format);
    t.set_title(s.name.clone());
    t.set_artist(s.artists.join(&cfg.id3_separator));
    t.set_album(s.album_name.clone());
    t.set_track(s.track_number);
    t.set_track_total(s.tracks_count);
    t.set_disk(s.disc_number);
    t.set_disk_total(s.disc_count);
    t.set_comment(s.download_url.clone().unwrap_or_default());
    let year = if s.year > 0 { s.year.to_string() } else { String::new() };
    let bpm = if s.bpm > 0.0 {
        if format == TagType::Id3v2 {
            s.bpm.round().to_string()
        } else {
            s.bpm.to_string()
        }
    } else {
        String::new()
    };
    let bpm_key = if format == TagType::Id3v2 { ItemKey::IntegerBpm } else { ItemKey::Bpm };
    let gain = if s.gain != 0.0 { format!("{} dB", s.gain) } else { String::new() };
    let compilation = if s.album_type == "compilation" { "1".into() } else { String::new() };
    let artists = if format == TagType::Id3v2 { vec![s.artists.join("\0")] } else { s.artists.clone() };
    let tags = vec![
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
        (ItemKey::AudioFileUrl, vec![s.url.clone()]),
        (ItemKey::AudioSourceUrl, vec![s.url.clone()]),
    ];
    tags.into_iter().for_each(|(key, values)| {
        values.into_iter().filter(|value| !value.is_empty()).for_each(|value| {
            let item_value = if key == ItemKey::AudioFileUrl || key == ItemKey::AudioSourceUrl {
                ItemValue::Locator(value)
            } else {
                ItemValue::Text(value)
            };
            let item = TagItem::new(key.clone(), item_value);
            if key == ItemKey::Unknown("RELEASETYPE".into()) {
                t.insert_unchecked(item);
            } else {
                t.push(item);
            }
        });
    });
    let art = s
        .cover_url
        .as_ref()
        .filter(|_| !cfg.skip_album_art)
        .and_then(|u| HTTP.get(u).call().ok()?.body_mut().read_to_vec().ok());
    art.into_iter()
        .for_each(|a| t.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Jpeg), None, a)));
    f.clear();
    if file.extension().is_some_and(|e| e == "wav") {
        f.insert_tag(lofty::iff::wav::RiffInfoList::from(t.clone()).into());
    }
    f.insert_tag(t);
    f.save_to_path(file, WriteOptions::default()).map_err(Into::into)
}

fn fetch(url: &str, out: &Path, id: &str, cfg: &Settings) -> R<()> {
    let fmt = [("m4a", "bestaudio[ext=m4a]/bestaudio/best"), ("opus", "bestaudio[ext=webm]/bestaudio/best")];
    let fmt = fmt.iter().find(|f| f.0 == cfg.format).map_or("bestaudio/best", |f| f.1);
    let mut y = Command::new(&cfg.yt_dlp);
    y.args(["-f", fmt, "--no-playlist", "--no-warnings"]).args([
        "--force-overwrites",
        "-q",
        "--print",
        "after_move:filepath",
        "-o",
    ]);
    y.arg(out.with_extension(format!("{id}.%(ext)s")));
    y.args(cfg.cookie_file.iter().flat_map(|c| ["--cookies", c.as_str()]));
    y.args(cfg.proxy.iter().flat_map(|p| ["--proxy", p.as_str()]));
    y.args(cfg.sponsor_block.then_some(["--sponsorblock-remove", "all"]).into_iter().flatten());
    y.args(cfg.yt_dlp_args.iter().flat_map(|a| a.split_whitespace()));
    search::deno_args(&mut y);
    y.arg(url);
    let res = search::yt_output(&mut y, cfg)?;
    let got = PathBuf::from(String::from_utf8_lossy(&res.stdout).trim());
    if !res.status.success() || !got.is_file() {
        let _ = fs::remove_file(&got);
        return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&res.stderr).trim()).into());
    }
    let ext = got.extension().unwrap_or_default().to_string_lossy();
    let keep = ["auto", "disable"].contains(&cfg.bitrate.as_str());
    if ext == cfg.format && keep {
        return fs::rename(&got, out).or_else(|e| fs::remove_file(&got).and(Err(e))).map_err(Into::into);
    }
    let codecs = [("mp3", "libmp3lame"), ("flac", "flac"), ("ogg", "libvorbis"), ("opus", "libopus"), ("m4a", "aac")];
    let codec = codecs.iter().find(|c| c.0 == cfg.format).map_or("pcm_s16le", |c| c.1);
    let codec = if keep && ext == "webm" && cfg.format == "opus" { "copy" } else { codec };
    let mut f = Command::new(&cfg.ffmpeg);
    f.args(["-nostdin", "-y", "-loglevel", "error", "-i"]).arg(&got).args(["-vn", "-map_metadata", "-1", "-c:a"]);
    f.arg(codec);
    if !keep {
        f.args(["-b:a", &cfg.bitrate]);
    }
    f.args(cfg.ffmpeg_args.iter().flat_map(|a| a.split_whitespace()));
    let temp = out.with_extension(format!("{id}.converted.{}", cfg.format));
    let res = f.arg(&temp).output();
    let _ = fs::remove_file(&got);
    let res = res?;
    if !res.status.success() {
        let _ = fs::remove_file(&temp);
        return Err(format!("ffmpeg: {}", String::from_utf8_lossy(&res.stderr).trim()).into());
    }
    fs::rename(&temp, out).or_else(|e| fs::remove_file(&temp).and(Err(e))).map_err(Into::into)
}

fn append_archive(song: &Song, cfg: &Settings) -> R<()> {
    if let Some(path) = &cfg.archive {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", song.url)?;
    }
    Ok(())
}

fn output_root(cfg: &Settings) -> PathBuf {
    let mut root = PathBuf::new();
    for component in Path::new(&cfg.output).components() {
        if component.as_os_str().to_string_lossy().contains('{') {
            break;
        }
        root.push(component);
    }
    if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root
    }
}

fn find_existing(dir: &Path, url: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        if entry.file_type().ok()?.is_dir() {
            return find_existing(&path, url);
        }
        let supported = ["mp3", "flac", "ogg", "opus", "m4a", "wav"]
            .iter()
            .any(|extension| path.extension().is_some_and(|value| value.to_string_lossy() == *extension));
        if !supported {
            return None;
        }
        let tags = lofty::read_from_path(&path).ok()?;
        tags.primary_tag()?
            .get(&ItemKey::AudioFileUrl)
            .and_then(|item| item.value().locator())
            .filter(|value| *value == url)
            .map(|_| path)
    })
}

fn finish(s: &mut Song, op: &str, cfg: &Settings) -> R<PathBuf> {
    let out = path(s, cfg);
    let detected = cfg.detect_formats.iter().map(|f| out.with_extension(f)).find(|p| p.is_file());
    let scanned = (cfg.scan_for_songs && matches!(op, "download" | "sync"))
        .then(|| find_existing(&output_root(cfg), &s.url))
        .flatten();
    let mut target =
        scanned.unwrap_or_else(|| if out.is_file() { out.clone() } else { detected.unwrap_or(out.clone()) });
    if op == "save" {
        if cfg.preload {
            s.download_url = search::find(s, cfg).ok().and_then(|urls| urls.into_iter().next());
        }
        s.lyrics = lyrics(s, cfg);
        return Ok(target);
    }
    let archived =
        cfg.archive.as_ref().and_then(|a| fs::read_to_string(a).ok()).is_some_and(|a| a.lines().any(|l| l == s.url));
    let skip = cfg.respect_skip_file && Path::new(&format!("{}.skip", target.display())).exists();
    let exists = target.is_file();
    if matches!(op, "download" | "sync")
        && (archived || skip || exists && cfg.overwrite == "skip" && !cfg.force_update_metadata)
    {
        if exists && !archived {
            append_archive(s, cfg)?;
        }
        return Ok(target);
    }
    let metadata_only = exists && cfg.overwrite == "metadata";
    let candidates = match &s.download_url {
        Some(u) => vec![u.clone()],
        None if metadata_only && op != "save" && op != "url" => Vec::new(),
        None => search::find(s, cfg)?,
    };
    if op == "save" || op == "url" {
        s.download_url = candidates.first().cloned();
        return Ok(target);
    }
    if !exists || cfg.overwrite == "force" {
        fs::create_dir_all(out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
        let id = if s.song_id.is_empty() { format!("{:x}", Sha1::digest(s.url.as_bytes())) } else { s.song_id.clone() };
        let mut error = None;
        let url = candidates.iter().find_map(|u| {
            fetch(u, &out, &id, cfg).inspect_err(|e| error = Some(e.to_string())).ok().map(|_| u.clone())
        });
        s.download_url = Some(url.ok_or_else(|| error.unwrap_or_else(|| "no match".into()))?);
        if target != out && target.is_file() {
            fs::remove_file(&target)?;
        }
        target = out.clone();
    }
    if s.lyrics.is_none() {
        s.lyrics = lyrics(s, cfg);
    }
    tag(&target, s, cfg)?;
    write_lrc(&target, s, cfg)?;
    cfg.create_skip_file.then(|| format!("{}.skip", target.display())).iter().try_for_each(|p| fs::write(p, ""))?;
    append_archive(s, cfg)?;
    Ok(target)
}

pub fn process(mut s: Song, op: &str, cfg: &Settings) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", s.artists.join(", "), s.name);
    let res = finish(&mut s, op, cfg);
    match &res {
        Ok(_) if op == "url" => println!("{}", s.download_url.as_deref().unwrap_or_default()),
        Ok(p) if cfg.simple_tui => println!("{label}: {}", p.display()),
        Ok(p) => crate::log(cfg, "INFO", &format!("{label}: {}", p.display())),
        Err(e) => {
            crate::log(cfg, "ERROR", &format!("{label}: {e}"));
            if cfg.add_unavailable {
                let _ = append_archive(&s, cfg);
            }
        }
    }
    (s, res)
}

pub fn prune(old: &[Song], new: &[Song], cfg: &Settings) {
    old.iter().for_each(|o| {
        let (from, to) = (path(o, cfg), new.iter().find(|n| n.url == o.url).map(|n| path(n, cfg)));
        match &to {
            Some(target) if target != &from && !target.exists() => {
                let _ = fs::rename(&from, target);
                if cfg.sync_remove_lrc {
                    let _ = fs::rename(from.with_extension("lrc"), target.with_extension("lrc"));
                }
            }
            Some(target) if target != &from => {
                let _ = fs::remove_file(&from);
                if cfg.sync_remove_lrc {
                    let _ = fs::remove_file(from.with_extension("lrc"));
                }
            }
            Some(_) => {}
            None => {
                let _ = fs::remove_file(&from);
                if cfg.sync_remove_lrc {
                    let _ = fs::remove_file(from.with_extension("lrc"));
                }
            }
        }
    });
}

fn tag_value(tag: &Tag, key: &ItemKey) -> Option<String> {
    let value = tag.get(key)?.value();
    value.text().or_else(|| value.locator()).map(String::from)
}

fn metadata_complete(tag: &Tag) -> bool {
    tag.title().is_some_and(|value| !value.is_empty())
        && tag.artist().is_some_and(|value| !value.is_empty())
        && tag_value(tag, &ItemKey::TrackArtists).is_some_and(|value| !value.is_empty())
        && tag_value(tag, &ItemKey::Lyrics).is_some_and(|value| !value.is_empty())
        && !tag.pictures().is_empty()
}

fn song_from_tag(tag: &Tag, cfg: &Settings) -> Song {
    let artist = tag.artist().unwrap_or_default().to_string();
    let artists = tag_value(tag, &ItemKey::TrackArtists)
        .map(|value| {
            let separator = if value.contains('\0') { "\0" } else { cfg.id3_separator.as_str() };
            value.split(separator).map(str::to_string).collect()
        })
        .unwrap_or_else(|| vec![artist.clone()]);
    Song {
        name: tag.title().unwrap_or_default().into(),
        artists,
        artist,
        url: tag_value(tag, &ItemKey::AudioFileUrl).unwrap_or_default(),
        download_url: tag.comment().map(String::from),
        ..Song::default()
    }
}

fn write_lrc(file: &Path, song: &Song, cfg: &Settings) -> R<()> {
    if let Some(lyrics) = song.lyrics.as_deref().filter(|text| cfg.generate_lrc && text.starts_with('[')) {
        fs::write(file.with_extension("lrc"), lyrics)?;
    }
    Ok(())
}

fn redownload_song(song: Song, cfg: &Settings) -> R<()> {
    let songs = if cfg.fetch_albums && !song.album_id.is_empty() {
        crate::spotify::resolve(&format!("spotify:album:{}", song.album_id), cfg, true)?
    } else {
        vec![song]
    };
    for (_, result) in crate::download_songs(songs, "download", cfg)? {
        result?;
    }
    Ok(())
}

fn meta_file(file: &Path, cfg: &Settings) -> R<()> {
    let tags = lofty::read_from_path(file)?;
    let primary = tags.primary_tag().ok_or("no tags")?;
    let query =
        tag_value(primary, &ItemKey::AudioFileUrl).filter(|value| value.contains("spotify")).unwrap_or_else(|| {
            format!("{} - {}", primary.artist().unwrap_or_default(), primary.title().unwrap_or_default())
        });
    let complete = metadata_complete(primary);
    let needs_lrc = cfg.generate_lrc && !file.with_extension("lrc").exists();
    if complete && !cfg.force_update_metadata && !cfg.redownload && !needs_lrc {
        return Ok(());
    }
    let stored_lyrics = tag_value(primary, &ItemKey::Lyrics);
    let mut song = if complete && !cfg.force_update_metadata && !cfg.redownload {
        song_from_tag(primary, cfg)
    } else {
        crate::spotify::resolve(&query, cfg, true)?.into_iter().next().ok_or("no match")?
    };
    song.lyrics = stored_lyrics.or_else(|| lyrics(&song, cfg));
    if !complete || cfg.force_update_metadata {
        tag(file, &song, cfg)?;
    }
    if needs_lrc {
        write_lrc(file, &song, cfg)?;
    }
    if cfg.redownload {
        redownload_song(song, cfg)?;
    }
    Ok(())
}

pub fn meta(files: &[String], cfg: &Settings) -> R<()> {
    let exts = ["mp3", "flac", "ogg", "opus", "m4a", "wav"];
    let mut listed = Vec::new();
    for file in files {
        collect_files(Path::new(file), &mut listed);
    }
    listed
        .into_iter()
        .filter(|p: &PathBuf| {
            p.extension()
                .is_some_and(|extension| exts.iter().any(|e| extension.to_string_lossy().eq_ignore_ascii_case(e)))
        })
        .for_each(|f| {
            let result = meta_file(&f, cfg);
            println!("{}: {}", f.display(), result.map_or_else(|e| e.to_string(), |_| "updated".into()));
        });
    Ok(())
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_file() {
        files.push(path.to_path_buf());
    } else if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            collect_files(&entry.path(), files);
        }
    }
}
