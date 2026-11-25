"""First run of the installed Windows desktop on a hosted guest: the practice page, a
registered hotkey and delivery into test-owned native editors.

The installed product runs unmodified. The desktop host is started through its Start menu
entry and starts its own daemon; everything else is done to the console the daemon serves in
the desktop window. Speech is the reference clip that ships with the pinned SenseVoice model,
played into the first test line, which is selected as the microphone. A second editor window
stands for "another window" in the delivery guard. Evidence is printed and kept under RUNNER_TEMP.
"""
import argparse
import array
import audioop
import contextlib
import ctypes
from ctypes import wintypes
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import wave

import sounddevice as sd
from playwright.sync_api import TimeoutError as PwTimeout
from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
BRANCH = 'ci/windows-candidate/setup-feedback-0.2.0'
CDP_PORT = 9333
MODEL, VAD = 'sense-voice-int8', 'silero-vad'
# test_wavs/en.wav inside the pinned model archive.
SPEECH_SHA256 = 'eb1eb008904465b74c304aad8342e8c7d3c6e61ffe9f66adcaca9cf0f76a93f4'
INSTALL = ['/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART']
SHORTCUT = Path('Microsoft/Windows/Start Menu/Programs/Voiceless.lnk')
PREFIX = 'Voiceless practice editor A: '
KEY, KEY_CODE, VK_KEY, SCAN_KEY = 'KEY_SCROLLLOCK', 'ScrollLock', 0x91, 0x46
EDITED = ' [edited in the preview]'


def either(*names):
    """A label in either interface language: the console follows the browser's."""
    return re.compile('^(?:' + '|'.join(re.escape(name) for name in names) + ')$')


UI = dict(
    setup=either('首次试说', 'First phrase'),
    shortcuts=either('快捷键与上屏', 'Shortcuts and typing'),
    input=either('输入设备', 'Input device'),
    recognition=either('识别方式', 'Recognition'),
    model=either('本地模型', 'Local model'),
    get_models=either('准备模型', 'Get the models'),
    apply=either('应用设置', 'Apply settings'),
    apply_dialog=either('应用', 'Apply'),
    start=either('开始试说', 'Try a phrase'),
    arm=either('用快捷键试说', 'Try with the shortcut'),
    finish=either('完成试说', 'Finish the practice'),
    test=either('测试上屏', 'Test typing'),
    result=either('练习结果', 'Practice result'),
    record_toggle=either('录制点按切换快捷键', 'Record the Tap to toggle shortcut'),
    save=either('保存', 'Save'),
)
READY = re.compile(r'识别就绪|Recognition ready')
ARMED = re.compile(r'现在按下快捷键|Press the shortcut now')
NO_KEY = re.compile(r'等待快捷键超时。.*开始试说|Timed out waiting for the shortcut\. .*Try a phrase')
SWITCH = re.compile(r'3 秒内切换|Switch to the field')
ACTIVE = re.compile(r'ScrollLock\s+(?:已生效|active)')
METHOD = re.compile(r'剪贴板|Clipboard')


def record(step, **facts):
    print(f'{step} ' + json.dumps(facts, ensure_ascii=False), flush=True)


def check(condition, message, **facts):
    if not condition:
        raise RuntimeError(f'{message}: ' + json.dumps(facts, ensure_ascii=False, default=str))


def wait_for(probe, description, timeout=30, interval=0.2):
    deadline = time.monotonic() + timeout
    while True:
        value = probe()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise RuntimeError(f'Timed out waiting for {description}')
        time.sleep(interval)


def norm(text):
    return text.replace('\r\n', '\n').replace('\r', '\n')


def agrees(text, reference):
    """The reference words in order, or at least five of them in a row (as meeting.py)."""
    mine, theirs = re.findall(r'[a-z]+', text.lower()), re.findall(r'[a-z]+', reference.lower())
    return mine == theirs or (len(theirs) >= 5 and any(
        ' '.join(theirs[i:i + 5]) in ' '.join(mine) for i in range(len(theirs) - 4)))


# ---------------------------------------------------------------- Windows

user32 = ctypes.WinDLL('user32', use_last_error=True)
kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
LPARAM, WPARAM, LRESULT = ctypes.c_ssize_t, ctypes.c_size_t, ctypes.c_ssize_t
HOOKPROC = ctypes.WINFUNCTYPE(LRESULT, ctypes.c_int, WPARAM, LPARAM)
WH_KEYBOARD_LL, HC_ACTION, WM_QUIT, PM_NOREMOVE = 13, 0, 0x0012, 0
LLKHF_INJECTED = 0x10
INPUT_KEYBOARD, KEYEVENTF_KEYUP = 1, 0x0002
MOD_SHIFT, MOD_NOREPEAT = 0x0004, 0x4000
VK_MENU, SCAN_MENU, VK_F7 = 0x12, 0x38, 0x76


