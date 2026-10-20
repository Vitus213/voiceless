# Voiceless console API

The console is served by `voiceless daemon` (and, without live control, by
`voiceless history serve`) on `http://127.0.0.1:8790/`. The page is a
single embedded app (`rust/assets/console/`); everything below is JSON over
HTTP unless noted.

## Security

- Every request must carry `Host: 127.0.0.1:<port>` or `localhost:<port>`
  (blocks DNS rebinding). Anything else → `421`.
- Every `/api/*` request must carry the session token: header
  `X-Voiceless-Token: <token>`, or `?token=<token>` for `EventSource` and
  `<audio src>` URLs. Missing/wrong → `403`. The token is random per server
  start and is embedded in the served page as
  `<meta name="voiceless-token" content="...">`; cross-site pages cannot
  read it.
- Mutating requests with an `Origin` header must have a loopback origin on
  the same port → otherwise `403`.
- Secrets are write-only: config reads return `""` for `api_key`,
  `access_key` (and the `[doubao]`/`[cloud]`/`[processing]` keys) plus a
  boolean `has_<field>`; writing `""` keeps the stored secret, writing
  `null` clears it.

Errors: non-2xx with body `{"error": "message"}`.

## Live session (daemon only; `history serve` answers 503 `{"error":"daemon not running"}`)

| Method | Path | Body / query | Response |
|---|---|---|---|
| GET | `/api/status` | — | `Status` |
| GET | `/api/events?token=` | — | `text/event-stream`, one `data: <json>` per event |
| POST | `/api/session/toggle` | `{"mode": "quick"}` (mode optional, `""` = default) | `{"ok": true}` |
| POST | `/api/session/start` | `{"mode": "..."}` | `{"ok": true}` |
| POST | `/api/session/stop` | — | `{"ok": true}` |
| POST | `/api/session/cancel` | — | `{"ok": true}` |
| POST | `/api/engine/reload` | — | `{"ok": true, "engine": "qwen3-asr-1.7b-gguf on cuda", "load_seconds": 2.1}` (re-reads config, reloads warm engine and hotkey listener) |

`Status`:

```json
{
  "daemon": true,
  "state": "idle | recording | stopping",
  "mode": "quick",
  "default_mode": "quick",
  "backend": "local-offline",
  "fallback_backend": null,
  "engine": {"loaded": true, "description": "qwen3-asr-1.7b-gguf on cuda", "load_seconds": 2.1, "error": null},
  "gpu": [{"name": "NVIDIA GeForce RTX 3070", "memory_used_mib": 4100, "memory_total_mib": 8192}],
  "hotkey": {"key": "KEY_SCROLLLOCK", "keyboards": 2, "error": null},
  "last_session": {"backend": "local-offline", "audio_seconds": 6.1, "stop_to_final_ms": 180, "chars": 32, "ended_at": "2026-10-20T14:05:00Z"} ,
  "uptime_seconds": 3600,
  "version": "0.1.0"
}
```

`gpu` is `[]` without `nvidia-smi`; `last_session` is `null` before the first session.

Events (`/api/events`) are the daemon's event lines (same schema as
`voiceless stream --json` / `voiceless ctl watch`):

- `{"type":"state","state":"recording","mode":"quick"}`
- `{"type":"ready"}`, `{"type":"transcript","transcript":{"authoritative_text":"…","partial_text":"…","confirmed_segments":[…],"is_final":false}}`
- `{"type":"level","rms":0.12}` — mic level 0..1, ~15 Hz while recording
- `{"type":"finalized","injection":{"ok":true,"method":"fcitx5","message":""}|null}`
- `{"type":"warning"|"error","message":"…"}`, `{"type":"cancelled"}`, `{"type":"completed"}`
- `{"type":"session","stop_to_final_ms":180,"audio_seconds":6.1,"chars":32}` — after each completed session
- `{"type":"log","line":"…"}` — each daemon log line

