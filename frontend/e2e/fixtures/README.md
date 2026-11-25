# Browser test fixtures

`bin/` holds stand-ins for the two PipeWire tools Voiceless runs. A test
puts the directory first on `PATH`; the daemon then records from
`jfk-16k-mono-s16le.pcm` instead of a microphone and lists the one
microphone of the canned graph. Both read their settings from the
environment (`VOICELESS_E2E_PCM`, `VOICELESS_E2E_RECORDER`,
`VOICELESS_E2E_GRAPH`), so a test can break and repair the desktop while
the daemon keeps running.

| File | Content |
|---|---|
| `jfk-16k-mono-s16le.pcm` | 11 s, 16 kHz, mono, signed 16-bit little-endian: "And so, my fellow Americans: ask not what your country can do for you — ask what you can do for your country." |
| `bin/pw-record` | Streams the sample at real-time speed, then waits for the SIGINT that ends a session |
| `bin/pw-dump` | Reports the default microphone and output, plus links from the selected sources to the dictation and meeting streams. `VOICELESS_E2E_GRAPH=none` returns an empty graph. |

The sample is the public-domain 1961 recording from
[`whisper.cpp`'s `samples/jfk.wav`](https://github.com/ggml-org/whisper.cpp/blob/master/samples/jfk.wav),
converted with `ffmpeg -i jfk.wav -f s16le -acodec pcm_s16le -ac 1 -ar 16000 jfk-16k-mono-s16le.pcm`.

## The real-model test

`onboarding.spec.ts` speaks the sample through a local model. It runs
when `VOICELESS_E2E_MODEL_STORE` names an XDG data directory whose
`voiceless/models/` holds a compatible English dictation model (for example,
`sense-voice-int8` on CPU). The fixture omits Silero VAD from the seed so
setup downloads it. Point `VOICELESS_BIN` at the build under test if needed.

```sh
cd frontend
VOICELESS_E2E_MODEL_STORE=/path/to/data \
  bun run test:e2e --grep onboarding
```
