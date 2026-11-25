#!/usr/bin/env python3
"""The first dual-track meeting of the installed candidate, asked of macOS and left unanswered.

The hosted guest comes with the microphone allowed to the job and no decision about recording
system audio. A dual-track meeting is started from the installed CLI's daemon and the system's own
dialog is then left to wait, as it does for a person who is not at the screen: for several seconds
the meetings directory is compared with what it held before the request (no audio and no journal
may appear before the answer), then the request is followed to its refusal, which must come on
time and leave nothing behind. The microphone, already allowed, records a meeting of its own
meanwhile, and a retry meets the same unanswered dialog. Nothing is pressed: approval of a privacy
prompt stays with the person at the screen, so the grant itself, and a dual-track meeting after
it, are recorded as not verified here. No privacy database is written or edited.

Usage: system-audio-consent.py ROOT EVIDENCE
  ROOT       a private home with the candidate installed by its install-cli.command
  EVIDENCE   where evidence.json, the screenshots and the logs are kept
"""
import array
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import wave

import consent
from acceptance import (DECISION, MICROPHONE, OUTPUT, Daemon, Evidence, Refused, magnitude, private_roots,
                        quiet, refuse, screenshot, tones, wait_for)

HEADER = 44  # bytes of the WAV header an unfinished track starts with
KINDS = ('microphone', 'system')
WATCH = 10  # seconds the system's dialog is left waiting before it is read one last time
ANSWER = 30  # seconds the daemon gives an unanswered dialog (desktop/macos.rs)
SERVICES = ('kTCCServiceMicrophone', 'kTCCServiceAudioCapture', 'kTCCServiceSystemAudioCapture')
DATABASES = (Path.home() / 'Library/Application Support/com.apple.TCC/TCC.db',
             Path('/Library/Application Support/com.apple.TCC/TCC.db'))
SYSTEM_AUDIO = re.compile('system audio', re.I)


def say(text):
    """A line of the job's log, under the check it belongs to."""
    print(f'     {text}', flush=True)


# ---- the system's dialog, read and never pressed ------------------------------------------


def onscreen():
    """The system-audio consent dialogs on screen, and every other window offering Allow and Don't Allow."""
    rows = consent.windows()
    asking = [row for row in rows if SYSTEM_AUDIO.search(row['text'])]
    return asking, [{'process': row['process'], 'text': row['text'][:200]} for row in rows
                    if row not in asking and consent.button(row['buttons'], 'allow')
                    and consent.button(row['buttons'], 'deny')]


def shown():
    """The system-audio dialogs on screen; [] when System Events cannot say."""
    try:
        return onscreen()[0]
    except Exception:  # System Events can be busy while a prompt takes the screen
        return []


def await_dialog(attempt, seconds=25):
    """The system-audio dialog once it is on screen; `Refused` when the start is answered without one."""
    other = []

    def look():
        try:
            asking, other[:] = onscreen()
        except Exception as error:  # System Events can be busy while a prompt takes the screen
            asking, other[:] = [], [f'{type(error).__name__}: {error}']
        if asking:
            return asking[0]
        if attempt.reply is not None:
            raise Refused(f'the start was answered with no system-audio dialog: {reply_of(attempt.reply)}')
    try:
        return wait_for(look, 'the system-audio dialog', timeout=seconds)
    except Refused as error:
        raise Refused(f'{error}; other prompts offering Allow or Don\'t Allow: {other}') from None


def described(dialog, after):
    return {'process': dialog['process'], 'text': dialog['text'], 'buttons': dialog['buttons'],
            'on_screen_after_seconds': after}


# ---- the daemon's answers ----------------------------------------------------------------


def request(daemon, path, body):
    """The console's status and body of one POST, whatever its status: JSON, or the text of an error."""
    sent = urllib.request.Request(daemon.origin + '/api/' + path, data=json.dumps(body).encode(),
                                  headers={'Origin': daemon.origin, 'X-Voiceless-Token': daemon.token,
                                           'Content-Type': 'application/json'})
    try:
        reply = daemon.opener.open(sent, timeout=3 * ANSWER)
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(errors='replace')
    with reply:
        return reply.status, json.load(reply)


