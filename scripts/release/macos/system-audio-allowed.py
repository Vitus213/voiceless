#!/usr/bin/env python3
"""The dual-track meetings of the installed candidate once macOS allows system audio at its own dialog.

The hosted guest comes with the microphone allowed to the job and no decision about recording system
audio. A dual-track meeting is started from the installed CLI's daemon; the system's dialog is read
while the meetings on disk stay as they were, then answered with its own Allow button, clicked through
the interface of this disposable guest the way the person at the screen would. The installed daemon
must turn the request the dialog held into a meeting whose two lines record their tones and end as
two WAVs for the queue, with no audio from before the answer, and the queue must transcribe both
with the SenseVoice model the installed CLI installs into that home. The selected system output is
then taken away the way an unplugged interface goes while both lines record: the meeting ends as
interrupted with the audio it had committed, a new start is refused for the missing output, and
recovery makes WAVs of what was kept and enters the queue. Put back, the output records a dual-track
meeting again, with no dialog; a last meeting carries the reference clip on the microphone line, and
the queue's job for that track transcribes it.

Nothing here grants or denies anything itself, writes a privacy database or resets a decision. A
dialog that is not the system's own system-audio prompt, or one that does not take the click, is
left as it is, and what depends on its answer is recorded as unverified.

Usage: system-audio-allowed.py ROOT EVIDENCE
  ROOT       a private home with the candidate installed by its install-cli.command
  EVIDENCE   where evidence.json, the screenshots and the logs are kept
"""
import importlib
import json
from pathlib import Path
import re
import subprocess
import sys
import time
import wave

import consent
from acceptance import (MICROPHONE, OUTPUT, Daemon, Evidence, Guest, Output, Outputs, Refused, private_roots, quiet,
                        refuse, screenshot, sine, stereo, tones, wait_for)

unanswered = importlib.import_module('system-audio-consent')

KINDS = ('microphone', 'system')
TONE = {'microphone': 330, 'system': 880}  # Hz of each line's fixture signal
WATCH = 5  # seconds the dialog is left waiting, and the meetings on disk watched, before it is answered
LOST = 'stream-ended'  # the failure of a line that was recording and stopped delivering audio
AUDIO = ('kTCCServiceSystemAudioCapture', 'kTCCServiceAudioCapture')


def say(text):
    unanswered.say(text)


def asking(attempt, seconds=25):
    """The system-audio dialog once it is on screen; `None` once the request is answered without one."""
    def look():
        rows = unanswered.shown()
        if rows:
            return rows[0]
        return 'answered' if attempt.reply is not None else None
    found = wait_for(look, 'the system-audio dialog or the request\'s answer', timeout=seconds)
    return None if found == 'answered' else found


def both(view):
    """Whether a meeting records its two lines, each with a second of committed tone."""
    return (view['state'] == 'recording' and {track['kind'] for track in view['tracks']} == set(KINDS)
            and all(track['committed_bytes'] >= 32000 and (track['level'] or 0) > .01 for track in view['tracks']))


def loud(daemon, path):
    """The meeting at `path` once both lines record their tone; `Refused` at once when it left the recording state."""
    view = daemon.api(path)[1]
    refuse(view['state'] == 'recording', f'the meeting left the recording state: {json.dumps(view)[:1500]}')
    return view if both(view) else None


def starting(daemon, selection):
    """A dual-track meeting of the two selected lines, as soon as it records; returns its path."""
    current = daemon.api('meetings/start', selection)[1]
    scope = current['scope']
    refuse(scope['microphone']['node'] == selection['microphone'] and scope['system']['node'] == selection['system'],
           f'not the two selected lines: {scope}')
    return 'meetings/' + current['id']


def recording(daemon, selection):
    """A dual-track meeting once both lines record their tone; returns its path."""
    path = starting(daemon, selection)
    wait_for(lambda: loud(daemon, path), 'both lines to receive their tone')
    return path


