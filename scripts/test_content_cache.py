"""Exercise the real WDX DLL in fresh processes; retain fixtures and timings under target/."""

import argparse
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


def worker(dll_path, paths):
    dll = ctypes.WinDLL(str(dll_path))
    dll.ContentGetSupportedField.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int]
    dll.ContentGetSupportedField.restype = ctypes.c_int
    dll.ContentGetValueW.argtypes = [ctypes.c_wchar_p, ctypes.c_int, ctypes.c_int,
                                   ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
    dll.ContentGetValueW.restype = ctypes.c_int
    dll.ContentPluginUnloading.argtypes = []
    dll.ContentPluginUnloading.restype = None
    fields = {}
    for index in range(256):
        name, units = ctypes.create_string_buffer(128), ctypes.create_string_buffer(128)
        kind = dll.ContentGetSupportedField(index, name, units, 128)
        if kind == 0:
            break
        fields[name.value.decode("ascii")] = (index, kind)

    groups = {
        ".bmp": ["Image_dHash", "Image_pHash", "Image_CoarseHash", "Image_Sharpness"],
        ".wav": ["Audio_Fingerprint", "Audio_PCM_Hash", "Audio_Duration_Sec"],
        ".avi": ["Video_Fingerprint", "Video_dHash_Mid", "Video_Duration_Sec"],
    }
    values = {}
    start = time.perf_counter()
    try:
        for path in paths:
            for name in groups[path.suffix]:
                index, kind = fields[name]
                buf = ctypes.create_string_buffer(8192)
                if name == groups[path.suffix][0] or name == "Image_Sharpness":
                    # A disk hit must still run off TC's UI thread; is_cached checks RAM only.
                    assert dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 1) == 0, name
                result = dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 0)
                assert result == kind, (name, result, kind)
                value = ctypes.wstring_at(buf) if kind == 11 else ctypes.c_int.from_buffer(buf).value
                assert value != "", name
                values[path.name + ":" + name] = value
                assert dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 1) == kind, name
    finally:
        dll.ContentPluginUnloading()
    print(json.dumps({"values": values, "ms": (time.perf_counter() - start) * 1000}))


