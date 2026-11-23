#!/usr/bin/env bash
# Assemble the npm package from a non-Nix `cargo build --release` output
# directory and pack it to <out>/voiceless-<version>.tgz. Prints the tarball
# path on stdout.
#
# The flake's binary is not distributable: its ELF interpreter and runpath
# point into /nix/store, and the default package is the CUDA build, which
# needs the NVIDIA driver libraries to load. The release workflow therefore
# builds the crate with plain cargo on a stock distribution and runs this
# script on the result. The package is the binary plus every shared object
# `sherpa-onnx-sys` copied into the profile directory. They load from the
# directory the binary lives in (`bin/`; npm's `.bin` entry is a symlink and
# the loader resolves $ORIGIN through it) through the runpath the workflow
# passes in RUSTFLAGS: the crate's own `cargo:rustc-link-arg` rpath does not
# reach dependent binaries.
#
# Name, version, description and license come from rust/Cargo.toml, so a
# release bumps one place. Linux x86_64 only: the `cpu` field is fixed.
# Requires cargo, jq and npm.
#
#   scripts/npm-package.sh --target-dir rust/target/release --out dist
set -euo pipefail

usage() {
  echo "usage: $0 --target-dir <cargo profile dir> --out <dist dir>" >&2
  exit 2
}

fail() {
  echo "$0: $*" >&2
  exit 1
}

target_dir=
out=
while [ $# -gt 0 ]; do
  case "$1" in
    --target-dir)
      [ $# -ge 2 ] || usage
      target_dir=$2
      shift 2
      ;;
    --out)
      [ $# -ge 2 ] || usage
      out=$2
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done
[ -n "$target_dir" ] && [ -n "$out" ] || usage

for tool in cargo jq npm; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

root=$(cd "$(dirname "$0")/.." && pwd)
binary=$target_dir/voiceless
[ -x "$binary" ] || fail "$binary is missing: run cargo build --release first"
compgen -G "$target_dir/*.so*" >/dev/null || fail "no shared objects in $target_dir"

# --no-deps lists exactly one package: the crate itself.
metadata=$(cargo metadata --no-deps --format-version 1 \
  --manifest-path "$root/rust/Cargo.toml")

stage=$out/npm
rm -rf "$stage"
mkdir -p "$stage/bin"
cp "$binary" "$stage/bin/voiceless"
cp "$target_dir"/*.so* "$stage/bin/"
cp "$root/README.md" "$root/LICENSE" "$stage/"

jq '.packages[0] | {
  name: "voiceless",
  version,
  description,
  license,
  homepage: "https://github.com/Vitus213/voiceless#readme",
  repository: {type: "git", url: "git+https://github.com/Vitus213/voiceless.git"},
  bugs: "https://github.com/Vitus213/voiceless/issues",
  keywords: ["voice-input", "speech-recognition", "asr", "wayland", "linux"],
  bin: {voiceless: "bin/voiceless"},
  files: ["bin"],
  os: ["linux"],
  cpu: ["x64"],
  libc: ["glibc"]
}' <<<"$metadata" >"$stage/package.json"

# npm pack prints the tarball file name.
tarball=$out/$(cd "$out" && npm pack ./npm)
[ -f "$tarball" ] || fail "npm pack did not produce $tarball"
echo "$tarball"