def settled(daemon, path, state):
    """The meeting at `path` once it is `state` and every track with its audio is in the queue."""
    def ready():
        view = daemon.api(path)[1]
        return view if view['state'] == state and all(track.get('job_id') for track in view['tracks']
                                                      if track['status'] == 'finalized') else None
    return wait_for(ready, f'the meeting to be {state} and queued')


def finished(daemon, view, seconds=60):
    """How the queue ended the jobs of a finished meeting's tracks, read once none waits or runs: each must have
    completed, and its text is what the job's segments say."""
    jobs = {job['id']: job for job in daemon.api('jobs')[1]['jobs']}

    def done():
        jobs.update({job['id']: job for job in daemon.api('jobs')[1]['jobs']})
        return all(jobs[track['job_id']]['status'] not in ('queued', 'running')
                   for track in view['tracks'] if track.get('job_id')) or None
    quiet(wait_for, done, 'the queue to take the tracks', timeout=seconds)
    ended = {track['kind']: {'id': track['job_id'], 'status': jobs[track['job_id']]['status'],
                            'error': jobs[track['job_id']].get('error'),
                            'text': quiet(lambda: ''.join(segment['text'] for segment in
                                                          daemon.api('jobs/' + track['job_id'])[1]['segments']))}
             for track in view['tracks'] if track.get('job_id')}
    refuse(all(job['status'] == 'completed' for job in ended.values()),
           f'the queue did not complete the job of every track: {json.dumps(ended, ensure_ascii=False)[:1500]}')
    return ended


def missing(daemon, selection, seconds=5):
    """A start refused for the output that is not there. One refused for the meeting that just ended instead is
    asked again until `seconds` pass, and every answer is kept."""
    answers, deadline = [], time.monotonic() + seconds
    while True:
        status, body = unanswered.request(daemon, 'meetings/start', selection)
        if status == 200:  # a meeting without its output: stopped at once, and said
            quiet(daemon.api, 'meetings/' + body['id'] + '/stop', {'runtime_id': selection['runtime_id']})
            refuse(False, f'a meeting started without its output: {body}')
        answers.append({'status': status, 'error': unanswered.refusal(body)})
        if status == 409 and 'unavailable' in str(answers[-1]['error']).lower():
            return {'refusal': answers[-1]['error'], 'answers': answers}
        refuse(time.monotonic() < deadline, f'the start was not refused for the missing output: {answers}')
        time.sleep(0.5)


def decided():
    """The privacy decisions macOS keeps about recording system audio, read only."""
    rows = [row for rows in unanswered.decisions().values() if isinstance(rows, list) for row in rows]
    return [row for row in rows if row['service'] in AUDIO]


def unverified(evidence, why, *names):
    """What cannot be looked at because what it depends on did not hold."""
    for name in names:
        evidence.checks.append({'name': name, 'required': True, 'passed': False,
                                'error': f'unverified: {why}'[:2000], 'seconds': 0})
        print(f'FAIL {name}\n     unverified: {why}', flush=True)