class Start:
    """One `meetings/start` request on a thread of its own: the daemon holds it while a dialog waits."""

    def __init__(self, daemon, selection):
        self.sent, self.reply = time.monotonic(), None  # (seconds after it was sent, status, body)
        threading.Thread(target=self.ask, args=(daemon, selection), daemon=True).start()

    def after(self):
        return round(time.monotonic() - self.sent, 1)

    def ask(self, daemon, selection):
        try:
            status, body = request(daemon, 'meetings/start', selection)
        except Exception as error:  # the request itself failed: its text says how
            status, body = None, f'{type(error).__name__}: {error}'
        self.reply = (self.after(), status, body)

    def answered(self):
        """The daemon's reply; `Refused` while the request is still held."""
        wait_for(lambda: self.reply, 'the daemon to answer meetings/start', timeout=3 * ANSWER + 10)
        return self.reply


def refusal(body):
    """What a refused request said, be its body JSON or text."""
    if isinstance(body, str):
        try:
            body = json.loads(body)
        except ValueError:
            return body
    return body.get('error')


def reply_of(reply):
    """A reply as the evidence keeps it."""
    after, status, body = reply
    kept = {'after_seconds': after, 'status': status}
    if status == 200:
        kept.update(id=body.get('id'), state=body.get('state'))
    else:
        kept['error'] = refusal(body)
    return kept


def node(catalog, kind, label):
    matches = [source['node'] for source in catalog[kind] if source['label'] == label]
    refuse(len(matches) == 1, f'{label} is not exactly one {kind}: {catalog}')
    return matches[0]


def settled(daemon, instance, path):
    """Stop the meeting and wait for the queue to take its tracks; returns its view."""
    daemon.api(path + '/stop', instance)

    def queued():
        view = daemon.api(path)[1]
        return view if view['state'] == 'completed' and all(track.get('job_id') for track in view['tracks']) else None
    return wait_for(queued, 'the meeting to be completed and queued')


def submitted(daemon, meeting):
    """Each finalized track as a job of the queue, named after its meeting and track."""
    jobs = {job['id']: job for job in daemon.api('jobs')[1]['jobs']}
    entries = {}
    for track in meeting['tracks']:
        job = jobs.get(track.get('job_id'))
        refuse(job is not None, f'the {track["kind"]} track is in no job: {track}')
        refuse(job['input']['name'] == f'{meeting["id"]}-{track["kind"]}.wav' and job['transport'] == 'local'
               and job['input']['path'] and os.path.samefile(job['input']['path'], track['path']),
               f'not the job of the {track["kind"]} track: {job}')
        entries[track['kind']] = {'id': job['id'], 'status': job['status'], 'name': job['input']['name']}
    return entries


def keeps(view, kind, hz):
    """The track `kind` of a finished meeting as a WAV that holds its own tone."""
    track, = [track for track in view['tracks'] if track['kind'] == kind]
    with wave.open(track['path'], 'rb') as wav:
        refuse((wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2),
               f'the {kind} track is not 16 kHz mono PCM16')
        samples = array.array('h', wav.readframes(wav.getnframes()))
    own, other = magnitude(samples, hz), magnitude(samples, 880 if hz == 330 else 330)
    refuse(len(samples) * 2 == track['committed_bytes'] and own > 10 and own > 3 * other,
           f'the {kind} WAV holds {own:.1f} at {hz} Hz against {other:.1f} at the other tone')
    return {'samples': len(samples), 'hz': hz, 'own': round(own, 1), 'other': round(other, 1)}


def microphone_only(daemon, instance, mic, seconds=3):
    """A meeting of the microphone alone, for `seconds`; returns it once the queue took it."""
    path = 'meetings/' + daemon.api('meetings/start', dict(instance, microphone=mic))[1]['id']
    time.sleep(seconds)
    return settled(daemon, instance, path)


def resting(daemon, instance):
    """Stop whatever meeting still records, so that the next step starts from a daemon at rest."""
    listing = quiet(daemon.api, 'meetings')
    for meeting in listing[1]['meetings'] if not isinstance(listing, str) else []:
        if meeting['state'] == 'recording':
            quiet(daemon.api, 'meetings/' + meeting['id'] + '/stop', instance)


# ---- what the meetings hold, and what the guest decided ------------------------------------


