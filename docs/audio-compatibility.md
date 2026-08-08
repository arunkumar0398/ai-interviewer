# Audio Device Format Compatibility

**Status:** documented limitation — not a DSP project in this pass.
**ADT (Audio Device Team) follow-up:** see "Follow-up task" below.

## Assumed formats (current implementation)

| Direction | Format | Where |
|-----------|--------|-------|
| Capture (microphone) | 16 kHz, mono, f32 frames | `src-tauri/src/audio/capture.rs` (`record_to_wav`, `record_test_clip`) |
| Piper TTS output | 22.05 kHz, mono, 16-bit PCM | `src-tauri/src/audio/playback.rs` (`PIPER_SAMPLE_RATE`), `src-tauri/src/audio/tts_supervisor.rs` |
| WAV on disk | 16-bit signed, mono | `hound::WavSpec` in capture/playback |

The default capture `StreamConfig` requests 16 kHz mono. The default output
`StreamConfig` requests 22.05 kHz mono (Piper raw PCM) or the WAV's own rate
(`play_wav`).

## Known limitation

- If the default device cannot satisfy the requested sample rate/format, the
  stream build fails **explicitly** (`build_input_stream` /
  `build_output_stream` return an error that is surfaced to the caller — never
  a hang).
- There is **no automatic negotiation** of supported/default device formats,
  **no sample-format conversion** (e.g. float vs int, bit depth), and **no
  resampling/downmixing** between capture, Piper, and playback.
- The device check (`record_test_clip`) is additionally bounded by a hard
  wall-clock deadline (`duration + 2s`) so a stream that starts but delivers
  no frames fails explicitly instead of hanging.

Failures are always explicit: capture errors are latched from the real-time
callback and fail the recording; output errors are latched and fail playback;
timeouts are explicit errors with cleanup.

## Follow-up task (deferred)

Implement supported/default config negotiation and conversion when it is
needed:

- Enumerate the default device's supported configs and pick the closest
  supported rate/channel layout instead of assuming 16 kHz mono.
- Add sample-format handling (float/int, bit depth) on both capture and
  playback.
- Add downmixing (e.g. stereo → mono) and resampling for devices that only
  support other rates.
- Consider surfacing the effective negotiated format to the frontend so the
  UI can show what the candidate actually recorded/played.

This is intentionally **not** part of the PR #2 remediation pass: existing
supported hardware works with the current assumptions, and no hardware has
reported a failure. Revisit before adding VAD/silence features or supporting
non-default devices.