def verify(daemon, evidence, guest, fixture):
    catalog = daemon.api('meetings/sources')[1]
    evidence.note('sources as the daemon names them', catalog)
    mic, line = (unanswered.node(catalog, 'microphones', 'BlackHole 2ch'),
                 unanswered.node(catalog, 'system_outputs', 'BlackHole 16ch'))
    refuse((mic, line) == ('coreaudio:' + MICROPHONE, 'coreaudio:' + OUTPUT), f'unexpected test lines: {mic} {line}')
    listing = daemon.api('meetings')[1]
    instance, meetings = {'runtime_id': listing['runtime_id']}, Path(listing['data_location'])
    selection, seen = dict(instance, microphone=mic, system=line), {}

    def asks():
        """Start the first dual-track meeting: the system's dialog comes up and, while it waits, nothing is written."""
        seen['before'] = before = unanswered.disk(meetings)
        seen['start'] = attempt = unanswered.Start(daemon, selection)
        row = seen['row'] = asking(attempt)
        seen['dialog'] = None if row is None else unanswered.described(row, attempt.after())
        if row is None:  # macOS decided before this exercise: there is no dialog to answer
            reply = unanswered.reply_of(attempt.answered())
            refuse(reply['status'] == 200,
                   f'the first dual-track request was refused with no dialog on screen: {reply}')
            say(f'no dialog came up: the request was answered {reply["after_seconds"]} s after it was sent')
            return {'dialog': None, 'reply': reply}
        refuse(consent.button(row['buttons'], 'allow') and consent.button(row['buttons'], 'deny'),
               f'not a consent dialog: {row}')
        say(f'the system asked {seen["dialog"]["on_screen_after_seconds"]} s after the request: '
            f'{row["text"].strip()} {row["buttons"]}')
        screenshot(evidence.directory / 'allowed-asking.png')
        state = unanswered.waited(unanswered.watching(attempt, meetings, before, WATCH))
        changed = 'are as they were before the request' if state['unchanged'] else 'CHANGED: ' + json.dumps(
            state['last'], ensure_ascii=False, default=str)
        say(f'after {WATCH} s of waiting the meetings on disk {changed}')
        refuse(state['unchanged'], f'the meetings on disk changed while the dialog waited: {state["last"]}')
        return {'dialog': seen['dialog'], 'waited': state}

    def answers():
        """The dialog's own Allow button is clicked; the request it held becomes a meeting recording both lines."""
        attempt, row = seen['start'], seen['row']
        if row is None:
            seen['allowed'], clicked = attempt.sent, None
        else:
            clicked = consent.button(row['buttons'], 'allow')
            refuse(clicked and row['process'] in consent.HOSTS and unanswered.SYSTEM_AUDIO.search(row['text']),
                   f'unverified: what is on screen is not the system\'s own system-audio dialog, '
                   f'so nothing is clicked: {row}')
            seen['allowed'] = time.monotonic()
            try:
                consent.applescript(consent.CLICK, row['process'], row['window'], clicked)
            except Exception as error:
                raise Refused(f'unverified: the dialog did not take the click ({type(error).__name__}: {str(error)[:300]}); '
                              'it is left unanswered') from None
            say(f'the dialog\'s {clicked} was clicked {attempt.after()} s after the request')
            screenshot(evidence.directory / 'allowed-answered.png')
            try:
                wait_for(lambda: not [entry for entry in unanswered.shown() if entry['text'] == row['text']],
                         'the answered dialog to leave the screen', timeout=20)
            except Refused as error:
                raise Refused(f'unverified: {error}; the dialog stays on screen') from None
        replies = [unanswered.reply_of(attempt.answered())]
        if replies[0]['status'] != 200 and row is not None:  # refused right after its answer: asked once more
            replies.append(unanswered.reply_of(unanswered.Start(daemon, selection).answered()))
        reply = replies[-1]
        for each in replies:
            say(f'{each["after_seconds"]} s after its request the daemon answered {each["status"]}: '
                f'{each.get("id") or each.get("error")}')
        refuse(reply['status'] == 200 and reply.get('id'),
               f'the request the dialog held was not answered as a meeting: {replies}')
        seen['path'] = path = 'meetings/' + reply['id']
        view = wait_for(lambda: loud(daemon, path), 'both lines to receive their tone')
        return {'clicked': clicked, 'replies': replies, 'state': view['state'],
                'committed_bytes': {track['kind']: track['committed_bytes'] for track in view['tracks']}}

    def completed():
        """Stopped after the answer: two WAVs of their own tone, none holding audio from before it, both in the queue."""
        path = seen['path']
        stopped = time.monotonic()
        daemon.api(path + '/stop', instance)
        view = settled(daemon, path, 'completed')
        tracks = {kind: unanswered.keeps(view, kind, TONE[kind]) for kind in KINDS}
        after = round(stopped - seen['allowed'], 1)
        refuse(all(track['samples'] / 16000 <= after + 1.5 for track in tracks.values()),
               f'{after} s passed between the answer and the stop, but the WAVs hold '
               + ', '.join(f'{track["samples"] / 16000:.1f} s of {kind}' for kind, track in tracks.items()))
        return {'state': view['state'], 'seconds_from_answer_to_stop': after, 'tracks': tracks,
                'started_at': view['started_at'],
                'first_audio_at': {track['kind']: track.get('first_audio_at') for track in view['tracks']},
                'queue': unanswered.submitted(daemon, view), 'jobs': finished(daemon, view)}

    def kept():
        """Read only: macOS keeps the allowance of system audio it gave at the dialog."""
        rows = decided()
        refuse([row for row in rows if row['decision'] == 'allowed'],
               f'macOS keeps no allowance of system audio for this job: {rows}')
        return {'rows': rows}

    def loses():
        """The selected output is taken away while both lines record: the meeting ends as interrupted with the
        audio it had committed, and the missing output is refused."""
        with tones():
            path = seen['lost'] = recording(daemon, selection)
            time.sleep(1)
        before = {track['kind']: track['committed_bytes'] for track in daemon.api(path)[1]['tracks']}

        def ended():
            view = daemon.api(path)[1]
            refuse(view['state'] != 'completed', f'the meeting ended as completed with no stop asked: {json.dumps(view)[:1500]}')
            return view if view['state'] == 'interrupted' else None
        try:
            guest.remove(OUTPUT)
            view = wait_for(ended, 'the meeting to end as interrupted once its output was removed', timeout=45)
            screenshot(evidence.directory / 'allowed-output-lost.png')
            failure = view['failure'] or {}
            refuse(failure.get('kind') == LOST and failure.get('source') in KINDS,
                   f'the meeting did not end as a line that stopped delivering audio: {view["failure"]}')
            refuse({'recover', 'discard'} <= set(view['actions']) and not any(track.get('job_id') for track in view['tracks'])
                   and all(track['status'] != 'recording' for track in view['tracks']),
                   f'the interrupted meeting does not wait for recover or discard: {view["actions"]} {view["tracks"]}')
            after = {track['kind']: track['committed_bytes'] for track in view['tracks']}
            refuse(all(after[kind] >= before[kind] for kind in KINDS),
                   f'audio committed before the output was lost is gone: {before} became {after}')
            refusal = missing(daemon, selection)
        finally:
            guest.restore(OUTPUT)
        return {'state': view['state'], 'failure': failure, 'actions': view['actions'],
                'tracks': {track['kind']: track['status'] for track in view['tracks']},
                'committed_before': before, 'committed_after': after,
                'restored': guest.device(OUTPUT)['name'], 'missing': refusal}

    def recovers():
        """The interrupted meeting recovers: its kept tracks become WAVs of their own tone and enter the queue."""
        path = seen['lost']
        waiting = daemon.api(path)[1]
        refuse(waiting['state'] == 'interrupted',
               f'the meeting is {waiting["state"]}, not one that waits for recovery: {json.dumps(waiting)[:1500]}')
        before = {track['kind']: track['committed_bytes'] for track in waiting['tracks']}
        daemon.api(path + '/recover', instance)
        view = settled(daemon, path, 'recovered')
        tracks = {kind: unanswered.keeps(view, kind, TONE[kind]) for kind in KINDS}
        refuse(all(tracks[kind]['samples'] * 2 >= before[kind] for kind in KINDS),
               f'recovery kept less than was committed: {before} against {tracks}')
        return {'state': view['state'], 'failure': view['failure'], 'tracks': tracks,
                'queue': unanswered.submitted(daemon, view), 'jobs': finished(daemon, view)}

    def again():
        """The output put back: a dual-track meeting records both lines again, and no dialog comes up."""
        dialogs = unanswered.shown()
        with tones():
            path = recording(daemon, selection)
            daemon.api(path + '/stop', instance)
        view = settled(daemon, path, 'completed')
        return {'state': view['state'], 'tracks': {kind: unanswered.keeps(view, kind, TONE[kind]) for kind in KINDS},
                'queue': unanswered.submitted(daemon, view), 'jobs': finished(daemon, view),
                'dialogs_on_screen': [entry['text'] for entry in dialogs + unanswered.shown()]}

    def speech():
        """The reference clip on the microphone line and the tone on the output: the queue's job for the microphone
        track transcribes the clip, and the output's track keeps its tone."""
        reference = baseline(fixture, fixture['speech'])
        with wave.open(fixture['speech'], 'rb') as wav:
            pcm, rate = wav.readframes(wav.getnframes()), wav.getframerate()
        seconds = len(pcm) / (2 * rate)
        path = starting(daemon, selection)
        with Outputs(Output('BlackHole 2ch', lambda target: stereo(pcm, rate, target) + bytes(16 * target), False),
                     Output('BlackHole 16ch', lambda target: sine(target, 880), True)):
            time.sleep(seconds + 1.5)  # the clip once, then a moment of the silence that follows it
            daemon.api(path + '/stop', instance)
        view = settled(daemon, path, 'completed')
        jobs = finished(daemon, view)
        words = re.findall(r'[a-z]+', reference.lower())
        said = ' '.join(re.findall(r'[a-z]+', jobs['microphone']['text'].lower()))
        refuse(len(words) >= 5 and any(' '.join(words[i:i + 5]) in said for i in range(len(words) - 4)),
               f'the microphone track\'s job did not recover the reference speech: the clip alone transcribes to '
               f'{reference!r}, the track to {jobs["microphone"]["text"]!r}')
        microphone, = [track for track in view['tracks'] if track['kind'] == 'microphone']
        heard = microphone['committed_bytes'] / (2 * 16000)
        refuse(seconds + 1 <= heard <= seconds + 4,
               f'the microphone track holds {heard:.1f} s for a clip of {seconds:.1f} s')
        return {'baseline': reference, 'clip_seconds': round(seconds, 2), 'jobs': jobs,
                'system_track': unanswered.keeps(view, 'system', 880),
                'microphone_track': {'seconds': round(heard, 2), 'job': microphone['job_id']}}

    completed_name = ('installed daemon: stopped after the answer, the meeting is completed as two WAVs of their own '
                      'tone, none holding audio from before the answer, and both enter the queue')
    lost_name = ('installed daemon: the selected system output taken away while both lines record ends the meeting as '
                 'interrupted, keeps the committed audio and is refused as missing')
    recovered_name = ('installed daemon: the interrupted dual-track meeting recovers: its kept tracks become WAVs of '
                      'their own tone and enter the queue')
    again_name = 'installed daemon: with the output back, a dual-track meeting records both lines again, with no dialog'
    speech_name = ('installed daemon: the reference clip on the microphone line is transcribed by the queue from the '
                   'meeting\'s track, and the output\'s track keeps its tone')
    with tones():  # both lines carry their tone from before the dialog until the first meeting is stopped
        shown = evidence.check('installed daemon: the first dual-track request shows the system-audio dialog, and the '
                               'meetings on disk stay as they were while it waits', True, asks)['passed']
        allowed = shown and evidence.check('installed daemon: the dialog is answered with its own Allow button, and the '
                                           'request it held becomes a meeting recording both lines', True, answers)['passed']
        if allowed:
            evidence.check(completed_name, True, completed)
    if not allowed:
        unanswered.resting(daemon, instance)
        unverified(evidence, 'no dual-track meeting recorded after the system-audio dialog',
                   completed_name, lost_name, recovered_name, again_name, speech_name)
        return
    evidence.check('macOS keeps the allowance of system audio it gave at the dialog (read only)', False, kept)
    unanswered.resting(daemon, instance)
    evidence.check(lost_name, True, loses)
    evidence.check(recovered_name, True, recovers)
    unanswered.resting(daemon, instance)
    evidence.check(again_name, True, again)
    evidence.check(speech_name, True, speech)