def make_bmp(path, alternate=False):
    width, height = 1600, 1200
    row = bytes([255]) * (width * 3) if alternate else bytes(
        (x * 37 + x // 61) % 256 for x in range(width * 3)
    )
    pixels = row * height
    header = b"BM" + struct.pack("<IHHI", 54 + len(pixels), 0, 0, 54)
    header += struct.pack("<IiiHHIIiiII", 40, width, height, 1, 24, 0, len(pixels), 0, 0, 0, 0)
    path.write_bytes(header + pixels)


def make_wav(path):
    rate = 22050
    samples = (int(12000 * math.sin(i * 2 * math.pi * 440 / rate)
                   + 6000 * math.sin(i * 2 * math.pi * 997 / rate)) for i in range(rate * 10))
    with wave.open(str(path), "wb") as out:
        out.setparams((1, 2, rate, 0, "NONE", "not compressed"))
        out.writeframes(b"".join(struct.pack("<h", sample) for sample in samples))


def make_avi(path):
    width, height, fps, frames = 128, 96, 10, 30

    def ints(*values):
        return struct.pack("<" + "I" * len(values), *values)

    def chunk(tag, body):
        return tag + ints(len(body)) + body

    def listing(kind, body):
        return chunk(b"LIST", kind + body)

    frame_size = width * height * 3
    avih = ints(1000000 // fps, frame_size * fps, 0, 0x10, frames, 0, 1,
                frame_size, width, height, 0, 0, 0, 0)
    strh = b"vidsDIB " + ints(0, 0, 0, 1, fps, 0, frames, frame_size, 0xFFFFFFFF, 0)
    strh += struct.pack("<4H", 0, 0, width, height)
    strf = ints(40, width, height) + struct.pack("<2H", 1, 24) + ints(0, frame_size, 0, 0, 0, 0)
    hdrl = listing(b"hdrl", chunk(b"avih", avih) + listing(b"strl", chunk(b"strh", strh) + chunk(b"strf", strf)))
    movi, idx = bytearray(), bytearray()
    for frame in range(frames):
        pixels = bytes(value for y in range(height - 1, -1, -1) for x in range(width)
                       for value in [(x * 3 + y * 5 + frame * 7) % 256] * 3)
        idx.extend(b"00db" + ints(0x10, 4 + len(movi), frame_size))
        movi.extend(chunk(b"00db", pixels))
    path.write_bytes(chunk(b"RIFF", b"AVI " + hdrl + listing(b"movi", movi) + chunk(b"idx1", idx)))


def run_check(dll, baseline):
    root = Path(__file__).resolve().parent.parent
    work = root / "target" / ("cache-check-" + uuid.uuid4().hex)
    work.mkdir(parents=True)
    paths = [work / "фото 🦊.bmp", work / "audio.wav", work / "video.avi"]
    make_bmp(paths[0])
    make_wav(paths[1])
    make_avi(paths[2])
    db = work / "cache.sqlite3"

    def run(cache, library=dll):
        env = dict(os.environ, LOCALAPPDATA=str(work / "local"))
        if cache is None:
            env.pop("MEDIARES_CONTENT_CACHE", None)
        else:
            env["MEDIARES_CONTENT_CACHE"] = str(cache)
        completed = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--worker",
                                    "--dll", str(library), *map(str, paths)], env=env,
                                   capture_output=True, text=True, timeout=30, check=True)
        return json.loads(completed.stdout)

    def rows():
        with sqlite3.connect(db) as conn:
            return conn.execute("SELECT path,slot,rowid,size,modified,version FROM entries ORDER BY path,slot").fetchall()

    cold = run(db)
    original = rows()
    assert len(original) == 4, original  # Image, audio, video and sharpness.
    warm = run(db)
    assert cold["values"] == warm["values"]
    assert rows() == original, "A fresh process recomputed cached results"
    disabled = run("off")
    assert disabled["values"] == warm["values"]
    assert not (work / "local").exists(), "Disabling created the default cache"
    assert run(None)["values"] == warm["values"]
    default_db = work / "local/mediares/content-cache-v1.sqlite3"
    assert default_db.is_file(), "The default cache was not created"
    with sqlite3.connect(default_db) as conn:
        default_rows = conn.execute("SELECT rowid,* FROM entries ORDER BY rowid").fetchall()
    assert run(None)["values"] == warm["values"]
    with sqlite3.connect(default_db) as conn:
        assert conn.execute("SELECT rowid,* FROM entries ORDER BY rowid").fetchall() == default_rows
    unavailable = run(work)  # A directory cannot be opened as a database.
    assert unavailable["values"] == warm["values"]
    corrupt = work / "broken.sqlite3"
    corrupt.write_bytes(b"broken database")
    assert run(corrupt)["values"] == warm["values"]
    assert corrupt.read_bytes() == b"broken database"

    old_stat = paths[0].stat()
    make_bmp(paths[0], alternate=True)
    os.utime(paths[0], ns=(old_stat.st_atime_ns, old_stat.st_mtime_ns + 2_000_000_000))
    modified = run(db)
    assert modified["values"] != warm["values"], "An edited photo kept stale results"
    changed = rows()
    assert sum(a != b for a, b in zip(original, changed)) == 2, "Unchanged audio/video were recomputed"
    with sqlite3.connect(db) as conn:
        conn.execute("UPDATE entries SET version=99")
    lock = sqlite3.connect(db)
    lock.execute("BEGIN IMMEDIATE")
    try:
        assert run(db)["values"] == modified["values"], "A locked database prevented analysis"
        assert all(row[-1] == 99 for row in rows()), "The writer lock was bypassed"
    finally:
        lock.rollback()
        lock.close()
    assert run(db)["values"] == modified["values"]
    refreshed = rows()
    assert all(row[-1] == 1 for row in refreshed), "Old algorithm versions were reused"
    assert run(db)["values"] == modified["values"]
    assert rows() == refreshed

    report = {"dll": str(dll), "cold_ms": cold["ms"], "warm_ms": warm["ms"],
              "disabled_ms": disabled["ms"], "database_bytes": db.stat().st_size,
              "dll_bytes": dll.stat().st_size,
              "checks": ["fresh-process hits", "default path", "RAM/UI delay", "Unicode paths", "file edit",
                         "algorithm version", "disabled", "unavailable", "corrupt", "locked"]}
    if baseline:
        previous = run("off", baseline)
        assert previous["values"] == modified["values"], "Field values changed from the baseline DLL"
        report.update(baseline_ms=previous["ms"], baseline_dll_bytes=baseline.stat().st_size)
    (work / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    print("Artifacts:", work)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dll", type=Path,
                        default=Path(__file__).resolve().parent.parent / "target/x86_64-pc-windows-gnu/release/mediares_wdx.dll")
    parser.add_argument("--baseline-dll", type=Path)
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("files", nargs="*", type=Path)
    args = parser.parse_args()
    if args.worker:
        worker(args.dll.resolve(), args.files)
    else:
        run_check(args.dll.resolve(), args.baseline_dll.resolve() if args.baseline_dll else None)