class KBDLLHOOKSTRUCT(ctypes.Structure):
    _fields_ = [('vkCode', wintypes.DWORD), ('scanCode', wintypes.DWORD), ('flags', wintypes.DWORD),
                ('time', wintypes.DWORD), ('dwExtraInfo', ctypes.c_size_t)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [('wVk', wintypes.WORD), ('wScan', wintypes.WORD), ('dwFlags', wintypes.DWORD),
                ('time', wintypes.DWORD), ('dwExtraInfo', ctypes.c_size_t)]


class INPUTUNION(ctypes.Union):
    # The mouse variant is the largest member; its size is the union's on x64.
    _fields_ = [('ki', KEYBDINPUT), ('pad', ctypes.c_byte * 32)]


class INPUT(ctypes.Structure):
    _anonymous_ = ('u',)
    _fields_ = [('type', wintypes.DWORD), ('u', INPUTUNION)]


user32.GetForegroundWindow.restype = wintypes.HWND
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.SetForegroundWindow.restype = wintypes.BOOL
user32.FindWindowExW.argtypes = [wintypes.HWND, wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR]
user32.FindWindowExW.restype = wintypes.HWND
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.GetWindowThreadProcessId.restype = wintypes.DWORD
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.RegisterHotKey.argtypes = [wintypes.HWND, ctypes.c_int, wintypes.UINT, wintypes.UINT]
user32.RegisterHotKey.restype = wintypes.BOOL
user32.UnregisterHotKey.argtypes = [wintypes.HWND, ctypes.c_int]
user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.SetWindowsHookExW.argtypes = [ctypes.c_int, HOOKPROC, wintypes.HINSTANCE, wintypes.DWORD]
user32.SetWindowsHookExW.restype = wintypes.HHOOK
user32.CallNextHookEx.argtypes = [wintypes.HHOOK, ctypes.c_int, WPARAM, LPARAM]
user32.CallNextHookEx.restype = LRESULT
user32.GetMessageW.argtypes = [ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT, wintypes.UINT]
user32.GetMessageW.restype = wintypes.BOOL
user32.PeekMessageW.argtypes = [ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT, wintypes.UINT, wintypes.UINT]
user32.PostThreadMessageW.argtypes = [wintypes.DWORD, wintypes.UINT, WPARAM, LPARAM]
user32.UnhookWindowsHookEx.argtypes = [wintypes.HHOOK]
kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
kernel32.GetModuleHandleW.restype = wintypes.HMODULE
kernel32.GetCurrentThreadId.restype = wintypes.DWORD


def send_keystrokes(events):
    """`events`: (virtual key, scan code, released) in order, through SendInput."""
    inputs = (INPUT * len(events))()
    for slot, (vk, scan, up) in zip(inputs, events, strict=True):
        slot.type = INPUT_KEYBOARD
        slot.ki = KEYBDINPUT(vk, scan, KEYEVENTF_KEYUP if up else 0, 0, 0)
    sent = user32.SendInput(len(events), inputs, ctypes.sizeof(INPUT))
    if sent != len(events):
        raise OSError(ctypes.get_last_error(), f'SendInput delivered {sent} of {len(events)} keystrokes')


def tap(vk, scan):
    send_keystrokes([(vk, scan, False)])
    time.sleep(0.08)
    send_keystrokes([(vk, scan, True)])


class KeyWatch:
    """A low-level keyboard hook of this process, like the daemon's own: what it saw."""

    def __init__(self):
        self.events = []
        self.thread = None
        self.thread_id = 0
        self.ready = threading.Event()

    def __enter__(self):
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        if not self.ready.wait(5):
            raise RuntimeError('the keyboard hook did not install')
        return self

    def _serve(self):
        def hook(code, wparam, lparam):
            if code == HC_ACTION:
                event = KBDLLHOOKSTRUCT.from_address(lparam)
                self.events.append((event.vkCode, event.flags))
            return user32.CallNextHookEx(None, code, wparam, lparam)
        callback = HOOKPROC(hook)  # kept alive by the frame while the hook is installed
        message = wintypes.MSG()
        user32.PeekMessageW(ctypes.byref(message), None, 0, 0, PM_NOREMOVE)
        handle = user32.SetWindowsHookExW(WH_KEYBOARD_LL, callback, kernel32.GetModuleHandleW(None), 0)
        if not handle:
            return
        self.thread_id = kernel32.GetCurrentThreadId()
        self.ready.set()
        while user32.GetMessageW(ctypes.byref(message), None, 0, 0) > 0:
            pass
        user32.UnhookWindowsHookEx(handle)

    def __exit__(self, *_):
        user32.PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0)
        self.thread.join(5)


