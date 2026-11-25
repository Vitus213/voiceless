#!/bin/bash
# Read-only picture of the hosted macOS guest: the session, what a command of
# this job may do, its audio endpoints and its signing identities. Nothing
# here changes a setting or can show a consent dialog.
set -euo pipefail

echo "== system"
sw_vers
echo "arch $(uname -m), $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) CPUs, $(( $(sysctl -n hw.memsize) / 1073741824 )) GiB"
csrutil status
df -h / | tail -1

echo "== session"
who
echo "console owner: $(stat -f '%Su' /dev/console), job user: $(id -un)"
if sudo -n true 2>/dev/null; then echo "sudo without a password: yes"; else echo "sudo without a password: no"; fi

echo "== privacy permissions (preflight calls only)"
mkdir -p "$RUNNER_TEMP/module-cache"
xcrun swiftc -module-cache-path "$RUNNER_TEMP/module-cache" scripts/release/macos/session.swift -o "$RUNNER_TEMP/session"
"$RUNNER_TEMP/session"

echo "== UI scripting through System Events"
# Twenty seconds at most: a consent prompt that cannot be answered must not hold the job.
if perl -e 'alarm shift; exec @ARGV' 20 osascript -e 'tell application "System Events" to get name of every application process whose background only is false' 2>&1; then :; else echo "System Events did not answer (exit $?)"; fi

echo "== audio endpoints"
system_profiler SPAudioDataType -json 2>/dev/null | python3 -B -c '
import json, sys
for item in json.load(sys.stdin).get("SPAudioDataType", []):
    for device in item.get("_items", []):
        print(" ", device.get("_name"), {
            key.removeprefix("coreaudio_"): device[key]
            for key in ("coreaudio_default_audio_input_device", "coreaudio_default_audio_output_device",
                        "coreaudio_default_system_output_device", "coreaudio_device_input_channels",
                        "coreaudio_device_output_channels", "coreaudio_device_transport")
            if key in device})
' || echo "system_profiler listed no audio device"

echo "== privacy entries of this image for microphone, input, screen and automation (read-only)"
if rows=$(sqlite3 -readonly "$HOME/Library/Application Support/com.apple.TCC/TCC.db" '
    select service, client, auth_value from access
    where service in ("kTCCServiceMicrophone", "kTCCServiceSystemAudioCapture", "kTCCServiceAccessibility",
                      "kTCCServiceListenEvent", "kTCCServicePostEvent", "kTCCServiceScreenCapture", "kTCCServiceAppleEvents")
    order by service, client' 2>&1); then
  echo "$rows"
else
  echo "the user privacy database is not readable by this job: $rows"
fi

echo "== editors and signing"
ls -d /System/Applications/TextEdit.app
security find-identity -v -p codesigning 2>&1 | tail -3
echo "Developer ID Application certificates: $(security find-certificate -a -c 'Developer ID Application' -Z 2>/dev/null | grep -c 'SHA-256')"

echo "== toolchain"
xcodebuild -version | tr '\n' ' '; echo
rustc --version 2>&1 || true
cargo --version 2>&1 || true
python3 --version
node --version 2>&1 || true
