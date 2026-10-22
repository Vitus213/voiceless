# Contributing to Voiceless

Thanks for helping. This page covers the development setup, the checks a
change must pass, the layout of the code, and a few quirks that cost time
when you do not know them. The Chinese edition is
[CONTRIBUTING.zh.md](CONTRIBUTING.zh.md).

## Development environment

Everything runs through the Nix flake at the repository root; the Rust
crate itself lives in `rust/`.

```bash
nix develop            # CUDA dev shell (default): cargo, clippy, rustfmt, pipewire, wtype, wl-clipboard, Sherpa-ONNX, CUDA toolchain
nix develop .#cpu      # CPU-only shell: what CI uses; no CUDA download
```

Both shells export `SHERPA_ONNX_LIB_DIR` and `LD_LIBRARY_PATH` so the
`sherpa-onnx` crate links against the nixpkgs build instead of downloading
binaries. The CUDA shell also exports `TRANSCRIBE_CMAKE_ARGS` and
`VOICELESS_CUDA_LIB_DIRS` for the transcribe.cpp engine
(`cargo build --features cuda`).

`just` wraps the common commands and always passes
`--manifest-path rust/Cargo.toml`:

| Recipe | What it runs |
| --- | --- |
| `just test` | `cargo test --all-targets` inside `nix develop` |
| `just lint` | `cargo fmt --check` and `cargo clippy --all-targets --all-features -D warnings` |
| `just check` | `just lint`, `just test`, then `nix flake check -L` (builds and tests the CPU package in the sandbox, including doctests) |
| `just run …` | `nix run . -- …` |
| `just bridge-test` | end-to-end Fcitx5 bridge test in a private D-Bus session (`scripts/fcitx5-bridge-test.sh`) |

### Quirks worth knowing

- **Clippy uses its own target dir** (`rust/target/clippy`). The
  `sherpa-onnx-sys` build script copies the read-only Nix store libraries
  into the profile directory and cannot overwrite them on a second run, so
  sharing a target dir between `cargo test` and `cargo clippy` fails with
  `Permission denied`. Use a separate `CARGO_TARGET_DIR` for any other
  cargo invocation that rebuilds the build script (for example
  `cargo doc`).
- **Use the shell's `cargo`/`clippy`/`rustfmt`.** A clippy from your user
  profile rejects crates built by the shell's rustc (E0514).
- **GPU tests need the driver.** `nix flake check` builds the CPU package
  only; run `cargo test --features cuda` locally for the GGUF engine.
- **Real-service tests skip, not pass.** Tests named `real_*` and the
  `benchmark` gates need credentials, installed models or a Wayland session;
  without them they report `skip`/`unverified` and never a pass.

### Driving a session without a microphone

The capture layer spawns `pw-record` from `PATH`, so an end-to-end run can
feed a WAV through a fake. Put an executable `pw-record` on `PATH` that
writes raw 16 kHz mono PCM16 to stdout and exits on SIGINT (install an
explicit handler: a daemon started with `&` from a script inherits
`SIG_IGN`). When testing the installed Nix package, call
`bin/.voiceless-wrapped`, because the wrapper prepends the real pipewire
to `PATH`. Point `XDG_DATA_HOME` at a scratch directory (with a `models`
symlink to your real models) to keep test sessions out of your history.

## Code layout

