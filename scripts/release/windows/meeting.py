"""Exercise installed Voiceless against two test-owned WASAPI lines on a hosted Windows guest."""
import argparse
import array
import audioop
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from contextlib import ExitStack, contextmanager
import urllib.error
import urllib.request
import wave
import winreg

import sounddevice as sd

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from validate import recognition_fixture


def wait_for(probe, description, timeout=25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.12)
    raise RuntimeError(f'Timed out waiting for {description}')


def line_index(number):
    devices = sd.query_devices()
    matches = [i for i, d in enumerate(devices) if d['max_output_channels'] >= 2
               and sd.query_hostapis(d['hostapi'])['name'] == 'Windows WASAPI'
               and f'Line {number} (' in d['name'] and 'Virtual Audio Cable' in d['name']]
    if len(matches) != 1:
        raise RuntimeError(f'Line {number} output is ambiguous: {matches}, {devices}')
    return matches[0]


def render(device, rate, stereo_pcm):
    offset = 0
    def fill(out, frames, timing, status):
        nonlocal offset
        size = frames * 4
        repeats = (offset + size + len(stereo_pcm) - 1) // len(stereo_pcm)
        out[:] = (stereo_pcm * repeats)[offset:offset + size]
        offset = (offset + size) % len(stereo_pcm)
    return sd.RawOutputStream(device=device, channels=2, samplerate=rate, dtype='int16', callback=fill)


def sine(rate, frequency):
    mono = array.array('h', (round(15000 * math.sin(math.tau * frequency * i / rate)) for i in range(rate)))
    return audioop.tostereo(mono.tobytes(), 2, 1, 1)


def validate_tracks(meeting):
    tracks = {t['kind']: t for t in meeting['tracks']}
    assert set(tracks) == {'microphone', 'system'}, tracks.keys()
    assert all(t['job_id'] for t in tracks.values()) and len({t['job_id'] for t in tracks.values()}) == 2
    for kind, track in tracks.items():
        with wave.open(track['path'], 'rb') as wav:
            assert (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2)
            samples = array.array('h', wav.readframes(wav.getnframes()))
        assert len(samples) * 2 == track['committed_bytes'] and len(samples) >= 16000, track
        def magnitude(hz):
            step = math.tau * hz / 16000
            return math.hypot(sum(x * math.sin(step*i) for i, x in enumerate(samples)),
                              sum(x * math.cos(step*i) for i, x in enumerate(samples))) / len(samples)
        at330, at880 = magnitude(330), magnitude(880)
        own, other = (at330, at880) if kind == 'microphone' else (at880, at330)
        print(f'{meeting["state"]} {kind}: {len(samples)} samples, 330 Hz={at330:.1f}, 880 Hz={at880:.1f}, job={track["job_id"]}')
        assert own > 10 and own > 3 * other, (kind, at330, at880)


@contextmanager
def denied_microphone():
    path = r'SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged'
    try:
        key = winreg.OpenKey(winreg.HKEY_CURRENT_USER, path, 0, winreg.KEY_READ | winreg.KEY_SET_VALUE)
        created = False
    except FileNotFoundError:
        key = winreg.CreateKey(winreg.HKEY_CURRENT_USER, path)
        created = True
    with key:
        try:
            saved = winreg.QueryValueEx(key, 'Value')
        except FileNotFoundError:
            saved = None
        winreg.SetValueEx(key, 'Value', 0, winreg.REG_SZ, 'Deny')
        try:
            yield
        finally:
            if saved is None:
                winreg.DeleteValue(key, 'Value')
            else:
                winreg.SetValueEx(key, 'Value', 0, saved[1], saved[0])
    if created:
        winreg.DeleteKey(winreg.HKEY_CURRENT_USER, path)


@contextmanager
def installed_daemon(cli, config, env, log):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with log.open('wb') as stream, ExitStack() as cleanup:
        daemon = subprocess.Popen([cli, '--config', config, 'daemon', '--no-inject'], env=env, stdout=stream, stderr=stream)
        def stop():
            if daemon.poll() is None:
                daemon.terminate()
            daemon.wait(timeout=15)
        cleanup.callback(stop)
        def announced():
            text = log.read_text(errors='replace')
            if daemon.poll() is not None:
                raise RuntimeError(text[-2000:])
            return re.search(r'Voiceless console: (http://127\.0\.0\.1:\d+)/', text)
        origin = wait_for(announced, 'installed daemon').group(1)
        token = re.search(rb'name="voiceless-token" content="([^"]+)"', opener.open(origin + '/', timeout=10).read())[1].decode()
        def api(path, body=None, expected=200):
            req = urllib.request.Request(origin + '/api/' + path, headers={'Origin':origin, 'X-Voiceless-Token':token, 'Content-Type':'application/json'}, data=json.dumps(body).encode() if body is not None else None)
            try:
                reply = opener.open(req, timeout=20)
            except urllib.error.HTTPError as error:
                reply = error
            with reply:
                data = json.load(reply)
                assert reply.status == expected, (path, reply.status, data)
                return data
        yield daemon, api


