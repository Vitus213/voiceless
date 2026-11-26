# Changelog

Releases are tagged `v<major>.<minor>.<patch>`. Entries up to 0.1.0 are grouped by
the day the work landed on `main`; commit hashes refer to
<https://github.com/Vitus213/voiceless>.

## Unreleased

### Added

- The console detects PipeWire microphones, reports the active source and
  shows recording through a Wayland overlay that preserves input focus.
- Dictation and explicit typing tests on single-seat Sway verify the focused
  window before sending text. A change of focus keeps the result available to copy.
- Import compatible GGUF models with `voiceless model import` or in the
  console. The model list includes languages and supported devices.
- Set separate keyboard shortcuts for hold, toggle, cancel, raw dictation
  and repaste. The console displays the active bindings and permission errors.
- `voiceless-desktop` opens one window over the console of the running
  daemon and, on a desktop with a tray, offers opening the window, pausing,
  repasting, starting at login and quitting. Closing the window leaves the
  daemon running and the launcher opens the window again.
- Output modes can match an application or an opted-in site hostname. Preview
  the ordered steps in the console, on the CLI or through local OpenCode MCP.
- Import and export scoped vocabularies and add selected history corrections
  to a mode or project. Output steps can run an authorized text-only Rhai script.

### Changed
- The web console is redesigned: an icon rail, a session strip above every
  page (push-to-talk key, live waveform, mode picker, last session's numbers),
  and forms that lead with icons and tooltips instead of helper text. Dark by
  default, following the OS light/dark setting unless pinned from the rail.
- The console's engine page exposes `[asr] batch_backend`, `local_model_dir`
  and `batch_model_dir`, `[local] fast_preview` and the `auto` and `vulkan`
  devices; secret fields can reveal what was typed; the live page shows a
  recording clock.
- `voiceless web` keeps the existing page links and supports keyboard
  navigation at narrow window widths. The Vite bundle is embedded in the binary.
- The console restores the current session after a disconnect and refreshes
  its session token after a daemon restart. Old controls cannot affect a new
  daemon instance.
- Concurrent settings edits merge by field. The console previews conflicts,
  exports a credential-free TOML template and can restore the active config.
- Model and shortcut changes are validated after the session ends. Failed
  changes keep the last working settings active.

### Fixed
- Stopping `pw-record` on PipeWire 1.6 preserves the final transcript and WAV.
  A disconnected recorder still fails the session.
- On Linux, `voiceless doctor` checks the same PipeWire or GStreamer tools
  used for recording. Ubuntu 22.04 needs GStreamer's PipeWire plugin.
- On Ubuntu 22.04, dictation can keep using the system default microphone
  across link checks. If a selected device disappears, dictation and meeting
  recording detect the loss on the next check and preserve recoverable audio.
- Linux capture identifies when PipeWire rejects its recorder and reports
  microphone access settings even when the recorder is silent.
- The CLI, daemon and web console can open a new history database concurrently.
  The console publishes its address after the history database is ready.
- File transcription accepts streamed DashScope WAVs and measures their
  duration from complete PCM frames when the encoder leaves placeholder lengths.
- On Windows, model verification recognizes nested files in the catalog, so
  correctly installed SenseVoice and bilingual Paraformer models load again.
- Windows reports the selected source when WASAPI denies recording. A meeting
  refused at startup also shows where to enable desktop microphone access in
  Windows Settings, alongside the original WASAPI error.
- Uninstalling the Windows CLI preserves the user's trailing PATH separator.
- On macOS, meetings wait for microphone and system audio permission before
  starting either track. An unanswered system audio request times out after
  30 seconds and leaves no meeting on disk. A microphone-only meeting can
  still record with an already authorized input.
- If the first-run shortcut times out, the console points to the binding in
  Shortcuts and typing and offers the Try a phrase button. On Windows and
  macOS, the Sound section directs users to the window for recording status.
- `record` and `transcribe` honor `--config`. Recording uses the microphone
  from the same configuration that selects the transcription backend.
- Windows checkouts with CRLF line endings can build the web console. The
  build still rejects a settings index that differs from the source.
- A second `voiceless daemon` on the same session bus is refused instead of
  taking the bus name from the running one, which left `voiceless ctl`, the
  overlay and the console without a daemon.
- The desktop host opens its window on the console's address directly and
  says when the running daemon is older than the desktop app.
- A setting found through the console's search while another page was still
  loading receives the keyboard focus once its own page is mounted.
- Orca announces each session phase and the result. The announcements go
  through one live region without a role, which a screen reader presents
  every time; the connection lamp and the first-run page's status line no
  longer compete with it, a practice ends with “试说完成”, and opening a
  dialog no longer repeats the last message.
- Dictation needs a verified original target before typing or pasting.
  Fcitx5 previews and commits stay bound to the original input context.
  When a send may have reached an application, Voiceless keeps the result
  available for a user-initiated repaste.

## 0.1.3 - 2026-11-24

### Added
- Linux x86-64 Bun packages and standalone archives include the Vulkan GGUF
  backend, also available through the `voiceless-vulkan` Nix package.
- `voiceless web` opens a running console or starts a local one for history
  and settings. `--no-open` prints the URL for manual use.

### Changed
- Vulkan builds default to `[local] provider = "auto"` for GGUF so CPU fallback
  works when a GPU cannot initialize. Explicit `vulkan` requires a device.

### Fixed
- x86-64 release builds disable system BLAS detection to keep the native
  archive and Bun package portable across the supported glibc baseline.

## 0.1.1 - 2026-11-24

### Changed
- No change to the program. This release validates publishing without an npm
  token: the workflow's OIDC identity publishes with provenance once each
  package lists `native-release.yml` as a trusted publisher.
- The publish step waits up to five minutes for npm to list a package and no
  longer fails on the placeholder tag of a staged version.

## 0.1.0 - 2026-11-24

### Added
- Build standard CPU Linux release archives and matching Bun packages from
  the same native payload. `voiceless update` follows the existing installation
  manager. Standalone installations can roll back to the previous local release.
- Stable version tags publish validated Linux packages to npm and GitHub
  Releases. Manual workflow runs produce candidates for inspection.
- Optional `local-offline` speech enhancement runs GTCRN and bounded gain
  ahead of speech detection and recognition. Install the `gtcrn` model and
  enable `[local] denoise` or use the console's recognition settings.
- Optional `[local] fast_preview` uses SenseVoice int8 on CPU for live
  previews and the selected Qwen ONNX or GGUF model for final transcription.
  Preview updates can continue while that model finalizes an earlier segment.
- Optional `voiceless-cpu-avx2` Nix package enables the CPU GGUF engine with
  AVX2 for x86-64-v3 machines.
- `voiceless stream` with a local backend now delegates to a running
  `voiceless daemon` over the new D-Bus method `StartSession(mode, inject)`
  instead of loading a second copy of the model (which could exhaust an
  8 GB GPU). Not delegated when `--config` or `--backend` is given.
  (0417c78)

### Fixed
- Local offline dictation confirms speech sooner on the default input path,
  retries empty or unchanged previews sooner, spaces costly decodes, and
  retains audio across VAD boundaries and at stop.
- GTCRN-enhanced dictation detects weaker speech onsets to retain more of
  the opening words in noisy audio.
- GTCRN-enhanced dictation blends 35% aligned original audio into the
  enhanced signal to improve transcription in simulated distant speech.
- Live sessions keep audio capture and backend events moving during bursts
  and preserve the send deadline for each queued audio frame.
- The terminal and Fcitx5 clear live text when recognition retracts it.
- Local offline dictation keeps waiting for the worker's final result after
  capture ends, so a slow decode can still return its transcript. The wait
  can be cancelled.
- Paraformer local streaming decodes the remaining short audio block at stop,
  so the last words can appear in the final transcript.
- The `local-streaming` daemon loads Paraformer once and starts a fresh
  recognition stream for each recording. The console requests an engine
  reload when the selected Paraformer model path changes.
- The daemon's evdev push-to-talk key keeps working after a keyboard is
  unplugged and replugged or re-enumerated (USB reset, KVM switch): the
  listener watches `/dev/input` with inotify instead of enumerating
  devices once at startup, and `hotkey.keyboards` in the status follows.
- The overlay watcher starts at login again: its unit now orders itself
  `After=graphical-session.target`, so systemd no longer breaks an ordering
  cycle (`voiceless-overlay.service` → `voiceless.service` →
  `graphical-session.target` → `voiceless-overlay.service`) by deleting the
  job, which left the pill dead after every reboot.
- `voiceless doctor` runs its blocking checks off the Tokio runtime thread.
  (156f6a4)
- `voiceless record --seconds N` now stops `pw-record` after N seconds; the
  value was silently ignored before.
- The console's `GET /api/audio/{id}` no longer panics on a reversed
  `Range` (`bytes=50-10`); it answers 416.
- `benchmark latency` marks the gate unverified when an interval was not
  measured (notably stop→inject without `--inject`) instead of passing it,
  and honours `--enforce` like `benchmark asr` (budget failures exit 1 only
  when enforced).
- A backend `error` event that arrives before `ready` (a rejected API key)
  now fails the session with that message instead of a 5 s ready timeout.
- Doubao `send_audio` is cancel-safe: a chunk whose send was interrupted
  and retried is no longer sent twice.
- `[console] host` accepts a hostname such as `localhost`, not only an IP
  literal.
- History rows record `asr_model` (cloud model, Doubao resource id or local
  catalog id); it was always NULL, so `history usage` had no per-model
  counts.
- Injection helpers kill a `wtype`/paste tool that exceeds
  `[inject] timeout_seconds` instead of orphaning it; the batch cloud client
  no longer sleeps after its final retry; local SenseVoice batch
  transcription enables inverse text normalization like the streaming path;
  the daemon's per-session `audio_seconds` metric starts at `ready` rather
  than at the state change.

### Changed
- D-Bus `Activate`/`Toggle`/`Start`/`StartSession`/`Stop`/`Cancel` wait for
  the gateway and reply `started`, `stopping` or `cancelled`; a rejected
  command (start while recording, stop while idle) is a D-Bus error, so
  `voiceless ctl` exits 1 instead of 0 with "queued". The console's
  `POST /api/session/*` likewise answers 409 with the reason and names the
  outcome in its body.
- The model list reports `progress: null` while a completed download is
  being verified, so the console shows a verifying state instead of 100 %.

### Removed
- Dead code: `AppConfig::save`, `ProcessingError::NoProvider`,
  `ProcessedText`, `capture::read_fixed_chunks`, `TrialStage::TailSent`.

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
