"""Two delivery checks of the installed desktop in TextEdit: a paste puts the clipboard back, and a dictation
that stops in another document delivers nothing, keeps its text and leaves both saved documents as they were."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

from acceptance import (MICROPHONE, TEXT, Evidence, Output, Refused, focused, open_editor, private_roots,
                        quiet, refuse, save, screenshot, stereo, wait_for)

UNTOUCHED = 'Voiceless guard B keeps this line'
SENTINEL = 'Voiceless clipboard sentinel 4b7e91'
KEPT = 'the text is kept for you to copy or type again'
ENDED = ('failed', 'cancelled', 'no-speech')


class Console:
    """The console of the running daemon, called as its page calls it."""

    def __init__(self, descriptor):
        self.base = 'http://' + descriptor['address']
        self.token = descriptor['token']
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(self, path, body=None):
        request = urllib.request.Request(
            self.base + '/api/' + path, data=None if body is None else json.dumps(body).encode(),
            headers={'Content-Type': 'application/json', 'X-Voiceless-Token': self.token})
        try:
            with self.opener.open(request, timeout=60) as reply:
                return json.load(reply)
        except urllib.error.HTTPError as error:
            raise Refused(f'{path} answered {error.code}: {error.read().decode(errors="replace")[:700]}') from None

    def get(self, path):
        return self.call(path)

    def post(self, path, body):
        return self.call(path, body)


def launch(root, log):
    """The installed desktop under private roots, started with the offline model it is configured for."""
    env = private_roots(root / 'profile')
    app = root / 'Voiceless.app'
    core = app / 'Contents/MacOS/voiceless'
    for model in ('sense-voice-int8', 'silero-vad'):
        subprocess.run([core, 'model', 'install', model], env=env, check=True, timeout=360)
    config = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text('[asr]\nstreaming_backend = "local-offline"\n[local]\nmodel = "sense-voice-int8"\nprovider = "cpu"\n',
                      encoding='utf-8')
    config.chmod(0o600)
    subprocess.run([Path(os.environ['RUNNER_TEMP']) / 'audio', 'default-input', MICROPHONE], check=True, timeout=15)
    return subprocess.Popen([app / 'Contents/MacOS/voiceless-desktop'], env=env, stdout=log,
                            stderr=subprocess.STDOUT), env


def stop(host, app):
    """End the installed desktop and the daemon it started: both are this test's own."""
    for pid in subprocess.run(['pgrep', '-f', str(app)], capture_output=True, text=True).stdout.split():
        subprocess.run(['kill', pid], capture_output=True)
    try:
        host.wait(timeout=30)
    except subprocess.TimeoutExpired:
        host.kill()


def loaded(console):
    """The daemon once its offline model is loaded, with the test microphone as the system default."""
    def ready():
        status = console.get('status')
        engine = status['engine']
        refuse(engine['error'] is None, f'the offline model did not load: {json.dumps(engine)}')
        return status if status['configured'] and engine['loaded'] else None
    status = wait_for(ready, 'the installed desktop to load the offline model', 150)
    audio = console.get('desktop/audio')
    refuse(audio['system_default'] == 'coreaudio:' + MICROPHONE, f'wrong test microphone: {audio["system_default"]}')
    return {'backend': status['backend'], 'engine': status['engine'], 'default_input': audio['system_default']}


def delivered(console, snapshot, directory, document):
    """The fixed text arrives in the focused document, its file holds exactly that text, and the clipboard is as
    the fixture left it."""
    open_editor(document)
    focused(snapshot, document)
    subprocess.run(['pbcopy'], input=SENTINEL.encode(), check=True, timeout=15)
    outcome = console.post('inject/test', {'text': TEXT, 'delay_ms': 0})
    refuse(outcome['ok'], f'the installed desktop did not deliver to the focused document: {outcome}')
    clipboard = subprocess.check_output(['pbpaste'], timeout=15).decode()
    refuse(clipboard == SENTINEL, 'the clipboard sentinel was not restored after delivery')
    screenshot(directory / 'delivered.png')

    def saved():
        data = save(document)
        return data if data.decode('utf-8-sig', errors='replace') == TEXT else None
    data = wait_for(saved, f'{document.name} to hold the delivered text once saved', 15)
    return {'injection': outcome['injection'], 'clipboard': 'the sentinel, put back', 'file_utf8_bytes': len(data)}


def session_at(console, identity, wanted, description, timeout):
    """The daemon's view of session `identity` once its phase is one of `wanted`. A session that ended on the way
    is refused at once with what the daemon says of it and of the microphone, so a refused or an unanswered
    permission shows as the limitation it is."""
    def ready():
        session = console.get('status').get('session') or {}
        if session.get('phase') in ENDED:
            checks = quiet(console.get, 'doctor')
            microphone = checks if isinstance(checks, str) else [check for check in checks if check['name'] == 'microphone']
            raise Refused(f'the dictation ended as {session["phase"]}: {json.dumps(session, ensure_ascii=False)}; '
                          f'microphone check: {microphone}')
        return session if session.get('id') == identity and session.get('phase') in wanted else None
    return wait_for(ready, description, timeout)


def opened(snapshot, document, *, close=False):
    """Save the fixture document. On the final read, wait for TextEdit to close it and finish saving."""
    open_editor(document)
    report = focused(snapshot, document)
    save(document)
    if close:
        script = '''on run argv
            set wanted to item 1 of argv
            tell application "System Events" to tell application process "TextEdit"
                if not frontmost or name of front window does not contain wanted then error "Wrong test document"
                keystroke "w" using command down
                repeat 100 times
                    if not (exists (windows whose name contains wanted)) then return
                    delay 0.1
                end repeat
                error "The test document did not close"
            end tell
        end run'''
        subprocess.run(['osascript', '-e', script, document.stem], check=True, timeout=15)
    return {'file': document.read_bytes(), 'title': report['title']}


