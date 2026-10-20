#!/usr/bin/env bash
# Run the Fcitx5 bridge end-to-end test inside a private D-Bus session with
# a throwaway Fcitx5, so the desktop input method is never touched.
# Expects VOICELESS_FCITX5 and VOICELESS_FCITX5_BRIDGE (set by the dev shell).
set -euo pipefail

if [ -z "${DBUS_RUN_SESSION_INNER:-}" ]; then
  export DBUS_RUN_SESSION_INNER=1
  exec env -u WAYLAND_DISPLAY -u DISPLAY dbus-run-session -- "$0" "$@"
fi

home=$(mktemp -d)
trap 'kill "${fcitx_pid:-}" 2>/dev/null && wait "$fcitx_pid" 2>/dev/null; rm -rf "$home"' EXIT
HOME=$home XDG_CONFIG_HOME=$home/.config \
  FCITX_ADDON_DIRS="$VOICELESS_FCITX5_BRIDGE/lib/fcitx5:$VOICELESS_FCITX5/lib/fcitx5" \
  XDG_DATA_DIRS="$VOICELESS_FCITX5_BRIDGE/share:$VOICELESS_FCITX5/share" \
  "$VOICELESS_FCITX5/bin/fcitx5" --disable=all \
  --enable=keyboard,dbus,dbusfrontend,voiceless-bridge >"$home/fcitx5.log" 2>&1 &
fcitx_pid=$!

for _ in $(seq 50); do
  busctl --user call org.fcitx.Fcitx5 /voiceless dev.voiceless.Fcitx5Bridge1 Status \
    >/dev/null 2>&1 && break
  sleep 0.1
done

cd "$(dirname "$0")/../rust"
# Tests share one Fcitx5 and its focus: run them one at a time.
VOICELESS_FCITX5_BRIDGE_TEST=1 cargo test --test fcitx5_bridge_live -- --nocapture --test-threads=1
