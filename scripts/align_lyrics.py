#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["stable-ts>=2.19", "faster-whisper>=1.1", "torch>=2.4", "torchaudio>=2.4"]
#
# # The CPU builds of torch are a small download; remove these lines to use the CUDA builds.
# [[tool.uv.index]]
# name = "pytorch-cpu"
# url = "https://download.pytorch.org/whl/cpu"
# explicit = true
#
# [tool.uv.sources]
# torch = { index = "pytorch-cpu" }
# torchaudio = { index = "pytorch-cpu" }
# ///
"""Time plain lyrics to a recording and write an .lrc file.

Usage: align_lyrics.py [--model NAME] AUDIO LYRICS OUT.lrc
       align_lyrics.py [--model NAME] --cache AUDIO...    (transcribe many songs with one model load)

spotdl-rs runs this as its `--lyrics-aligner`. LYRICS may be plain or synced. Synced lyrics whose times agree with
the recording are written as they are; other lyrics often come from another version of the song, so the script
first finds out which lines the recording has:

1. Whisper transcribes the recording, with a time for each word.
2. The lyric words are matched to the transcript words, in order.
3. A run of lyric lines that the recording does not have is dropped (an intro of another version): no line of the
   run is heard, and the recording has no time for it between the lines around it.
4. A long part of the transcript that no lyric line matches is added as lines (a verse that the text does not
   have).
5. Whisper aligns the resulting text to the recording. A line whose aligned time is far from the time of its
   matched words takes the time of its matched words; a line that neither places gets a time between its
   neighbours.

Transcripts are cached by the audio's content in $ALIGN_CACHE (default ~/.cache/align-lyrics), so trying several
lyrics for one song transcribes it once. For a GPU, run the script with a Python that has a CUDA build of torch.

It exits with status 3 and writes nothing when the lyrics are for another song: fewer than 40% of the lyric lines
are heard, and no 8 lines in a row are heard.
"""

import argparse
import difflib
import hashlib
import itertools
import json
import os
import re
import statistics
import sys

NO_FIT = 3  # Exit status when the lyrics are not the words of the recording.
MIN_HEARD = 0.4  # Part of the lyric lines that must be heard, or the lyrics are for another song,
MIN_RUN = 8  # unless this many lines in a row are heard, with no two lines in a row missed.
LINE_HEARD = 0.25  # Part of a line's words that must be heard for the line to count as heard.
WORD_TIME = 0.25  # Seconds that a word takes at least.
MAX_DRIFT = 2.0  # Seconds that an aligned line can be away from its matched words.
NEW_WORDS = 12  # Unmatched transcript words in one place that make new lines.
OTHER_WORDS = 0.15  # Lines not heard are dropped when less than this part of the words heard in their time match.
OTHER_LINES = 4  # for runs of at least this many lines; Whisper mishears a line or two of rap.
LINE_SPAN = 8.0  # Seconds that the matched words of one line can be apart, or 1 s a word for long lines.
LOOP_TIME = 0.15  # Seconds that four transcript words take at least; faster words are a Whisper loop.
REPEAT = 0.6  # How closely the new words must match lyric lines to repeat those lines instead.
SURE = 0.5  # Mean word probability of a new line from the transcript.
SYNC_DRIFT = 1.5  # Seconds that a synced line can be away from its matched words,
SYNC_CLOSE = 0.7  # for this part of the heard lines, or the synced lyrics are timed again.

CREDIT = re.compile(r"作曲|作词|编曲|^(composer|lyricist|producer|written by)\b", re.IGNORECASE)


def words(text):
    return [w for w in re.sub(r"[^a-z0-9' ]", " ", text.lower().replace("’", "'")).replace("'", "").split() if w]


