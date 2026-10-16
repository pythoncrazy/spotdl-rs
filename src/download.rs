use crate::{search, spotify::HTTP, Settings, Song, R};
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::*,
    tag::{ItemKey, ItemValue, Tag, TagItem},
};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !"/\\?%*:|\"<>".contains(*c) && !c.is_control())
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .into()
}

pub fn path(s: &Song, cfg: &Settings) -> PathBuf {
    let vars = [
        ("{title}", s.name.clone()),
        ("{artists}", s.artists.join(", ")),
        ("{artist}", s.artist.clone()),
        ("{album}", s.album_name.clone()),
        ("{album-artist}", s.album_artist.clone()),
        ("{genre}", s.genres.first().cloned().unwrap_or_default()),
        ("{disc-number}", s.disc_number.to_string()),
        ("{disc-count}", s.disc_count.to_string()),
        ("{duration}", (s.duration as u64).to_string()),
        ("{year}", s.year.to_string()),
        ("{original-date}", s.date.clone()),
        ("{track-number}", format!("{:02}", s.track_number)),
        ("{tracks-count}", s.tracks_count.to_string()),
        ("{isrc}", s.isrc.clone().unwrap_or_default()),
        ("{track-id}", s.song_id.clone()),
        ("{publisher}", s.publisher.clone()),
        ("{list-length}", s.list_length.unwrap_or_default().to_string()),
        ("{list-position}", s.list_position.unwrap_or_default().to_string()),
        ("{list-name}", s.list_name.clone().unwrap_or_default()),
        ("{output-ext}", cfg.format.clone()),
    ];
    let tpl = if cfg.output.contains('{') {
        cfg.output.clone()
    } else {
        format!("{}/{{artists}} - {{title}}.{{output-ext}}", cfg.output)
    };
    let tpl = if tpl.ends_with("{output-ext}") { tpl } else { format!("{tpl}.{{output-ext}}") };
    tpl.split('/')
        .map(|part| {
            let v = vars.iter().fold(part.to_string(), |p, (k, v)| p.replace(k, &clean(v)));
            let v = match cfg.restrict.as_deref() {
                Some("strict") => search::slug(&v).replace('-', "_"),
                Some("ascii") => deunicode::deunicode(&v),
                _ => v,
            };
            if cfg.max_filename_length > 0 {
                v.chars().take(cfg.max_filename_length).collect()
            } else {
                v
            }
        })
        .collect::<PathBuf>()
        .with_extension(&cfg.format)
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
    let mut t = Tag::new(f.primary_tag_type());
    t.set_title(s.name.clone());
    t.set_artist(s.artists.join(&cfg.id3_separator));
    t.set_album(s.album_name.clone());
    t.set_track(s.track_number);
    t.set_track_total(s.tracks_count);
    t.set_disk(s.disc_number);
    t.set_disk_total(s.disc_count);
    t.set_comment(s.download_url.clone().unwrap_or_default());
    [
        (ItemKey::AlbumArtist, s.album_artist.clone()),
        (ItemKey::RecordingDate, s.date.clone()),
        (ItemKey::Year, s.year.to_string()),
        (ItemKey::Isrc, s.isrc.clone().unwrap_or_default()),
        (ItemKey::Label, s.publisher.clone()),
        (ItemKey::CopyrightMessage, s.copyright_text.clone().unwrap_or_default()),
        (ItemKey::Genre, s.genres.join(&cfg.id3_separator)),
        (ItemKey::Lyrics, s.lyrics.clone().unwrap_or_default()),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty() && v != "0")
    .for_each(|(k, v)| {
        t.insert_text(k, v);
    });
    t.insert(TagItem::new(ItemKey::AudioFileUrl, ItemValue::Locator(s.url.clone())));
    let art = s
        .cover_url
        .as_ref()
        .filter(|_| !cfg.skip_album_art)
        .and_then(|u| HTTP.get(u).call().ok()?.body_mut().read_to_vec().ok());
    art.into_iter()
        .for_each(|a| t.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Jpeg), None, a)));
    f.clear();
    f.insert_tag(t);
    f.save_to_path(file, WriteOptions::default())?;
    Ok(())
}