def hotkey_registration():
    """What RegisterHotKey answers this process for the daemon's combination and for others."""
    def attempt(modifiers, vk):
        ctypes.set_last_error(0)
        ok = user32.RegisterHotKey(None, 0x7FFF, modifiers, vk)
        error = 0 if ok else ctypes.get_last_error()
        if ok:
            user32.UnregisterHotKey(None, 0x7FFF)
        return {'ok': bool(ok), 'error': error}
    return {
        'same_combination': attempt(MOD_NOREPEAT, VK_KEY),
        'same_key_other_modifier': attempt(MOD_NOREPEAT | MOD_SHIFT, VK_KEY),
        'other_key': attempt(MOD_NOREPEAT, VK_F7),
    }


def window_of(process_id, timeout=90):
    """The visible top-level window titled Voiceless that `process_id` owns."""
    def find():
        after = None
        while True:
            window = user32.FindWindowExW(None, after, None, 'Voiceless')
            if not window:
                return None
            owner = wintypes.DWORD()
            user32.GetWindowThreadProcessId(window, ctypes.byref(owner))
            if owner.value == process_id and user32.IsWindowVisible(window):
                return window
            after = window
    return wait_for(find, f'the Voiceless window of process {process_id}', timeout, 0.3)


def foreground():
    """The window in front, as a handle."""
    return user32.GetForegroundWindow()


# ------------------------------------------------------------ the product

def powershell(script, env, timeout=120):
    prefix = "$ProgressPreference = 'SilentlyContinue'; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; "
    result = subprocess.run(['pwsh', '-NoProfile', '-NonInteractive', '-Command', prefix + script],
                            capture_output=True, text=True, encoding='utf-8', env=env, timeout=timeout)
    if result.returncode != 0:
        raise RuntimeError(f'pwsh ({result.returncode}): {result.stderr.strip()[-1500:]}')
    return result.stdout.strip()


def processes(env):
    query = ("Get-CimInstance Win32_Process -Filter \"Name='voiceless.exe' OR Name='voiceless-desktop.exe'\" | "
             'Select-Object ProcessId, ParentProcessId, Name, CommandLine | ConvertTo-Json -Compress')
    out = powershell(query, env)
    items = json.loads(out) if out else []
    return [item for item in (items if isinstance(items, list) else [items]) if item]


def install(candidates, kind, version, destination, env):
    setup = candidates / f'voiceless-{kind}-v{version}-windows-x64.exe'
    if not setup.is_file():
        raise FileNotFoundError(setup)
    subprocess.run([str(setup), *INSTALL, f'/DIR={destination}'], check=True, env=env, timeout=240)


class Console:
    """The daemon's loopback API, with the token its console page is served with."""

    def __init__(self, data_home):
        self.descriptor = Path(data_home) / 'voiceless' / 'console-endpoint.json'
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.endpoint = None

    def ready(self):
        try:
            endpoint = json.loads(self.descriptor.read_text(encoding='utf-8'))
            request = urllib.request.Request(f"http://{endpoint['address']}/api/status",
                                             headers={'X-Voiceless-Token': endpoint['token']})
            with self.opener.open(request, timeout=5) as reply:
                status = json.load(reply)
        except (OSError, ValueError, KeyError):
            return False
        if status.get('runtime_id') != endpoint.get('runtime_id'):
            return False
        self.endpoint = endpoint
        return True

    def call(self, path, body=None, expected=200):
        if self.endpoint is None:
            raise RuntimeError('the console is not ready')
        headers = {'X-Voiceless-Token': self.endpoint['token']}
        data = None
        if body is not None:
            headers['Content-Type'] = 'application/json'
            data = json.dumps(body).encode()
        request = urllib.request.Request(f"http://{self.endpoint['address']}/{path}", headers=headers, data=data)
        try:
            reply = self.opener.open(request, timeout=60)
        except urllib.error.HTTPError as error:
            reply = error
        with reply:
            raw = reply.read()
            payload = json.loads(raw) if raw else None
            if reply.status != expected:
                raise RuntimeError(f'{path}: HTTP {reply.status}: {payload}')
            return payload

    def status(self):
        return self.call('api/status')

    @property
    def runtime_id(self):
        return self.endpoint['runtime_id']

    @property
    def url(self):
        return f"http://{self.endpoint['address']}/"


