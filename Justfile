set dotenv-load := false

test:
    nix develop -c cargo test --all-targets

# End-to-end Fcitx5 bridge test in a private D-Bus session.
bridge-test:
    nix develop -c scripts/fcitx5-bridge-test.sh

lint:
    nix develop -c cargo fmt --check
    nix develop -c cargo clippy --all-targets --all-features -- -D warnings

check:
    just lint
    just test
    nix flake check -L

run *ARGS:
    nix run . -- {{ARGS}}