def lyric_lines(path):
    """The lyric lines, each with its time when the lyrics are synced (else None). Credits and [Section] lines are
    left out."""
    out = []
    with open(path, encoding="utf-8", errors="ignore") as f:
        for line in f:
            stamp = re.match(r"\[(\d+):(\d+(?:\.\d+)?)\]", line.strip())
            text = re.sub(r"^(\[[0-9:.]+\])+", "", line.strip()).strip()
            if words(text) and not CREDIT.search(text) and not re.fullmatch(r"\[.*\]", text):
                out.append((text, int(stamp.group(1)) * 60 + float(stamp.group(2)) if stamp else None))
    return out


def transcribe(audio, model_name, model):
    """Transcript words (word, start, text, probability), cached by the audio's content: a song is transcribed
    once, also when several lyrics are tried for it. A word like "5-0" is two words, with its text on the first."""
    with open(audio, "rb") as f:
        key = hashlib.sha1(f.read()).hexdigest()
    cache = os.path.join(os.environ.get("ALIGN_CACHE", os.path.expanduser("~/.cache/align-lyrics")), model_name)
    path = os.path.join(cache, key + ".json")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            return paced([tuple(w) for w in json.load(f)])
    # Greedy decoding: three times faster than faster-whisper's 5 beams, and it hears as many sung words.
    heard = model().transcribe(samples(audio), language="en", word_timestamps=True, verbose=None, beam_size=1)
    said = [
        (w, word.start, word.word if k == 0 else "", word.probability)
        for seg in heard.segments
        for word in seg.words
        for k, w in enumerate(words(word.word))
    ]
    os.makedirs(cache, exist_ok=True)
    with open(path + ".tmp", "w", encoding="utf-8") as f:
        json.dump(said, f)
    os.replace(path + ".tmp", path)
    return paced(said)


def paced(said):
    """The transcript without the words that Whisper repeats in a loop: those come many to a moment, and a singer
    takes longer than LOOP_TIME for four words."""
    kept = []
    for word in said:
        if len(kept) < 3 or word[1] - kept[-3][1] >= LOOP_TIME:
            kept.append(word)
    return kept


def samples(audio):
    """The audio as 16 kHz samples, decoded by ffmpeg (faster-whisper's own decoder fails with new PyAV)."""
    import whisper.audio

    return whisper.audio.load_audio(audio)


def spread(times, weights):
    """Fills the missing times (None) between known ones, in proportion to the words of the lines."""
    n = len(times)
    i = 0
    while i < n:
        if times[i] is not None:
            i += 1
            continue
        j = i
        while j < n and times[j] is None:
            j += 1
        start = times[i - 1] if i else None
        stop = times[j] if j < n else None
        span = weights[i - 1 : j] if i else weights[i:j]
        if start is None:
            start = max(0.0, (stop or 0.0) - WORD_TIME * 2 * sum(span))
        if stop is None:
            stop = start + WORD_TIME * 2 * sum(span)
        done = weights[i - 1] if i else 0
        for k in range(i, j):
            times[k] = start + (stop - start) * done / max(sum(span), 1)
            done += weights[k]
        i = j
    return times


def repeated_lines(lines, gap):
    """The lyric lines that the gap's transcript words repeat (a chorus that the text has once, or a verse that the
    text has in another place), each with the time of its first matched word; or None."""
    heard = [s[0] for s in gap]
    best, window = 0.0, None
    for a in range(len(lines)):
        for b in range(a, len(lines)):
            text = [w for line in lines[a : b + 1] for w in words(line)]
            if len(text) > 1.5 * len(heard):
                break
            ratio = difflib.SequenceMatcher(None, text, heard, autojunk=False).ratio()
            if ratio > best:
                best, window = ratio, (a, b)
    if best < REPEAT:
        return None
    a, b = window
    text = [(w, i) for i in range(a, b + 1) for w in words(lines[i])]
    first = {}
    matcher = difflib.SequenceMatcher(None, [w for w, _ in text], heard, autojunk=False)
    for block in matcher.get_matching_blocks():
        for k in range(block.size):
            first.setdefault(text[block.a + k][1], gap[block.b + k][1])
    return [(first.get(i), lines[i]) for i in range(a, b + 1)]