def queued_meeting(api, path, state):
    def linked():
        meeting = api(path)
        if meeting['state'] == state and all(track['job_id'] for track in meeting['tracks']):
            return meeting
    return wait_for(linked, f'{path} to be {state} with both transcription jobs')


def run(cli, root, env, speech, expect_wasapi_denial=False):
    config = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
    config.write_text(config.read_text() + 'batch_backend="local"\n[console]\nport=0\n[audio]\npause_media=false\ncues=false\n', encoding='utf-8')
    with installed_daemon(cli, config, env, root / 'daemon.log') as (daemon, api):
        devices = api('meetings/sources')
        print('Voiceless input and output labels:', {kind:[d['label'] for d in devices[kind]] for kind in ('microphones','system_outputs')})
        def node(kind, variable):
            expected = os.environ[variable].casefold()
            matches = [d['node'] for d in devices[kind] if d['node'].casefold() == expected]
            assert len(matches) == 1, (kind, expected, devices)
            return matches[0]
        mic, output = node('microphones', 'VOICELESS_VAC_MIC'), node('system_outputs', 'VOICELESS_VAC_OUTPUT')
        instance = {'runtime_id':api('meetings')['runtime_id']}
        selection = dict(instance, microphone=mic, system=output)
        if expect_wasapi_denial:
            listing = api('meetings')
            directory = Path(listing['data_location'])
            assert not listing['meetings'] and not any(directory.rglob('*'))
            for attempt in range(1, 3):
                refusal = api('meetings/start', selection, 409)['error']
                assert refusal.startswith(f'the system denied access to the microphone "{mic}"'), refusal
                assert refusal.count('Windows Settings > Privacy > Microphone') == 1, refusal
                assert 'enable microphone access for desktop apps' in refusal, refusal
                # Windows can remember the first WASAPI refusal in its privacy settings.
                backend_refusal = '0x80070005' in refusal
                remembered_refusal = 'Microphone access is denied. In Windows Settings' in refusal
                assert backend_refusal or (attempt == 2 and remembered_refusal), refusal
                assert not api('meetings')['meetings'] and not any(directory.rglob('*'))
                print(f'WASAPI refusal {attempt}: HTTP 409, no meeting or track on disk: {refusal}')
            return
        feed, selected = line_index(1), line_index(2)
        rate1, rate2 = (int(sd.query_devices(i)['default_samplerate']) for i in (feed, selected))
        def capture():
            current = api('meetings/start', selection)
            assert current['scope']['microphone']['node'] == mic and current['scope']['system']['node'] == output
            path = 'meetings/' + current['id']
            def loud():
                view = api(path)
                if view['state'] != 'recording':
                    raise RuntimeError(view)
                return all(t['committed_bytes'] >= 32000 and (t['level'] or 0) > .01 for t in view['tracks'])
            wait_for(loud, 'both live tracks to receive the two tones')
            return path
        with render(feed, rate1, sine(rate1, 330)), render(selected, rate2, sine(rate2, 880)):
            path = capture()
            api(path + '/stop', instance)
            validate_tracks(queued_meeting(api, path, 'completed'))
            interrupted_path = capture()
        endpoint_switch = ['pwsh', '-NoProfile', '-NonInteractive', '-File', Path(__file__).with_name('endpoint.ps1')]
        subprocess.run([*endpoint_switch, 'disable'], check=True, timeout=30)
        try:
            wait_for(lambda: all(d['node'] != mic for d in api('meetings/sources')['microphones']),
                     'the selected capture endpoint to disappear')
            def interrupted():
                view = api(interrupted_path)
                return view if view['state'] == 'interrupted' else None
            view = wait_for(interrupted, 'hot-unplug detection')
            print('Hot-unplug:', view['failure'], view['actions'])
            assert 'recover' in view['actions'] and view['failure'], view
            missing = api('meetings/start', selection, 409)['error']
            assert mic.lower() in missing.lower() and 'unavailable' in missing.lower(), missing
            api(interrupted_path + '/recover', instance)
            validate_tracks(queued_meeting(api, interrupted_path, 'recovered'))
        finally:
            subprocess.run([*endpoint_switch, 'enable'], check=True, timeout=30)
        wait_for(lambda: any(d['node'] == mic for d in api('meetings/sources')['microphones']), 'reconnected microphone')
        with wave.open(str(speech), 'rb') as wav:
            assert wav.getsampwidth() == 2 and wav.getnchannels() == 1
            pcm, _ = audioop.ratecv(wav.readframes(wav.getnframes()), 2, 1, wav.getframerate(), rate1, None)
        clip_seconds = len(pcm) / (2*rate1)
        signal = bytes(4*rate1) + audioop.tostereo(pcm, 2, 1, 1) + bytes(4*rate1*6)
        # A separate explicit config must win even when the default config cannot parse.
        record_env = dict(env, XDG_CONFIG_HOME=str(root / 'cli-config'))
        default_config = Path(record_env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
        default_config.parent.mkdir(parents=True)
        default_config.write_text('[asr]\nbatch_backend="unused-default"\n', encoding='utf-8')
        capture_config = root / 'record.toml'
        capture_config.write_text(config.read_text() + f'source={json.dumps(mic)}\n', encoding='utf-8')
        command = [cli, '--config', capture_config]
        baseline = json.loads(subprocess.check_output([*command, 'transcribe', speech, '--backend', 'local', '--json'], env=record_env, timeout=75))['text']
        # Re-open PortAudio after the Windows endpoint was removed and registered again.
        sd._terminate()
        sd._initialize()
        with render(line_index(1), rate1, signal):
            record = [*command, 'record', '--seconds', str(clip_seconds + 3), '--backend', 'local', '--no-inject', '--json']
            recorded = json.loads(subprocess.check_output(record, env=record_env, timeout=90))['text']
        words = re.findall(r'[a-z]+', baseline.lower())
        spoken = ' '.join(re.findall(r'[a-z]+', recorded.lower()))
        assert len(words) >= 5 and any(' '.join(words[i:i+5]) in spoken for i in range(len(words)-4)), (baseline, recorded)
        print('Installed microphone recording recovered the reference speech:', recorded)
        previous_ids = [m['id'] for m in api('meetings')['meetings']]
        with denied_microphone():
            refusal = api('meetings/start', selection, 409)['error']
            assert refusal.startswith(f'the system denied access to the microphone "{mic}"'), refusal
            assert [m['id'] for m in api('meetings')['meetings']] == previous_ids
            print('Privacy refusal, HTTP 409, zero new meetings:', refusal)

        with render(line_index(1), rate1, sine(rate1, 330)), render(line_index(2), rate2, sine(rate2, 880)):
            crash_path = capture()
            committed = {t['kind']: t['committed_bytes'] for t in api(crash_path)['tracks']}
            daemon.kill()
            daemon.wait(timeout=15)

    with installed_daemon(cli, config, env, root / 'restart.log') as (_, api):
        restarted = {'runtime_id': api('meetings')['runtime_id']}
        assert restarted != instance, restarted
        pending = api(crash_path)
        assert pending['state'] == 'interrupted' and pending['failure']['kind'] == 'process-interrupted', pending
        assert {'recover', 'discard'}.issubset(pending['actions']), pending
        for track in pending['tracks']:
            assert track['job_id'] is None and track['committed_bytes'] >= committed[track['kind']], track
        api(crash_path + '/recover', restarted)
        validate_tracks(queued_meeting(api, crash_path, 'recovered'))
        print('Daemon restart preserved both committed tracks and waited for explicit recovery:', committed)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidates', type=Path)
    parser.add_argument('--expect-wasapi-denial', action='store_true')
    args = parser.parse_args()
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or not os.environ.get('VOICELESS_VAC_DEVICE'):
        parser.error('requires the hosted evaluation device')
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root/'config'), XDG_DATA_HOME=str(root/'data'), XDG_CACHE_HOME=str(root/'cache'))
        speech = recognition_fixture(root, env)
        dest = root / 'core'
        flags = ['/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART']
        version = json.loads((Path(__file__).resolve().parents[3] / 'desktop/tauri.conf.json').read_text())['version']
        installer = args.candidates / f'voiceless-core-v{version}-windows-x64.exe'
        subprocess.run([installer, *flags, f'/DIR={dest}'], check=True, timeout=120)
        try:
            run(dest/'voiceless.exe', root, env, speech, args.expect_wasapi_denial)
        finally:
            subprocess.run([dest/'unins000.exe', *flags], check=True, timeout=90)


if __name__ == '__main__':
    main()
