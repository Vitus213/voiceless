#!/bin/bash
set -euo pipefail
root=$(cd "$(dirname "$0")" && pwd)
base="$HOME/Library/Application Support/Voiceless/cli"
bin="$HOME/.local/bin"
mkdir -p "$base" "$bin"
if [[ -e "$bin/voiceless" || -L "$bin/voiceless" ]]; then
  if [[ ! -L "$bin/voiceless" || $(readlink "$bin/voiceless") != "$base/"*/bin/voiceless ]]; then
    echo "Another installation owns $bin/voiceless" >&2
    exit 1
  fi
fi
release=$(mktemp -d "$base/release.XXXXXXXX")
trap 'rm -rf "$release"' EXIT
/usr/bin/ditto "$root/voiceless-cli" "$release"
"$release/bin/voiceless" --version
linkdir=$(mktemp -d "$bin/.voiceless.XXXXXXXX")
trap 'rm -rf "$release" "$linkdir"' EXIT
ln -s "$release/bin/voiceless" "$linkdir/voiceless"
mv -fh "$linkdir/voiceless" "$bin/voiceless"
trap - EXIT
rmdir "$linkdir"
printf 'Installed %s. Add %s to PATH to run voiceless.\n' "$bin/voiceless" "$bin"
