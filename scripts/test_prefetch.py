"""WDX prefetch lifecycle checks and optional read-only benchmarks of real media.

python scripts/test_prefetch.py --dll target/release/mediares_wdx.dll
python scripts/test_prefetch.py --dll target/release/mediares_wdx.dll --benchmark D:/MP3 D:/фотки
"""

import argparse
from contextlib import closing
import ctypes
import json
import math
import os
from pathlib import Path
import sqlite3
import struct
import subprocess
import sys
import time
import uuid
import wave
import zlib

sys.dont_write_bytecode = True
from test_content_cache import content_plugin, stage_plugin, make_avi, make_exif, make_video_tags


def values(dll, fields, paths, field):
    result = []
    index, _ = fields[field]
    for path in paths:
        buf = ctypes.create_string_buffer(8192)
        kind = dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 0)
        value = ctypes.wstring_at(buf) if kind == 11 else buf.raw[:8].hex()
        result.append((kind, value))
    return result


def worker(args):
    spec = json.loads(Path(args.worker).read_text(encoding="utf-8"))
    dll, fields = content_plugin(args.dll, spec.get("tc_ini"))
    started = time.perf_counter()
    result = values(dll, fields, spec["paths"], spec["field"])
    if spec.get("probe_independence"):
        # With workers disabled, a quick request must defer unrelated audio algorithms even
        # after this one is in RAM. Checking a disk row alone cannot prove RAM isolation.
        shared = (
            {"Audio_Fingerprint", "Audio_PCM_Hash"}
            if spec["field"] in ("Audio_Fingerprint", "Audio_PCM_Hash")
            else {spec["field"]}
        )
        for other in ("Audio_Fingerprint", "Audio_Fast_Fingerprint", "Audio_PCM_Hash"):
            if other not in shared:
                buf = ctypes.create_string_buffer(8192)
                assert dll.ContentGetValueW(spec["paths"][0], fields[other][0], 0,
                                            buf, len(buf), 1) == 0, other
    elapsed = time.perf_counter() - started
    if spec.get("also_field"):
        values(dll, fields, spec.get("also_paths") or spec["paths"], spec["also_field"])
    time.sleep(spec.get("linger", 0))
    dll.ContentStopGetValueW(None)
    stop = time.perf_counter()
    dll.ContentPluginUnloading()
    stop_elapsed = time.perf_counter()-stop
    if spec.get("restart"):
        assert values(dll, fields, spec["paths"], spec["field"]) == result
        dll.ContentPluginUnloading()
    print(json.dumps(dict(seconds=elapsed, stop_seconds=stop_elapsed, values=result,
                         returned=sum(kind > 0 for kind, _ in result))))


def measure(dll, work, paths, field, threads, name, linger=0, probe_independence=False,
            also_field=None, also_paths=None):
    dll = stage_plugin(dll, work / f"plugin-{threads}", threads)
    tc_dir = work / "tc-settings"
    tc_dir.mkdir(exist_ok=True)
    (tc_dir / "mediares.ini").write_text("[Settings]\nPrefetchThreads=0\n", encoding="utf-16")
    if threads == 0:
        # No portable INI: read TC's settings directory via ContentSetDefaultParams.
        dll.with_name("mediares.ini").unlink()
    # Other modes prove that the portable INI takes priority over TC's zero setting.
    spec = work / f"{name}-{threads}.json"
    spec.write_text(json.dumps(dict(paths=list(map(str, paths)), field=field, linger=linger,
                                   tc_ini=str(tc_dir / "contplug.ini"),
                                   restart=name == "bounded",
                                   probe_independence=probe_independence,
                                   also_field=also_field,
                                   also_paths=list(map(str, also_paths)) if also_paths else None)), encoding="utf-8")
    env = dict(os.environ, MEDIARES_CONTENT_CACHE=str(work / f"{name}-{threads}.sqlite3"))
    output = subprocess.check_output([sys.executable, __file__, "--dll", str(dll),
                                      "--worker", str(spec)], env=env, text=True, timeout=300)
    return json.loads(output)


