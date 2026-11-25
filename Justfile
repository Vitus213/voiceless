# Developer entry points (README "Development"). Every recipe runs inside
# `nix develop` (the default, CUDA-capable dev shell) so the toolchain and
# the Sherpa-ONNX libraries match the flake; CI runs fmt, test and the flake
# check the same way through the `.#cpu` shell. The benchmark gates are not
# here: they need real credentials and are run by hand
# (`just run benchmark asr|latency`).
set dotenv-load := false

# The crate lives in rust/; the flake (and nix develop) at the repository root.
manifest := "--manifest-path rust/Cargo.toml"

# The console's TypeScript/Vite bundle, embedded in the binary at compile
# time: build it before anything that compiles the crate.
console:
    nix develop -c npm --prefix frontend ci --ignore-scripts --no-audit --no-fund
    nix develop -c npm --prefix frontend run build

# Full cargo test suite (unit, contract, integration, examples compile).
test: console
    nix develop -c sh -c 'exec cargo test --config "$VOICELESS_CARGO_CONFIG" {{manifest}} --all-targets'

# Browser tests against the binary `just test` built; extra arguments are
# playwright's, e.g. `just e2e --grep accessibility`.
e2e *ARGS:
    nix develop -c npm --prefix frontend run test:e2e -- {{ARGS}}

# End-to-end Fcitx5 bridge test in a private D-Bus session.
bridge-test:
    nix develop -c scripts/fcitx5-bridge-test.sh

# rustfmt check plus clippy over all targets and features with warnings as errors.
lint:
    nix develop -c cargo fmt {{manifest}} --check
    nix develop -c sh -c 'exec cargo clippy --config "$VOICELESS_CARGO_CONFIG" {{manifest}} --all-targets --all-features -- -D warnings'

# Everything CI does: lint, test, then the sandboxed CPU package build with its tests.
check:
    just lint
    just test
    nix flake check -L

# Run the flake's default (CUDA) build with the given CLI arguments, e.g. `just run doctor`.
run *ARGS:
    nix run . -- {{ARGS}}