def closest_line(lines, text):
    """The lyric line that a transcript line repeats, with Whisper's mistakes, or else the transcript line."""
    heard = words(text)
    ratio = [difflib.SequenceMatcher(None, words(line), heard, autojunk=False).ratio() for line in lines]
    best = max(range(len(lines)), key=ratio.__getitem__, default=None)
    return lines[best] if best is not None and ratio[best] >= REPEAT else text


def transcript_lines(said):
    """Lines from transcript words (start, text, probability): a line ends at a pause, at punctuation, or before a
    capital letter, and has 4 to 12 words. Lines that Whisper is not sure of are left out."""
    lines, line = [], []
    for k, word in enumerate(said):
        line.append(word)
        if k + 1 == len(said):
            break
        text, (start, following, _) = word[1].strip(), said[k + 1]
        pause = start - word[0] > 1.5
        capital = following.strip()[:1].isupper() and words(following)[:1] not in (
            ["i"],
            ["im"],
            ["ill"],
            ["id"],
            ["ive"],
        )
        if len(line) >= 12 or pause or (len(line) >= 4 and (text[-1:] in ",.?!;" or capital)):
            lines.append(line)
            line = []
    lines.append(line)
    sure = [line for line in lines if line and sum(p for _, _, p in line) / len(line) >= SURE]
    return [(line[0][0], "".join(text for _, text, _ in line).strip().rstrip(",")) for line in sure]


def stamp(seconds):
    seconds = max(seconds, 0.0)
    return f"[{int(seconds // 60):02d}:{seconds % 60:05.2f}]"