class Editor:
    """A test-owned native editor kept open by `editor.ps1 -Serve`."""

    def __init__(self, directory, name, env):
        self.name = name
        directory = Path(directory)
        directory.mkdir(parents=True, exist_ok=True)
        self.command_file = directory / f'{name}.cmd'
        self.condition = threading.Condition()
        self.history = []
        self.seen = 0
        self.stderr = []
        self.process = subprocess.Popen(
            ['pwsh', '-NoProfile', '-STA', '-File', str(HERE / 'editor.ps1'), '-Serve', str(directory), '-Editor', name],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True, encoding='utf-8')
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=lambda: self.stderr.append(self.process.stderr.read()), daemon=True).start()
        self.form = self.wait(lambda state: state['event'] == 'shown', 30, 'the editor window')['form']

    def _read(self):
        for line in self.process.stdout:
            line = line.strip()
            if not line.startswith('{'):
                continue
            try:
                state = json.loads(line)
            except ValueError:
                continue
            with self.condition:
                self.history.append(state)
                self.condition.notify_all()

    def wait(self, predicate, timeout, description):
        with self.condition:
            deadline = time.monotonic() + timeout
            while True:
                while self.seen < len(self.history):
                    state = self.history[self.seen]
                    self.seen += 1
                    if predicate(state):
                        return state
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f'Editor {self.name}: timed out waiting for {description}; last state: '
                                       f'{self.history[-1] if self.history else None}; stderr: {"".join(self.stderr)[-800:]}')
                self.condition.wait(remaining)

    def command(self, name):
        self.command_file.write_text(name, encoding='utf-8')
        return self.wait(lambda state: state['event'] == f'done:{name}', 15, f'command {name}')

    @property
    def text(self):
        with self.condition:
            return norm(self.history[-1]['text'])


def focus(editor):
    """Bring `editor` to the front as a user's Alt+Tab would: a tap on Alt lets this process
    set the foreground window. The caret then sits at the end of the editor's text."""
    if foreground() != editor.form:
        tap(VK_MENU, SCAN_MENU)
        user32.SetForegroundWindow(editor.form)
        time.sleep(0.3)
    state = editor.command('state')
    if not (state['foreground'] and state['focused']):
        state = editor.command('activate')
    editor.command('caret-end')
    check(state['foreground'] and state['focused'], f'Editor {editor.name} is not the focused foreground window', state=state)


def is_line(label, number):
    """Voiceless names a test line "Line 1"; PnP and PortAudio may add "(Virtual Audio Cable)"."""
    return re.fullmatch(rf'Line {number}(?:\s*\(.*\))?', str(label).strip(), re.IGNORECASE) is not None


def line_index(number, direction):
    """The WASAPI device of test line `number`: its recording or its playback end."""
    channels = 'max_input_channels' if direction == 'input' else 'max_output_channels'
    wasapi = [(i, d) for i, d in enumerate(sd.query_devices())
              if sd.query_hostapis(d['hostapi'])['name'] == 'Windows WASAPI']
    matches = [i for i, d in wasapi
               if d[channels] >= (1 if direction == 'input' else 2) and is_line(d['name'], number)]
    if len(matches) != 1:
        raise RuntimeError(f'Line {number} {direction} is ambiguous: {matches}; WASAPI devices: '
                           + str([(d['name'], d[channels]) for _, d in wasapi]))
    return matches[0]