```text
rust/src/
  main.rs            clap CLI, exit codes                 lib.rs            crate docs and module map
  coordinator.rs     one session: capture → transport → text → inject → history (the only side-effect boundary)
  session.rs         transcript state machine             capture.rs        pw-record + WavRecorder
  stream.rs          `stream` command: config → backend (+fallback) → coordinator; injection methods
  realtime.rs        DashScope realtime WebSocket         doubao.rs         Volcengine streaming protocol
  local_asr.rs       Sherpa-ONNX streaming Paraformer     offline_asr.rs    VAD-segmented offline decoders
  gguf_asr.rs        transcribe.cpp GGUF engine (feature) engine.rs         warm engine kept by the daemon
  daemon.rs          gateway state machine, D-Bus, hotkey machine, console startup
  ctl.rs             `voiceless ctl` D-Bus client         hotkey.rs         evdev push-to-talk listener
  fcitx5.rs          Fcitx5 bridge client + preedit sink  media.rs          MPRIS pause, start/stop cues
  text.rs            `[text]` cleanup                     processing.rs     LLM modes (openai-compatible, ollama)
  modes.rs           mode repository (modes.json)         history.rs        SQLite history, schema v2
  models.rs          pinned model catalog + installer     config.rs         strict TOML schema, 0600 gate
  batch.rs / batch_cmd.rs       `transcribe`, `record`    doctor.rs         dependency checks
  benchmark*.rs / latency*.rs   CER and latency gates     console/          axum web console (+ assets/console)
  *_cmd.rs           thin CLI adapters for model / mode / history
rust/tests/          integration tests; `common/mod.rs` holds the fakes (capture, transport, injector, history)
fcitx5-bridge/       C++ Fcitx5 addon exposing dev.voiceless.Fcitx5Bridge1
contrib/eww-overlay/ eww overlay driven by `voiceless ctl watch`
nix/                 Home Manager module, Fcitx5 bridge package
docs/                architecture, console API, historical plans
openspec/            specs and change proposals
scripts/asr_benchmark/  corpus, TTS generation and normalization for `benchmark asr`
```

Read `docs/architecture.md` before touching the session path; it lists the
invariants the tests enforce (bounded queue, stop order, single injection,
no injection on cancel).

## Making a change

1. **Keep the contracts.** CLI names and flags, TOML keys, JSON event
   fields, model ids and D-Bus names are stable identifiers. Removing or
   renaming one needs a note in `CHANGELOG.md` and the README tables.
   Unknown TOML keys and removed legacy values are rejected on purpose; do
   not add silent rewrites.
2. **Tests first for behaviour.** Every behaviour change gets an
   integration test under `rust/tests/` using the fakes in
   `tests/common/mod.rs`; protocol code gets a unit test with captured
   frames. Tests that need real services must skip cleanly.
3. **Docs in the same commit.** User-visible changes update `README.md`
   and `README.zh.md` (both), `docs/console-api.md` for console endpoints,
   `docs/architecture.md` for new modules or invariants, and the
   `CHANGELOG.md` entry.
4. **Comments explain why.** Module docs (`//!`) describe the module's
   place in the pipeline and its guarantees; `///` on public items; inline
   comments for timeouts, protocol details and workarounds. No fenced Rust
   code in doc comments (they become doctests in `nix flake check`).
5. **Run `just check`** before opening a pull request. CI runs the same
   `cargo fmt --check`, `cargo test --all-targets` (CPU shell) and
   `nix flake check`.

### Commit messages

Conventional-commit style, imperative mood, one change per commit:
`feat: …`, `fix: …`, `docs: …`, `test: …`, `ci: …`, `chore: …`, optionally
scoped (`fix(ci): …`). Reference the affected config section or command in
the subject when it helps (`feat: [text] cleanup — …`).

### Pull requests

Describe the behaviour change, how you verified it (which tests, and on
which desktop if it touches injection or hotkeys), and any contract
changes. Screenshots for console changes go in the PR, not in `docs/images`
unless the README uses them.

## Adding things

- **A streaming backend**: implement `BackendTransport` (see
  `coordinator.rs`), add a `"<name>"` arm in `stream::open_transport`, a
  config section in `config.rs` with validation, a `StreamError` variant
  for missing credentials, protocol tests under `rust/tests/`, and README
  rows in "Configuration" and "Fit". The ready gate must fail before
  capture starts.
- **A model**: add a `ModelSpec` to `models::catalog()` with the pinned
  URL, byte size, SRI SHA-256, archive member allow-list and
  `license_status`, then teach `offline_asr::OfflineFamily::for_model` (or
  the streaming path) how to load it. Verify the hash independently before
  committing.
- **A console endpoint**: handler in `console/`, a row in
  `docs/console-api.md`, a test in `tests/console.rs` covering the token
  and `Host` checks, and the page code in `rust/assets/console/app.js`.
- **A config key**: field with a default in `config.rs`, validation, the
  README key reference in both languages, and the console settings page if
  users should edit it.

## Releasing

There is no release pipeline yet. Users pin the flake by revision, so
"shipping" means pushing to `main`, bumping the `voiceless` input in the
consumer's `flake.lock`, and `home-manager switch`. Keep `CHANGELOG.md`
current so a tag can be cut from it later.
