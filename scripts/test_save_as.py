"""Native regression check; build the lister_harness example first."""
import ctypes as c
import os
import subprocess
import time
from ctypes import wintypes as w
from pathlib import Path
import argparse
import shutil
import sys
import uuid
sys.dont_write_bytecode = True
from test_content_cache import make_bmp

def main():
    parser = argparse.ArgumentParser(description='Exercise the real Save As dialog and decline replacement of the final filename.')
    parser.add_argument('--harness', type=Path, default=Path(__file__).resolve().parent.parent / 'target/debug/examples/lister_harness.exe')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent / 'target' / ('save-as-check-' + uuid.uuid4().hex)
    root.mkdir(parents=True)
    shutil.copy2(args.harness, root / 'lister_harness.exe')
    (root / 'mediares.ini').write_text('[Settings]\nLanguage=en\nStartFullscreen=0\n', encoding='utf-8')
    source = root / 'source.bmp'
    make_bmp(source)
    env = dict(os.environ, LOCALAPPDATA=str(root / 'local'), MEDIARES_CONTENT_CACHE='off')
    proc = subprocess.Popen([str(root / 'lister_harness.exe'), str(source), str(root / 'shots'), 'wait:2000', 'ctrl-key:53', 'wait:300'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    u = c.WinDLL('user32', use_last_error=True)
    u.EnumWindows.argtypes = [c.c_void_p, w.LPARAM]
    u.EnumChildWindows.argtypes = [w.HWND, c.c_void_p, w.LPARAM]
    u.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
    u.GetClassNameW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
    u.GetWindowTextW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
    u.IsWindowVisible.argtypes = [w.HWND]
    u.GetDlgCtrlID.argtypes = [w.HWND]
    u.SendMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
    u.SendMessageW.restype = w.LPARAM
    u.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
    CB = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
    WM_SETTEXT, WM_COMMAND, WM_CLOSE = 0x000C, 0x0111, 0x0010
    IDOK, IDNO = 1, 7

    def name(h):
        b = c.create_unicode_buffer(1024)
        u.GetWindowTextW(h, b, 1024)
        return b.value

    def cls(h):
        b = c.create_unicode_buffer(100)
        u.GetClassNameW(h, b, 100)
        return b.value

    def dialogs():
        out = []

        @CB
        def visit(h, p):
            pid = w.DWORD()
            u.GetWindowThreadProcessId(h, c.byref(pid))
            if pid.value == proc.pid and u.IsWindowVisible(h) and (cls(h) == '#32770'):
                out.append(h)
            return True
        u.EnumWindows(visit, 0)
        return out
    try:
        deadline = time.monotonic() + 10
        save = None
        while time.monotonic() < deadline:
            ds = dialogs()
            if ds:
                save = ds[0]
                break
            time.sleep(0.05)
        assert save, 'Save dialog not found'
        time.sleep(0.3)
        children = []

        @CB
        def child(h, p):
            if cls(h) in ('Edit', 'ComboBox', 'ComboBoxEx32'):
                children.append((h, u.GetDlgCtrlID(h), cls(h), name(h)))
            return True
        u.EnumChildWindows(save, child, 0)
        target = root / 'overwrite.backup.jpg'
        target.write_bytes(b'original file')
        entered = c.create_unicode_buffer(str(root / 'overwrite.backup'))
        filename = next((h for h, i, cl, t in children if i == 1001 and cl == 'Edit'))
        u.SendMessageW(filename, WM_SETTEXT, 0, c.addressof(entered))
        u.PostMessageW(save, WM_COMMAND, IDOK, 0)
        deadline = time.monotonic() + 5
        confirm = None
        while time.monotonic() < deadline:
            for h in dialogs():
                if h != save:
                    confirm = h
                    break
            if confirm:
                break
            time.sleep(0.05)
        assert confirm, 'Final overwrite confirmation missing'
        texts = []

        @CB
        def text_child(h, p):
            texts.append(name(h))
            return True
        u.EnumChildWindows(confirm, text_child, 0)
        assert any((str(target) in t for t in texts)), 'Confirmation must show final filename'
        u.PostMessageW(confirm, WM_COMMAND, IDNO, 0)
        stdout, stderr = proc.communicate(timeout=10)
        assert target.read_bytes() == b'original file', 'Declining replaced the file'
        assert proc.returncode == 0, (stdout, stderr)
        (root / 'harness.log').write_text(stdout + stderr, encoding='utf-8')
        print('Save As: final-name confirmation and declined overwrite passed')
        print('Artifacts:', root)
    finally:
        if proc.poll() is None:
            for h in dialogs():
                u.PostMessageW(h, WM_CLOSE, 0, 0)
            try:
                proc.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                proc.terminate()
                proc.communicate()


if __name__ == "__main__":
    main()
