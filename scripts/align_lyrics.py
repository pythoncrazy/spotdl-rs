#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["stable-ts>=2.19", "torch>=2.4", "torchaudio>=2.4"]
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

spotdl-rs runs this as its `--lyrics-aligner` for songs that have no synced lyrics for their recording. The lyrics
often come from another version of the song, so the script first finds out which lines the recording has:

1. Whisper transcribes the recording, with a time for each word.
2. The lyric words are matched to the transcript words, in order.
3. A run of lyric lines that the recording does not have is dropped (an intro of another version): no line of the
   run is heard, and the recording has no time for it between the lines around it.
4. A long part of the transcript that no lyric line matches is added as lines (a verse that the text does not
   have).
5. Whisper aligns the resulting text to the recording. A line whose aligned time is far from the time of its
   matched words takes the time of its matched words.

It exits with status 3 and writes nothing when the lyrics are for another song: fewer than 40% of the lyric lines
are heard, and no 8 lines in a row are heard.
"""

import argparse
import difflib
import itertools
import re
import sys

NO_FIT = 3  # Exit status when the lyrics are not the words of the recording.
MIN_HEARD = 0.4  # Part of the lyric lines that must be heard, or the lyrics are for another song,
MIN_RUN = 8  # unless this many lines in a row are heard.
LINE_HEARD = 0.25  # Part of a line's words that must be heard for the line to count as heard.
WORD_TIME = 0.25  # Seconds that a word takes at least.
MAX_DRIFT = 2.0  # Seconds that an aligned line can be away from its matched words.
NEW_WORDS = 12  # Unmatched transcript words in one place that make new lines.
REPEAT = 0.6  # How closely the new words must match lyric lines to repeat those lines instead.
SURE = 0.5  # Mean word probability of a new line from the transcript.

CREDIT = re.compile(r"作曲|作词|编曲|^(composer|lyricist|producer|written by)\b", re.IGNORECASE)


def words(text):
    return [w for w in re.sub(r"[^a-z0-9' ]", " ", text.lower().replace("’", "'")).replace("'", "").split() if w]


def lyric_lines(path):
    with open(path, encoding="utf-8", errors="ignore") as f:
        lines = [re.sub(r"^(\[[0-9:.]+\])+", "", line.strip()).strip() for line in f]
    return [line for line in lines if words(line) and not CREDIT.search(line) and not re.fullmatch(r"\[.*\]", line)]


def repeated_lines(lines, keep, gap):
    """The lyric lines that the gap's transcript words repeat (a chorus that the text has once), each with the
    time of its first matched word; or None."""
    heard = [s[0] for s in gap]
    best, window = 0.0, None
    for a in range(len(lines)):
        for b in range(a, len(lines)):
            text = [w for line in lines[a : b + 1] for w in words(line)]
            if len(text) > 1.5 * len(heard):
                break
            ratio = difflib.SequenceMatcher(None, text, heard, autojunk=False).ratio()
            if ratio > best and all(keep[a : b + 1]):
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


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--model", default="small.en", help="Whisper model (default: small.en)")
    parser.add_argument("audio")
    parser.add_argument("lyrics")
    parser.add_argument("out")
    args = parser.parse_args()

    import stable_whisper

    lines = lyric_lines(args.lyrics)
    if not lines:
        sys.exit("no lyrics")
    model = stable_whisper.load_model(args.model)
    heard = model.transcribe(args.audio, language="en", word_timestamps=True, verbose=None)
    # Transcript words: (word, start, text, probability); a word like "5-0" is two words, with its text on the
    # first.
    said = [
        (w, word.start, word.word if k == 0 else "", word.probability)
        for seg in heard.segments
        for word in seg.words
        for k, w in enumerate(words(word.word))
    ]

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
    is_heard = [len(found[i]) >= max(min(2, count[i]), LINE_HEARD * count[i]) for i in range(len(lines))]
    # The words of another song match a line here and there; the words of another version of this song match
    # most lines, or a whole verse in a row.
    run = longest = 0
    for heard_line in is_heard:
        run = run + 1 if heard_line else 0
        longest = max(longest, run)
    if sum(is_heard) < MIN_HEARD * len(lines) and longest < MIN_RUN:
        print(f"lyrics do not fit the recording: {sum(is_heard)} of {len(lines)} lines heard", file=sys.stderr)
        sys.exit(NO_FIT)

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
        if stop - start < WORD_TIME * sum(count[i : end + 1]):
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
        added[left] = repeated_lines(lines, keep, gap) or transcript_lines([s[1:] for s in gap if s[2]])
    for left in sorted(added):
        if added[left]:
            print(f"adding {len(added[left])} lines at {stamp(added[left][0][0] or 0)}", file=sys.stderr)
    dropped = [lines[i] for i in range(len(lines)) if not keep[i]]
    if dropped:
        print(f"dropping {len(dropped)} lines the recording does not have: {dropped[0]!r}...", file=sys.stderr)

    position = -1
    for i, line in enumerate(lines):
        first = min(found[i]) if found[i] else None
        for left in sorted(k for k in added if position <= k < (first if first is not None else -2)):
            out += [(text, t) for t, text in added.pop(left)]
        if keep[i]:
            out.append((line, said[first][1] if first is not None else None))
        if found[i]:
            position = max(found[i])
    for left in sorted(added):
        out += [(text, t) for t, text in added.pop(left)]

    # Whisper aligns the text; matched words correct lines that it puts far away.
    aligned = model.align(args.audio, "\n".join(text for text, _ in out), language="en", original_split=True)
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
    # Lines with neither a time from the aligner nor matched words share the time between their neighbours.
    for i, t in enumerate(times):
        if t is None:
            prev = next((times[j] for j in range(i - 1, -1, -1) if times[j] is not None), 0.0)
            times[i] = prev
    for i in range(1, len(times)):
        times[i] = max(times[i], times[i - 1])
    with open(args.out, "w", encoding="utf-8") as f:
        f.writelines(f"{stamp(t)}{text}\n" for (text, _), t in zip(out, times))
    print(
        f"{len(out)} lines: {sum(is_heard)}/{len(lines)} lyric lines heard, {len(dropped)} dropped, "
        f"{sum(a is None for _, a in out)} without matched words, {moved} moved to their matched words",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