def preflight(guest, here):
    """What the guest came with: a console session that is not locked, the microphone allowed to this job, no decision
    about system audio, and System Events able to read its dialogs."""
    probe = guest.temp / 'session'  # guest.sh's read-only picture of the session, built in the same job
    if not probe.is_file():
        guest.build(probe, [here / 'session.swift'], frameworks=('AppKit', 'ApplicationServices', 'AVFoundation'))
    session = json.loads(subprocess.check_output([str(probe)], text=True, timeout=60))
    refuse(session['on_console'] and not session['locked'],
           f'the session is not an unlocked console session: {session}')
    rows = decided()
    refuse(not rows, f'the guest came with a decision about recording system audio: {rows}')
    return dict(session, microphone=unanswered.preflight(), system_audio='undecided')


def install(cli, env, evidence):
    """The installed CLI installs, into the private home, the SenseVoice model the queue transcribes with and the voice
    activity detector it segments with; returns their payload directories and the reference clip the model ships."""
    payload = {}
    for model in ('sense-voice-int8', 'silero-vad'):
        result = subprocess.run([str(cli), 'model', 'install', model], env=env, capture_output=True, text=True,
                                timeout=900)
        evidence.record(f'model install {model}', result.stdout + result.stderr)
        refuse(result.returncode == 0, f'model install {model} exited {result.returncode}: {result.stderr[-800:]}')
        payload[model] = result.stdout.strip().splitlines()[-1]
    speech = Path(payload['sense-voice-int8']) / 'test_wavs/en.wav'
    refuse(speech.is_file(), f'the installed SenseVoice payload ships no reference clip: {payload}')
    return dict(payload, speech=str(speech))


