set dotenv-load := false

# The crate lives in rust/; the flake (and nix develop) at the repository root.
manifest := "--manifest-path rust/Cargo.toml"

test:
    nix develop -c cargo test {{manifest}} --all-targets

# End-to-end Fcitx5 bridge test in a private D-Bus session.
bridge-test:
    nix develop -c scripts/fcitx5-bridge-test.sh

lint:
    nix develop -c cargo fmt {{manifest}} --check
    # Own target dir: sherpa-onnx-sys copies read-only Nix store libraries
    # into the profile dir and cannot overwrite them when clippy reruns its
    # build script over the test build.
    CARGO_TARGET_DIR=rust/target/clippy nix develop -c cargo clippy {{manifest}} --all-targets --all-features -- -D warnings

check:
    just lint
    just test
    nix flake check -L

run *ARGS:
    nix run . -- {{ARGS}}
