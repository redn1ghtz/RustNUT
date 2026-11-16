"""Exercise a release EXE on an interactive Windows desktop with a fake UPS."""

import argparse
import ctypes as c
from ctypes import wintypes as w
from contextlib import closing
import json
import os
from pathlib import Path
import socket
import shutil
import struct
import subprocess
import tempfile
import threading
import time
import winreg
import zlib


user = c.WinDLL("user32", use_last_error=True)
shell = c.WinDLL("shell32", use_last_error=True)
gdi = c.WinDLL("gdi32", use_last_error=True)


def bind(dll, name, result, *args):
    function = getattr(dll, name)
    function.argtypes = args
    function.restype = result
    return function


find = bind(user, "FindWindowW", w.HWND, w.LPCWSTR, w.LPCWSTR)
item = bind(user, "GetDlgItem", w.HWND, w.HWND, c.c_int)
visible = bind(user, "IsWindowVisible", w.BOOL, w.HWND)
enabled = bind(user, "IsWindowEnabled", w.BOOL, w.HWND)
post = bind(user, "PostMessageW", w.BOOL, w.HWND, w.UINT, w.WPARAM, w.LPARAM)
send = bind(user, "SendMessageTimeoutW", w.LPARAM, w.HWND, w.UINT, w.WPARAM,
            w.LPARAM, w.UINT, w.UINT, c.POINTER(c.c_size_t))


def message(hwnd, code, wp=0, lp=0):
    assert hwnd, f"Control for message {code} is missing"
    result = c.c_size_t()
    if not send(hwnd, code, wp, lp, 2, 2000, c.byref(result)):
        raise c.WinError(c.get_last_error())
    return result.value


def wait_for(test, description, timeout=12):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        value = test()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(description)


def read(window, identifier):
    control = item(window, identifier)
    if not control:
        return ""
    text = c.create_unicode_buffer(1024)
    message(control, 0x000D, len(text), c.addressof(text))  # WM_GETTEXT
    return text.value


def viewport(window):
    rect = w.RECT()
    assert bind(user, "GetClientRect", w.BOOL, w.HWND, c.POINTER(w.RECT))(window, c.byref(rect))
    return rect.right, rect.bottom


def window_rect(window):
    rect = w.RECT()
    assert bind(user, "GetWindowRect", w.BOOL, w.HWND, c.POINTER(w.RECT))(window, c.byref(rect))
    return rect


def bounds(window):
    rect = window_rect(window)
    return rect.right - rect.left, rect.bottom - rect.top


def check_gauge_grid(window):
    rows = [[122, 123, 121], [909, 120, 124]]
    cards = [item(window, identifier) for row in rows for identifier in row]
    assert len({bounds(card) for card in cards}) == 1
    assert len({window_rect(item(window, identifier)).top for identifier in rows[0]}) == 1
    assert len({window_rect(item(window, identifier)).top for identifier in rows[1]}) == 1
    for upper, lower in zip(*rows):
        assert window_rect(item(window, upper)).left == window_rect(item(window, lower)).left


def check_footer_grid(window):
    reset, save = item(window, 227), item(window, 103)
    assert bounds(reset) == bounds(save)
    assert window_rect(reset).top == window_rect(save).top