@contextlib.contextmanager
def speech_on(number, speech, pause=1.3):
    """Play the clip once into the playback end of line `number`, then keep a second of silence."""
    device = line_index(number, 'output')
    rate = int(sd.query_devices(device)['default_samplerate'])
    pcm, _ = audioop.ratecv(speech.pcm, 2, 1, speech.rate, rate, None)
    signal = audioop.tostereo(pcm, 2, 1, 1) + bytes(4 * rate)
    offset = 0

    def fill(out, frames, timing, status):
        nonlocal offset
        size = frames * 4
        out[:] = (signal * ((offset + size + len(signal) - 1) // len(signal)))[offset:offset + size]
        offset = (offset + size) % len(signal)

    stream = sd.RawOutputStream(device=device, channels=2, samplerate=rate, dtype='int16', callback=fill)
    stream.start()
    try:
        yield rate
        time.sleep(speech.seconds + pause)
    finally:
        stream.stop()
        stream.close()


class Speech:
    """The pinned reference clip, as installed with the model."""

    def __init__(self, path):
        self.path = Path(path)
        digest = hashlib.sha256(self.path.read_bytes()).hexdigest()
        if digest != SPEECH_SHA256:
            raise RuntimeError(f'{self.path} is not the pinned clip: {digest}')
        with wave.open(str(self.path), 'rb') as wav:
            self.rate, self.channels, self.width = wav.getframerate(), wav.getnchannels(), wav.getsampwidth()
            self.pcm = wav.readframes(wav.getnframes())
        if (self.channels, self.width) != (1, 2):
            raise RuntimeError(f'{self.path}: expected mono PCM16')
        self.seconds = len(self.pcm) / (2 * self.rate)


def transcribe(cli, wav, env):
    """Use the installed SenseVoice model chosen in the scratch batch configuration."""
    out = subprocess.check_output([str(cli), 'transcribe', str(wav), '--backend', 'local', '--json'], env=env, timeout=150)
    return json.loads(out)['text']


def cable_report(speech, evidence):
    """What the recording end of line 1 delivers while the clip plays into it."""
    source = line_index(1, 'input')
    rate = int(sd.query_devices(source)['default_samplerate'])
    captured = bytearray()

    def collect(data, frames, timing, status):
        captured.extend(bytes(data))

    stream = sd.RawInputStream(device=source, channels=1, samplerate=rate, dtype='int16', callback=collect)
    stream.start()
    try:
        time.sleep(0.5)
        with speech_on(1, speech, pause=1.5):
            pass
    finally:
        stream.stop()
        stream.close()
    pcm, _ = audioop.ratecv(bytes(captured), 2, 1, rate, 16000, None)
    path = evidence / 'cable-line-1.wav'
    with wave.open(str(path), 'wb') as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(16000)
        wav.writeframes(pcm)
    samples = array.array('h', pcm)
    levels = [round(sum(abs(x) for x in samples[i:i + 16000]) / 16000) for i in range(0, len(samples) - 16000 + 1, 16000)]
    return path, levels


# -------------------------------------------------------------- the window

def attach(manager, console, timeout=120):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def loaded():
        try:
            with opener.open(f'http://127.0.0.1:{CDP_PORT}/json/list', timeout=5) as reply:
                return any(t.get('type') == 'page' and t.get('url', '').startswith(console.url) for t in json.load(reply))
        except OSError:
            return False

    wait_for(loaded, 'the desktop window to load the console', timeout, 0.5)
    browser = manager.chromium.connect_over_cdp(f'http://127.0.0.1:{CDP_PORT}')
    for context in browser.contexts:
        for page in context.pages:
            if page.url.startswith(console.url):
                return page
    raise RuntimeError('the console page is not among the WebView2 targets')


def button(page, pattern):
    return page.get_by_role('button', name=pattern).first


def note(page, pattern):
    return page.get_by_text(pattern).first


def shot(page, evidence, name):
    try:
        page.screenshot(path=str(evidence / name))
    except Exception as error:  # evidence only
        record('screenshot', name=name, error=str(error)[:300])


def recording(console):
    status = console.status()
    return status['state'] == 'recording' and (status.get('session') or {}).get('phase') == 'recording'


def settled(console):
    """The status once the session ended; `None` while one runs."""
    status = console.status()
    phase = (status.get('session') or {}).get('phase')
    return status if status['state'] == 'idle' and phase in ('completed', 'no-speech', 'failed', 'cancelled') else None


def say(console, speech, start, stop, during=None):
    """Record the clip through the product and return the finished session; `during` runs
    once the recording is live, before the clip plays."""
    identity = start()
    wait_for(lambda: recording(console), 'the recording to start', 60)
    if during:
        during()
    with speech_on(1, speech):
        pass
    stop(identity)
    session = wait_for(lambda: settled(console), 'the finished session', 240)['session']
    check(session['phase'] == 'completed', 'the session did not complete', session=session)
    check(session['transcript'] and session['transcript']['is_final'], 'no final transcript', session=session)
    return session

def run(candidates, root, env, evidence):
    version = json.loads((REPO / 'desktop/tauri.conf.json').read_text(encoding='utf-8'))['version']
    for kind in ('core', 'desktop'):
        install(candidates, kind, version, root / kind, env)
    cli = root / 'desktop' / 'voiceless.exe'
    record('installed', version=subprocess.check_output([str(cli), '--version'], env=env, text=True).strip())

    shortcut = Path(os.environ['APPDATA']) / SHORTCUT
    target = powershell(f"(New-Object -ComObject WScript.Shell).CreateShortcut('{shortcut}').TargetPath", env)
    check(os.path.normcase(str(Path(target).resolve())) == os.path.normcase(str(root / 'desktop' / 'voiceless-desktop.exe')),
          'The Start menu entry points elsewhere', target=target)
    record('entry', shortcut=str(shortcut), target=target)

    # The WebView2 debugging port is only how the test reaches the page the user sees.
    env['WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS'] = f'--remote-debugging-port={CDP_PORT}'
    env['WEBVIEW2_USER_DATA_FOLDER'] = str(root / 'webview')
    host = int(powershell(f"(Start-Process -FilePath '{shortcut}' -PassThru).Id", env))
    console = Console(env['XDG_DATA_HOME'])
    wait_for(console.ready, 'the daemon the desktop entry starts', 120)
    window = window_of(host)
    running = processes(env)
    daemon = next(p for p in running if p['Name'] == 'voiceless.exe')
    check(daemon['ParentProcessId'] == host and 'daemon' in daemon['CommandLine'], 'The daemon was not started by the desktop host', daemon=daemon)
    status = console.status()
    record('first-run', host_pid=host, window=window, console=console.url, processes=running,
           configured=status['configured'], state=status['state'], backend=status['backend'],
           hotkey=status['hotkey'], engine=status['engine'])

    editors = []
    with sync_playwright() as manager:
        page = attach(manager, console)
        page.set_default_timeout(60000)
        try:
            page.wait_for_function("location.hash === '#setup'", timeout=90000)
            page.get_by_role('heading', name=UI['setup']).first.wait_for(state='visible')
            record('first-run', url=page.url, title=page.title(), lang=page.evaluate('document.documentElement.lang'))
            shot(page, evidence, '01-first-run.png')

            # The microphone, the recognition and the model, as the page offers them.
            audio = console.call('api/desktop/audio')
            record('microphones', labels=[m['label'] for m in audio['microphones']], system_default=audio['system_default'])
            found = [m for m in audio['microphones'] if is_line(m['label'], 1)]
            check(len(found) == 1 and found[0]['id'].lower() == os.environ['VOICELESS_VAC_MIC'].lower(),
                  'Line 1 is not the one microphone lines.ps1 named', found=found)
            page.get_by_label(UI['input']).select_option(found[0]['id'])
            page.get_by_label(UI['recognition']).select_option('local-offline')
            page.get_by_label(UI['model']).select_option(MODEL)
            record('setup', microphone=found[0], recognition='local-offline', model=MODEL)

            button(page, UI['get_models']).click()
            deadline, last = time.monotonic() + 600, None
            while True:
                models = {m['id']: m for m in console.call('api/models')}
                view = {i: (models[i]['installed'], models[i]['progress']) for i in (MODEL, VAD) if i in models}
                if view != last:
                    record('models', view=view)
                    last = view
                check(not any(models[i]['error'] for i in (MODEL, VAD) if i in models), 'Model installation failed', models=models)
                if all(i in models and models[i]['installed'] for i in (MODEL, VAD)):
                    break
                check(time.monotonic() < deadline, 'Model installation did not finish', models=models)
                time.sleep(1)
            button(page, UI['apply']).click()
            applied = wait_for(lambda: (s := console.status())['config_apply']['state'] == 'applied'
                               and s['engine']['loaded'] and s['configured'] and s or None, 'the settings to take effect', 300)
            note(page, READY).wait_for(state='visible')
            record('applied', config_apply=applied['config_apply'], engine=applied['engine'], backend=applied['backend'])
            shot(page, evidence, '02-setup-applied.png')

            # The reference transcript from the installed CLI, and what the line itself carries.
            data = Path(env['XDG_DATA_HOME']) / 'voiceless'
            clip, model = next(data.rglob('test_wavs/en.wav'), None), next(data.rglob('model.int8.onnx'), None)
            check(clip and model, 'The installed model is incomplete', data=str(data))
            speech = Speech(clip)
            batch = dict(env, XDG_CONFIG_HOME=str(root / 'batch-config'))
            (root / 'batch-config' / 'voiceless').mkdir(parents=True)
            (root / 'batch-config' / 'voiceless' / 'config.toml').write_text(
                f'[asr]\nbatch_backend = "local"\nbatch_model_dir = {json.dumps(str(model.parent))}\n', encoding='utf-8')
            record('wasapi', devices=[d['name'] for d in sd.query_devices()
                                      if sd.query_hostapis(d['hostapi'])['name'] == 'Windows WASAPI'])
            baseline = transcribe(cli, speech.path, batch)
            record('baseline', text=baseline)
            path, levels = cable_report(speech, evidence)
            through = transcribe(cli, path, batch)
            record('speech', clip=str(speech.path), seconds=round(speech.seconds, 3), baseline=baseline,
                   cable=dict(path=str(path), mean_abs_per_second=levels, transcript=through, agrees=agrees(through, baseline)))

            # The hotkey: recorded with the page's key widget, saved, then applied.
            page.get_by_role('link', name=UI['shortcuts']).first.click()
            commands = page.get_by_text(either('快捷键的 CLI 命令', 'CLI commands for shortcuts'))
            overlay = page.get_by_text('On this platform, the Voiceless window shows the recording status.', exact=True)
            commands.wait_for(state='visible')
            overlay.wait_for(state='visible')
            record('shortcut-help', commands=commands.text_content(), recording_status=overlay.text_content())
            button(page, UI['record_toggle']).click()
            page.keyboard.press(KEY_CODE)
            button(page, UI['save']).click()
            # The page asks whether to apply the saved settings now; the apply slot offers the same.
            apply_now = page.get_by_role('dialog').get_by_role('button', name=UI['apply_dialog'])
            try:
                apply_now.wait_for(state='visible', timeout=10000)
                apply_now.click()
            except PwTimeout:
                button(page, UI['apply']).click()

            def bound(status):
                return [b for b in status['hotkey']['bindings'] if b['field'] == 'toggle_key' and b['chord'] == KEY and b['keyboards'] > 0]

            try:
                hotkey = wait_for(lambda: (s := console.status())['config_apply']['state'] == 'applied' and bound(s) and s or None,
                                  'the daemon to bind the shortcut', 300)
            except RuntimeError:
                raise RuntimeError(f'The shortcut was not bound: {console.status()["hotkey"]}') from None
            note(page, ACTIVE).wait_for(state='visible')
            registration = hotkey_registration()
            check(not registration['same_combination']['ok'] and registration['same_combination']['error'] == 1409
                  and registration['other_key']['ok'], 'RegisterHotKey does not show the daemon as the owner', registration=registration)
            record('hotkey', field='hotkey.toggle_key', chord=KEY, bindings=hotkey['hotkey'], registration=registration)
            shot(page, evidence, '03-shortcut.png')

            # A software key gesture: armed through the page, then ScrollLock through SendInput.
            page.get_by_role('link', name=UI['setup']).first.click()
            button(page, UI['arm']).click()
            note(page, ARMED).wait_for(state='visible')
            with KeyWatch() as watch:
                tap(VK_KEY, SCAN_KEY)
                time.sleep(1.5)
            deadline, reacted = time.monotonic() + 21, None
            while time.monotonic() < deadline and not reacted:
                status = console.status()
                reacted = status if status['state'] != 'idle' else None
                time.sleep(0.5)
            note(page, NO_KEY).wait_for(state='visible', timeout=40000)
            if reacted:
                console.call('api/session/cancel', {'runtime_id': console.runtime_id})
                wait_for(lambda: settled(console), 'the session to end', 60)
            record('gesture', seen=[dict(vk=hex(vk), flags=hex(flags), injected=bool(flags & LLKHF_INJECTED)) for vk, flags in watch.events],
                   daemon_reacted=bool(reacted), state_after=console.status()['state'], page=note(page, NO_KEY).text_content())

            # Test-owned native editors: A is where the user works, B stands for another window.
            a = Editor(root / 'editors', 'A', env)
            b = Editor(root / 'editors', 'B', env)
            editors.extend((a, b))
            focus(a)
            check(a.text == PREFIX.format('A'), 'Editor A does not start with its own text', text=a.text)

            # Practice from the page's button: the result goes to an editable preview, nowhere else.
            before = a.text
            button(page, UI['start']).click()
            practice = say(console, speech, lambda: None, lambda _: button(page, UI['finish']).click())
            preview = wait_for(lambda: (v := page.get_by_label(UI['result']).input_value()) and v or None, 'the preview', 30)
            time.sleep(0.8)
            check(practice['practice'] and not (practice['injection'] or {}).get('ok'), 'The practice delivered something', session=practice)
            check(preview == practice['transcript']['authoritative_text'], 'The preview is not the transcript', preview=preview, session=practice)
            check(agrees(preview, baseline), 'The practice transcript does not match the reference', preview=preview, baseline=baseline)
            check(a.text == before and foreground() == a.form, 'The practice changed the editor or the focus', editor=a.text)
            record('practice', purpose=practice['purpose'], injection=practice['injection'], preview=preview, agrees=True,
                   editor_a_unchanged=True, foreground_is_editor=True)
            shot(page, evidence, '04-practice-result.png')

            # The preview can be edited; "Test typing" then pastes exactly what it shows.
            edited = preview + EDITED
            page.get_by_label(UI['result']).fill(edited)
            focus(a)
            button(page, UI['test']).click()
            note(page, SWITCH).wait_for(state='visible')
            a.wait(lambda state: norm(state['text']) != before, 20, 'the test typing')
            time.sleep(0.8)
            check(a.text == before + edited, 'Test typing did not paste the preview', editor=a.text, expected=before + edited)
            check(foreground() == a.form, 'Test typing moved the focus')
            typed = a.text
            record('test-typing', editor_a=a.text, pasted=edited, page=note(page, METHOD).text_content(), foreground_is_editor=True)
            shot(page, evidence, '05-test-typing.png')

            # Dictation started through the console API while editor A is the focused window.
            focus(a)
            before = a.text
            dictation = say(console, speech,
                            lambda: console.call('api/session/start', {'mode': '', 'inject': True, 'runtime_id': console.runtime_id}),
                            lambda identity: console.call('api/session/stop', {'runtime_id': console.runtime_id, 'session_id': identity['session_id']}))
            time.sleep(0.8)
            text = dictation['transcript']['authoritative_text']
            check(dictation['injection'] and dictation['injection']['ok'], 'Dictation did not deliver', session=dictation)
            check(a.text == before + text and foreground() == a.form, 'The dictation text is not in the focused editor', editor=a.text)
            record('dictation', injection=dictation['injection'], text=text, agrees=agrees(text, baseline), editor_a=a.text, foreground_is_editor=True)

            # The guard: the window changes while the recording runs, so nothing is typed anywhere.
            focus(a)
            before_a, before_b = a.text, b.text
            guarded = say(console, speech,
                          lambda: console.call('api/session/start', {'mode': '', 'inject': True, 'runtime_id': console.runtime_id}),
                          lambda identity: console.call('api/session/stop', {'runtime_id': console.runtime_id, 'session_id': identity['session_id']}),
                          during=lambda: focus(b))
            time.sleep(0.8)
            refusal = (guarded['injection'] or {}).get('message', '')
            check(guarded['injection'] and not guarded['injection']['ok'] and 'changed' in refusal,
                  'The guard let the delivery through or gave no reason', session=guarded)
            check(a.text == before_a and b.text == before_b, 'The guarded session typed into an editor', a=a.text, b=b.text)
            record('guard', injection=guarded['injection'], editor_a_unchanged=True, editor_b_unchanged=True,
                   foreground_is_other=foreground() == b.form)
            shot(page, evidence, '06-final.png')
            summary = dict(practice=dict(preview=preview, injection=practice['injection']),
                           test_typing=dict(editor=typed),
                           dictation=dict(injection=dictation['injection'], text=text),
                           guard=dict(injection=guarded['injection']), hotkey=hotkey['hotkey'], registration=registration,
                           speech=dict(baseline=baseline, cable=through, cable_levels=levels))
            (evidence / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding='utf-8')
            (evidence / 'logs.txt').write_text('\n'.join(console.call('api/logs')['lines']), encoding='utf-8')
            record('accepted', practice_preview=True, test_typing=True, api_dictation=True,
                   hotkey_registration=True, physical_keypress='not testable on this guest: injected keys are ignored by design')
        except Exception:
            try:
                shot(page, evidence, 'failure.png')
                (evidence / 'page.txt').write_text(page.evaluate('document.body.innerText'), encoding='utf-8')
                (evidence / 'logs.txt').write_text('\n'.join(console.call('api/logs')['lines']), encoding='utf-8')
            except Exception as error:
                record('evidence-error', error=str(error)[:300])
            raise
        finally:
            for editor in editors:
                with contextlib.suppress(Exception):
                    editor.command('stop')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidates', type=Path)
    args = parser.parse_args()
    if (os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('GITHUB_REF_NAME') != BRANCH
            or not os.environ.get('VOICELESS_VAC_MIC')):
        parser.error('requires the hosted practice test guest that lines.ps1 prepared')
    sys.stdout.reconfigure(encoding='utf-8', errors='replace')
    evidence = Path(os.environ.get('RUNNER_TEMP', '.')) / 'practice-evidence'
    evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory).resolve()
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'),
                   XDG_CACHE_HOME=str(root / 'cache'), XDG_STATE_HOME=str(root / 'state'), PYTHONUTF8='1')
        try:
            run(args.candidates.resolve(), root, env, evidence)
        finally:
            for step, action in (
                    ('stop', lambda: powershell(
                        f"Get-Process -Name voiceless, voiceless-desktop -ErrorAction SilentlyContinue | "
                        f"Where-Object {{ $_.Path -like '{root}*' }} | Stop-Process -Force", env)),
                    ('uninstall', lambda: [subprocess.run([str(root / kind / 'unins000.exe'), *INSTALL], env=env, timeout=150)
                                           for kind in ('desktop', 'core')])):
                try:
                    action()
                except Exception as error:
                    record(f'cleanup-{step}', error=str(error)[:300])


if __name__ == '__main__':
    main()
