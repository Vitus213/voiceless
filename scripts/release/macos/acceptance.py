#!/usr/bin/env python3
"""Native acceptance of the macOS candidate on the hosted Apple Silicon guest.

Through the real privacy dialogs of the disposable guest: a microphone first
refused and then granted, two test-owned loopback endpoints for a dual-track
meeting and for their removal while recording, and delivery into a native
editor through Voiceless's bridge and through the installed daemon. Nothing
here grants or denies anything itself: the decisions are macOS's, made in its
own dialogs. Every check lands in evidence.json; the exit status says whether
a required one failed.

Usage: acceptance.py --candidates DIR --evidence DIR [--library libvoiceless_macos.a]
"""
import argparse
import array
import json
import math
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import wave

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from validate import recognition_fixture, verify_recognition  # noqa: E402

import sounddevice as sd  # noqa: E402

MICROPHONE, OUTPUT = 'BlackHole2ch_UID', 'BlackHole16ch_UID'
DRIVER = {MICROPHONE: 'BlackHole2ch', OUTPUT: 'BlackHole16ch'}
TEXT = 'Voiceless 中文 Ω'
CARGO = ['cargo', 'test', '--release', '--locked', '--manifest-path', 'rust/Cargo.toml']
PRIVACY = ('kTCCServiceMicrophone', 'kTCCServiceSystemAudioCapture', 'kTCCServiceListenEvent',
           'kTCCServicePostEvent', 'kTCCServiceAccessibility')
DECISION = {0: 'denied', 1: 'unknown', 2: 'allowed'}


class Refused(Exception):
    """A check did not hold."""


def wait_for(probe, description, timeout=25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.1)
    raise Refused(f'timed out waiting for {description}')


def refuse(condition, message):
    if not condition:
        raise Refused(message)


class Reader(threading.Thread):
    """Keeps a process's output as it arrives, so the main thread can wait for a line."""

    def __init__(self, stream):
        super().__init__(daemon=True)
        self.stream, self.lines = stream, []
        self.start()

    def run(self):
        for line in self.stream:
            self.lines.append(line)


class Evidence:
    def __init__(self, directory):
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)
        self.checks, self.notes = [], []
        self.log = (directory / 'commands.log').open('w', encoding='utf-8')

    def record(self, label, text):
        self.log.write(f'\n===== {time.strftime("%H:%M:%S")} {label}\n{text.rstrip()}\n')
        self.log.flush()

    def note(self, name, value):
        self.notes.append({'name': name, 'value': value})

    def check(self, name, required, action):
        started = time.monotonic()
        row = {'name': name, 'required': required}
        try:
            row.update(passed=True, detail=action() or {})
        except Exception as error:  # recorded, so that the other checks still run
            row.update(passed=False, error=f'{type(error).__name__}: {error}'[:2000])
        row['seconds'] = round(time.monotonic() - started, 1)
        self.checks.append(row)
        print(f'{"PASS" if row["passed"] else "FAIL"} {name} ({row["seconds"]} s)'
              + ('' if row['passed'] else f'\n     {row["error"]}'), flush=True)
        return row

    def failed(self):
        return [row['name'] for row in self.checks if row['required'] and not row['passed']]

    def write(self):
        (self.directory / 'evidence.json').write_text(
            json.dumps({'checks': self.checks, 'notes': self.notes}, indent=2, ensure_ascii=False) + '\n')


def quiet(action, *args, **options):
    """The action's result, or its error as text: for an attempt that may legitimately fail."""
    try:
        return action(*args, **options)
    except Exception as error:
        return f'{type(error).__name__}: {error}'