## Config

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/config` | — | full `AppConfig` as JSON (secrets masked, see above) |
| PUT | `/api/config` | partial `AppConfig` JSON: any subset of sections/keys, e.g. `{"text":{"hotwords":["X"]}}` | `{"ok": true, "restart_engine": false}` or 400 with validation error |

The server merges the patch into the TOML file preserving comments and
formatting (toml_edit), validates the result with the normal config loader
before writing (0600), and reports `restart_engine: true` when `[local]`,
`[text].hotwords`, `[asr].streaming_backend` or `[hotkey]` changed (the
page then offers "重新加载引擎" → `POST /api/engine/reload`). Other changes
apply from the next session.

Sections: `asr`, `cloud`, `doubao`, `local`, `text`, `hotkey`, `audio`,
`inject`, `processing`, `history` — same keys as `config.toml` (README
"Configuration").

| Method | Path | Body | Response |
|---|---|---|---|
| POST | `/api/text/preview` | `{"text": "…", "config": <TextConfig>}` (config optional = saved one) | `{"ok": true, "text": "cleaned"}` or 400 for an invalid rule |

## Models

| Method | Path | Response |
|---|---|---|
| GET | `/api/models` | `[{"id","version","size_bytes","license_status","installed":bool,"installing":bool,"progress":0.0-1.0|null,"error":null,"kind":"gguf|sherpa|vad|streaming","in_use":bool}]` |
| POST | `/api/models/{id}/install` | `{"ok": true}` (starts a background install; poll `/api/models`) |
| DELETE | `/api/models/{id}` | `{"ok": true}` (refused with 409 if it is the loaded engine) |

## Modes

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/modes` | — | `[{"id","name","prompt","processing_label","builtin","sort_order"}]` |
| POST | `/api/modes` | `{"name","prompt","processing_label"}` | the new mode |
| PUT | `/api/modes/{id}` | any of `{"name","prompt","processing_label"}` | the updated mode |
| DELETE | `/api/modes/{id}` | — | `{"ok": true}` (builtins refused with 409) |
| GET | `/api/prompt` | — | `{"prompt","model","provider"}` (organize prompt; unchanged from the history console) |
| POST | `/api/prompt` | `{"prompt": "…"}` | `{"ok": true}` |

## History

| Method | Path | Query/body | Response |
|---|---|---|---|
| GET | `/api/records` | `limit`, `cursor` | `{"records":[HistoryRecord],"next_cursor"}` (unchanged) |
| GET | `/api/totals` | — | `{"records","characters","duration_seconds"}` (unchanged) |
| GET | `/api/audio/{id}?token=` | `Range` supported | `audio/wav` (unchanged) |
| DELETE | `/api/records/{id}` | — | `{"ok": true}` (also removes the recording) |
| POST | `/api/records/{id}/retranscribe` | — | `{"ok": true, "text": "…", "cleaned": "…", "seconds": 0.21, "engine": "…"}` (daemon with a warm local engine; else 503) |
| POST | `/api/corrections` | `{"original": "…", "corrected": "…"}` | `{"ok": true, "suggestions": [{"from": "张三峰", "to": "张三丰"}]}` — diff-based replacement suggestions; the page then saves chosen ones as `text.replacements` / `text.hotwords` through `PUT /api/config` |

`HistoryRecord`: `{"id","created_at","duration_seconds","raw_text","processing_mode","processed_text","final_text","status","character_count","asr_provider","asr_model","audio_path","has_audio"}`.

## Hotkey & injection

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/hotkey/keys` | — | `[{"code":"ScrollLock","evdev":"KEY_SCROLLLOCK"}, …]` browser `KeyboardEvent.code` → evdev name for the key-capture widget |
| POST | `/api/inject/test` | `{"text": "…", "delay_ms": 3000}` | `{"ok": true, "injection": InjectionResult}` — waits `delay_ms` (user focuses a field), then injects with the configured method |

## Diagnostics

| Method | Path | Response |
|---|---|---|
| GET | `/api/doctor` | `[{"name","present","required","detail"}]` |
| GET | `/api/logs` | `{"lines": ["…"]}` — last 500 daemon log lines (live ones arrive as `log` events) |
| POST | `/api/benchmark` | body `{"limit": 10}` → runs the warm engine over the latest recordings that have audio; `{"ok":true,"engine":"…","results":[{"id","audio_seconds","decode_seconds","rtf","reference","text","cer"}],"mean_rtf","mean_cer"}` (`reference` = the record's `raw_text`, i.e. what produced it originally) |
