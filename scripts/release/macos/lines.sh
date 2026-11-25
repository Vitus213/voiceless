#!/bin/bash
# Two test-owned loopback endpoints on the disposable macOS guest: BlackHole 2ch
# stands for the microphone, BlackHole 16ch for the system output. The
# installers are the publisher's, checked by SHA-256 and by their Apple
# developer signature; they are fetched here and are never part of a candidate.
set -euo pipefail
if [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ]; then
  echo 'Requires the GitHub-hosted macOS test guest' >&2
  exit 64
fi
case "${GITHUB_REF_NAME:-}" in
  ci/macos-candidate/*) ;;
  *) echo 'Requires a ci/macos-candidate branch' >&2; exit 64 ;;
esac

here=$(cd "$(dirname "$0")" && pwd)
work="$RUNNER_TEMP/loopback"
mkdir -p "$work" "$RUNNER_TEMP/module-cache"
if [ ! -x "$RUNNER_TEMP/audio" ]; then
  xcrun swiftc -module-cache-path "$RUNNER_TEMP/module-cache" "$here/audio.swift" -o "$RUNNER_TEMP/audio"
fi

# Published installers of BlackHole 0.6.0 (GPL-3.0), pinned by SHA-256.
for line in '2ch 3e0f8102ed27e3528f0c186927837f9f0d843a26580c2ea7507d7c8ef019eadd' \
            '16ch e8dea1315c15e03a7063d2d953db9d318296f0b9b6fda35a7a2c88a583418c31'; do
  set -- $line
  package="$work/BlackHole$1.v0.6.0.pkg"
  curl -fsSL --retry 3 --max-time 120 -o "$package" "https://existential.audio/downloads/BlackHole$1.v0.6.0.pkg"
  echo "$2  $package" | shasum -a 256 -c -
  pkgutil --check-signature "$package"
  if ! sudo spctl --assess --type install --verbose=2 "$package"; then
    echo "Gatekeeper does not accept this installer (exit $?)"
  fi
  sudo installer -pkg "$package" -target /
done

# The HAL loads its plug-ins when coreaudiod starts.
echo "== devices as the installers left them, and what PortAudio sees"
"$RUNNER_TEMP/audio" list
python3 -B -c 'import sounddevice as sd; print(sd.query_devices())' || echo "PortAudio could not list the devices (exit $?)"
sudo launchctl kickstart -k system/com.apple.audio.coreaudiod || echo "restarting coreaudiod exited $?"
status=0
"$RUNNER_TEMP/audio" wait BlackHole2ch_UID present 40 || status=$?
"$RUNNER_TEMP/audio" wait BlackHole16ch_UID present 20 || status=$?
echo "== devices after coreaudiod restarted"
"$RUNNER_TEMP/audio" list
if [ "$status" != 0 ]; then
  echo "The loopback endpoints did not appear (exit $status)"
  ls -la /Library/Audio/Plug-Ins/HAL/ "/Library/Audio/Plug-Ins/HAL/"*/Contents/MacOS/
  pgrep -l coreaudiod || echo "coreaudiod is not running"
  system_profiler SPAudioDataType 2>/dev/null | grep -E '^ {8}[^ ]|Transport|BlackHole' | head -30
  exit "$status"
fi
echo "VOICELESS_LOOPBACK=$work" >> "$GITHUB_ENV"
