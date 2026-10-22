# Developer entry points (README "Development"). Every recipe runs inside
# `nix develop` (the default, CUDA-capable dev shell) so the toolchain and
# the Sherpa-ONNX libraries match the flake; CI runs fmt, test and the flake
# check the same way through the `.#cpu` shell. The benchmark gates are not
# here: they need real credentials and are run by hand
# (`just run benchmark asr|latency`).
set dotenv-load := false

# The crate lives in rust/; the flake (and nix develop) at the repository root.
manifest := "--manifest-path rust/Cargo.toml"

# Full cargo test suite (unit, contract, integration, examples compile).
test:
    nix develop -c cargo test {{manifest}} --all-targets

# End-to-end Fcitx5 bridge test in a private D-Bus session.
bridge-test:
    nix develop -c scripts/fcitx5-bridge-test.sh

# rustfmt check plus clippy over all targets and features with warnings as errors.
lint:
    nix develop -c cargo fmt {{manifest}} --check
    # Own target dir: sherpa-onnx-sys copies read-only Nix store libraries
    # into the profile dir and cannot overwrite them when clippy reruns its
    # build script over the test build.
    CARGO_TARGET_DIR=rust/target/clippy nix develop -c cargo clippy {{manifest}} --all-targets --all-features -- -D warnings

# Everything CI does: lint, test, then the sandboxed CPU package build with its tests.
check:
    just lint
    just test
    nix flake check -L

# Run the flake's default (CUDA) build with the given CLI arguments, e.g. `just run doctor`.
run *ARGS:
    nix run . -- {{ARGS}}
