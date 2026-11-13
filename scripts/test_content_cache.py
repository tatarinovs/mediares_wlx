"""Exercise the real WDX DLL in fresh processes; retain fixtures and timings under target/."""

import argparse
from contextlib import closing
import ctypes
from ctypes import wintypes
import json
import math
import os
from pathlib import Path
import shutil
import sqlite3
import struct
import subprocess
import sys
import time
import uuid
import wave


def content_plugin(dll_path):
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
    return dll, fields


def worker(dll_path, paths):
    dll, fields = content_plugin(dll_path)

    groups = {
        ".bmp": ["Image_dHash", "Image_pHash", "Image_CoarseHash", "Image_Sharpness",
                 "Image_Width", "Image_Height", "Image_Bit_Depth", "Image_Has_Alpha"],
        ".wav": ["Audio_Fingerprint", "Audio_PCM_Hash", "Audio_Duration_Sec", "Audio_Title",
                 "Audio_Artist", "Audio_Sample_Rate_Hz", "Audio_Codec", "Audio_Channels"],
        ".avi": ["Video_Width", "Video_Height", "Video_Dimensions", "Video_Duration_Sec",
                 "Video_Frame_Rate", "Video_Codec", "Video_Fingerprint", "Video_dHash_Mid"],
        ".jpg": ["Photo_Make", "Photo_Model", "Photo_Orientation", "Photo_ISO", "Photo_FNumber"],
        ".mp4": ["Video_Title", "Video_Artist", "Video_Year"],
    }
    delayed = {"Image_dHash", "Image_Sharpness", "Audio_Fingerprint", "Audio_Title",
               "Video_Fingerprint", "Video_Width", "Video_Title", "Photo_Make"}
    values = {}
    start = time.perf_counter()
    try:
        for path in paths:
            for name in groups[path.suffix]:
                index, kind = fields[name]
                buf = ctypes.create_string_buffer(8192)
                if name in delayed:
                    # A disk hit must still run off TC's UI thread; is_cached checks RAM only.
                    assert dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 1) == 0, name
                result = dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 0)
                if result == -3 and (name == "Video_Codec" or path.name in {"no-exif.jpg", "no-tags.mp4"}):
                    # Successful empty metadata must survive a restart as well.
                    values[path.name + ":" + name] = None
                    assert dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 1) == -3
                    continue
                assert result == kind, (name, result, kind)
                if kind == 11:
                    value = ctypes.wstring_at(buf)
                elif kind == 3:
                    value = ctypes.c_double.from_buffer(buf).value
                else:
                    value = ctypes.c_int.from_buffer(buf).value
                assert value != "", name
                values[path.name + ":" + name] = value
                assert dll.ContentGetValueW(str(path), index, 0, buf, len(buf), 1) == kind, name
    finally:
        dll.ContentPluginUnloading()
    print(json.dumps({"values": values, "ms": (time.perf_counter() - start) * 1000}))


def quick_worker(dll_path, path):
    dll, fields = content_plugin(dll_path)
    buf = ctypes.create_string_buffer(128)
    start = time.perf_counter()
    try:
        code = dll.ContentGetValueW(str(path), fields["Image_Width"][0], 0, buf, len(buf), 1)
        assert code == 1 and ctypes.c_int.from_buffer(buf).value == 1600
        print(json.dumps({"first_width_ms": (time.perf_counter() - start) * 1000}))
    finally:
        dll.ContentPluginUnloading()