class Guest:
    """What the acceptance needs from the disposable guest."""

    def __init__(self, here, evidence):
        self.here, self.evidence = here, evidence
        self.temp = Path(os.environ['RUNNER_TEMP'])
        if (os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted'
                or not os.environ.get('GITHUB_REF_NAME', '').startswith('ci/macos-candidate/')):
            raise Refused('requires the hosted macOS guest of a ci/macos-candidate branch')
        self.audio = self.temp / 'voiceless-audio'
        if not self.audio.is_file():
            self.build(self.audio, [here / 'audio.swift'])

    def build(self, target, sources, objects=(), libraries=(), frameworks=('AppKit', 'ApplicationServices'), header=None):
        """Compile a Swift helper of the test for this guest."""
        module = self.temp / 'module-cache'
        module.mkdir(parents=True, exist_ok=True)
        command = ['xcrun', 'swiftc', '-module-cache-path', str(module), *map(str, sources), *map(str, objects), '-o', str(target)]
        if header is not None:
            command += ['-import-objc-header', str(header)]
        for framework in frameworks:
            command += ['-framework', framework]
        for library in libraries:
            command += ['-L', str(library.parent), '-l' + library.name[3:-2]]
        result = subprocess.run(command, capture_output=True, text=True, timeout=600)
        self.evidence.record(f'swiftc {target.name}', result.stdout + result.stderr)
        refuse(result.returncode == 0, f'{target.name} did not compile: {result.stderr[-1500:]}')

    def devices(self):
        return json.loads(subprocess.check_output([str(self.audio), 'list'], text=True, timeout=30))

    def device(self, uid):
        matches = [device for device in self.devices() if device['uid'] == uid]
        refuse(len(matches) == 1, f'{uid} is not exactly one device: {matches}')
        return matches[0]

    def default_input(self, uid):
        subprocess.run([str(self.audio), 'default-input', uid], check=True, capture_output=True, text=True, timeout=30)

    def restart_audio(self):
        subprocess.run(['sudo', 'launchctl', 'kickstart', '-k', 'system/com.apple.audio.coreaudiod'],
                       check=True, capture_output=True, text=True, timeout=60)

    def wait_device(self, uid, wanted, seconds):
        subprocess.run([str(self.audio), 'wait', uid, wanted, str(seconds)], check=True, capture_output=True, text=True, timeout=seconds + 30)
        sd._terminate()
        sd._initialize()

    def remove(self, uid):
        """Take a loopback endpoint away the way an unplugged interface goes: its driver leaves, the HAL restarts."""
        subprocess.run(['sudo', 'rm', '-rf', f'/Library/Audio/Plug-Ins/HAL/{DRIVER[uid]}.driver'],
                       check=True, capture_output=True, text=True, timeout=60)
        self.restart_audio()
        self.wait_device(uid, 'absent', 40)
        self.evidence.note(f'devices after removing {uid}', self.devices())

    def restore(self, uid):
        package = Path(os.environ['VOICELESS_LOOPBACK']) / f'{DRIVER[uid]}.v0.6.0.pkg'
        subprocess.run(['sudo', 'installer', '-pkg', str(package), '-target', '/'],
                       check=True, capture_output=True, text=True, timeout=240)
        self.restart_audio()
        self.wait_device(uid, 'present', 60)
        self.evidence.note(f'devices after restoring {uid}', self.devices())

    def decisions(self):
        """The privacy decisions macOS keeps for this user, read only."""
        database = Path.home() / 'Library/Application Support/com.apple.TCC/TCC.db'
        connection = sqlite3.connect('file:' + str(database).replace('?', '%3f') + '?mode=ro', uri=True)
        try:
            rows = connection.execute(
                f'select service, client, auth_value from access where service in ({",".join("?" * len(PRIVACY))}) order by service, client',
                PRIVACY).fetchall()
        finally:
            connection.close()
        return [{'service': service, 'client': client, 'decision': DECISION.get(value, str(value))}
                for service, client, value in rows]

    def reset_microphone(self):
        """Forget every microphone decision of this user through the supported tool: the next request asks again."""
        result = subprocess.run(['tccutil', 'reset', 'Microphone'], capture_output=True, text=True, timeout=60)
        self.evidence.record('tccutil reset Microphone', result.stdout + result.stderr)
        return (result.stdout + result.stderr).strip()


class Consent:
    """The process that answers the guest's real privacy dialogs."""

    def __init__(self, guest, directory):
        self.directory, self.log = directory, directory / 'consent.log'
        self.policy = directory / 'consent-policy.json'
        self.set('allow')
        self.process = subprocess.Popen([sys.executable, '-B', str(guest.here / 'consent.py'),
                                         str(self.policy), str(self.log), str(directory)])

    def set(self, microphone):
        self.policy.write_text(json.dumps({'microphone': microphone}) + '\n')

    def events(self):
        if not self.log.is_file():
            return []
        events = []
        for line in self.log.read_text().splitlines():
            if line.startswith('{'):
                try:
                    events.append(json.loads(line))
                except ValueError:
                    pass  # a line still being written
        return events

    def dialogs(self):
        return [event for event in self.events() if event.get('event') == 'dialog']

    def stop(self):
        (self.directory / 'stop').touch()
        try:
            self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()


def output_index(label):
    matches = [index for index, device in enumerate(sd.query_devices())
               if device['max_output_channels'] >= 2 and sd.query_hostapis(device['hostapi'])['name'] == 'Core Audio'
               and device['name'].startswith(label)]
    refuse(len(matches) == 1, f'{label} is not exactly one output: {[device["name"] for device in sd.query_devices()]}')
    return matches[0]


