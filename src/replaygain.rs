//! ReplayGain: loudness measured by FFmpeg or taken from Deezer, as track and album gains.

use crate::files::{temp_file, AUDIO};
use crate::settings::settings;
use crate::tags::{audio, save};
use crate::{log, search, Song, R};
use lofty::{prelude::*, tag::ItemKey};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// The ReplayGain 2.0 reference loudness, in LUFS.
const RG_REFERENCE: f64 = -18.0;

/// Deezer's gain reads about 0.4 dB below the EBU R128 loudness FFmpeg measures for the same audio (deemix writes
/// -(gain + 18.4) too), so gains from it at -18 played 0.4 dB louder than measured ones.
const DEEZER_REFERENCE: f64 = RG_REFERENCE - 0.4;

/// The EBU R128 integrated loudness (LUFS) and true peak (linear) of a file, measured by FFmpeg.
fn loudness(file: &Path) -> Option<(f64, f64)> {
    let mut ffmpeg = Command::new(settings().ffmpeg.as_str());
    ffmpeg.args(["-nostats", "-hide_banner", "-i"]).arg(file);
    ffmpeg.args(["-map", "0:a", "-filter:a", "ebur128=peak=true", "-f", "null", "-"]);
    // Without FFmpeg the gain silently fell back to Deezer's, or none.
    let output = search::run(&mut ffmpeg).map_err(|error| log("WARNING", &format!("ReplayGain: {error}"))).ok()?;
    let log = String::from_utf8_lossy(&output.stderr);
    let summary = &log[log.rfind("Summary:")?..];
    let value = |label: &str, unit: &str| summary.split_once(label)?.1.split_once(unit)?.0.trim().parse::<f64>().ok();
    // A silent track's peak is "-inf".
    let peak = value("Peak:", "dBFS").unwrap_or(-70.0);
    Some((value("I:", "LUFS")?, 10f64.powf(peak / 20.0)))
}

/// The track gain and peak tags: measured with `--replaygain`, else from Deezer's gain (no peak).
pub(crate) fn track_gain(file: &Path, s: &Song) -> (String, String) {
    match settings().replaygain.then(|| loudness(file)).flatten() {
        Some((lufs, peak)) => (format!("{:.2} dB", RG_REFERENCE - lufs), format!("{peak:.6}")),
        None if s.extra.gain != 0.0 => (format!("{:.2} dB", DEEZER_REFERENCE - s.extra.gain), String::new()),
        None => (String::new(), String::new()),
    }
}

/// With `--replaygain`, the album gain and peak of each album in the folders of `files`, from the track gains and peaks
/// of all of the folder's songs of that album: the album's loudness is the songs' loudness averaged as energy over
/// their lengths, and its peak the highest song peak. An album with a song that has no measured gain is left as it is.
pub(crate) fn album_gains(files: &[PathBuf]) {
    if !settings().replaygain {
        return;
    }
    album_dirs(files).into_par_iter().for_each(|dir| {
        if let Err(error) = album_gain(dir) {
            log("ERROR", &format!("{}: album gain: {error}", dir.display()));
        }
    });
}

/// The folders of `files`. A bare file name's parent is "", which `read_dir` cannot open, so it counts as ".".
fn album_dirs(files: &[PathBuf]) -> HashSet<&Path> {
    files
        .iter()
        .filter_map(|file| file.parent())
        .map(|dir| if dir.as_os_str().is_empty() { Path::new(".") } else { dir })
        .collect()
}

fn album_gain(dir: &Path) -> R<()> {
    type Track = (PathBuf, Option<(f64, f64)>, f64);
    let mut albums: HashMap<String, Vec<Track>> = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let file = entry?.path();
        let extension = file.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !AUDIO.contains(&extension.as_str()) || temp_file(&file) {
            continue;
        }
        let audio = audio(&file)?;
        let Some(tag) = audio.primary_tag() else {
            continue;
        };
        let number = |key: ItemKey| tag.get_string(&key)?.split_whitespace().next()?.parse::<f64>().ok();
        let loud = number(ItemKey::ReplayGainTrackGain).zip(number(ItemKey::ReplayGainTrackPeak));
        let length = audio.properties().duration().as_secs_f64();
        albums.entry(tag.album().unwrap_or_default().into()).or_default().push((file, loud, length));
    }
    for tracks in albums.values() {
        let Some(measured) =
            tracks.iter().map(|(_, loud, length)| loud.map(|loud| (loud, *length))).collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let total: f64 = measured.iter().map(|(_, length)| length).sum();
        if total <= 0.0 {
            continue;
        }
        let energy: f64 =
            measured.iter().map(|((gain, _), length)| length * 10f64.powf((RG_REFERENCE - gain) / 10.0)).sum();
        let gain = format!("{:.2} dB", RG_REFERENCE - 10.0 * (energy / total).log10());
        let peak = format!("{:.6}", measured.iter().map(|((_, peak), _)| *peak).fold(0.0, f64::max));
        for (file, _, _) in tracks {
            let mut audio = audio(file)?;
            let tag = audio.primary_tag_mut().ok_or("no tags")?;
            if tag.get_string(&ItemKey::ReplayGainAlbumGain) == Some(gain.as_str())
                && tag.get_string(&ItemKey::ReplayGainAlbumPeak) == Some(peak.as_str())
            {
                continue;
            }
            tag.insert_text(ItemKey::ReplayGainAlbumGain, gain.clone());
            tag.insert_text(ItemKey::ReplayGainAlbumPeak, peak.clone());
            save(&audio, file)?;
        }
    }
    Ok(())
}

/// The track gain and peak written into a file that has its other tags: `--replaygain` on a song from before it.
pub(crate) fn write_track_gain(file: &Path) -> R<()> {
    let (gain, peak) = track_gain(file, &Song::default());
    if gain.is_empty() {
        return Err("could not measure the loudness".into());
    }
    let mut audio = audio(file)?;
    let tag = audio.primary_tag_mut().ok_or("no tags")?;
    tag.insert_text(ItemKey::ReplayGainTrackGain, gain);
    tag.insert_text(ItemKey::ReplayGainTrackPeak, peak);
    save(&audio, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deezer's gain was taken against -18 LUFS, but it reads 0.4 dB below the loudness FFmpeg measures: Get Lucky
    /// (Deezer's -11.3) measures -7.10 dB, not -6.70, so unmeasured songs played 0.4 dB louder than measured ones.
    #[test]
    fn deezer_gain_is_converted_like_a_measured_one() {
        crate::settings::use_defaults();
        let mut song = Song::default();
        song.extra.gain = -11.3;
        assert_eq!(track_gain(&PathBuf::from("no-such-file.opus"), &song), ("-7.10 dB".into(), String::new()));
    }

    /// With the default template a song is a bare file name, whose parent "" `read_dir` cannot open, so `--replaygain`
    /// logged ": album gain: No such file or directory" and wrote no album gain.
    #[test]
    fn album_gain_reads_the_current_folder_for_bare_file_names() {
        let files = [PathBuf::from("a.opus"), PathBuf::from("./b.opus"), PathBuf::from("x/c.opus")];
        let dirs = album_dirs(&files);
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(std::path::Path::new(".")) && dirs.contains(std::path::Path::new("x")));
    }
}