def viewer_worker(dll_path, image, media):
    # The caller stages the DLL without libmpv: this regression belongs to the MF backend.
    dll = ctypes.WinDLL(str(dll_path))
    ole = ctypes.WinDLL("ole32")
    assert ole.CoInitializeEx(None, 2) in (0, 1)
    user = ctypes.WinDLL("user32", use_last_error=True)
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    user.CreateWindowExW.argtypes = [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR,
                                   wintypes.DWORD, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                   ctypes.c_int, wintypes.HWND, wintypes.HMENU,
                                   wintypes.HINSTANCE, wintypes.LPVOID]
    user.CreateWindowExW.restype = wintypes.HWND
    user.DestroyWindow.argtypes = [wintypes.HWND]
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel.GetModuleHandleW.restype = wintypes.HMODULE
    dll.ListLoadW.argtypes = [wintypes.HWND, wintypes.LPCWSTR, ctypes.c_int]
    dll.ListLoadW.restype = wintypes.HWND
    dll.ListLoadNextW.argtypes = [wintypes.HWND, wintypes.HWND, wintypes.LPCWSTR, ctypes.c_int]
    dll.ListLoadNextW.restype = ctypes.c_int
    dll.ListCloseWindow.argtypes = [wintypes.HWND]
    host = user.CreateWindowExW(0, "STATIC", "Hidden Mediares cache test", 0x00CF0000,
                               0, 0, 640, 480, None, None, None, None)
    assert host
    viewer, lock = None, None
    try:
        lock = kernel.CreateFileW(str(media), 0x80000000, 0, None, 3, 0x80, None)
        assert lock != ctypes.c_void_p(-1).value, ctypes.get_last_error()
        assert media.stat().st_size > 0  # Identity is still available while opening is denied.
        rejected = dll.ListLoadW(host, str(media), 0)
        if rejected:
            dll.ListCloseWindow(rejected)
        assert not rejected, "Cached metadata accepted unreadable media in ListLoadW"
        viewer = dll.ListLoadW(host, str(image), 0)
        assert viewer
        assert dll.ListLoadNextW(host, viewer, str(media), 0) == 1, "ListLoadNextW accepted locked media"
        assert dll.ListLoadNextW(host, viewer, str(image), 0) == 0
        assert not kernel.GetModuleHandleW("libmpv-2.dll"), "Expected Media Foundation"
    finally:
        if viewer:
            dll.ListCloseWindow(viewer)
        if lock and lock != ctypes.c_void_p(-1).value:
            kernel.CloseHandle(lock)
        user.DestroyWindow(host)
        dll.ContentPluginUnloading()
        ole.CoUninitialize()
    print(json.dumps({"cached_locked_load_rejected": True, "cached_locked_next_rejected": True}))


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
    info = bytearray(b"INFO")
    for tag, text in [(b"INAM", "Cache song"), (b"IART", "Cache artist")]:
        data = text.encode("ascii") + b"\0"
        info.extend(tag + struct.pack("<I", len(data)) + data + b"\0" * (len(data) % 2))
    data = bytearray(path.read_bytes())
    data.extend(b"LIST" + struct.pack("<I", len(info)) + info)
    struct.pack_into("<I", data, 4, len(data) - 8)
    path.write_bytes(data)


def make_exif(path, alternate=False):
    # Header-only JPEG: these EXIF fields need no image decoding.
    make, model = (b"Other camera\0" if alternate else b"Cache camera\0"), b"Model\0"
    start = 8 + 2 + 4 * 12 + 4
    sub = start + len(make) + len(model)

    def entry(tag, kind, count, value):
        return struct.pack("<HHII", tag, kind, count, value)

    tiff = b"II" + struct.pack("<HIH", 42, 8, 4)
    tiff += entry(0x10F, 2, len(make), start)
    tiff += entry(0x110, 2, len(model), start + len(make))
    tiff += entry(0x112, 3, 1, 6)
    tiff += entry(0x8769, 4, 1, sub) + struct.pack("<I", 0) + make + model
    tiff += struct.pack("<H", 2) + entry(0x829D, 5, 1, sub + 30)
    tiff += entry(0x8827, 3, 1, 400) + struct.pack("<III", 0, 28, 10)
    app = b"Exif\0\0" + tiff
    path.write_bytes(b"\xff\xd8\xff\xe1" + struct.pack(">H", len(app) + 2) + app + b"\xff\xd9")


def make_video_tags(path):
    def atom(tag, body):
        return struct.pack(">I", len(body) + 8) + tag + body

    ilst = b"".join(atom(tag, atom(b"data", struct.pack(">II", 1, 0) + text.encode("utf-8")))
                    for tag, text in [(b"\xa9nam", "Фильм 🦊"), (b"\xa9ART", "Автор"),
                                      (b"\xa9day", "2026-11-12")])
    path.write_bytes(atom(b"ftyp", b"isom\0\0\0\0isom")
                     + atom(b"moov", atom(b"udta", atom(b"meta", b"\0" * 4 + atom(b"ilst", ilst)))))


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


