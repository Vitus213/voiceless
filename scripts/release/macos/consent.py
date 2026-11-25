#!/usr/bin/env python3
"""Answer the real privacy prompts of the disposable macOS guest through System Events.

The image of the hosted macOS guest lets the commands of a job script its
interface. This process waits for the system's own consent dialog, records
what it says, and presses the button the policy names, the way a person at the
screen would. It never writes a privacy database: the decision is macOS's own.

Usage: consent.py POLICY LOG DIRECTORY
  POLICY      JSON file read again before every look, {"microphone": "allow" or "deny"};
              every other prompt is allowed
  LOG         one JSON line per dialog seen and answered
  DIRECTORY   a file named "stop" there ends the loop; screenshots are kept there
"""
import json
import subprocess
import sys
import time
from pathlib import Path

ALLOW = {'allow', 'ok', 'allow while using the app'}
DENY = {'deny', "don't allow", 'don’t allow', 'do not allow', 'don’t allow'}
# The processes macOS shows its privacy prompts from; their controls may sit in groups.
HOSTS = ['UserNotificationCenter', 'tccd', 'UniversalAccessAuthWarn', 'CoreServicesUIAgent',
         'SecurityAgent', 'ScreenCaptureKitPicker', 'ControlCenter', 'SystemUIServer']
# Every window of every process: process, index within it, name, static texts, button names.
LIST = '''
set output to {}
set saved to AppleScript's text item delimiters
set hosts to ''' + '{' + ', '.join('"%s"' % host for host in HOSTS) + '}''' + '''
tell application "System Events"
    repeat with p in application processes
        try
            set pname to name of p
            set windex to 0
            repeat with w in windows of p
                set windex to windex + 1
                try
                    try
                        set wname to name of w as text
                    on error
                        set wname to ""
                    end try
                    set sayings to {}
                    set pressables to {}
                    try
                        repeat with t in static texts of w
                            try
                                set end of sayings to (value of t) as text
                            end try
                        end repeat
                    end try
                    try
                        repeat with b in buttons of w
                            try
                                set end of pressables to name of b as text
                            end try
                        end repeat
                    end try
                    if hosts contains pname then
                        try
                            set deep to entire contents of w
                            repeat with e in deep
                                try
                                    if class of e is button then
                                        set end of pressables to name of e as text
                                    else if class of e is static text then
                                        set end of sayings to value of e as text
                                    end if
                                end try
                            end repeat
                        end try
                    end if
                    set AppleScript's text item delimiters to " ¦ "
                    set textlist to sayings as text
                    set namelist to pressables as text
                    set AppleScript's text item delimiters to tab
                    set end of output to {pname, windex, wname, textlist, namelist} as text
                end try
            end repeat
        end try
    end repeat
end tell
set AppleScript's text item delimiters to linefeed
set joined to output as text
set AppleScript's text item delimiters to saved
return joined
'''
CLICK = '''
on run argv
    tell application "System Events"
        tell process (item 1 of argv)
            click button (item 3 of argv) of window ((item 2 of argv) as integer)
        end tell
    end tell
end run
'''


def applescript(source, *arguments):
    result = subprocess.run(['osascript', '-e', source, *arguments], capture_output=True, text=True, timeout=90)
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def service(text):
    words = text.lower()
    for keys, name in (
            (('microphone',), 'microphone'),
            (('audio',), 'system-audio'),
            (('accessibility', 'control this computer'), 'accessibility'),
            (('input monitoring', 'keystroke'), 'input-monitoring'),
            (('screen',), 'screen-recording'),
            (('automation', 'control'), 'automation')):
        if any(key in words for key in keys):
            return name
    return 'other'


def windows():
    rows = []
    for line in applescript(LIST).splitlines():
        parts = line.split('\t')
        if len(parts) == 5:
            rows.append({'process': parts[0], 'window': parts[1], 'name': parts[2], 'text': parts[3],
                         'buttons': list(dict.fromkeys(name.strip() for name in parts[4].split(' ¦ ') if name.strip()))})
    return rows


def button(names, choice):
    wanted = ALLOW if choice == 'allow' else DENY
    return next((name for name in names if name.lower() in wanted), None)


def main():
    policy_file, log_file, directory = (Path(argument) for argument in sys.argv[1:4])
    seen, count = set(), 0
    with log_file.open('a', encoding='utf-8', buffering=1) as log:
        def note(**row):
            log.write(json.dumps(dict(at=time.strftime('%Y-%m-%dT%H:%M:%S'), **row), ensure_ascii=False) + '\n')

        note(event='listening')
        while not (directory / 'stop').exists():
            try:
                found = windows()
            except Exception as error:  # System Events can be busy while a prompt takes the screen
                note(event='lookup-failed', error=str(error)[:300])
                time.sleep(1)
                continue
            policy = json.loads(policy_file.read_text())
            for row in found:
                kind = service(row['text'])
                if kind == 'other' and row['process'] not in HOSTS:
                    continue
                choice = policy.get(kind, policy.get('default', 'allow'))
                pressed = button(row['buttons'], choice)
                key = (row['process'], row['window'], row['text'], tuple(row['buttons']), pressed)
                if key not in seen:
                    count += 1
                    shot = directory / f'prompt-{count}.png'
                    subprocess.run(['screencapture', '-x', '-t', 'png', str(shot)], capture_output=True, timeout=30)
                    seen.add(key)
                    note(event='dialog', process=row['process'], window=row['name'], text=row['text'][:400],
                         buttons=row['buttons'], service=kind, policy=choice, press=pressed,
                         screenshot=shot.name if shot.is_file() else None)
                if not pressed:
                    continue
                try:
                    applescript(CLICK, row['process'], row['window'], pressed)
                    note(event='pressed', process=row['process'], button=pressed, service=kind)
                except Exception as error:
                    note(event='press-failed', error=str(error)[:300])
                time.sleep(1)
            time.sleep(0.4)
        note(event='stopped')


if __name__ == '__main__':
    main()