def digest(documents):
    """Name, size and SHA-256 of what each document's file holds."""
    return {name: {'bytes': len(state['file']), 'sha256': hashlib.sha256(state['file']).hexdigest()}
            for name, state in documents.items()}


def refused(console, snapshot, directory, first, second):
    """A dictation starts with `first` focused and stops with `second` focused: nothing is delivered, the text is
    kept, the clipboard is left alone, and both saved documents are as they were."""
    before = {}
    for document in (second, first):  # `first` is focused last: the dictation starts there
        before[document.name] = opened(snapshot, document)
    subprocess.run(['pbcopy'], input=SENTINEL.encode(), check=True, timeout=15)
    pcm = (Path(__file__).resolve().parents[3] / 'frontend/e2e/fixtures/jfk-16k-mono-s16le.pcm').read_bytes()
    started = console.post('session/start', {})
    refuse(started['outcome'] == 'started', f'the dictation did not start: {started}')
    identity = started['session_id']
    session = session_at(console, identity, ('recording',), 'the dictation to record', 50)
    refuse(session['purpose'] == 'dictation', f'not a dictation: {session}')
    with Output('BlackHole 2ch', lambda rate: stereo(pcm, 16000, rate), loop=False):
        time.sleep(len(pcm) / 32000 + 0.65)
    open_editor(second)
    moved = focused(snapshot, second)
    refuse(moved['bridge_confirmed_field'], f'the second document is no confirmed input: {moved}')
    console.post('session/stop', {'session_id': identity})
    session = session_at(console, identity, ('completed',), 'the stopped dictation to finish', 100)
    wait_for(lambda: console.get('status')['state'] == 'idle', 'the daemon to return to idle', 30)
    screenshot(directory / 'refused.png')
    injection = session['injection'] or {}
    refuse(injection.get('ok') is False and injection.get('delivery') == 'not-delivered'
           and KEPT in injection.get('message', ''),
           f'the stopped dictation was not refused with its text kept: {json.dumps(session, ensure_ascii=False)}')
    text = (session.get('transcript') or {}).get('authoritative_text', '')
    words = ' '.join(re.findall(r'[a-z]+', text.lower()))
    refuse('your country can do for you' in words and 'you can do for your country' in words,
           f'the kept text does not hold both clauses of the reference: {text!r}')
    records = console.get('records?limit=1')['records']
    refuse(records, 'the history holds no row of this dictation')
    record = records[0]
    refuse(record['raw_text'] == text and record['final_text'] == (record['processed_text'] or text)
           and record['app_id'] == 'com.apple.TextEdit',
           f'the history row does not hold the kept text of a dictation started in TextEdit: {record}')
    clipboard = subprocess.check_output(['pbpaste'], timeout=15).decode()
    refuse(clipboard == SENTINEL, 'the clipboard sentinel changed during refused delivery')
    after = {}
    for document in (second, first):
        after[document.name] = opened(snapshot, document, close=True)
    refuse(all(after[name]['file'] == state['file'] for name, state in before.items()),
           f'the saved documents changed: {digest(before)} -> {digest(after)}')
    return {'injection': injection, 'kept': record['final_text'], 'focus_at_start': before[first.name]['title'],
            'focus_at_stop': moved['title'], 'clipboard': 'the sentinel, untouched', 'documents': digest(after)}


def run(root, evidence):
    app = root / 'Voiceless.app'
    with (evidence.directory / 'desktop.log').open('w') as log:
        host, env = launch(root, log)
        console = None
        try:
            descriptor = Path(env['XDG_DATA_HOME']) / 'voiceless/console-endpoint.json'
            wait_for(descriptor.is_file, 'the installed desktop to start its daemon', 90)
            console = Console(json.loads(descriptor.read_text()))
            snapshot, first, second = root / 'focus-snapshot', root / 'Voiceless guard A.txt', root / 'Voiceless guard B.txt'
            first.touch()
            second.write_bytes(UNTOUCHED.encode())
            evidence.check('installed desktop: the offline model is loaded and the test microphone is the system default',
                           True, lambda: loaded(console))
            evidence.check('delivery: the installed desktop pastes into TextEdit, the file holds the text and the '
                           'clipboard is put back', True, lambda: delivered(console, snapshot, evidence.directory, first))
            evidence.check('delivery: a dictation that stops in another TextEdit document delivers nothing, keeps its '
                           'text and leaves both saved documents unchanged', True,
                           lambda: refused(console, snapshot, evidence.directory, first, second))
            if evidence.failed():
                screenshot(evidence.directory / 'failure.png')
        finally:
            if console is not None:
                evidence.record('daemon status', json.dumps(quiet(console.get, 'status'), ensure_ascii=False))
                evidence.record('daemon logs', json.dumps(quiet(console.get, 'logs'), ensure_ascii=False))
            stop(host, app)


if __name__ == '__main__':
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'macOS':
        raise SystemExit('requires a disposable hosted macOS guest')
    root, directory = (Path(p).resolve() for p in sys.argv[1:])
    evidence = Evidence(directory)
    try:
        run(root, evidence)
    finally:
        evidence.write()
    failed = evidence.failed()
    print(json.dumps({'checks': len(evidence.checks), 'failed': failed}, ensure_ascii=False), flush=True)
    sys.exit(1 if failed else 0)