def main(args):
    work = Path("target/prefetch-tests").resolve() / uuid.uuid4().hex[:8]
    work.mkdir(parents=True)
    fixtures = work / "fixtures"
    fixtures.mkdir()
    # Valid tiny BMPs need no imaging package. More than LOOKAHEAD entries checks boundedness.
    for i in range(96):
        pixels = bytes([i, 42, 73, 0] * 4)
        bmp = b"BM" + struct.pack("<IHHI", 54+len(pixels), 0, 0, 54)
        bmp += struct.pack("<IiiHHIIiiII", 40, 1, 4, 1, 24, 0, len(pixels), 0, 0, 0, 0)
        (fixtures / f"{i:03}.bmp").write_bytes(bmp + pixels)
    paths = sorted(fixtures.glob("*.bmp"))
    mixed = work / "mixed"
    mixed.mkdir()
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(b"\0\x20\x40\x60")) + chunk(b"IEND", b"")
    for i in range(4):
        (mixed / f"{i}.{'BMP' if i % 2 else 'bmp'}").write_bytes(paths[0].read_bytes())
        (mixed / f"{i}.png").write_bytes(png)
    for both in (False, True):
        name = "extensions-both" if both else "extensions-one"
        measure(args.dll, work, [mixed / "0.bmp"], "Image_pHash", 3, name, linger=0.5,
                also_field="Image_pHash" if both else None,
                also_paths=[mixed / "0.png"] if both else None)
        with closing(sqlite3.connect(work / f"{name}-3.sqlite3")) as db:
            cached = [Path(bytes(row[0]).decode("utf-16-le"))
                      for row in db.execute("SELECT path FROM entries")]
            assert sum(p.suffix.lower() == ".bmp" for p in cached) == 4, cached
            assert sum(p.suffix.lower() == ".png" for p in cached) == (4 if both else 0), cached
    baseline = measure(args.dll, work, paths, "Image_Width", 0, "sizes")
    parallel = measure(args.dll, work, paths, "Image_Width", 3, "sizes")
    assert baseline["values"] == parallel["values"]
    assert all(v[0] == 1 for v in parallel["values"])
    # A metadata-only query must not create analysis or sharpness records.
    with closing(sqlite3.connect(work / "sizes-3.sqlite3")) as db:
        assert db.execute("SELECT count(*) FROM entries WHERE slot IN (0,1)").fetchone()[0] == 0
    audio = work / "audio"
    audio.mkdir()
    for i in range(8):
        with wave.open(str(audio / f"{i}.wav"), "wb") as wav:
            wav.setparams((1, 2, 8000, 0, "NONE", "not compressed"))
            # Several audible seconds, plus one silent file to exercise cached None.
            samples = (int(12000 * math.sin(2 * math.pi * (220+i*37) * t / 8000))
                       if i < 7 else 0 for t in range(32000))
            wav.writeframes(struct.pack("<32000h", *samples))
    tags = measure(args.dll, work, sorted(audio.glob("*.wav")), "Audio_Channels", 3, "tags")
    assert tags["returned"] == 8
    with closing(sqlite3.connect(work / "tags-3.sqlite3")) as db:
        assert db.execute("SELECT count(*) FROM entries WHERE slot IN (0,1,10,11,12)").fetchone()[0] == 0
    audio_paths = sorted(audio.glob("*.wav"))
    metadata_cases = [(paths, "Image_Width", 7), (paths, "Image_Bit_Depth", 8),
                      (audio_paths, "Audio_Channels", 3)]
    for name, extension, create, field, slot in (
            ("exif", ".jpg", make_exif, "Photo_Make", 2),
            ("video", ".avi", make_avi, "Video_Width", 4),
            ("video-tags", ".mp4", make_video_tags, "Video_Title", 6)):
        folder = work / name
        folder.mkdir()
        first = folder / ("0" + extension)
        create(first)
        data = first.read_bytes()
        for i in range(1, 8):
            (folder / (str(i) + extension)).write_bytes(data)
        metadata_cases.append((sorted(folder.glob("*")), field, slot))
    # A single metadata query must cache exactly that file, regardless of worker count.
    # Leave time for speculative work: checking only an absence of hash rows misses
    # unwanted reads of neighbors in the same metadata family.
    for candidates, field, slot in metadata_cases:
        reference = None
        for threads in (0, 3):
            name = f"on-demand-{field}"
            result = measure(args.dll, work, candidates[:1], field, threads, name, linger=0.5)
            assert result["returned"] == 1, field
            reference = result["values"] if reference is None else reference
            assert result["values"] == reference, field
            with closing(sqlite3.connect(work / f"{name}-{threads}.sqlite3")) as db:
                rows = db.execute("SELECT path,slot FROM entries").fetchall()
                assert rows == [(str(candidates[0]).encode("utf-16-le"), slot)], (field, rows)
    for field, slot in (("Audio_Fingerprint", 10), ("Audio_PCM_Hash", 10),
                        ("Audio_Fast_Fingerprint", 11), ("Audio_Duration_Sec", 12)):
        baseline = measure(args.dll, work, audio_paths, field, 0, field,
                           probe_independence=True)
        parallel = measure(args.dll, work, audio_paths, field, 3, field)
        assert baseline["values"] == parallel["values"], field
        for threads in (0, 3):
            with closing(sqlite3.connect(work / f"{field}-{threads}.sqlite3")) as db:
                rows = db.execute("SELECT slot,fingerprint,pcm_hash FROM entries").fetchall()
                assert len(rows) == 8 and {r[0] for r in rows} == {slot}, (field, rows)
                if slot not in (10, 11):
                    assert all(r[1] is None for r in rows)
                if slot != 10:
                    assert all(r[2] is None for r in rows)
                else:
                    assert all(r[2] is not None for r in rows)
        if slot in (10, 11) and field != "Audio_PCM_Hash":
            assert baseline["returned"] == 7  # Silence is a successful empty fingerprint.
        else:
            assert baseline["returned"] == 8
        # Only hashes prefetch neighbors; decoded duration stays on demand.
        measure(args.dll, work, audio_paths[:1], field, 3, f"ahead-{field}", linger=0.5)
        with closing(sqlite3.connect(work / f"ahead-{field}-3.sqlite3")) as db:
            rows = db.execute("SELECT slot FROM entries").fetchall()
            expected_count = len(rows) == 1 if slot == 12 else 2 <= len(rows) <= 8
            assert expected_count and {r[0] for r in rows} == {slot}, (field, rows)
    measure(args.dll, work, audio_paths[:1], "Audio_Fingerprint", 3, "both-audio",
            linger=0.5, also_field="Audio_Fast_Fingerprint")
    with closing(sqlite3.connect(work / "both-audio-3.sqlite3")) as db:
        counts = dict(db.execute("SELECT slot,count(*) FROM entries GROUP BY slot"))
        assert counts.keys() == {10, 11} and all(2 <= n <= 8 for n in counts.values()), counts
    measure(args.dll, work, paths[:1], "Image_pHash", 0, "bounded", linger=0.5)
    with closing(sqlite3.connect(work / "bounded-0.sqlite3")) as db:
        assert db.execute("SELECT count(*) FROM entries").fetchone()[0] == 1
    measure(args.dll, work, paths[:1], "Image_pHash", 3, "bounded", linger=0.5)
    with closing(sqlite3.connect(work / "bounded-3.sqlite3")) as db:
        assert 2 <= db.execute("SELECT count(*) FROM entries").fetchone()[0] <= 33
    measure(args.dll, work, paths[:1], "Image_Sharpness", 3, "sharpness", linger=0.5)
    with closing(sqlite3.connect(work / "sharpness-3.sqlite3")) as db:
        rows = db.execute("SELECT slot FROM entries").fetchall()
        assert 2 <= len(rows) <= 33 and {r[0] for r in rows} == {1}, rows
    print("PASS: INI disable/portable priority, metadata/duration only on demand, independent audio hashes, hash/sharpness prefetch, silence, bounded read-ahead, cancellation/unload", flush=True)
    report = []
    for root in args.benchmark or []:
        audio = root.name.lower() == "mp3"
        extensions = {".mp3", ".flac", ".wav"} if audio else {".jpg", ".jpeg", ".png"}
        # First sufficiently populated folder; no source files are changed or copied.
        selected = []
        for parent, _, names in os.walk(root):
            selected = sorted(Path(parent) / n for n in names if Path(n).suffix.lower() in extensions)
            if len(selected) >= 64:
                break
        selected = selected[:96]
        if not selected:
            raise RuntimeError(f"No media in {root}")
        for field in (["Audio_Fingerprint", "Audio_Fast_Fingerprint", "Audio_PCM_Hash", "Audio_Artist"] if audio else ["Image_pHash", "Photo_Make"]):
            reference = None
            for repeat in range(args.repeats):
                for threads in ((0, 3, 7) if repeat % 2 == 0 else (7, 3, 0)):
                    row = measure(args.dll, work, selected, field, threads, f"{field}-{repeat}")
                    reference = row["values"] if reference is None else reference
                    assert row["values"] == reference, field
                    row.pop("values")
                    row.update(folder=str(selected[0].parent), files=len(selected), field=field, threads=threads, repeat=repeat)
                    report.append(row)
                    print(json.dumps(row, ensure_ascii=False), flush=True)
    (work / "results.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"Results: {work}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dll", type=Path, required=True)
    parser.add_argument("--worker", type=Path)
    parser.add_argument("--benchmark", type=Path, nargs="*")
    parser.add_argument("--repeats", type=int, default=3)
    options = parser.parse_args()
    options.dll = options.dll.resolve()
    worker(options) if options.worker else main(options)