def run_check(dll, baseline, viewer_dll, stress):
    root = Path(__file__).resolve().parent.parent
    work = root / "target" / ("cache-check-" + uuid.uuid4().hex)
    work.mkdir(parents=True)
    paths = [work / "фото 🦊.bmp", work / "audio.wav", work / "video.avi",
             work / "camera.jpg", work / "tags.mp4", work / "no-exif.jpg", work / "no-tags.mp4"]
    make_bmp(paths[0])
    make_wav(paths[1])
    make_avi(paths[2])
    make_exif(paths[3])
    make_video_tags(paths[4])
    paths[5].write_bytes(b"\xff\xd8\xff\xe0\x00\x04\x00\x00\xff\xd9")
    paths[6].write_bytes(struct.pack(">I4s4sI4s", 12, b"ftyp", b"isom", 8, b"moov"))
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
        with closing(sqlite3.connect(db)) as conn:
            return conn.execute("SELECT path,slot,rowid,size,modified,version FROM entries ORDER BY path,slot").fetchall()

    cold = run(db)
    original = rows()
    assert len(original) == 12, original  # Includes successful empty JPEG EXIF and video tags.
    with closing(sqlite3.connect(db)) as conn:
        slots = {row[0] for row in conn.execute("SELECT DISTINCT slot FROM entries")}
        assert slots == {0, 1, 2, 3, 4, 6, 7, 8}, slots
    warm = run(db)
    assert cold["values"] == warm["values"]
    assert rows() == original, "A fresh process recomputed cached results"
    analysis_rows = [row for row in original if row[1] < 2]
    with sqlite3.connect(db) as conn:
        conn.execute("DELETE FROM entries WHERE slot >= 2")
        conn.execute("ALTER TABLE entries DROP COLUMN payload")
        conn.execute("PRAGMA user_version=1")
    conn.close()
    assert run(db)["values"] == warm["values"], "The old schema was not upgraded"
    original = rows()
    assert [row for row in original if row[1] < 2] == analysis_rows, "Upgrading recomputed hashes"
    # A malformed individual metadata record must fall back and repair only that record.
    with sqlite3.connect(db) as conn:
        conn.execute("UPDATE entries SET payload='broken JSON' WHERE slot=2 AND path=?",
                     (str(paths[3]).encode("utf-16-le"),))
    conn.close()
    assert run(db)["values"] == warm["values"]
    repaired = rows()
    assert sum(a != b for a, b in zip(original, repaired)) == 1
    original = repaired
    disabled = run("off")
    assert disabled["values"] == warm["values"]
    assert not (work / "local").exists(), "Disabling created the default cache"
    assert run(None)["values"] == warm["values"]
    default_db = work / "local/mediares/content-cache-v1.sqlite3"
    assert default_db.is_file(), "The default cache was not created"
    with closing(sqlite3.connect(default_db)) as conn:
        default_rows = conn.execute("SELECT rowid,* FROM entries ORDER BY rowid").fetchall()
    assert run(None)["values"] == warm["values"]
    with closing(sqlite3.connect(default_db)) as conn:
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
    assert sum(a != b for a, b in zip(original, changed)) == 4, "Unchanged audio/video were recomputed"
    old_exif = paths[3].stat()
    make_exif(paths[3], alternate=True)
    os.utime(paths[3], ns=(old_exif.st_atime_ns, old_exif.st_mtime_ns + 2_000_000_000))
    edited_metadata = run(db)
    assert edited_metadata["values"]["camera.jpg:Photo_Make"] == "Other camera"
    assert sum(a != b for a, b in zip(changed, rows())) == 1, "Unchanged metadata was recomputed"
    modified = edited_metadata
    with sqlite3.connect(db) as conn:
        conn.execute("UPDATE entries SET version=99")
    conn.close()
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
              "checks": ["fresh-process hits", "metadata hits", "metadata edit", "schema upgrade",
                         "malformed metadata", "default path", "RAM/UI delay", "Unicode paths", "file edit",
                         "algorithm version", "disabled", "unavailable", "corrupt", "locked",
                         "empty EXIF", "empty video tags"]}
    if viewer_dll:
        viewer_dir = work / "viewer-mf"
        viewer_dir.mkdir()
        staged = viewer_dir / "mediares.wlx64"
        shutil.copy2(viewer_dll, staged)
        (viewer_dir / "mediares.ini").write_text("[Settings]\nResumeVideo=0\nVolume=0\n", encoding="utf-8")
        env = dict(os.environ, MEDIARES_CONTENT_CACHE=str(db), LOCALAPPDATA=str(work / "local"))
        completed = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--viewer-worker",
                                    "--dll", str(staged), str(paths[0]), str(paths[2])],
                                   env=env, capture_output=True, text=True, timeout=30, check=True)
        report["viewer"] = json.loads(completed.stdout)
        report["checks"].extend(["MF cached ListLoadW rejection", "MF cached ListLoadNextW rejection"])
        with closing(sqlite3.connect(db)) as conn:
            # The actual AudioTags JSON includes every AudioStreamMeta field. Seed its stream
            # slot so neither the Rust decoder nor the MF fallback can trust a cached record.
            conn.execute("""INSERT INTO entries(path,slot,size,modified,version,kind,payload)
                            SELECT path,5,size,modified,version,5,payload FROM entries
                            WHERE slot=3 AND path=?""", (str(paths[1]).encode("utf-16-le"),))
            conn.commit()
        completed = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--viewer-worker",
                                    "--dll", str(staged), str(paths[0]), str(paths[1])],
                                   env=env, capture_output=True, text=True, timeout=30, check=True)
        report["audio_viewer"] = json.loads(completed.stdout)
        report["checks"].append("MF cached audio rejection")
    if stress:
        large = work / "large.sqlite3"
        run(large)
        with closing(sqlite3.connect(large)) as conn:
            count = conn.execute("SELECT count(*) FROM entries").fetchone()[0]
            conn.execute("""WITH RECURSIVE n(i) AS
                            (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?)
                            INSERT INTO entries(path,slot,size,modified,version,kind,sharpness)
                            SELECT CAST(printf('%0199d',i) AS BLOB),1,1,zeroblob(16),1,4,42 FROM n""",
                         (500001 - count,))
            conn.commit()
        large_bytes = large.stat().st_size
        env = dict(os.environ, MEDIARES_CONTENT_CACHE=str(large), LOCALAPPDATA=str(work / "local"))
        command = [sys.executable, str(Path(__file__).resolve()), "--quick-worker", "--dll", str(dll), str(paths[0])]
        fast = subprocess.run(command, env=env, capture_output=True, text=True, timeout=30, check=True)
        with closing(sqlite3.connect(large)) as conn:
            assert conn.execute("SELECT count(*) FROM entries").fetchone()[0] == 500001, "A quick read pruned the database"
        command = [sys.executable, str(Path(__file__).resolve()), "--worker", "--dll", str(dll), *map(str, paths)]
        workers = [subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                   for _ in range(2)]
        for process in workers:
            stdout, stderr = process.communicate(timeout=30)
            assert process.returncode == 0, (stdout, stderr)
            assert json.loads(stdout)["values"] == modified["values"]
        assert run(large)["values"] == modified["values"]
        with closing(sqlite3.connect(large)) as conn:
            remaining = conn.execute("SELECT count(*) FROM entries").fetchone()[0]
            assert remaining <= 500000
            assert conn.execute("PRAGMA quick_check").fetchone()[0] == "ok"
            assert conn.execute("PRAGMA page_count").fetchone()[0] * conn.execute("PRAGMA page_size").fetchone()[0] <= 256 * 1024 * 1024
            # Retain timings, not hundreds of MiB of synthetic records.
            conn.execute("DELETE FROM entries")
            conn.commit()
            conn.execute("VACUUM")
        report["large_cache"] = dict(json.loads(fast.stdout), bytes=large_bytes, rows_before=500001,
                                     rows_after=remaining)
        report["checks"].extend(["large cache quick read", "concurrent writers", "background pruning", "database integrity"])
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
    parser.add_argument("--viewer-dll", type=Path, help="Also test MF rejection through the WLX API")
    parser.add_argument("--stress", action="store_true", help="Test 500k rows and concurrent processes")
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--quick-worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--viewer-worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("files", nargs="*", type=Path)
    args = parser.parse_args()
    if args.worker:
        worker(args.dll.resolve(), args.files)
    elif args.quick_worker:
        quick_worker(args.dll.resolve(), args.files[0])
    elif args.viewer_worker:
        viewer_worker(args.dll.resolve(), *args.files)
    else:
        run_check(args.dll.resolve(), args.baseline_dll.resolve() if args.baseline_dll else None,
                  args.viewer_dll.resolve() if args.viewer_dll else None, args.stress)