def disk(root):
    """Every meeting directory under `root`: its journal and, for each of its files, the size and a digest."""
    meetings = []
    for directory in sorted(root.glob('*')):
        files = {}
        try:
            if not directory.is_dir():
                continue
            for entry in sorted(directory.iterdir()):
                if entry.name.startswith('.meeting-'):  # the journal's temporary while it is being replaced
                    continue
                data = entry.read_bytes()
                files[entry.name] = [len(data), hashlib.sha256(data).hexdigest()[:16]]
            journal = json.loads((directory / 'meeting.json').read_text())
        except OSError:  # a start that failed is taking its directory away
            continue
        except ValueError:
            journal = None
        meetings.append({'id': directory.name, 'files': files, 'journal': journal})
    return meetings


def held(meeting):
    """What one meeting directory holds for each track: the audio in its files and what its journal keeps."""
    tracks = {track['kind']: track for track in (meeting['journal'] or {}).get('tracks', [])}
    return {kind: {'audio_bytes': max(sum(meeting['files'].get(f'{kind}.{suffix}', [0])[0]
                                           for suffix in ('wav', 'wav.partial')) - HEADER, 0),
                   'committed_bytes': tracks.get(kind, {}).get('committed_bytes', 0),
                   'first_audio_at': tracks.get(kind, {}).get('first_audio_at')} for kind in KINDS}


def watching(attempt, root, before, seconds, step=1, until_answered=False):
    """How the meetings on disk differed from `before`, read every `step` seconds while the dialog waits."""
    known, seen, started = {meeting['id']: meeting for meeting in before}, [], time.monotonic()
    while True:
        left = seconds - (time.monotonic() - started)
        if left <= 0 or until_answered and attempt.reply is not None:
            return seen
        time.sleep(min(step, left))
        if not until_answered and attempt.reply is not None:
            raise Refused(f'the start was answered while the dialog waited: {reply_of(attempt.reply)}')
        seen.append({'after_seconds': round(time.monotonic() - started, 1),
                     'changed': [{'id': meeting['id'], 'state': (meeting['journal'] or {}).get('state'),
                                  'files': {name: size for name, (size, _) in meeting['files'].items()}, 'tracks': held(meeting)}
                                 for meeting in disk(root) if meeting != known.get(meeting['id'])]})


def waited(samples):
    """Whether the meetings on disk stayed what they were while the dialog waited; what differed, if anything did."""
    changed = [meeting for sample in samples for meeting in sample['changed']]
    return {'looks': len(samples), 'unchanged': not changed, 'last': changed[-1] if changed else None}


def decided(database):
    """The privacy decisions macOS keeps in one database for the microphone and system audio, read only."""
    connection = sqlite3.connect('file:' + str(database).replace('?', '%3f') + '?mode=ro', uri=True)
    try:
        rows = connection.execute(
            f'select service, client, auth_value from access where service in ({",".join("?" * len(SERVICES))}) order by service, client',
            SERVICES).fetchall()
    finally:
        connection.close()
    return [{'service': service, 'client': client, 'decision': DECISION.get(value, str(value))}
            for service, client, value in rows]


def decisions():
    return {str(database): quiet(decided, database) for database in DATABASES}


def threads(pid):
    """How many lines `ps` prints for the daemon's task and threads: a difference is a thread come or gone."""
    result = subprocess.run(['ps', '-M', '-p', str(pid)], capture_output=True, text=True, timeout=20)
    rows = [line for line in result.stdout.splitlines() if line.split()[:1] == [str(pid)]]
    refuse(rows, (result.stdout + result.stderr).strip()[:300])
    return len(rows)


# ---- the exercise ------------------------------------------------------------------------