def check_icon_resources(exe):
    kernel = c.WinDLL("kernel32")
    library = bind(kernel, "LoadLibraryExW", w.HMODULE, w.LPCWSTR, w.HANDLE, w.DWORD)(str(exe), None, 0x2)
    assert library
    try:
        find_resource = bind(kernel, "FindResourceW", w.HANDLE, w.HMODULE, c.c_void_p, c.c_void_p)
        load_resource = bind(kernel, "LoadResource", w.HANDLE, w.HMODULE, w.HANDLE)
        lock_resource = bind(kernel, "LockResource", c.c_void_p, w.HANDLE)
        size_of = bind(kernel, "SizeofResource", w.DWORD, w.HMODULE, w.HANDLE)

        def read_resource(identifier, kind):
            resource = find_resource(library, identifier, kind)
            assert resource
            return c.string_at(lock_resource(load_resource(library, resource)), size_of(library, resource))

        data = read_resource(1, 14)  # RT_GROUP_ICON
        _, kind, count = struct.unpack_from("<HHH", data)
        sizes = [data[6 + index * 14] or 256 for index in range(count)]
        assert kind == 1 and sizes == [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
        for index, width in enumerate(sizes):
            identifier = struct.unpack_from("<H", data, 6 + index * 14 + 12)[0]
            bitmap = read_resource(identifier, 3)  # RT_ICON
            header, stored_width, stored_height, _, depth = struct.unpack_from("<IIIHH", bitmap)
            assert (header, stored_width, stored_height, depth) == (40, width, width * 2, 32)
            pixels = bitmap[header : header + width * width * 4]
            colors = set(struct.iter_unpack("<BBBB", pixels))
            assert {(0x1a, 0x1e, 0x2b, 255), (0x4a, 0x6b, 0xff, 255)} <= colors
            assert any(0 < alpha < 255 for _, _, _, alpha in colors)
    finally:
        bind(kernel, "FreeLibrary", w.BOOL, w.HMODULE)(library)


class Icon(c.Structure):
    _fields_ = [("size", w.DWORD), ("hwnd", w.HWND), ("id", w.UINT), ("guid", c.c_byte * 16)]


def icon_exists(window):
    query = bind(shell, "Shell_NotifyIconGetRect", c.c_long, c.POINTER(Icon), c.POINTER(w.RECT))
    icon = Icon(c.sizeof(Icon), window, 1)
    return query(c.byref(icon), c.byref(w.RECT())) >= 0


def screenshot(window, path):
    """Capture only the app window via Win32."""
    time.sleep(0.5)
    bind(c.WinDLL("dwmapi"), "DwmFlush", c.c_long)()
    get_rect = bind(user, "GetWindowRect", w.BOOL, w.HWND, c.POINTER(w.RECT))
    print_window = bind(user, "PrintWindow", w.BOOL, w.HWND, w.HDC, w.UINT)
    dc = bind(gdi, "CreateCompatibleDC", w.HDC, w.HDC)(None)
    create = bind(gdi, "CreateDIBSection", w.HBITMAP, w.HDC, c.c_void_p, w.UINT,
                  c.POINTER(c.c_void_p), w.HANDLE, w.DWORD)
    select = bind(gdi, "SelectObject", w.HANDLE, w.HDC, w.HANDLE)
    rect = w.RECT()
    assert get_rect(window, c.byref(rect))
    width, height = rect.right - rect.left, rect.bottom - rect.top
    raw = c.c_void_p()
    size = width * height * 4
    header = struct.pack("<IiiHHIIiiII", 40, width, -height, 1, 32, 0, size, 0, 0, 0, 0)
    bitmap = create(dc, header, 0, c.byref(raw), None, 0)
    assert bitmap
    old = select(dc, bitmap)
    try:
        assert print_window(window, dc, 2)
        pixels = c.string_at(raw, size)
        rows = bytearray()
        for start in range(0, size, width * 4):
            line = pixels[start:start + width * 4]
            rgb = bytearray(width * 3)
            rgb[0::3], rgb[1::3], rgb[2::3] = line[2::4], line[1::4], line[0::4]
            rows.extend(b"\0" + rgb)

        def chunk(kind, data):
            return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

        path.write_bytes(b"\x89PNG\r\n\x1a\n"
                         + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                         + chunk(b"IDAT", zlib.compress(rows))
                         + chunk(b"IEND", b""))
    finally:
        select(dc, old)
        bind(gdi, "DeleteObject", w.BOOL, w.HANDLE)(bitmap)
        bind(gdi, "DeleteDC", w.BOOL, w.HDC)(dc)


class Fixture:
    def __init__(self, listener):
        self.listener = listener
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.state = "OL"
        self.runtime = "2479"
        self.online = True
        self.battery_available = True
        self.watts_available = True
        self.polls = 0

    def serve(self):
        self.listener.settimeout(0.4)
        while not self.stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except TimeoutError:
                continue
            with conn, conn.makefile("rb") as stream:
                conn.settimeout(4)
                try:
                    for command in stream:
                        with self.lock:
                            if not self.online or self.stop.is_set():
                                break
                            status = self.state
                            runtime = self.runtime
                            battery_available = self.battery_available
                            watts_available = self.watts_available
                        if command == b"LIST UPS\n":
                            conn.sendall(b'BEGIN LIST UPS\nUPS fixture "RustNUT Lab"\nEND LIST UPS\n')
                        elif command == b'LIST VAR "fixture"\n':
                            response = 'BEGIN LIST VAR fixture\n'
                            for key, value in {
                                "ups.status": status, "ups.model": "RustNUT Lab",
                                "battery.charge": "92", "battery.runtime": runtime,
                                "ups.load": "24",
                                "input.voltage": "230.1", "output.voltage": "229.8",
                                "input.frequency": "50.1", "device.mfr": "Fixture Electric",
                                "ups.serial": "FIXTURE-101",
                            }.items():
                                response += f'VAR fixture {key} "{value}"\n'
                            if battery_available:
                                response += 'VAR fixture battery.voltage "26,4"\n'
                            if watts_available:
                                response += 'VAR fixture ups.realpower "148"\n'
                            for index in range(16):
                                response += f'VAR fixture test.sensor.{index:02d} "{index}"\n'
                            conn.sendall((response + 'END LIST VAR fixture\n').encode("ascii"))
                            self.polls += 1
                        else:
                            raise AssertionError(f"Unexpected command {command!r}")
                except (TimeoutError, ConnectionError):
                    continue


def measure(process, seconds):
    kernel = c.WinDLL("kernel32")
    psapi = c.WinDLL("psapi")
    handle = bind(kernel, "OpenProcess", w.HANDLE, w.DWORD, w.BOOL, w.DWORD)(0x0410, False, process.pid)
    assert handle
    times = bind(kernel, "GetProcessTimes", w.BOOL, w.HANDLE, *[c.POINTER(w.FILETIME)] * 4)
    usage = bind(psapi, "GetProcessMemoryInfo", w.BOOL, w.HANDLE, c.c_void_p, w.DWORD)

    class Memory(c.Structure):
        _fields_ = [("size", w.DWORD), ("faults", w.DWORD), ("peak_ws", c.c_size_t),
                    ("ws", c.c_size_t)] + [(f"field{i}", c.c_size_t) for i in range(6)] + [("private", c.c_size_t)]

    def cpu():
        values = [w.FILETIME() for _ in range(4)]
        assert times(handle, *(c.byref(v) for v in values))
        return sum((v.dwHighDateTime << 32) + v.dwLowDateTime for v in values[2:]) * 1e-7

    try:
        assert process.poll() is None
        start, before = time.perf_counter(), cpu()
        working, private = [], []
        while time.perf_counter() - start < seconds:
            memory = Memory(c.sizeof(Memory))
            assert usage(handle, c.byref(memory), memory.size)
            working.append(memory.ws)
            private.append(memory.private)
            time.sleep(0.5)
        cpu_percent = 100 * (cpu() - before) / (time.perf_counter() - start)
        return f"{max(working) / 2**20:.2f} MiB working set, {max(private) / 2**20:.2f} MiB private, {cpu_percent:.3f}% of one logical core"
    finally:
        bind(kernel, "CloseHandle", w.BOOL, w.HANDLE)(handle)


def startup_value():
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Run") as key:
            return winreg.QueryValueEx(key, "RustNUT")
    except FileNotFoundError:
        return None


def run(exe, samples, images, require_tray):
    assert not find("RustNUT.Window", None), "Close the running RustNUT instance before testing"
    check_icon_resources(exe)
    if images:
        images.mkdir(parents=True, exist_ok=True)
    with (
        tempfile.TemporaryDirectory(prefix="smoke-", dir=Path("target").resolve()) as root,
        closing(socket.socket()) as listener,
    ):
        root = Path(root)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        fixture = Fixture(listener)
        thread = threading.Thread(target=fixture.serve, daemon=True)
        thread.start()
        settings = root / "portable" / "settings.json"
        settings.parent.mkdir()
        staged = settings.parent / exe.name
        shutil.copy2(exe, staged)
        working = root / "working-directory"
        working.mkdir()
        settings.write_text(json.dumps({
            "host": "127.0.0.1", "port": listener.getsockname()[1], "transport": "Plain",
            "ups": "fixture", "poll": 2, "notifications": True,
        }), encoding="utf-8")
        env = dict(os.environ, LOCALAPPDATA=str(root / "profile"))
        app = subprocess.Popen([staged, "--background"], env=env, cwd=working)
        try:
            hwnd = wait_for(lambda: find("RustNUT.Window", None), "background window")
            assert not visible(hwnd)
            if require_tray:
                wait_for(lambda: icon_exists(hwnd), "tray icon")
            wait_for(lambda: fixture.polls >= 1, "background polling")
            if samples:
                print("Started in tray:", measure(app, samples))
            app2 = subprocess.run([staged], env=env, cwd=working, timeout=4)
            assert app2.returncode == 0
            wait_for(lambda: visible(hwnd) and read(hwnd, 110) == "On utility power", "single-instance open and telemetry")
            check_gauge_grid(hwnd)
            assert read(hwnd, 909).splitlines()[1] == "92 %"
            assert read(hwnd, 123).splitlines()[1] == "229.8 V"
            assert read(hwnd, 121).splitlines()[1] == "50.1 Hz"
            assert read(hwnd, 124).splitlines()[1] == "26.4 V"
            assert read(hwnd, 120).splitlines()[2] == "148 W"
            assert read(hwnd, 929) == "Runtime  00:41:19"
            with fixture.lock:
                fixture.runtime = "90123"
            wait_for(lambda: read(hwnd, 929) == "Runtime  25:02:03", "runtime longer than one day")
            with fixture.lock:
                fixture.runtime = ""
            wait_for(lambda: read(hwnd, 929) == "Runtime  No data", "runtime unavailable")
            with fixture.lock:
                fixture.runtime = "800"
            wait_for(lambda: read(hwnd, 929) == "Runtime  00:13:20", "runtime with zero hours")
            assert read(hwnd, 263) == ""
            assert "Fixture Electric" in read(hwnd, 934) and "FIXTURE-101" in read(hwnd, 934)
            with fixture.lock:
                fixture.battery_available = False
            wait_for(lambda: read(hwnd, 124).splitlines()[1] == "No data", "driver without battery voltage")
            with fixture.lock:
                fixture.battery_available = True
            wait_for(lambda: read(hwnd, 124).splitlines()[1] == "26.4 V", "battery voltage returns")
            listbox = item(hwnd, 112)
            assert message(listbox, 0x018B) == 28  # LB_GETCOUNT
            message(listbox, 0x020A, 0xff88 << 16)  # wheel down
            assert message(listbox, 0x018E) == 3  # LB_GETTOPINDEX
            with fixture.lock:
                fixture.runtime = "801"
            wait_for(lambda: read(hwnd, 929) == "Runtime  00:13:21", "runtime refreshed while scrolled")
            assert message(listbox, 0x018E) == 3
            bar = item(hwnd, 933)
            message(listbox, 0x0197)  # LB_SETTOPINDEX
            message(bar, 0x0201, 1, 8 << 16 | 8)
            message(bar, 0x0200, 1, (viewport(bar)[1] - 9) << 16 | 8)
            message(bar, 0x0202, 0, (viewport(bar)[1] - 9) << 16 | 8)
            assert message(listbox, 0x018E) > 3
            message(listbox, 0x0197)
            message(listbox, 0x0100, 0x23)  # End
            assert message(listbox, 0x018E) > 0
            message(listbox, 0x0197)  # LB_SETTOPINDEX
            initial_list = viewport(listbox)
            original = bounds(hwnd)
            resize = bind(user, "SetWindowPos", w.BOOL, w.HWND, w.HWND,
                          c.c_int, c.c_int, c.c_int, c.c_int, w.UINT)
            assert resize(hwnd, None, 0, 0, original[0] + 145, original[1] + 85, 0x16)
            assert all(a > b for a, b in zip(viewport(listbox), initial_list))
            if original[0] >= 860 and original[1] >= 775:
                assert resize(hwnd, None, 0, 0, original[0] - 125, original[1] - 45, 0x16)
                assert all(a < b for a, b in zip(viewport(listbox), initial_list))
                check_gauge_grid(hwnd)
                if images:
                    screenshot(hwnd, images / "compact-light.png")
            assert resize(hwnd, None, 0, 0, *original, 0x16)
            style = bind(user, "GetWindowLongW", c.c_long, w.HWND, c.c_int)
            assert not item(hwnd, 264)
            assert post(hwnd, 0x0100, 0x7A, 0)  # F11
            wait_for(lambda: style(hwnd, -16) & 0x80000000, "fullscreen")
            assert all(a > b for a, b in zip(viewport(listbox), initial_list))
            if images:
                screenshot(hwnd, images / "fullscreen-light.png")
            assert post(hwnd, 0x0100, 0x1B, 0)  # Escape
            wait_for(lambda: style(hwnd, -16) & 0x00C00000, "restore from fullscreen")
            assert viewport(listbox) == initial_list
            if images:
                screenshot(hwnd, images / "dashboard-light.png")
            if samples:
                print("Window open:", measure(app, samples))
            message(hwnd, 0x0010)  # WM_CLOSE minimizes to tray
            assert not visible(hwnd) and app.poll() is None
            if require_tray:
                assert icon_exists(hwnd)
            if samples:
                print("Minimized to tray:", measure(app, samples))
            with fixture.lock:
                fixture.watts_available = False
                polls = fixture.polls
            wait_for(lambda: fixture.polls >= polls + 2, "telemetry changed while hidden")
            message(hwnd, 0x8003)  # OPEN
            wait_for(lambda: read(hwnd, 120).splitlines()[2] == "No data", "hidden telemetry is shown on reopen")
            with fixture.lock:
                fixture.watts_available = True
                fixture.state = "OB LB"
            wait_for(lambda: read(hwnd, 110) == "Battery low", "on-battery update")
            with fixture.lock:
                fixture.online = False
            wait_for(lambda: read(hwnd, 110) == "Disconnected", "disconnect")
            assert all(read(hwnd, key).splitlines()[1] == "No data" for key in [120, 121, 122, 123, 124, 909])
            with fixture.lock:
                fixture.online = True
                fixture.state = "OL"
            wait_for(lambda: read(hwnd, 110) == "On utility power", "automatic reconnect")

            # Save from the real settings page without changing the startup preference.
            before = startup_value()
            message(hwnd, 0x0111, 101)
            wait_for(lambda: item(hwnd, 211), "settings page")
            assert message(item(hwnd, 211), 0x0146) == 3  # CB_GETCOUNT
            check_footer_grid(hwnd)
            assert read(hwnd, 266) == "GitHub · redn1ghtz"
            if images:
                screenshot(hwnd, images / "settings-light.png")
            assert message(item(hwnd, 210), 0x0146) == 2  # CB_GETCOUNT
            assert not enabled(item(hwnd, 205))
            message(hwnd, 0x0111, 261)
            assert enabled(item(hwnd, 214)) and not enabled(item(hwnd, 219))
            assert not enabled(item(hwnd, 268)) and not enabled(item(hwnd, 269))
            assert read(hwnd, 267) == "Power actions affect this Windows PC."
            message(item(hwnd, 214), 0x00F5)  # BM_CLICK
            assert enabled(item(hwnd, 219))
            zero = c.create_unicode_buffer("0")
            for control in (208, 209, 219):
                message(item(hwnd, control), 0x000C, 0, c.addressof(zero))
            message(item(hwnd, 268), 0x00F5)
            assert all(not enabled(item(hwnd, control)) for control in (208, 209, 219))
            message(item(hwnd, 224), 0x00F5)
            for action, label in (
                (1, "Sleep on battery power"),
                (2, "Hibernate on battery power"),
                (0, "Shut down PC on battery power"),
            ):
                message(item(hwnd, 220), 0x014E, action)
                message(hwnd, 0x0111, 0x10000 | 220)
                assert read(hwnd, 268) == label
                assert bool(enabled(item(hwnd, 269))) == (action == 0)
            assert message(item(hwnd, 269), 0x00F0) == 0  # BM_GETCHECK
            message(item(hwnd, 269), 0x00F5)
            assert post(hwnd, 0x0111, 103, 0)  # Save opens a modal confirmation.
            dialog = wait_for(lambda: find("#32770", "RustNUT"), "plain TCP power confirmation")
            assert post(dialog, 0x0111, 7, item(dialog, 7))  # IDNO
            wait_for(lambda: not find("#32770", "RustNUT"), "rejected confirmation closed")
            assert not json.loads(settings.read_text(encoding="utf-8")).get("shutdown", False)
            assert post(hwnd, 0x0111, 103, 0)
            dialog = wait_for(lambda: find("#32770", "RustNUT"), "second plain TCP power confirmation")
            assert post(dialog, 0x0111, 6, item(dialog, 6))  # IDYES with fixture on utility power
            wait_for(lambda: read(hwnd, 110) == "On utility power", "TCP power settings saved")
            saved = json.loads(settings.read_text(encoding="utf-8"))
            assert saved["shutdown"] is True and saved["charge"] == saved["battery_seconds"] == 0
            power = saved["power"]
            assert power["on_battery"] and power["immediate"] and power["force_close"]
            assert power["action"] == "Shutdown" and power["runtime"] == 0
            message(hwnd, 0x0111, 101)
            message(hwnd, 0x0111, 261)
            for control in (268, 224, 269):
                assert message(item(hwnd, control), 0x00F0) == 1
                message(item(hwnd, control), 0x00F5)
            message(item(hwnd, 214), 0x00F5)
            message(hwnd, 0x0111, 103)
            wait_for(lambda: read(hwnd, 110) == "On utility power", "TCP power actions disabled again")
            assert json.loads(settings.read_text(encoding="utf-8"))["shutdown"] is False
            print("Pass: battery-only trigger and forced shutdown opt-in, with fixture on utility power.")
            message(hwnd, 0x0111, 101)
            message(item(hwnd, 204), 0x014E, 0)
            message(hwnd, 0x0111, 0x10000 | 204)
            assert enabled(item(hwnd, 205))
            message(item(hwnd, 211), 0x014E, 1)  # CB_SETCURSEL
            message(item(hwnd, 210), 0x014E, 0)
            message(hwnd, 0x0111, 261)
            assert visible(item(hwnd, 219)) and not visible(item(hwnd, 210))
            assert not enabled(item(hwnd, 219))
            message(item(hwnd, 214), 0x00F5)  # BM_CLICK
            assert enabled(item(hwnd, 219))
            assert not enabled(item(hwnd, 223))
            message(item(hwnd, 222), 0x00F5)
            assert enabled(item(hwnd, 223))
            message(item(hwnd, 224), 0x00F5)
            assert not enabled(item(hwnd, 221)) and not enabled(item(hwnd, 223))
            message(item(hwnd, 224), 0x00F5)
            message(item(hwnd, 214), 0x00F5)
            runtime = c.create_unicode_buffer("120")
            message(item(hwnd, 219), 0x000C, 0, c.addressof(runtime))
            message(item(hwnd, 220), 0x014E, 2)  # Hibernate, with automatic actions disabled
            message(hwnd, 0x0111, 260)
            message(item(hwnd, 204), 0x014E, 2)
            message(hwnd, 0x0111, 0x10000 | 204)
            message(hwnd, 0x0111, 262)
            assert visible(item(hwnd, 240)) and not visible(item(hwnd, 219))
            assert all(read(hwnd, index) for index in range(240, 250))
            assert enabled(item(hwnd, 240))
            message(item(hwnd, 265), 0x00F5)
            assert not enabled(item(hwnd, 240)) and not enabled(item(hwnd, 226))
            assert read(hwnd, 240) == "90"
            message(item(hwnd, 265), 0x00F5)
            assert enabled(item(hwnd, 240))
            maximum = c.create_unicode_buffer("220,0")
            message(item(hwnd, 241), 0x000C, 0, c.addressof(maximum))
            message(item(hwnd, 226), 0x014E, 1)
            message(hwnd, 0x0111, 103)
            wait_for(lambda: read(hwnd, 110) == "Питание от сети", "saved Russian dashboard")
            assert read(hwnd, 929) == "Автономность  00:13:21"
            assert read(hwnd, 263) == "Вне диапазона · 1"
            saved = json.loads(settings.read_text(encoding="utf-8"))
            assert saved["russian"] and saved["dark"]
            assert saved["power"]["runtime"] == 120 and saved["power"]["action"] == "Hibernate"
            assert saved["limits"]["nominal_hz"] == 50 and saved["limits"]["ranges"][0]["max"] == 220
            assert not (working / "settings.json").exists() and not (root / "profile").exists()
            assert startup_value() == before
            with fixture.lock:
                fixture.battery_available = False
                fixture.watts_available = False
                fixture.state = "OB"
            wait_for(lambda: read(hwnd, 110) == "Питание от батареи", "Russian battery event")
            assert read(hwnd, 124).splitlines()[1] == "Нет данных"
            assert read(hwnd, 120).splitlines()[2] == "Нет данных"
            with fixture.lock:
                fixture.state = "OL"
            if images:
                screenshot(hwnd, images / "dashboard-dark.png")
                screenshot(hwnd, images / "missing-battery-dark.png")
                message(hwnd, 0x0111, 101)
                check_footer_grid(hwnd)
                screenshot(hwnd, images / "settings-dark.png")
                message(hwnd, 0x0111, 261)
                screenshot(hwnd, images / "power-dark.png")
                message(hwnd, 0x0111, 262)
                screenshot(hwnd, images / "limits-dark.png")
            message(hwnd, 0x0111, 218)  # Exit command from the tray menu
            assert app.wait(timeout=5) == 0
            assert not find("RustNUT.Window", None)
            app = subprocess.Popen([staged], env=env, cwd=working)
            hwnd = wait_for(lambda: find("RustNUT.Window", None), "restart")
            wait_for(lambda: read(hwnd, 110) == "Питание от сети", "persisted Russian language after restart")
            message(hwnd, 0x0111, 101)
            assert message(item(hwnd, 211), 0x0147) == 1  # CB_GETCURSEL
            message(hwnd, 0x0111, 261)
            assert read(hwnd, 268) == "Гибернация при работе от батареи"
            message(item(hwnd, 220), 0x014E, 0)
            message(hwnd, 0x0111, 0x10000 | 220)
            assert read(hwnd, 268) == "Выключать ПК при работе от батареи"
            message(hwnd, 0x0111, 260)
            message(item(hwnd, 211), 0x014E, 2)  # System theme
            message(hwnd, 0x0111, 103)
            wait_for(lambda: read(hwnd, 110) == "Питание от сети", "system theme saved")
            assert json.loads(settings.read_text(encoding="utf-8"))["system_theme"] is True
            message(hwnd, 0x0111, 101)
            assert message(item(hwnd, 211), 0x0147) == 2
            if before is None:
                assert post(hwnd, 0x0111, 227, 0)
                dialog = wait_for(lambda: find("#32770", "RustNUT"), "reset confirmation")
                assert post(dialog, 0x0111, 6, item(dialog, 6))  # IDYES
                wait_for(lambda: not settings.exists() and read(hwnd, 201) == "", "portable settings reset")
                assert message(item(hwnd, 211), 0x0147) == 2
                assert startup_value() is None
                print("Pass: confirmed reset removed portable settings and restored defaults.")
            else:
                print("Reset skipped because RustNUT has a real Windows startup entry.")
            message(hwnd, 0x0111, 218)
            assert app.wait(timeout=5) == 0
            print("Pass: PE icon sizes, grid alignment, startup, six gauges, scrolling, resize, F11/Escape, limits and reconnect.")
            if require_tray:
                print("Pass: Explorer tray icon registered with the window open and hidden.")
        finally:
            if app.poll() is None:
                app.kill()
                app.wait(timeout=5)
            fixture.stop.set()
            thread.join(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("exe", type=lambda value: Path(value).resolve(strict=True))
    parser.add_argument("--samples", type=float, default=0, help="seconds of resource sampling per mode")
    parser.add_argument("--images", type=Path, help="optional directory for window captures")
    parser.add_argument("--skip-tray", action="store_true", help="for a desktop without an Explorer taskbar")
    args = parser.parse_args()
    run(args.exe, args.samples, args.images, not args.skip_tray)
