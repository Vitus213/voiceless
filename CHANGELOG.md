# Changelog

All notable changes to Voiceless are listed here. The project has no
tagged releases yet; entries are grouped by the day the work landed on
`main`. Commit hashes refer to <https://github.com/Vitus213/voiceless>.

## Unreleased (2026-10-21)

### Added
- `voiceless stream` with a local backend now delegates to a running
  `voiceless daemon` over the new D-Bus method `StartSession(mode, inject)`
  instead of loading a second copy of the model (which could exhaust an
  8 GB GPU). Not delegated when `--config` or `--backend` is given.
  (0417c78)

### Fixed
- `voiceless doctor` runs its blocking checks off the Tokio runtime thread.
  (156f6a4)

### Documentation
- Module docs (`//!`) and item docs (`///`) across the whole crate, the
  tests, the console front end, the Fcitx5 bridge, the Nix files and the
  scripts; `cargo doc --no-deps --document-private-items` renders the
  crate without warnings and clap subcommands now print descriptions in
  `--help`.
- New `docs/architecture.md` / `docs/architecture.zh.md` (rewritten for the
  daemon, local-offline engine, text pipeline, Fcitx5 injection and
  console), `CONTRIBUTING.md` / `CONTRIBUTING.zh.md`, `SECURITY.md` and
  this changelog.
- README: the Home Manager example no longer suggests putting an API key in
  `settings` (the generated file lives in the Nix store and fails the
  `0600` gate).

## 2026-10-20 — resident daemon, GPU engine, console

### Added
- **Web console** served by the daemon on `http://127.0.0.1:8790/`: live
  session control with a mic level meter, engine and fallback settings,
  model install with progress, hotword / replacement / regex editing with a
  live preview, custom modes and prompts, hotkey capture, injection test,
  history with waveform playback, re-recognition and corrections, doctor
  and GPU diagnostics, and an in-place benchmark over saved recordings. The
  API is documented in `docs/console-api.md`. `voiceless history serve`
  serves the same app without the live page. (e48eec0)
- **evdev push-to-talk** (`[hotkey] key`): hold past `hold_threshold_ms` to
  talk, tap to toggle, Ctrl + key for a second mode. Works on compositors
  without key-release bindings. **Media pause** (MPRIS) and optional
  **start/stop cues** (`[audio]`). (2a73053)
- **`[text]` cleanup** applied to every final result: filler removal,
  pinyin-based hotword correction with a confusion-aware threshold, exact
  replacements and regex rules, optional trailing-period stripping.
  (3fb3d7b)
- **GPU engine**: Qwen3-ASR 1.7B / 0.6B GGUF on transcribe.cpp (CUDA,
  cargo feature `cuda`), **backend fallback** (`[asr] fallback_backend`),
  and **in-field preedit** through the Fcitx5 bridge
  (`[inject] preedit`). (eba70b3)
- **Resident daemon** with a warm engine, `voiceless ctl
  toggle|start|stop|cancel|status|watch`, `Event` D-Bus signals and an
  eww overlay (`contrib/eww-overlay`, Home Manager `overlay.enable`).
  (a5d9ebc)
- **`local-offline` backend**: Silero VAD segmentation, re-decoded partials,
  Sherpa-ONNX offline models (`qwen3-asr-0.6b-int8`, `sense-voice-int8`).
  (0ee74ea)
- **Volcengine (Doubao) streaming backend** (`doubao-realtime`, `[doubao]`).
  (d7d1abc)
- **Fcitx5 bridge module** (`fcitx5-bridge/`, `pkgs.voiceless-fcitx5-bridge`)
  and the `fcitx5` injection method. (b9085ff)

### Changed
- Project renamed from Syllune to Voiceless; binary, XDG directories, D-Bus
  names and the flake all use the new name. (438a20a)

## 2026-09-13

### Fixed
- CI runs cargo in `rust/`. (f4dbbe6, #1)

## 2026-08-17

### Added
- Clipboard injection restores the previous clipboard, `paste_tool` and
  `focus_command` options, optional X11 clipboard mirroring
  (`x11_clipboard_command`) for rootless Xwayland. (1d24920, 081d609)

## 2026-08-15 — native Rust cutover

### Added
- Session recordings retained as 16 kHz WAV (`[history] save_audio`),
  history schema v2 with `audio_path`, and a loopback history web console.
  (ce2bb57)
- Portal `GlobalShortcuts` integration with graceful fallback to the D-Bus
  control bus. (a4f6847)
- CER benchmark (`benchmark asr`, gate ≤ 0.02) and end-to-end latency
  benchmark (`benchmark latency`, stop→inject p99 ≤ 1.0 s). (c25e269,
  4ed5244, 59d9a6f)
- Headless daemon with a one-session gateway. (7a2a4f1)
- `transcribe` and `record` batch workflows; `doctor`. (7874d52, 8f83eef)
- Modes, processing providers and SQLite history wired into the stream
  pipeline. (5134775, 2029213)
- Integrity-checked model manager and `model` command. (e3334ce)
- Session coordinator with bounded audio backlog, ready gate, fixed
  stop-flush order and single injection. (81bb20d … df9fca5)

### Removed
- The Python distribution (type4me-linux). Voiceless is a single Rust
  binary; old XDG directories are never read or modified. (c9d1877)

## 2026-06-30 — 2026-08-08 (Python era)

- Scaffold, local Qwen3 Sherpa backend, CLI e2e coverage and CI, resident
  desktop voice input, CUDA hybrid recognition, latency work and the
  streaming recognition plan that led to the Rust rewrite
  (`docs/low-latency-rust-plan.md`).
