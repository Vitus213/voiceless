"""Check the installed console's explicit delivery to a test-owned plain-text document."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.request


def run(root, cli):
    env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'),
               XDG_CACHE_HOME=str(root / 'cache'))
    config = root / 'config.toml'
    config.write_text('[inject]\nprefer = "clipboard"\n', encoding='utf-8')
    config.chmod(0o600)
    url_pattern = re.compile(r'Voiceless console: (http://127\.0\.0\.1:\d+)/')
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    document = root / 'Voiceless acceptance.txt'
    document.touch()
    text = 'Voiceless 中文 Ω'
    log = root / 'web.log'
    with log.open('w') as stream, subprocess.Popen([cli, '--config', config, 'web', '--port', '0', '--no-open'],
                                                    env=env, stdout=stream, stderr=stream) as process:
        try:
            deadline = time.monotonic() + 30
            while not (match := url_pattern.search(log.read_text())):
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(log.read_text()[-600:])
                time.sleep(.1)
            url = match[1]
            token = re.search(rb'name="voiceless-token" content="([^" ]+)"', opener.open(url, timeout=5).read())[1].decode()
            subprocess.run(['open', '-a', 'TextEdit', str(document)], check=True, timeout=15)
            deadline = time.monotonic() + 25
            while True:
                snap = json.loads(subprocess.check_output([root / 'focus-snapshot'], text=True, timeout=15))
                main = snap['main']
                if document.stem in main.get('title', '') and main.get('app_role') == 'AXTextArea':
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError(f'TextEdit did not focus the test document: {snap}')
                time.sleep(.35)
            print('TEXTEDIT_FOCUS', json.dumps(snap, ensure_ascii=False), flush=True)
            request = urllib.request.Request(url + '/api/inject/test', data=json.dumps({'text': text, 'delay_ms': 0}).encode(),
                                             headers={'Content-Type':'application/json', 'Origin':url, 'X-Voiceless-Token':token})
            with opener.open(request, timeout=20) as response:
                outcome = json.load(response)
            print('TEXTEDIT_DELIVERY', json.dumps(outcome, ensure_ascii=False), flush=True)
            if not outcome['ok']:
                raise RuntimeError('The installed CLI refused delivery to the focused TextEdit document')
            time.sleep(.5)
            subprocess.run(['osascript', '-e', 'tell application "System Events" to tell application process "TextEdit"',
                            '-e', 'if not frontmost then error "TextEdit lost focus"', '-e', 'keystroke "s" using command down',
                            '-e', 'end tell'], check=True, timeout=12)
            deadline = time.monotonic() + 10
            while document.read_text(encoding='utf-8-sig') != text:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f'Test document holds {document.read_bytes()!r}')
                time.sleep(.15)
            print('TEXTEDIT_SAVED', json.dumps({'utf8_bytes':len(text.encode()), 'exact':True}), flush=True)
        finally:
            process.terminate()


if __name__ == '__main__':
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'macOS':
        raise SystemExit('Requires a disposable hosted macOS guest')
    run(Path(sys.argv[1]), sys.argv[2])
