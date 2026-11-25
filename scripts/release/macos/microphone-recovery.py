"""Check committed audio from an explicitly selected, preauthorized virtual microphone."""
import array
import json
from pathlib import Path
import subprocess
import sys
import wave

from acceptance import (Daemon, Evidence, Guest, MICROPHONE, magnitude, private_roots,
                        recognition_fixture, refuse, tones, wait_for)


def start_line(daemon, mic):
    instance = {'runtime_id': daemon.api('meetings')[1]['runtime_id']}
    selected = daemon.api('meetings/start', dict(instance, microphone=mic))[1]
    refuse(selected['scope']['microphone']['node'] == mic and len(selected['tracks']) == 1,
           'the recording must contain only the test microphone')
    path = 'meetings/' + selected['id']

    def audible():
        view = daemon.api(path)[1]
        refuse(view['state'] == 'recording', f'capture ended early: {view}')
        track, = view['tracks']
        return track['committed_bytes'] if (track['level'] or 0) > 0.01 and track['committed_bytes'] >= 32000 else None
    return path, wait_for(audible, 'the 330 Hz microphone to commit at least a second of audio'), instance


def recover_line(daemon, path, committed, instance, failure_kind=None):
    def interrupted():
        view = daemon.api(path)[1]
        return view if view['state'] == 'interrupted' else None
    pending = wait_for(interrupted, 'terminal interrupted state', timeout=45)
    track, = pending['tracks']
    refuse({'recover', 'discard'}.issubset(pending['actions']) and not track.get('job_id'),
           f'unexpected actions or premature submission: {pending}')
    refuse(pending['failure'] and (not failure_kind or pending['failure']['kind'] == failure_kind),
           f'unexpected cause: {pending["failure"]}')
    refuse(track['committed_bytes'] >= committed, 'previously committed bytes were lost')
    daemon.api(path + '/recover', instance)

    def queued():
        view = daemon.api(path)[1]
        return view if view['state'] == 'recovered' and view['tracks'][0].get('job_id') else None
    view = wait_for(queued, 'the recovered microphone to enter the transcription queue')
    track, = view['tracks']
    audio = Path(view['data_location']) / track['path']
    with wave.open(str(audio), 'rb') as wav:
        refuse((wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2), 'wrong WAV format')
        samples = array.array('h', wav.readframes(wav.getnframes()))
    own, other = magnitude(samples, 330), magnitude(samples, 880)
    refuse(len(samples) * 2 == track['committed_bytes'] and len(samples) * 2 >= committed,
           'the recovered WAV and its journal disagree')
    refuse(own > 10 and own > 3 * other, 'the recovered WAV did not contain the test microphone tone')
    return dict(cause=pending['failure']['kind'], committed_before=committed, recovered_bytes=len(samples) * 2,
                hz330=round(own, 1), hz880=round(other, 1), queued=True)


def verify(root, evidence):
    here = Path(__file__).resolve().parent
    guest = Guest(here, evidence)
    permission = subprocess.check_output(['swift', '-e', 'import AVFoundation; print(AVCaptureDevice.authorizationStatus(for: .audio).rawValue)'], text=True).strip()
    refuse(permission == '3', f'the hosted session requires a microphone decision: {permission}')
    env = private_roots(root / 'user', root / 'home')
    recognition_fixture(root, env)
    cfg = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
    cfg.write_text(cfg.read_text() + 'batch_backend="local"\n[console]\nport=0\n[audio]\npause_media=false\ncues=false\n')
    cli = root / 'home/.local/bin/voiceless'
    daemon = Daemon(cli, cfg, env, root / 'initial.log', evidence)
    try:
        catalog = daemon.api('meetings/sources')[1]['microphones']
        mic, = [entry['node'] for entry in catalog if entry['label'] == 'BlackHole 2ch']
        refuse(mic == 'coreaudio:' + MICROPHONE, f'unexpected microphone: {mic}')
        with tones(output=False):
            path, committed, original = start_line(daemon, mic)
            daemon.process.kill()
            daemon.process.wait(timeout=12)
    finally:
        daemon.stop()
    daemon = Daemon(cli, cfg, env, root / 'restarted.log', evidence)
    try:
        instance = {'runtime_id': daemon.api('meetings')[1]['runtime_id']}
        refuse(instance != original, 'the runtime identifier did not change after restart')
        evidence.check('process restart retains committed microphone audio until recovery', True,
                       lambda: recover_line(daemon, path, committed, instance, 'process-interrupted'))
        with tones(output=False):
            path, committed, instance = start_line(daemon, mic)
        try:
            guest.remove(MICROPHONE)
            def disconnected():
                result = recover_line(daemon, path, committed, instance)
                missing = daemon.api('meetings/start', dict(instance, microphone=mic), 409)[1]['error']
                refuse(mic.lower() in missing.lower() and 'unavailable' in missing.lower(), f'wrong refusal: {missing}')
                return dict(result, missing_input=missing)
            evidence.check('losing the selected microphone preserves audio and refuses the missing input', True, disconnected)
        finally:
            guest.restore(MICROPHONE)
    finally:
        daemon.stop()


if __name__ == '__main__':
    root = Path(sys.argv[1]).resolve()
    evidence = Evidence(Path(sys.argv[2]))
    try:
        verify(root, evidence)
    finally:
        evidence.write()
    print('MICROPHONE_RECOVERY', json.dumps(evidence.checks), flush=True)
    sys.exit(bool(evidence.failed()))
