#!/bin/sh
# eww deflisten feeder: tail the overlay FIFO forever. Each line the pump
# writes is one JSON frame; deflisten pushes it into the `stream` variable.
#
# At the start of every connect cycle we emit a fresh "listening" frame, so a
# (re)opened window always shows the default state and never the previous
# session's transcript. The border colour follows the current mode, which the
# pump writes to the mode file before opening the window.
#
# The pump is voiceless-overlay.py (its `frame` function writes the JSON,
# `open_fifo` holds the write end for the whole run). eww starts this
# script once when it loads eww.yuck; `cat` blocks on the FIFO until the
# pump opens it and returns when the pump exits, after which the loop
# re-emits the fresh frame and waits again (sleep 1 only if the FIFO does
# not exist yet). The frame's field names and colour values must stay in
# sync with `frame` and `STATE_COLORS` in the pump.
RUNTIME="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
MODEFILE="$RUNTIME/voiceless-overlay.mode"
FIFO="$RUNTIME/voiceless-overlay.fifo"

if [ "$(cat "$MODEFILE" 2>/dev/null)" = "prompt-optimize" ]; then
  border="rgba(240, 201, 108, 0.9)"   # organize gold
else
  border="rgba(98, 160, 234, 0.85)"   # live blue
fi
fresh="{\"text\":\"聆听中…\",\"width\":170,\"pillbg\":\"rgba(28, 37, 48, 0.78)\",\"pillborder\":\"$border\"}"

while :; do
  printf '%s\n' "$fresh"
  cat "$FIFO" || sleep 1
done