fn fetch(url: &str, out: &Path, cfg: &Settings) -> R<()> {
    let fmt = match cfg.format.as_str() {
        "m4a" => "bestaudio[ext=m4a]/bestaudio/best",
        "opus" => "bestaudio[ext=webm]/bestaudio/best",
        _ => "bestaudio/best",
    };
    let tmp = out.with_extension("spotdl-tmp");
    let mut y = Command::new(&cfg.yt_dlp);
    y.args(["-f", fmt, "--no-playlist", "--no-warnings", "-q", "--print", "after_move:filepath", "-o"])
        .arg(tmp.with_extension("%(ext)s"));
    y.args(cfg.cookie_file.iter().flat_map(|c| ["--cookies", c.as_str()]));
    y.args(cfg.proxy.iter().flat_map(|p| ["--proxy", p.as_str()]));
    y.args(cfg.sponsor_block.then_some(["--sponsorblock-remove", "all"]).into_iter().flatten());
    y.args(cfg.yt_dlp_args.iter().flat_map(|a| a.split_whitespace()));
    let res = y.arg(url).output()?;
    let got = PathBuf::from(String::from_utf8_lossy(&res.stdout).trim());
    if !res.status.success() || !got.is_file() {
        return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&res.stderr).trim()).into());
    }
    let ext = got.extension().unwrap_or_default().to_string_lossy();
    let same = ext == cfg.format || (ext == "webm" && cfg.format == "opus");
    if same && (cfg.bitrate == "disable" || cfg.bitrate == "auto") {
        return Ok(fs::rename(&got, out)?);
    }
    let codec = match cfg.format.as_str() {
        "mp3" => "libmp3lame",
        "flac" => "flac",
        "ogg" => "libvorbis",
        "opus" => "libopus",
        "m4a" => "aac",
        _ => "pcm_s16le",
    };
    let mut f = Command::new(&cfg.ffmpeg);
    f.args(["-nostdin", "-y", "-loglevel", "error", "-i"]).arg(&got).args(["-vn", "-map_metadata", "-1"]);
    f.args(if same { vec!["-c:a", "copy"] } else { vec!["-c:a", codec] });
    f.args(
        (!same && !["auto", "disable"].contains(&cfg.bitrate.as_str()))
            .then_some(["-b:a", cfg.bitrate.as_str()])
            .into_iter()
            .flatten(),
    );
    f.args(cfg.ffmpeg_args.iter().flat_map(|a| a.split_whitespace()));
    let res = f.arg(out).output()?;
    let _ = fs::remove_file(&got);
    if res.status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg: {}", String::from_utf8_lossy(&res.stderr).trim()).into())
    }
}

fn finish(mut s: Song, op: &str, cfg: &Settings) -> R<(Song, PathBuf)> {
    let out = path(&s, cfg);
    let archived =
        cfg.archive.as_ref().and_then(|a| fs::read_to_string(a).ok()).is_some_and(|a| a.lines().any(|l| l == s.url));
    let skip = cfg.respect_skip_file && Path::new(&format!("{}.skip", out.display())).exists();
    let exists = out.exists() || cfg.detect_formats.iter().any(|f| out.with_extension(f).exists());
    if (op == "download" || op == "sync")
        && (archived || skip || (exists && cfg.overwrite == "skip" && !cfg.force_update_metadata))
    {
        return Ok((s, out));
    }
    if s.download_url.is_none() && (op != "save" || cfg.preload) && !(exists && cfg.overwrite == "metadata") {
        s.download_url = Some(search::find(&s, cfg)?);
    }
    if op == "save" || op == "url" {
        return Ok((s, out));
    }
    if !exists || cfg.overwrite == "force" || cfg.redownload {
        fs::create_dir_all(out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
        let url = s.download_url.as_deref().unwrap_or_default();
        fetch(url, &out, cfg).or_else(|_| fetch(url, &out, cfg))?;
    }
    s.lyrics = s.lyrics.clone().or_else(|| lyrics(&s, cfg));
    tag(&out, &s, cfg)?;
    let lrc = s.lyrics.as_ref().filter(|l| cfg.generate_lrc && l.starts_with('['));
    lrc.iter().try_for_each(|l| fs::write(out.with_extension("lrc"), l))?;
    let append = |p: String, line: &str| {
        OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| writeln!(f, "{line}"))
    };
    cfg.archive.iter().try_for_each(|a| append(a.clone(), &s.url))?;
    cfg.create_skip_file.then(|| format!("{}.skip", out.display())).iter().try_for_each(|p| append(p.clone(), ""))?;
    Ok((s, out))
}

pub fn process(s: Song, op: &str, cfg: &Settings) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", s.artists.join(", "), s.name);
    match finish(s.clone(), op, cfg) {
        Ok((s, p)) => {
            println!(
                "{}",
                if op == "url" {
                    s.download_url.clone().unwrap_or_default()
                } else {
                    format!("{label}: {}", p.display())
                }
            );
            (s, Ok(p))
        }
        Err(e) => {
            eprintln!("{label}: {e}");
            (s, Err(e))
        }
    }
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
    let files: Vec<PathBuf> = files
        .iter()
        .flat_map(|f| {
            fs::read_dir(f).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_else(|_| vec![PathBuf::from(f)])
        })
        .filter(|p| {
            p.extension().is_some_and(|e| ["mp3", "flac", "ogg", "opus", "m4a", "wav"].contains(&&*e.to_string_lossy()))
        })
        .collect();
    files.iter().for_each(|f| {
        let res = lofty::read_from_path(f).map_err(Into::into).and_then(|t| -> R<()> {
            let tg = t.primary_tag().ok_or("no tags")?;
            let url =
                tg.get(&ItemKey::AudioFileUrl).and_then(|i| i.value().locator()).filter(|u| u.contains("spotify"));
            let q = url.map(String::from).unwrap_or(format!(
                "{} - {}",
                tg.artist().unwrap_or_default(),
                tg.title().unwrap_or_default()
            ));
            let song = crate::spotify::resolve(&q, cfg)?.into_iter().next().ok_or("no match")?;
            let song = Song { lyrics: lyrics(&song, cfg), download_url: tg.comment().map(String::from), ..song };
            tag(f, &song, cfg)
        });
        println!("{}: {}", f.display(), res.map_or_else(|e| e.to_string(), |_| "updated".into()));
    });
    Ok(())
}