def verify(daemon, evidence):
    catalog = daemon.api('meetings/sources')[1]
    evidence.note('sources as the daemon names them', catalog)
    mic, line = node(catalog, 'microphones', 'BlackHole 2ch'), node(catalog, 'system_outputs', 'BlackHole 16ch')
    refuse((mic, line) == ('coreaudio:' + MICROPHONE, 'coreaudio:' + OUTPUT), f'unexpected test lines: {mic} {line}')
    listing = daemon.api('meetings')[1]
    instance, meetings = {'runtime_id': listing['runtime_id']}, Path(listing['data_location'])
    selection = dict(instance, microphone=mic, system=line)
    seen = {'threads_before': quiet(threads, daemon.process.pid)}

    def asks():
        """Start the first dual-track meeting and let the system's dialog wait, comparing the meetings on disk."""
        seen['before'] = before = disk(meetings)
        seen['start'] = attempt = Start(daemon, selection)
        dialog = await_dialog(attempt)
        seen['dialog'] = described(dialog, attempt.after())
        refuse(consent.button(dialog['buttons'], 'allow') and consent.button(dialog['buttons'], 'deny'),
               f'not a consent dialog: {dialog}')
        say(f'the system asked {seen["dialog"]["on_screen_after_seconds"]} s after the request, and nobody answers: '
            f'{dialog["text"].strip()} {dialog["buttons"]}')
        screenshot(evidence.directory / 'unanswered-asking.png')
        samples = watching(attempt, meetings, before, WATCH)
        screenshot(evidence.directory / 'unanswered-still-waiting.png')
        state = waited(samples)
        say(f'after {WATCH} s of waiting the meetings on disk '
            f'{"are as they were before the request" if state["unchanged"] else "CHANGED: " + json.dumps(state["last"], ensure_ascii=False, default=str)}')
        refuse(state['unchanged'], f'the meetings on disk changed while the dialog waited: {state["last"]}')
        return {'dialog': seen['dialog'], 'waited': state, 'threads': seen['threads_before']}

    def gives_up():
        """Nobody answers: the start is refused on time as one that waited for an answer, and nothing is left on disk."""
        refuse('dialog' in seen, 'no system-audio dialog was seen')
        attempt = seen['start']
        watched = waited(watching(attempt, meetings, seen['before'], ANSWER + 20, step=2, until_answered=True))
        refuse(attempt.reply is not None, f'the daemon still held the request {ANSWER + 20} s after the dialog was shown')
        reply = reply_of(attempt.reply)
        say(f'nobody answered: {reply["after_seconds"]} s after the request the daemon answered '
            f'{reply["status"]}: {reply.get("error")}')
        refuse(reply['status'] == 409 and 'not answered within 30 seconds' in str(reply.get('error')),
               f'the unanswered dialog was not refused as such: {reply}')
        refuse(ANSWER - 1 <= reply['after_seconds'] <= ANSWER + 15,
               f'the unanswered dialog was given up after {reply["after_seconds"]} s, not {ANSWER}')
        refuse(watched['unchanged'], f'the meetings on disk changed while nobody answered: {watched["last"]}')
        refuse(disk(meetings) == seen['before'], 'the start refused for want of an answer left meetings on disk')
        seen['still_waiting'] = [described(entry, None) for entry in shown()]
        refuse([entry['text'] for entry in seen['still_waiting']] == [seen['dialog']['text']],
               f'the dialog is not on screen unanswered any more: {seen["still_waiting"]}')
        screenshot(evidence.directory / 'unanswered-given-up.png')
        return {'reply': reply, 'waited': watched, 'dialog_still_on_screen': True,
                'threads_while_unanswered': quiet(threads, daemon.process.pid)}

    def microphone_meanwhile():
        """The microphone needs no dialog: its meeting records, ends as a WAV of its tone and enters the queue."""
        with tones(microphone=True, output=False):
            view = microphone_only(daemon, instance, mic)
        return {'state': view['state'], 'track': keeps(view, 'microphone', 330), 'queue': submitted(daemon, view),
                'dialogs_on_screen': [entry['text'] for entry in shown()]}

    def retried():
        """A retry while the dialog keeps waiting: refused the same way after its own wait, with the one dialog."""
        before = disk(meetings)
        attempt = Start(daemon, selection)
        watched = waited(watching(attempt, meetings, before, ANSWER + 20, step=5, until_answered=True))
        refuse(attempt.reply is not None, f'the daemon still held the retry {ANSWER + 20} s after it was sent')
        reply = reply_of(attempt.reply)
        say(f'the retry was answered {reply["after_seconds"]} s after it was sent: {reply["status"]}: {reply.get("error")}')
        refuse(reply['status'] == 409 and 'not answered within 30 seconds' in str(reply.get('error')),
               f'the retry was not refused as the first request was: {reply}')
        refuse(ANSWER - 1 <= reply['after_seconds'] <= ANSWER + 15,
               f'the retry was given up after {reply["after_seconds"]} s, not {ANSWER}')
        refuse(watched['unchanged'], f'the meetings on disk changed during the retry: {watched["last"]}')
        refuse(disk(meetings) == before, 'the retry left meetings on disk')
        dialogs = [described(entry, None) for entry in shown()]
        refuse([entry['text'] for entry in dialogs] == [seen['dialog']['text']],
               f'not the one unanswered dialog while the retry waited: {dialogs}')
        screenshot(evidence.directory / 'unanswered-at-the-end.png')
        return {'reply': reply, 'dialogs_on_screen': len(dialogs), 'threads_after': quiet(threads, daemon.process.pid)}

    with tones():
        evidence.check('installed daemon: the first dual-track request shows the system-audio dialog, '
                       'and the meetings on disk stay as they were while it waits', True, asks)
        given_up = evidence.check('installed daemon: left unanswered, the dialog is given up after 30 s and the start '
                                  'is refused as such, leaving nothing on disk', True, gives_up)
    resting(daemon, instance)
    evidence.check('installed daemon: the microphone, already allowed, records a meeting and submits it to the queue '
                   'while the system-audio dialog keeps waiting', True, microphone_meanwhile)
    if given_up['passed']:
        with tones():
            evidence.check('installed daemon: a retry while the dialog keeps waiting is refused the same way, '
                           'with the one dialog and nothing on disk', True, retried)
    resting(daemon, instance)
    evidence.checks.append({'name': 'not exercised here: system audio allowed by a person at the dialog, then a '
                                    'dual-track meeting recording both lines', 'required': False, 'passed': False,
                            'error': 'unverified: this exercise presses nothing, so the first grant of system audio '
                                     'and what follows it need a person at the screen', 'seconds': 0})
    say('unverified, as intended: the first grant of system audio needs a person at the dialog; '
        'the dialog is left on screen unanswered')
    evidence.note('the dialog at the end', [described(entry, None) for entry in shown()])


