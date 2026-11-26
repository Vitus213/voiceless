"""Remove only the selected Core Audio output while the installed daemon records two tones."""
from contextlib import contextmanager
import importlib
import subprocess
import uuid

import sounddevice as sd

import consent
from acceptance import MICROPHONE, OUTPUT, Output, Outputs, Refused, refuse, sine, tones, wait_for

allowed = importlib.import_module('system-audio-allowed')
requests = importlib.import_module('system-audio-consent')
OUTPUT_NAME = 'Voiceless acceptance output'


@contextmanager
def removable(guest, daemon, uid):
    def listed():
        return any(d['node'] == 'coreaudio:' + uid for d in daemon.api('meetings/sources')[1]['system_outputs'])

    process = subprocess.Popen([guest.audio, 'temporary-output', OUTPUT, uid], stdin=subprocess.PIPE)
    try:
        def visible():
            refuse(process.poll() is None, 'the temporary device process exited before its output appeared')
            return listed()
        try:
            wait_for(visible, 'the installed daemon to list the new output', timeout=30)
        except Refused:
            guest.evidence.note('Device lists while waiting for the aggregate', {
                'uid': uid, 'process_status': process.poll(), 'hal': guest.devices(),
                'meeting_sources': daemon.api('meetings/sources')[1],
            })
            raise
        # PortAudio caches its device indices. Refresh before either signal stream is open.
        sd._terminate()
        sd._initialize()
        yield
    finally:
        process.stdin.close()
        try:
            refuse(process.wait(timeout=10) == 0, 'the test output could not be created or removed')
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        wait_for(lambda: not listed(), 'the selected output to leave the installed device list', timeout=30)
        sd._terminate()
        sd._initialize()


def signals():
    return Outputs(Output('BlackHole 2ch', lambda rate: sine(rate, 330), True),
                   Output(OUTPUT_NAME, lambda rate: sine(rate, 880), True))


def start(daemon, selection):
    attempt = requests.Start(daemon, selection)
    dialog = allowed.asking(attempt)
    if dialog:
        button = consent.button(dialog['buttons'], 'allow')
        refuse(button and dialog['process'] in consent.HOSTS and requests.SYSTEM_AUDIO.search(dialog['text']),
               f'unexpected permission dialog: {dialog}')
        consent.applescript(consent.CLICK, dialog['process'], dialog['window'], button)
    reply = requests.reply_of(attempt.answered())
    if dialog and reply['status'] == 409:
        reply = requests.reply_of(requests.Start(daemon, selection).answered())
    refuse(reply['status'] == 200, f'dual-track start returned {reply}')
    path = 'meetings/' + reply['id']
    view = wait_for(lambda: allowed.loud(daemon, path), 'both selected lines to carry a signal')
    refuse(view['scope']['microphone']['node'] == selection['microphone'] and
           view['scope']['system']['node'] == selection['system'], f'wrong sources: {view["scope"]}')
    return path, view


def sizes(view):
    return {track['kind']: track['committed_bytes'] for track in view['tracks']}


def coreaudiod():
    return subprocess.check_output(['pgrep', '-x', 'coreaudiod'], text=True).splitlines()


def verify(daemon, evidence, guest, _fixture):
    uid = 'dev.voiceless.acceptance.' + str(uuid.uuid4())
    microphone, output = 'coreaudio:' + MICROPHONE, 'coreaudio:' + uid
    instance = {'runtime_id': daemon.api('meetings')[1]['runtime_id']}
    selection = dict(instance, microphone=microphone, system=output)
    initial_server = coreaudiod()

    with removable(guest, daemon, uid), signals():
        path, active = start(daemon, selection)
        before = sizes(active)
        evidence.note('selected sources recording before removal', {'scope': active['scope'], 'bytes': before})

    catalog = daemon.api('meetings/sources')[1]
    refuse(output not in {d['node'] for d in catalog['system_outputs']} and
           microphone in {d['node'] for d in catalog['microphones']}, f'unexpected sources after removal: {catalog}')
    refuse(coreaudiod() == initial_server, 'Core Audio restarted during removal')
    evidence.note('only the selected output was removed', {'microphone': microphone, 'removed_output': output,
                                                           'coreaudiod': initial_server, **instance})

    def interrupted():
        def ended():
            view = daemon.api(path)[1]
            return view if view['state'] != 'recording' else None
        try:
            view = wait_for(ended, 'system track interruption after only the output was removed', timeout=15)
        finally:
            last = daemon.api(path)[1]
            evidence.note('meeting after output removal', {**{k: last[k] for k in ('state', 'failure', 'actions')},
                                                          'bytes': sizes(last)})
        failure = view.get('failure') or {}
        refuse(view['state'] == 'interrupted' and failure.get('source') == 'system' and
               failure.get('kind') == allowed.LOST, f'unexpected failure: {view["state"]}, {failure}')
        refuse({'recover', 'discard'} <= set(view['actions']) and
               all(not t.get('job_id') and t['status'] != 'recording' for t in view['tracks']),
               f'interrupted tracks are not waiting for recovery: {view["tracks"]}')
        return failure

    if evidence.check('Removing just the selected output interrupts the system track', True, interrupted)['passed']:
        def recover():
            retained = sizes(daemon.api(path)[1])
            daemon.api(path + '/recover', instance)
            view = allowed.settled(daemon, path, 'recovered')
            tracks = {k: requests.keeps(view, k, hz) for k, hz in allowed.TONE.items()}
            refuse(all(tracks[k]['samples'] * 2 >= retained[k] >= before[k] for k in tracks),
                   f'committed samples were lost: {before}, {retained}, {tracks}')
            return {'tracks': tracks, 'queue': requests.submitted(daemon, view)}
        evidence.check('Explicit recovery preserves both tones and queues both WAVs', True, recover)
    else:
        requests.resting(daemon, instance)

    evidence.check('A new start identifies the unavailable output', True, lambda: allowed.missing(daemon, selection))

    def mic_alone():
        with tones(microphone=True, output=False):
            view = requests.microphone_only(daemon, instance, microphone)
        return requests.keeps(view, 'microphone', 330)
    evidence.check('The original microphone still records 330 Hz while the selected output is absent', True, mic_alone)

    def reconnected():
        with removable(guest, daemon, uid), signals():
            path = allowed.recording(daemon, selection)
            view = requests.settled(daemon, instance, path)
        refuse(coreaudiod() == initial_server and daemon.api('meetings')[1]['runtime_id'] == instance['runtime_id'],
               'an audio server or daemon restart coincided with recovery')
        return {k: requests.keeps(view, k, hz) for k, hz in allowed.TONE.items()}
    evidence.check('Reconnecting the same output restores distinct microphone and system audio', True, reconnected)


if __name__ == '__main__':
    allowed.main(verify)
