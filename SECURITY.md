# Security policy

Voiceless runs on your own desktop and talks to at most three kinds of
remote services: the ASR provider you configure (DashScope or Volcengine),
the LLM provider used by non-`quick` modes, and the model download hosts
listed in `rust/src/models.rs`. Nothing else leaves the machine.

## What the program protects

- **API keys.** `config.toml` must be mode `0600` or stricter when it
  contains a key; the loader refuses to start otherwise. The web console
  never returns a stored key: config reads mask secrets and report only a
  `has_<field>` boolean, and writing `""` keeps the stored value.
- **Web console.** Binds to loopback only by default (`[console] host`),
  rejects any request whose `Host` header is not loopback (DNS rebinding),
  requires a random per-start token on every `/api/*` request (embedded in
  the served page, unreadable cross-site), and refuses mutating requests
  whose `Origin` is not the console's own origin. Audio URLs carry a record
  id, never a path; the path is resolved from the database row.
- **Model downloads.** Every catalog entry pins the URL, byte size and an
  SRI SHA-256. Installs verify size and hash, reject archive members outside
  an allow-list, write a manifest and activate atomically. Models are
  re-verified before each session (except the daemon's warm engine, which is
  verified once at load).
- **History and recordings.** `history.sqlite3` is `0600`; recordings live
  under the user's XDG data dir and are deleted together with their rows.
- **Hotkey.** The evdev listener only reads key events; it never grabs the
  device, so no keystrokes are hidden from other applications. It needs the
  `input` group, which also lets the process read other keys; the code only
  forwards the configured key and Ctrl state.

## Reporting a vulnerability

Please do not open a public issue for security problems. Use GitHub's
private vulnerability reporting on
<https://github.com/Vitus213/voiceless/security/advisories/new>, or email
the maintainer address shown on the GitHub profile. Include the Voiceless
version (`voiceless --version`), your configuration with secrets removed,
and steps to reproduce. You should hear back within a week.

## Scope notes

- Text you dictate is sent to the configured cloud ASR and, for non-`quick`
  modes, to the configured LLM provider. Use `local-offline` or
  `local-streaming` with `[processing] provider = "none"` for a fully
  offline setup.
- The console is a convenience for a single-user desktop. Exposing it on a
  non-loopback address is unsupported and the `Host` check will block it.
