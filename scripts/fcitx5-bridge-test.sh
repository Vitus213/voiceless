#!/usr/bin/env bash
# Run the Fcitx5 bridge end-to-end test inside a private D-Bus session with
# a throwaway Fcitx5, so the desktop input method is never touched.
# Expects VOICELESS_FCITX5 and VOICELESS_FCITX5_BRIDGE (set by the dev shell).
#
# What it exercises (rust/tests/fcitx5_bridge_live.rs, opt-in through
# VOICELESS_FCITX5_BRIDGE_TEST=1): the test plays an application, creates
# and focuses an input context through Fcitx5's D-Bus frontend
# (org.fcitx.Fcitx.InputMethod1), then drives the real injection path
# (`stream::inject_text` with prefer = "fcitx5") and checks that
# CommitString reaches that context and that preedit shows inline and is
# cleared before the commit. Invoked by `just bridge-test`.
#
# The private session: `dbus-run-session` starts a fresh session bus and
# re-executes this script inside it (guarded by DBUS_RUN_SESSION_INNER).
# WAYLAND_DISPLAY and DISPLAY are unset so the throwaway Fcitx5 cannot
# attach to the live compositor and every client talks to it over D-Bus
# only. HOME and XDG_CONFIG_HOME point at a temp dir, so no user Fcitx5
# profile is read or written; FCITX_ADDON_DIRS and XDG_DATA_DIRS make
# Fcitx5 find the bridge module and descriptor from the Nix store build.
set -euo pipefail

if [ -z "${DBUS_RUN_SESSION_INNER:-}" ]; then
  export DBUS_RUN_SESSION_INNER=1
  exec env -u WAYLAND_DISPLAY -u DISPLAY dbus-run-session -- "$0" "$@"
fi

# Temp HOME is removed and the throwaway Fcitx5 killed on any exit path.
home=$(mktemp -d)
trap 'kill "${fcitx_pid:-}" 2>/dev/null && wait "$fcitx_pid" 2>/dev/null; rm -rf "$home"' EXIT
# --disable=all --enable=... loads only what the test needs: keyboard
# (a real input method so contexts activate), dbus (bus connection the
# bridge registers on), dbusfrontend (lets the test create contexts) and
# the bridge itself.
HOME=$home XDG_CONFIG_HOME=$home/.config \
  FCITX_ADDON_DIRS="$VOICELESS_FCITX5_BRIDGE/lib/fcitx5:$VOICELESS_FCITX5/lib/fcitx5" \
  XDG_DATA_DIRS="$VOICELESS_FCITX5_BRIDGE/share:$VOICELESS_FCITX5/share" \
  "$VOICELESS_FCITX5/bin/fcitx5" --disable=all \
  --enable=keyboard,dbus,dbusfrontend,voiceless-bridge >"$home/fcitx5.log" 2>&1 &
fcitx_pid=$!

# Wait up to 5 s for the bridge object to answer Status on the private bus
# (busctl --user follows the DBUS_SESSION_BUS_ADDRESS dbus-run-session
# set). If it never does, cargo test fails with a bus error, not a hang.
for _ in $(seq 50); do
  busctl --user call org.fcitx.Fcitx5 /voiceless dev.voiceless.Fcitx5Bridge1 Status \
    >/dev/null 2>&1 && break
  sleep 0.1
done

cd "$(dirname "$0")/../rust"
# Tests share one Fcitx5 and its focus: run them one at a time.
VOICELESS_FCITX5_BRIDGE_TEST=1 cargo test --test fcitx5_bridge_live -- --nocapture --test-threads=1