def preflight():
    """What the guest came with: the microphone allowed to this job, and System Events able to read its dialogs."""
    status = subprocess.check_output(
        ['swift', '-e', 'import AVFoundation; print(AVCaptureDevice.authorizationStatus(for: .audio).rawValue)'],
        text=True, timeout=600).strip().splitlines()[-1]
    refuse(status == '3', f'the microphone is not allowed to this job: AVCaptureDevice says {status}')

    def listed():  # System Events can be busy for a moment: asked again until it says
        try:
            return onscreen()[1] or ['none']
        except Exception:
            return None
    return {'microphone': status, 'other_prompts_on_screen': wait_for(listed, 'System Events to list the windows', timeout=30)}


def main():
    root, directory = (Path(argument).resolve() for argument in sys.argv[1:3])
    evidence = Evidence(directory)
    daemon = None
    try:
        evidence.note('privacy decisions as the guest came', decisions())
        evidence.check('the guest: the microphone is already allowed to this job and System Events reads its dialogs',
                       True, preflight)
        refuse(not evidence.failed(), 'the guest is not as this exercise needs it')
        env = private_roots(root / 'user', root / 'home')
        config = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
        config.parent.mkdir(parents=True)
        config.write_text('batch_backend="local"\n[console]\nport=0\n[audio]\npause_media=false\ncues=false\n')
        config.chmod(0o600)
        evidence.note('installed version', subprocess.check_output([root / 'home/.local/bin/voiceless', '--version'],
                                                                   env=env, text=True, timeout=60).strip())
        daemon = Daemon(root / 'home/.local/bin/voiceless', config, env, root / 'daemon.log', evidence)
        verify(daemon, evidence)
    except Exception as error:  # outside a check: still recorded, still an exit status
        evidence.checks.append({'name': 'the exercise ran to its end', 'required': True, 'passed': False,
                                'error': f'{type(error).__name__}: {error}'[:2000], 'seconds': 0})
        print(f'FAIL the exercise ran to its end\n     {type(error).__name__}: {error}', flush=True)
    finally:
        if daemon is not None:
            daemon.stop()
        evidence.note('privacy decisions at the end', decisions())
        evidence.write()
    failed = evidence.failed()
    print(json.dumps({'checks': len(evidence.checks), 'failed': failed}, ensure_ascii=False), flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