def baseline(fixture, speech):
    """What the installed CLI transcribes the reference clip to, offline and on its own, with the daemon's model."""
    return json.loads(subprocess.check_output(
        [fixture['cli'], '--config', fixture['config'], 'transcribe', speech, '--backend', 'local', '--json'],
        env=fixture['env'], text=True, timeout=180))['text']


def main(verify=verify):
    root, directory = (Path(argument).resolve() for argument in sys.argv[1:3])
    evidence = Evidence(directory)
    daemon, guest = None, None
    try:
        here = Path(__file__).resolve().parent
        guest = Guest(here, evidence)
        evidence.note('privacy decisions as the guest came', unanswered.decisions())
        evidence.check('the guest: a console session that is not locked, the microphone already allowed to this job, '
                       'no decision about system audio, and System Events reads its dialogs', True,
                       lambda: preflight(guest, here))
        refuse(not evidence.failed(), 'the guest is not as this exercise needs it')
        env = private_roots(root / 'user', root / 'home')
        cli = root / 'home/.local/bin/voiceless'
        evidence.note('installed version', subprocess.check_output([str(cli), '--version'], env=env, text=True,
                                                                   timeout=60).strip())
        models = evidence.check('installed CLI: SenseVoice and its voice activity detector are installed into the '
                                'private home', True, lambda: install(cli, env, evidence))
        refuse('speech' in models.get('detail', {}), 'the models the queue needs are not installed')
        config = Path(env['XDG_CONFIG_HOME']) / 'voiceless/config.toml'
        config.parent.mkdir(parents=True)
        config.write_text('[local]\nmodel="sense-voice-int8"\n[asr]\nbatch_backend="local"\n'
                          f'batch_model_dir={json.dumps(models["detail"]["sense-voice-int8"])}\n'
                          '[console]\nport=0\n[audio]\npause_media=false\ncues=false\n')
        config.chmod(0o600)
        daemon = Daemon(cli, config, env, root / 'daemon.log', evidence)
        verify(daemon, evidence, guest, dict(models['detail'], cli=cli, config=config, env=env))
    except Exception as error:  # outside a check: still recorded, still an exit status
        evidence.checks.append({'name': 'the exercise ran to its end', 'required': True, 'passed': False,
                                'error': f'{type(error).__name__}: {error}'[:2000], 'seconds': 0})
        print(f'FAIL the exercise ran to its end\n     {type(error).__name__}: {error}', flush=True)
    finally:
        if daemon is not None:
            daemon.stop()
        evidence.note('privacy decisions at the end', unanswered.decisions())
        evidence.write()
    failed = evidence.failed()
    print(json.dumps({'checks': len(evidence.checks), 'failed': failed}, ensure_ascii=False), flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
