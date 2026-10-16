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

fn lyrics(s: &Song, cfg: &Settings) -> Option<String> {
    let page = |r: ureq::RequestBuilder<ureq::typestate::WithoutBody>| r.call().ok()?.body_mut().read_to_string().ok();
    cfg.lyrics_providers.iter().find_map(|p| {
        let text = match p.as_str() {
            "synced" => {
                let r = HTTP
                    .get("https://lrclib.net/api/search")
                    .query("artist_name", &s.artist)
                    .query("track_name", &s.name);
                let v: Vec<Value> = r.call().ok()?.body_mut().read_json().ok()?;
                let hit = v.iter().find(|x| (x["duration"].as_f64().unwrap_or_default() - s.duration).abs() < 4.0)?;
                hit["syncedLyrics"].as_str().or(hit["plainLyrics"].as_str())?.to_string()
            }
            "genius" => {
                let r = HTTP.get("https://genius.com/api/search/song").query("q", format!("{} {}", s.artist, s.name));
                let v: Value = r.call().ok()?.body_mut().read_json().ok()?;
                let url = v["response"]["sections"][0]["hits"][0]["result"]["url"].as_str()?;
                html(&page(HTTP.get(url))?, "data-lyrics-container=\"true\"", "LyricsFooter")?
            }
            "azlyrics" => {
                let key = |x: &str| search::slug(x).replace('-', "");
                let url = format!("https://www.azlyrics.com/lyrics/{}/{}.html", key(&s.artist), key(&s.name));
                html(&page(HTTP.get(&url))?, "<!-- Usage of azlyrics.com", "</div>")?
            }
            _ => {
                let url =
                    format!("https://www.musixmatch.com/lyrics/{}/{}", search::slug(&s.artist), search::slug(&s.name));
                let body = page(HTTP.get(&url))?.split_once("\"body\":")?.1.to_string();
                serde_json::Deserializer::from_str(&body).into_iter::<String>().next()?.ok()?
            }
        };
        Some(text.trim().to_string()).filter(|t| !t.is_empty())
    })
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
    [
        (ItemKey::AlbumArtist, s.album_artist.clone()),
        (ItemKey::RecordingDate, s.date.clone()),
        (ItemKey::ReleaseDate, s.date.clone()),
        (ItemKey::OriginalReleaseDate, s.original_date.clone()),
        (ItemKey::Year, year),
        (ItemKey::Isrc, s.isrc.clone().unwrap_or_default()),
        (ItemKey::Label, s.publisher.clone()),
        (ItemKey::CopyrightMessage, s.copyright_text.clone().unwrap_or_default()),
        (ItemKey::Lyrics, s.lyrics.clone().unwrap_or_default()),
        (bpm_key, bpm),
        (ItemKey::Barcode, s.upc.clone()),
        (ItemKey::ReplayGainTrackGain, gain),
        (ItemKey::FlagCompilation, compilation),
        (ItemKey::ParentalAdvisory, if s.explicit { "1" } else { "0" }.into()),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .for_each(|(k, v)| {
        t.insert_text(k, v);
    });
    s.artists.iter().filter(|a| !a.is_empty()).for_each(|a| {
        t.push(TagItem::new(ItemKey::TrackArtists, ItemValue::Text(a.clone())));
    });
    s.genres.iter().filter(|g| !g.is_empty()).for_each(|g| {
        t.push(TagItem::new(ItemKey::Genre, ItemValue::Text(g.clone())));
    });
    if !s.album_type.is_empty() {
        t.insert_unchecked(TagItem::new(ItemKey::Unknown("RELEASETYPE".into()), ItemValue::Text(s.album_type.clone())));
    }
    [ItemKey::AudioFileUrl, ItemKey::AudioSourceUrl].into_iter().for_each(|k| {
        t.insert(TagItem::new(k, ItemValue::Locator(s.url.clone())));
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
    y.arg(url);
    let res = search::yt_output(&mut y)?;
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

fn finish(mut s: Song, op: &str, cfg: &Settings) -> R<(Song, PathBuf)> {
    let out = path(&s, cfg);
    let detected = cfg.detect_formats.iter().map(|f| out.with_extension(f)).find(|p| p.is_file());
    let mut target = if out.is_file() { out.clone() } else { detected.unwrap_or(out.clone()) };
    let archived =
        cfg.archive.as_ref().and_then(|a| fs::read_to_string(a).ok()).is_some_and(|a| a.lines().any(|l| l == s.url));
    let skip = cfg.respect_skip_file && Path::new(&format!("{}.skip", target.display())).exists();
    let exists = target.is_file();
    if matches!(op, "download" | "sync")
        && (archived || skip || exists && cfg.overwrite == "skip" && !cfg.force_update_metadata)
    {
        if exists && !archived {
            append_archive(&s, cfg)?;
        }
        return Ok((s, target));
    }
    let metadata_only = exists && cfg.overwrite == "metadata" && !cfg.redownload;
    crate::spotify::enrich(&mut s).ok();
    let candidates = match &s.download_url {
        Some(u) => vec![u.clone()],
        None if metadata_only && op != "save" && op != "url" => Vec::new(),
        None => search::find(&s, cfg)?,
    };
    if op == "save" || op == "url" {
        s.download_url = candidates.first().cloned();
        return Ok((s, target));
    }
    if !exists || cfg.overwrite == "force" || cfg.redownload {
        fs::create_dir_all(out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
        let id = if s.song_id.is_empty() { format!("{:x}", Sha1::digest(s.url.as_bytes())) } else { s.song_id.clone() };
        let mut error = None;
        let url = candidates.iter().find_map(|u| {
            fetch(u, &out, &id, cfg).inspect_err(|e| error = Some(e.to_string())).ok().map(|_| u.clone())
        });
        s.download_url = Some(url.ok_or_else(|| error.unwrap_or_else(|| "no match".into()))?);
        target = out.clone();
    }
    s.lyrics = s.lyrics.clone().or_else(|| lyrics(&s, cfg));
    tag(&target, &s, cfg)?;
    let lrc = s.lyrics.as_ref().filter(|l| cfg.generate_lrc && l.starts_with('['));
    lrc.iter().try_for_each(|l| fs::write(target.with_extension("lrc"), l))?;
    cfg.create_skip_file.then(|| format!("{}.skip", target.display())).iter().try_for_each(|p| fs::write(p, ""))?;
    append_archive(&s, cfg)?;
    Ok((s, target))
}

pub fn process(s: Song, op: &str, cfg: &Settings) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", s.artists.join(", "), s.name);
    let res = finish(s.clone(), op, cfg);
    match &res {
        Ok((s, _)) if op == "url" => println!("{}", s.download_url.clone().unwrap_or_default()),
        Ok((_, p)) => println!("{label}: {}", p.display()),
        Err(e) => {
            eprintln!("{label}: {e}");
            if cfg.add_unavailable {
                let _ = append_archive(&s, cfg);
            }
        }
    }
    res.map_or_else(|e| (s, Err(e)), |(s, p)| (s, Ok(p)))
}

pub fn prune(old: &[Song], new: &[Song], cfg: &Settings) {
    old.iter().for_each(|o| {
        let (from, to) = (path(o, cfg), new.iter().find(|n| n.url == o.url).map(|n| path(n, cfg)));
        let _ = match &to {
            Some(t) if t != &from && !t.exists() => fs::rename(&from, t),
            Some(_) => Ok(()),
            None => fs::remove_file(&from),
        };
        let lrc = cfg.sync_remove_lrc && to.is_none();
        let _ = lrc.then(|| fs::remove_file(from.with_extension("lrc")));
    });
}

pub fn meta(files: &[String], cfg: &Settings) -> R<()> {
    let exts = ["mp3", "flac", "ogg", "opus", "m4a", "wav"];
    let listed = files
        .iter()
        .flat_map(|f| fs::read_dir(f).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or(vec![f.into()]));
    listed.filter(|p: &PathBuf| p.extension().is_some_and(|e| exts.contains(&&*e.to_string_lossy()))).for_each(|f| {
        let res = lofty::read_from_path(&f).map_err(Into::into).and_then(|t| -> R<()> {
            let tg = t.primary_tag().ok_or("no tags")?;
            let url =
                tg.get(&ItemKey::AudioFileUrl).and_then(|i| i.value().locator()).filter(|u| u.contains("spotify"));
            let q = url.map(String::from).unwrap_or(format!(
                "{} - {}",
                tg.artist().unwrap_or_default(),
                tg.title().unwrap_or_default()
            ));
            let mut song = crate::spotify::resolve(&q, cfg, true)?.into_iter().next().ok_or("no match")?;
            crate::spotify::enrich(&mut song).ok();
            tag(&f, &Song { lyrics: lyrics(&song, cfg), download_url: tg.comment().map(String::from), ..song }, cfg)
        });
        println!("{}: {}", f.display(), res.map_or_else(|e| e.to_string(), |_| "updated".into()));
    });
    Ok(())
}
