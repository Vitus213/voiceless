#!/bin/bash
# Compile and try the test's own helpers on the disposable guest, so that a
# build is not spent before they are known to work here: the CoreAudio
# helper, the editor fixture, the shell observer and the consent lookup.
set -euo pipefail
if [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ] || [ -z "${VOICELESS_LOOPBACK:-}" ]; then
  echo 'Requires the hosted macOS guest after lines.sh' >&2
  exit 64
fi
here=$(cd "$(dirname "$0")" && pwd)
temp="$RUNNER_TEMP"
work="$temp/smoke"
mkdir -p "$work" "$temp/module-cache"

echo "== helpers compile"
xcrun swiftc -module-cache-path "$temp/module-cache" "$here/shell.swift" -o "$temp/voiceless-shell"
xcrun swiftc -module-cache-path "$temp/module-cache" "$here/editor.swift" "$here/editor-serve.swift" -o "$temp/voiceless-editor"
# The guard variant needs the bridge archive of a build; its Swift and its C are checked alone here.
xcrun swiftc -typecheck -module-cache-path "$temp/module-cache" -import-objc-header "$here/editor.h" "$here/editor.swift" "$here/editor-guard.swift"
xcrun clang -c "$here/editor.c" -o "$temp/editor-stubs.o"
"$temp/audio" list

echo "== editor fixture on the guest's screen"
"$temp/voiceless-editor" serve "$work" & fixture=$!
sleep 3
echo '{"id": 1, "field": 1}' > "$work/command.json"
sleep 1
cat "$work/status.json"; echo
touch "$work/stop"
wait "$fixture"

echo "== loopback of the microphone line, recorded through PortAudio"
python3 -B - "$here" <<'PY'
import array, json, math, sys, time
sys.path.insert(0, sys.argv[1])
import consent
import sounddevice as sd

def index(label, output):
    names = [device['name'] for device in sd.query_devices()]
    found = [i for i, device in enumerate(sd.query_devices())
             if device['name'].startswith(label) and (device['max_output_channels'] if output else device['max_input_channels']) >= 2
             and sd.query_hostapis(device['hostapi'])['name'] == 'Core Audio']
    assert len(found) == 1, (label, output, names)
    return found[0]

line, rate = index('BlackHole 2ch', True), 48000
state = {'offset': 0}
period = array.array('h', (round(12000 * math.sin(math.tau * 330 * i / rate)) for i in range(rate))).tobytes() * 2

def fill(out, frames, timing, status):
    size = frames * 4
    state['offset'] = (state['offset'] + size) % len(period)
    out[:] = (period * 2)[state['offset']:state['offset'] + size]

kept = []
def collect(data, frames, timing, status):
    kept.append(bytes(data))

speaker = sd.RawOutputStream(device=line, channels=2, samplerate=rate, dtype='int16', callback=fill)
microphone = sd.RawInputStream(device=index('BlackHole 2ch', False), channels=2, samplerate=rate, dtype='int16', callback=collect)
speaker.start(); microphone.start()
time.sleep(1.5)
microphone.stop(); speaker.stop()
microphone.close(); speaker.close()
samples = array.array('h', b''.join(kept))
rms = math.sqrt(sum(x * x for x in samples) / max(len(samples), 1)) / 32768
print(json.dumps({'recorded_seconds': round(len(samples) / 2 / rate, 2), 'rms': round(rms, 4)}))
assert rms > 0.1, 'the loopback line carried no signal'
PY

echo "== windows that offer Allow or Deny right now (the consent lookup)"
python3 -B - "$here" <<'PY'
import sys
sys.path.insert(0, sys.argv[1])
import consent
rows = [row for row in consent.windows() if set(name.lower() for name in row['buttons']) & (consent.ALLOW | consent.DENY)]
print(len(rows), 'such window(s)')
for row in rows[:10]:
    print(' ', row)
PY
