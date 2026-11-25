"""Run first practice inside the installed app's WKWebView and read back its delivery in TextEdit."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.request

from acceptance import MICROPHONE, Output, open_editor, private_roots, refuse, saved_text, screenshot, stereo, wait_for


class Window:
    def __init__(self, executable, pid):
        self.command = [str(executable), str(pid)]

    def control(self, action, role, *names, timeout=40):
        def attempt():
            result = subprocess.run([*self.command, action, role, *names], capture_output=True, text=True, timeout=15)
            if result.returncode == 1:
                return None
            result.check_returncode()
            return result.stdout if action == 'value' else True
        return wait_for(attempt, f'{action} {role} {names}', timeout)

    def press(self, *names, timeout=40):
        self.control('press', 'AXButton', *names, timeout=timeout)


def run(root, evidence):
    env = private_roots(root / 'profile')
    app = root / 'Voiceless.app'
    core = app / 'Contents/MacOS/voiceless'
    subprocess.run([core, 'model', 'install', 'sense-voice-int8'], env=env, check=True, timeout=360)
    config = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
    refuse(not config.exists(), 'model installation unexpectedly configured the first-run profile')
    subprocess.run([Path(os.environ['RUNNER_TEMP']) / 'audio', 'default-input', MICROPHONE], check=True, timeout=15)
    document = root / 'Voiceless practice.txt'
    document.touch()
    open_editor(document)
    pcm = (Path(__file__).resolve().parents[3] / 'frontend/e2e/fixtures/jfk-16k-mono-s16le.pcm').read_bytes()
    descriptor = Path(env['XDG_DATA_HOME']) / 'voiceless/console-endpoint.json'
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with (root / 'host.log').open('w') as output, subprocess.Popen(
        [app / 'Contents/MacOS/voiceless-desktop'], env=env, stdout=output, stderr=subprocess.STDOUT,
    ) as host:
        window = Window(root / 'practice-controls', host.pid)
        try:
            wait_for(descriptor.is_file, 'the installed desktop to start the daemon', 90)
            endpoint = json.loads(descriptor.read_text())
            base = 'http://' + endpoint['address']
            def api(path):
                request = urllib.request.Request(base + '/api/' + path, headers={'X-Voiceless-Token': endpoint['token']})
                with opener.open(request, timeout=10) as reply:
                    return json.load(reply)
            window.control('seen', 'AXButton', 'Try a phrase', '开始试说', timeout=90)
            first = api('status')
            refuse(not first['configured'], 'setup was already complete')
            audio = api('desktop/audio')
            refuse(audio['system_default'] == 'coreaudio:' + MICROPHONE, 'wrong test microphone')
            screenshot(evidence / 'first-phrase.png')
            window.press('Get the models', '准备模型')
            window.press('Apply settings', '应用设置', timeout=300)
            def applied():
                current = api('status')
                return current if current['engine']['loaded'] and current['config_apply']['state'] == 'applied' else None
            status = wait_for(applied, 'the selected offline model to load', 150)
            print('PRACTICE_SETUP', json.dumps({'default_input': audio['system_default'], 'engine': status['engine']}), flush=True)
            window.press('Try a phrase', '开始试说')
            def recording():
                session = api('status').get('session') or {}
                return session.get('practice') and session.get('phase') == 'recording'
            wait_for(recording, 'the first practice to record', 50)
            open_editor(document)
            with Output('BlackHole 2ch', lambda rate: stereo(pcm, 16000, rate), loop=False):
                time.sleep(len(pcm) / 32000 + 0.65)
            window.press('Finish the practice', '完成试说')
            result = window.control('value', 'AXTextArea', 'Practice result', '练习结果', timeout=100)
            words = ' '.join(re.findall(r'[a-z]+', result.lower()))
            refuse('your country can do for you' in words and 'you can do for your country' in words,
                   'the practice result did not contain both clauses of the reference: ' + result)
            session = api('status')['session']
            refuse(session['practice'] and session['phase'] == 'completed' and not session.get('injection') and
                   session['transcript']['authoritative_text'] == result, 'practice and runtime disagree')
            open_editor(document)
            refuse(saved_text(document) == '', 'practice inserted text before explicit delivery')
            screenshot(evidence / 'before-explicit-delivery.png')
            window.press('Test typing', '测试上屏')
            open_editor(document)
            time.sleep(3.8)
            wait_for(lambda: saved_text(document) == result, 'the saved TextEdit document to equal the preview', 15)
            screenshot(evidence / 'saved-document.png')
            print('PRACTICE_SAVED', json.dumps({'practice': result, 'before': 0, 'utf8_bytes': len(result.encode()), 'exact': True}), flush=True)
        except Exception:
            screenshot(evidence / 'failure.png')
            with (evidence / 'window.json').open('w') as dump:
                subprocess.run([*window.command, 'dump'], stdout=dump, timeout=30)
            print('WINDOW', (evidence / 'window.json').read_text()[:3600], flush=True)
            raise
        finally:
            host.terminate()


if __name__ == '__main__':
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'macOS':
        raise SystemExit('requires a disposable hosted macOS guest')
    root, evidence = (Path(p).resolve() for p in sys.argv[1:])
    evidence.mkdir(parents=True, exist_ok=True)
    run(root, evidence)