def stereo(pcm, rate, target):
    """Mono PCM16 at `rate` as stereo PCM16 at `target`, resampled linearly."""
    samples = array.array('h', pcm)
    step = rate / target
    out = array.array('h')
    for position in range(int(len(samples) / step)):
        at = position * step
        low = min(int(at), len(samples) - 1)
        high = min(low + 1, len(samples) - 1)
        value = round(samples[low] + (samples[high] - samples[low]) * (at - int(at)))
        out.extend((value, value))
    return out.tobytes()


def sine(rate, frequency):
    """One second of a tone, stereo PCM16 at `rate`."""
    period = array.array('h', (round(12000 * math.sin(math.tau * frequency * i / rate)) for i in range(rate)))
    return stereo(period.tobytes(), rate, rate)


class Output:
    """Raw stereo PCM16 on one output of the guest, for the length of a `with` block.
    `signal` builds the PCM for the device's rate; a looping signal repeats, another ends in silence."""

    def __init__(self, label, signal, loop):
        self.index = output_index(label)
        self.rate = int(sd.query_devices(self.index)['default_samplerate'])
        self.signal, self.loop = signal(self.rate), loop

    def __enter__(self):
        state = {'offset': 0}

        def fill(out, frames, timing, status):
            size = frames * 4
            start = state['offset']
            if self.loop:
                chunk = (self.signal * ((start + size) // len(self.signal) + 1))[start:start + size]
                state['offset'] = (start + size) % len(self.signal)
            else:
                chunk = self.signal[start:start + size]
                state['offset'] = start + size
            out[:] = chunk if len(chunk) == size else chunk + bytes(size - len(chunk))

        self.stream = sd.RawOutputStream(device=self.index, channels=2, samplerate=self.rate, dtype='int16', callback=fill)
        self.stream.start()
        return self

    def __exit__(self, *_):
        self.stream.stop()
        self.stream.close()


class Outputs:
    """Several outputs started together and stopped together."""

    def __init__(self, *outputs):
        self.outputs, self.running = outputs, []

    def __enter__(self):
        try:
            for output in self.outputs:
                self.running.append(output.__enter__())
        except BaseException:
            self.__exit__()
            raise
        return self

    def __exit__(self, *_):
        for output in reversed(self.running):
            output.__exit__()
        self.running = []


def tones(microphone=True, output=True):
    """The two fixture signals: 330 Hz at the microphone line, 880 Hz at the system output line."""
    return Outputs(*([Output('BlackHole 2ch', lambda rate: sine(rate, 330), True)] if microphone else []),
                   *([Output('BlackHole 16ch', lambda rate: sine(rate, 880), True)] if output else []))


def magnitude(samples, hz):
    step = math.tau * hz / 16000
    return math.hypot(sum(x * math.sin(step * i) for i, x in enumerate(samples)),
                      sum(x * math.cos(step * i) for i, x in enumerate(samples))) / len(samples)


def validate_tracks(meeting):
    """Both tracks hold their own tone: 330 Hz on the microphone, 880 Hz on the system output."""
    tracks = {track['kind']: track for track in meeting['tracks']}
    refuse(set(tracks) == {'microphone', 'system'}, f'unexpected tracks: {sorted(tracks)}')
    report = {}
    for kind, track in tracks.items():
        path = Path(track['path'])
        if not path.is_absolute():
            path = Path(meeting.get('data_location') or '') / path
        with wave.open(str(path), 'rb') as wav:
            refuse((wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2),
                   f'{kind} track is {wav.getframerate()} Hz, {wav.getnchannels()} channels, {wav.getsampwidth()} bytes')
            samples = array.array('h', wav.readframes(wav.getnframes()))
        refuse(len(samples) * 2 == track['committed_bytes'] and len(samples) >= 16000,
               f'{kind} track holds {len(samples)} samples, committed {track["committed_bytes"]}')
        at330, at880 = magnitude(samples, 330), magnitude(samples, 880)
        own, other = (at330, at880) if kind == 'microphone' else (at880, at330)
        report[kind] = {'samples': len(samples), '330_hz': round(at330, 1), '880_hz': round(at880, 1), 'job': bool(track.get('job_id'))}
        refuse(own > 10 and own > 3 * other, f'{kind} track carries the wrong signal: 330 Hz {at330:.1f}, 880 Hz {at880:.1f}')
    return report


class Daemon:
    """The installed daemon under private roots."""

    def __init__(self, cli, config, env, log_path, evidence):
        self.evidence, self.log_path = evidence, log_path
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with log_path.open('w') as stream:
            self.process = subprocess.Popen([str(cli), '--config', str(config), 'daemon', '--no-inject'], env=env,
                                            stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            self.origin = wait_for(self.announced, 'the installed daemon')
            page = self.opener.open(self.origin + '/', timeout=10).read()
            self.token = re.search(rb'name="voiceless-token" content="([^"]+)"', page)[1].decode()
        except BaseException:
            self.stop()
            raise

    def announced(self):
        text = self.log_path.read_text(errors='replace')
        refuse(self.process.poll() is None, f'the daemon exited: {text[-1500:]}')
        found = re.search(r'Voiceless console: (http://127\.0\.0\.1:\d+)/', text)
        return found[1] if found else None

    def api(self, path, body=None, expected=200):
        """Call the console; returns its status and body, refusing anything but `expected`."""
        request = urllib.request.Request(
            self.origin + '/api/' + path, data=json.dumps(body).encode() if body is not None else None,
            headers={'Origin': self.origin, 'X-Voiceless-Token': self.token, 'Content-Type': 'application/json'})
        try:
            reply = self.opener.open(request, timeout=75)
        except urllib.error.HTTPError as error:
            reply = error
        with reply:
            status, data = reply.status, json.load(reply)
        refuse(status == expected, f'{path} answered {status}: {json.dumps(data, ensure_ascii=False)[:700]}')
        return status, data

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
        self.evidence.record('daemon log', self.log_path.read_text(errors='replace')[-8000:])


class Editor:
    """The native editor fixture, steered through its files."""

    def __init__(self, executable, directory):
        self.directory, self.identifier = directory, 0
        directory.mkdir(parents=True, exist_ok=True)
        self.process = subprocess.Popen([str(executable), 'serve', str(directory)], stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL, start_new_session=True)

    def status(self):
        try:
            return json.loads((self.directory / 'status.json').read_text())
        except (OSError, ValueError):
            return {}

    def command(self, field):
        """Bring the editor forward with field `field` editing, or hide it (`None`)."""
        self.identifier += 1
        (self.directory / 'command.json').write_text(json.dumps({'id': self.identifier, 'field': field}))

        def applied():
            state = self.status()
            if state.get('applied') != self.identifier:
                return None
            if field is None:
                return state if state.get('visible') is False else None
            return state if state.get('editing') == field and state.get('frontmost') and state.get('key') else None
        return wait_for(applied, 'the editor to ' + ('hide' if field is None else f'edit field {field}'), timeout=15)

    def stop(self):
        (self.directory / 'stop').touch()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()


def private_roots(root, home=None):
    """Private XDG roots; HOME is replaced too when a private home is given."""
    env = dict(os.environ)
    for key in ('DATA', 'CACHE', 'CONFIG', 'STATE', 'RUNTIME'):
        (root / key).mkdir(mode=0o700, parents=True)
        env['XDG_' + key + ('_DIR' if key == 'RUNTIME' else '_HOME')] = str(root / key)
    if home is not None:
        env['HOME'] = str(home)
    return env


def screenshot(path):
    subprocess.run(['screencapture', '-x', '-t', 'png', str(path)], capture_output=True, timeout=30)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--candidates', type=Path, required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--library', type=Path, help='libvoiceless_macos.a of the build, for the bridge guard')
    arguments = parser.parse_args()
    here = Path(__file__).resolve().parent
    repository = here.parents[2]
    evidence = Evidence(arguments.evidence.resolve())
    with tempfile.TemporaryDirectory(prefix='voiceless-acceptance-') as directory:
        root = Path(directory)
        guest = Guest(here, evidence)
        consent = Consent(guest, evidence.directory)  # its screenshots of the dialogs stay with the evidence
        try:
            accept(guest, here, repository, arguments, evidence, root, consent)
        except Exception as error:  # outside a check: still recorded, still an exit status
            evidence.checks.append({'name': 'the acceptance ran to its end', 'required': True, 'passed': False,
                                    'error': f'{type(error).__name__}: {error}'[:2000], 'seconds': 0})
            print(f'FAIL the acceptance ran to its end\n     {type(error).__name__}: {error}', flush=True)
        finally:
            consent.stop()
            evidence.note('privacy decisions at the end', quiet(guest.decisions))
            evidence.note('consent log', consent.events())
            evidence.write()
    failed = evidence.failed()
    print(json.dumps({'checks': len(evidence.checks), 'failed': failed}, ensure_ascii=False), flush=True)
    sys.exit(1 if failed else 0)


def accept(guest, here, repository, arguments, evidence, root, consent):
    version = json.loads((repository / 'desktop/tauri.conf.json').read_text())['version']
    microphone, output = f'coreaudio:{MICROPHONE}', f'coreaudio:{OUTPUT}'
    test_env = private_roots(root / 'tests')
    os.chdir(repository)

    def cargo(target, name, ignored, extra=None, timeout=240):
        """One test of the workspace, by its exact name."""
        command = CARGO + ['--test', target, name, '--', '--exact', '--nocapture'] + (['--ignored'] if ignored else [])
        try:
            process = subprocess.run(command, env=dict(test_env, **(extra or {})), capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            evidence.record(' '.join(command), f'TIMEOUT after {timeout} s\n{error.stdout or ""}')
            raise Refused(f'{name} did not end within {timeout} s')
        evidence.record(' '.join(command), process.stdout + process.stderr)
        verdict = re.search(r'^test result: .*$', process.stdout, re.M)
        refuse(process.returncode == 0 and verdict and 'test result: ok. 1 passed;' in verdict[0],
               f'{verdict[0] if verdict else "no verdict"} (exit {process.returncode})')
        return verdict[0]

    def contract(name, ignored=False, extra=None, timeout=240):
        return {'verdict': cargo('desktop_contract', 'macos::' + name, ignored, extra, timeout)}

    def meeting(name, extra=None, timeout=150, ignored=True):
        return {'verdict': cargo('capture_workflows', 'native_meeting::' + name, ignored, extra, timeout)}

    def fixture():
        return {'VOICELESS_TEST_MEETING_MIC': microphone, 'VOICELESS_TEST_MEETING_OUTPUT': output}

    evidence.note('devices', guest.devices())
    evidence.note('privacy decisions as the guest came', quiet(guest.decisions))

    # ---- the workspace's own tests ------------------------------------------------------
    evidence.check('contract: the CLI carries its microphone usage description', True,
                   lambda: contract('macos_cli_embeds_microphone_usage_description'))
    evidence.check('meetings: the catalog names the guest\'s devices and refuses an absent source', True,
                   lambda: meeting('catalog_resolves_host_ids_and_rejects_a_missing_selection_before_recording', ignored=False))
    evidence.check('microphone as the image granted it: the first capture returns PCM and releases the device', True,
                   lambda: contract('macos_first_capture_returns_pcm_and_releases_the_device', True, timeout=90))

    def library_dual_track():
        name = 'selected_fixture_tracks_drain_and_release_the_meeting_lease'
        before = len(consent.dialogs())
        with tones():
            # The system output is tapped for the first time here: an attempt refused while a
            # consent prompt was answered gets one more.
            first = quiet(meeting, name, fixture())
            verdict = meeting(name, fixture()) if isinstance(first, str) else first
        refuse(not isinstance(verdict, str), f'{verdict}')
        return {'first_attempt': 'passed' if not isinstance(first, str) else first, **verdict,
                'dialogs': consent.dialogs()[before:]}
    evidence.check('dual-track meeting of the two loopback lines through the library', True, library_dual_track)

    def library_refused():
        before = len(consent.dialogs())
        consent.set('deny')
        reset = guest.reset_microphone()
        verdict = quiet(contract, 'macos_denied_microphone_reports_privacy_settings', True, timeout=150)
        refused = quiet(meeting, 'denied_fixture_microphone_identifies_source_and_rolls_back_start',
                        {'VOICELESS_TEST_MEETING_MIC': microphone})
        row = {'reset': reset, 'verdict': verdict, 'meeting': refused,
               'dialogs': consent.dialogs()[before:], 'decisions': quiet(guest.decisions)}
        refuse(not (isinstance(verdict, str) or isinstance(refused, str)),
               json.dumps(row, ensure_ascii=False, default=str)[:1800])
        return row
    evidence.check('microphone refused through the system dialog', True, library_refused)

    def library_granted():
        before = len(consent.dialogs())
        consent.set('allow')
        reset = guest.reset_microphone()
        return {'reset': reset,
                'verdict': contract('macos_first_capture_returns_pcm_and_releases_the_device', True, timeout=150),
                'dialogs': consent.dialogs()[before:], 'decisions': quiet(guest.decisions)}
    evidence.check('microphone granted through the system dialog', True, library_granted)

    def removed():
        guest.device(MICROPHONE)
        guest.default_input(MICROPHONE)
        command = CARGO + ['--test', 'desktop_contract', 'macos::macos_unplugging_default_input_ends_capture',
                           '--', '--exact', '--nocapture', '--ignored']
        process = subprocess.Popen(command, env=test_env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        reader = Reader(process.stdout)

        def asking():
            if process.poll() is not None:
                raise Refused(f'the test ended before it asked: {"".join(reader.lines)[-900:]}')
            return 'Unplug the selected microphone now' in ''.join(reader.lines)
        try:
            wait_for(asking, 'the capture test to open the default microphone', timeout=90)
            guest.remove(MICROPHONE)
            process.wait(timeout=80)
        finally:
            if process.poll() is None:
                process.kill()
        reader.join(10)
        text = ''.join(reader.lines)
        evidence.record('unplugging test', text)
        verdict = re.search(r'^test result: .*$', text, re.M)
        refuse(process.returncode == 0 and verdict and 'ok.' in verdict[0], f'{verdict[0] if verdict else text[-1200:]} (exit {process.returncode})')
        return {'verdict': verdict[0]}
    evidence.check('capture ends when the selected microphone is removed', True, removed)
    evidence.check('the removed loopback endpoint is put back', True, lambda: {'device': (guest.restore(MICROPHONE), guest.device(MICROPHONE))[1]})

    # ---- the installed candidate ---------------------------------------------------------
    home = root / 'installed'
    env = private_roots(home, home)
    speech = recognition_fixture(home, env)
    config = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
    config.write_text(config.read_text() + 'batch_backend="local"\n[console]\nport=0\n[hotkey]\ntoggle_key="Ctrl+Shift+KEY_F19"\n'
                      '[audio]\npause_media=false\ncues=false\n')
    installed = {}

    def install(kind):
        image, = arguments.candidates.glob(f'voiceless-{kind}-v{version}-macos-arm64.dmg')
        point = root / f'mount-{kind}'
        point.mkdir()
        subprocess.run(['hdiutil', 'attach', '-readonly', '-nobrowse', '-noautoopen', '-mountpoint', str(point), str(image)],
                       check=True, capture_output=True, text=True, timeout=180)
        try:
            if kind == 'cli':
                subprocess.run([str(point / 'install-cli.command')], env=env, check=True, capture_output=True, text=True, timeout=240)
                installed['cli'] = (home / '.local/bin/voiceless').resolve()
                core = installed['cli']
            else:
                shutil.copytree(point / 'Voiceless.app', home / 'Voiceless.app', symlinks=True)
                installed['app'] = home / 'Voiceless.app'
                installed['desktop'] = core = home / 'Voiceless.app/Contents/MacOS/voiceless'
        finally:
            subprocess.run(['hdiutil', 'detach', str(point)], check=True, capture_output=True, text=True, timeout=180)
        reported = subprocess.check_output([str(core), '--version'], env=env, text=True, timeout=60).strip()
        refuse(reported == f'voiceless {version}', f'unexpected installed version: {reported}')
        verify_recognition(core, speech, env)
        return {'version': reported, 'core': str(core.relative_to(home)), 'fixture': 'transcribed offline'}
    evidence.check('installed CLI: version and offline transcription of the fixture', True, lambda: install('cli'))
    evidence.check('installed desktop core: version and offline transcription of the fixture', True, lambda: install('desktop'))

    daemon = None

    def running():
        nonlocal daemon
        daemon = Daemon(installed['cli'], config, env, root / 'daemon.log', evidence)
        doctor = {check['name']: check for check in daemon.api('doctor')[1]}
        _, audio = daemon.api('desktop/audio')
        _, status = daemon.api('status')
        report = {'doctor': doctor, 'hotkey': status['hotkey'], 'version': status['version'],
                  'audio': {'effective': audio['effective'], 'system_default': audio['system_default'],
                            'microphones': [device['label'] for device in audio['microphones']]}}
        refuse(doctor['microphone']['present'], f'the daemon does not see the microphone: {doctor["microphone"]}')
        refuse(doctor['global input permission']['present'], f'the daemon does not see the input permissions: {doctor["global input permission"]}')
        refuse(status['hotkey']['error'] is None and status['hotkey']['keyboards'] >= 1, f'the hotkey listener is not active: {status["hotkey"]}')
        return report
    evidence.check('installed daemon: microphone, input permissions, hotkey listener and audio sources', True, running)
    if daemon is None:
        return
    try:
        instance = {'runtime_id': daemon.api('meetings')[1]['runtime_id']}
        catalog = daemon.api('meetings/sources')[1]

        def node(kind, label):
            matches = [source['node'] for source in catalog[kind] if source['label'] == label]
            refuse(len(matches) == 1, f'{label} is not exactly one {kind}: {catalog}')
            return matches[0]
        mic, line = node('microphones', 'BlackHole 2ch'), node('system_outputs', 'BlackHole 16ch')
        selection = dict(instance, microphone=mic, system=line)

        def recording(selection_, wait=True):
            """Start a meeting and let both tracks receive their tone; returns its path."""
            current = daemon.api('meetings/start', selection_)[1]
            refuse(current['scope']['microphone']['node'] == mic, f'wrong microphone: {current["scope"]}')
            path = 'meetings/' + current['id']

            def loud():
                view = daemon.api(path)[1]
                refuse(view['state'] == 'recording', f'the meeting left the recording state: {view}')
                wanted = {'microphone', 'system'} if selection_.get('system') else {'microphone'}
                return all(track['committed_bytes'] >= 32000 and (track['level'] or 0) > .01 for track in view['tracks']) \
                    and {track['kind'] for track in view['tracks']} == wanted
            if wait:
                wait_for(loud, 'the tracks to receive their tone')
            return path

        def settled(path, state, validate=True):
            def ready():
                view = daemon.api(path)[1]
                return view if view['state'] == state and all(track.get('job_id') for track in view['tracks']) else None
            view = wait_for(ready, f'the meeting to be {state}')
            return {'state': view['state'], 'tracks': validate_tracks(view) if validate else [track['kind'] for track in view['tracks']]}

        def refused_daemon():
            before = len(consent.dialogs())
            consent.set('deny')
            reset = guest.reset_microphone()
            _, reply = daemon.api('meetings/start', selection, 409)
            refuse('denied' in reply['error'].lower() and mic.lower() in reply['error'].lower(), f'unexpected refusal: {reply}')
            doctor = {check['name']: check for check in daemon.api('doctor')[1]}
            refuse(not doctor['microphone']['present'] and 'Privacy & Security > Microphone' in doctor['microphone']['detail'],
                   f'the doctor does not name the refused microphone: {doctor["microphone"]}')
            refuse(daemon.api('meetings')[1]['meetings'] == [], 'a refused meeting left a record')
            return {'reset': reset, 'refusal': reply['error'], 'doctor': doctor['microphone'],
                    'dialogs': consent.dialogs()[before:], 'decisions': quiet(guest.decisions)}
        evidence.check('installed daemon: microphone refused through the system dialog', True, refused_daemon)

        def granted_daemon():
            before = len(consent.dialogs())
            consent.set('allow')
            reset = guest.reset_microphone()
            with tones(output=False):
                path = recording(dict(instance, microphone=mic))
                daemon.api(path + '/stop', instance)
            settled(path, 'completed', validate=False)
            doctor = {check['name']: check for check in daemon.api('doctor')[1]}
            refuse(doctor['microphone']['present'], f'the microphone stays refused: {doctor["microphone"]}')
            return {'reset': reset, 'doctor': doctor['microphone'], 'dialogs': consent.dialogs()[before:],
                    'decisions': quiet(guest.decisions)}
        evidence.check('installed daemon: microphone granted through the system dialog', True, granted_daemon)

        def dual_track():
            # The system output is tapped for the first time here: an attempt refused while a
            # consent prompt was answered gets one more.
            with tones():
                try:
                    path = recording(selection)
                except Refused:
                    path = recording(selection)
                daemon.api(path + '/stop', instance)
            return settled(path, 'completed')
        evidence.check('installed daemon: dual-track meeting of both loopback lines', True, dual_track)

        def interrupted():
            with tones():
                path = recording(selection)
            guest.remove(OUTPUT)
            try:
                def ended():
                    view = daemon.api(path)[1]
                    return view if view['state'] != 'recording' else None
                view = wait_for(ended, 'the meeting to end with the removed output', timeout=30)
                _, missing = daemon.api('meetings/start', selection, 409)
                refuse('unavailable' in missing['error'].lower(), f'unexpected refusal for the removed output: {missing}')
                refuse('recover' in view['actions'] and view['failure'], f'the interrupted meeting offers no recovery: {view}')
                daemon.api(path + '/recover', instance)
                return {'state': view['state'], 'failure': view['failure'], 'actions': view['actions'],
                        'missing': missing['error'], **settled(path, 'recovered')}
            finally:
                guest.restore(OUTPUT)
        evidence.check('installed daemon: meeting interrupted by the removed output, then recovered', True, interrupted)

        # ---- delivery into a native editor ----------------------------------------------
        if arguments.library and arguments.library.is_file():

            def guard():
                stub = root / 'editor-stubs.o'
                subprocess.run(['xcrun', 'clang', '-c', str(here / 'editor.c'), '-o', str(stub)], check=True, capture_output=True, timeout=120)
                executable = root / 'voiceless-editor-guard'
                guest.build(executable, [here / 'editor.swift', here / 'editor-guard.swift'], objects=[stub],
                            libraries=[arguments.library], header=here / 'editor.h',
                            frameworks=('AppKit', 'ApplicationServices', 'AVFoundation', 'CoreAudio'))
                report = json.loads(subprocess.check_output([str(executable), 'guard', str(root)], text=True, timeout=90))
                expected = {'refused_after_focus_change': 1, 'second_after_refusal': '', 'first_after_refusal': '',
                            'delivered': 0, 'first_after_delivery': TEXT, 'second_after_delivery': '',
                            'clipboard_after_delivery': 'unchanged clipboard', 'protected_focus_key': 0}
                wrong = {key: report.get(key) for key, value in expected.items() if report.get(key) != value}
                refuse(not wrong, f'{wrong}, of {report}')
                return report
            evidence.check('delivery: the bridge delivers to the original field only and puts the clipboard back', True, guard)

        executable = root / 'voiceless-editor'
        guest.build(executable, [here / 'editor.swift', here / 'editor-serve.swift'])
        editor = Editor(executable, root / 'editor')
        try:

            def inject():
                return daemon.api('inject/test', {'text': TEXT, 'delay_ms': 0})[1]

            def delivered():
                editor.command(0)
                reply = inject()
                refuse(reply['ok'], f'not delivered: {reply}')

                def arrived():
                    fields = editor.status().get('fields')
                    return fields if fields and fields[0] == TEXT else None
                fields = wait_for(arrived, 'the text to arrive in the first field')
                screenshot(evidence.directory / 'editor-after-delivery.png')
                return {'reply': reply, 'fields': fields, 'fixture': editor.status()}
            evidence.check('delivery: the installed daemon pastes into the native editor', True, delivered)

            def protected():
                editor.command(2)
                reply = inject()
                fields = editor.status().get('fields') or ['', '', '']
                refuse(not reply['ok'] and fields[2] == '', f'the protected field was not refused: {reply} {fields}')
                return {'reply': reply, 'fields': fields}
            evidence.check('delivery: the installed daemon refuses a protected field', True, protected)

            def spoken():
                """The installed CLI hears the reference speech played into the microphone line."""
                guest.device(MICROPHONE)
                capture = root / 'record.toml'
                capture.write_text(config.read_text() + f'source={json.dumps(mic)}\n')
                with wave.open(str(speech), 'rb') as wav:
                    pcm, rate = wav.readframes(wav.getnframes()), wav.getframerate()
                baseline = json.loads(subprocess.check_output(
                    [str(installed['cli']), '--config', str(capture), 'transcribe', str(speech), '--backend', 'local', '--json'],
                    env=env, text=True, timeout=120))['text']
                seconds = round(len(pcm) / (2 * rate)) + 3
                line = Output('BlackHole 2ch', lambda target: stereo(pcm, rate, target) + bytes(16 * target), loop=False)
                with line:
                    recorded = json.loads(subprocess.check_output(
                        [str(installed['cli']), '--config', str(capture), 'record', '--seconds', str(seconds),
                         '--backend', 'local', '--no-inject', '--json'], env=env, text=True, timeout=seconds + 120))['text']
                words = re.findall(r'[a-z]+', baseline.lower())
                said = ' '.join(re.findall(r'[a-z]+', recorded.lower()))
                refuse(len(words) >= 5 and any(' '.join(words[i:i + 5]) in said for i in range(len(words) - 4)),
                       f'the recording did not recover the speech: baseline {baseline!r}, recorded {recorded!r}')
                return {'baseline': baseline, 'recorded': recorded, 'seconds': seconds}
            evidence.check('installed CLI: a recording of the microphone line recovers the reference speech', True, spoken)
        finally:
            editor.stop()

        # ---- the desktop shell -------------------------------------------------------------
        def shell():
            """The window, the status item and the one resident desktop, as on a person's Mac."""
            executable = root / 'voiceless-shell'
            guest.build(executable, [here / 'shell.swift'], frameworks=('AppKit', 'ApplicationServices'))
            desktop = installed['app'] / 'Contents/MacOS/voiceless-desktop'
            with (root / 'desktop.log').open('w') as stream:
                host = subprocess.Popen([str(desktop), '--autostart'], env=env, stdout=stream, stderr=subprocess.STDOUT,
                                        start_new_session=True)
            try:
                descriptor = Path(env['XDG_DATA_HOME']) / 'voiceless/console-endpoint.json'
                wait_for(descriptor.is_file, 'the desktop to start its daemon', timeout=60)
                original = json.loads(descriptor.read_text())
                result = subprocess.run([str(executable), str(desktop), str(host.pid), str(descriptor)],
                                        capture_output=True, text=True, timeout=120)
                evidence.record('desktop shell', result.stdout + result.stderr + (root / 'desktop.log').read_text()[-3000:])
                refuse(result.returncode == 0, f'{result.stdout[-800:]} {result.stderr[-800:]}')
                refuse(json.loads(descriptor.read_text())['runtime_id'] == original['runtime_id'], 'reopening replaced the daemon')
                return {'lines': result.stdout.splitlines(), 'runtime_id': original['runtime_id']}
            finally:
                for pid in subprocess.run(['pgrep', '-f', str(installed['app'])], capture_output=True, text=True).stdout.split():
                    subprocess.run(['kill', pid], capture_output=True)
                try:
                    host.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    host.kill()
        evidence.check('installed desktop: window, status item and one resident desktop', False, shell)
    finally:
        daemon.stop()


if __name__ == "__main__":
    main()