def write(out, lines):
    """Write the .lrc lines to OUT, or to stdout when OUT is "-"."""
    if out == "-":
        sys.stdout.writelines(lines)
        return
    with open(out, "w", encoding="utf-8") as f:
        f.writelines(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--model", default="large-v3-turbo", help="Whisper model (default: large-v3-turbo)")
    parser.add_argument("--cache", action="store_true", help="only transcribe AUDIO files into the cache")
    parser.add_argument("audio")
    parser.add_argument("lyrics", nargs="?")
    parser.add_argument("out", nargs="?", help='the .lrc file to write, or "-" for stdout')
    parser.add_argument("more", nargs="*", help=argparse.SUPPRESS)
    args = parser.parse_args()

    loaded = []

    def model():
        if not loaded:
            import stable_whisper

            # faster-whisper (CTranslate2) transcribes several times faster than OpenAI's Whisper, on a GPU and on
            # CPUs; large-v3-turbo hears about twice as many sung words as medium.en.
            loaded.append(stable_whisper.load_faster_whisper(args.model))
        return loaded[0]

    if args.cache:
        for audio in [args.audio, args.lyrics, args.out, *args.more]:
            if audio:
                try:
                    transcribe(audio, args.model, model)
                except (OSError, RuntimeError) as error:  # An unreadable file must not stop the others.
                    print(f"{audio}: {error}", file=sys.stderr)
        return
    if not args.out:
        parser.error("AUDIO LYRICS OUT are needed")
    timed = lyric_lines(args.lyrics)
    if not timed:
        sys.exit("no lyrics")
    lines = [text for text, _ in timed]
    said = transcribe(args.audio, args.model, model)

    # Each lyric word, the line it is on; matched to transcript words in order.
    lyric = [(w, i) for i, line in enumerate(lines) for w in words(line)]
    matcher = difflib.SequenceMatcher(None, [w for w, _ in lyric], [s[0] for s in said], autojunk=False)
    match = {}  # lyric word index -> transcript word index
    for block in matcher.get_matching_blocks():
        match.update({block.a + k: block.b + k for k in range(block.size)})
    count = [len(words(line)) for line in lines]
    found = [[] for _ in lines]  # transcript word indexes per line
    for a, b in match.items():
        found[lyric[a][1]].append(b)
    # A line is sung in a few seconds: of its matched words, only the most that lie that close together count.
    for i in range(len(lines)):
        span = max(LINE_SPAN, WORD_TIME * 4 * count[i])
        best = []
        for k, b in enumerate(found[i]):
            close = [c for c in found[i][k:] if said[c][1] - said[b][1] <= span]
            if len(close) > len(best):
                best = close
        found[i] = best
    match = {a: b for a, b in match.items() if b in found[lyric[a][1]]}
    is_heard = [len(found[i]) >= max(min(2, count[i]), LINE_HEARD * count[i]) for i in range(len(lines))]
    # The words of another song match a line here and there; the words of another version of this song match
    # most lines, or a whole verse in a row.
    # A run of heard lines goes on past one line that is not heard (Whisper misses a line here and there).
    run = longest = missed = 0
    for heard_line in is_heard:
        missed = 0 if heard_line else missed + 1
        run = run + 1 if heard_line else 0 if missed > 1 else run
        longest = max(longest, run)
    if sum(is_heard) < MIN_HEARD * len(lines) and longest < MIN_RUN:
        print(f"lyrics do not fit the recording: {sum(is_heard)} of {len(lines)} lines heard", file=sys.stderr)
        sys.exit(NO_FIT)
    # A word or two of a line that is not heard match by chance (a "we" in another part); they do not place it.
    for i in range(len(lines)):
        if not is_heard[i]:
            found[i] = []
    match = {a: b for a, b in match.items() if is_heard[lyric[a][1]]}

    # Synced lyrics whose times agree with the recording are kept as they are.
    if all(t is not None for _, t in timed):
        diffs = [timed[i][1] - said[min(found[i])][1] for i in range(len(lines)) if found[i] and is_heard[i]]
        close = sum(abs(d) <= SYNC_DRIFT for d in diffs)
        if len(diffs) >= 5 and close >= SYNC_CLOSE * len(diffs) and abs(statistics.median(diffs)) <= 1.0:
            write(args.out, [f"{stamp(t)}{text}\n" for text, t in timed])
            print(f"synced lyrics fit: {close} of {len(diffs)} heard lines within {SYNC_DRIFT} s", file=sys.stderr)
            return
        print(
            f"synced lyrics are off: {close} of {len(diffs)} heard lines within {SYNC_DRIFT} s; timing again",
            file=sys.stderr,
        )

    # Transcript word positions around each line: the last matched word before it and the first after it.
    def before(i):
        return max((max(found[j]) for j in range(i) if found[j]), default=-1)

    def after(i):
        return min((min(found[j]) for j in range(i + 1, len(lines)) if found[j]), default=len(said))

    # Runs of lines that are not heard: Whisper often misses sung words, so a run is dropped only when the
    # recording has no time for it between the lines around it (an intro of another version).
    keep = [True] * len(lines)
    i = 0
    while i < len(lines):
        if is_heard[i]:
            i += 1
            continue
        end = i
        while end + 1 < len(lines) and not is_heard[end + 1]:
            end += 1
        start = said[before(i)][1] + WORD_TIME if before(i) >= 0 else 0.0
        stop = said[after(end)][1] if after(end) < len(said) else float("inf")
        # The recording has other words in that time (Whisper hears speech well, and sung words in part).
        heard_there = [s[0] for s in said[before(i) + 1 : after(end)] if s[0] != "music"]
        run_words = [w for line in lines[i : end + 1] for w in words(line)]
        same = difflib.SequenceMatcher(None, run_words, heard_there, autojunk=False).get_matching_blocks()
        other = end - i + 1 >= OTHER_LINES and len(heard_there) >= NEW_WORDS and sum(b.size for b in same) < OTHER_WORDS * len(heard_there)
        if stop - start < WORD_TIME * sum(count[i : end + 1]) or other:
            keep[i : end + 1] = [False] * (end + 1 - i)
        i = end + 1

    # Long unmatched parts of the transcript with no lyric lines for them become lines. The lyric lines that are
    # kept, in order, with the transcript words they bracket.
    matched = sorted(match.values())
    out = []  # (text, matched word time or None)
    edges = [-1] + matched + [len(said)]
    added = {}  # transcript index after which new lines go -> lines
    for left, right in itertools.pairwise(edges):
        if right - left - 1 < NEW_WORDS:
            continue
        unheard_lyrics = sum(
            count[i] for i in range(len(lines)) if keep[i] and not found[i] and before(i) <= left and after(i) >= right
        )
        if unheard_lyrics >= 0.3 * (right - left - 1):
            continue
        gap = said[left + 1 : right]
        added[left] = repeated_lines(lines, gap) or [
            (t, closest_line(lines, text)) for t, text in transcript_lines([s[1:] for s in gap if s[2]])
        ]
    for left in sorted(added):
        if added[left]:
            print(f"adding {len(added[left])} lines at {stamp(added[left][0][0] or 0)}", file=sys.stderr)
    dropped = [lines[i] for i in range(len(lines)) if not keep[i]]
    if dropped:
        print(f"dropping {len(dropped)} lines the recording does not have: {dropped[0]!r}...", file=sys.stderr)

    for i, line in enumerate(lines):
        first = min(found[i]) if found[i] else None
        # New lines go before the first lyric line after them; a line without matched words lies somewhere
        # before the next matched word, so the new lines in that stretch go first.
        for left in sorted(k for k in added if k < (first if first is not None else after(i))):
            out += [(text, t) for t, text in added.pop(left)]
        if keep[i]:
            out.append((line, said[first][1] if first is not None else None))
        if found[i]:
            # New lines among this line's own words (it is sung twice) go after it.
            for left in sorted(k for k in added if k < max(found[i])):
                out += [(text, t) for t, text in added.pop(left)]
    for left in sorted(added):
        out += [(text, t) for t, text in added.pop(left)]

    # Whisper aligns the text; matched words correct lines that it puts far away.
    aligned = model().align(samples(args.audio), "\n".join(text for text, _ in out), language="en", original_split=True)
    starts = [seg.start for seg in aligned.segments if seg.text.strip()]
    if len(starts) != len(out):
        print(f"aligner returned {len(starts)} lines for {len(out)}; using matched words only", file=sys.stderr)
        starts = [None] * len(out)
    times, moved = [], 0
    for (text, anchor), start in zip(out, starts):
        if anchor is not None and (start is None or abs(start - anchor) > MAX_DRIFT):
            moved += start is not None
            start = anchor
        times.append(start)
    # A line without matched words keeps its aligned time only when it lies between the lines with matched words
    # around it, and is not the time of the line before or after (the aligner puts lines it cannot place on one time).
    anchored = [i for i, (_, anchor) in enumerate(out) if anchor is not None]
    given = list(times)
    for i, (_, anchor) in enumerate(out):
        if anchor is None and times[i] is not None:
            low = max((times[j] for j in anchored if j < i), default=0.0)
            high = min((times[j] for j in anchored if j > i), default=float("inf"))
            same = any(
                0 <= j < len(given) and given[j] is not None and abs(given[i] - given[j]) < 0.3 for j in (i - 1, i + 1)
            )
            if not low < times[i] < high or same:
                times[i] = None
    # The other lines share the time between their neighbours, in proportion to their words.
    times = spread(times, [max(1, len(words(text))) for text, _ in out])
    for i in range(1, len(times)):
        times[i] = max(times[i], times[i - 1] + 0.01)
    write(args.out, [f"{stamp(t)}{text}\n" for (text, _), t in zip(out, times)])
    print(
        f"{len(out)} lines: {sum(is_heard)}/{len(lines)} lyric lines heard, {len(dropped)} dropped, "
        f"{sum(a is None for _, a in out)} without matched words, {moved} moved to their matched words",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
