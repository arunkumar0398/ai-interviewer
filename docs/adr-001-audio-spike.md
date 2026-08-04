# ADR-001: Audio Spike — Native Audio Roundtrip

## Status

**Accepted** — 2026-08-04

## Context

Phase 0 of the AI Interviewer MVP requires proving that the core audio loop works end-to-end on Windows before building the full interview system. The loop is:

1. Piper TTS generates speech from text
2. Candidate window plays the audio
3. 500ms settling period
4. Native mic captures the candidate's answer
5. WAV written to disk
6. whisper.cpp transcribes the audio
7. Transcript is returned to the Rust backend

This spike validates steps 1, 5, 6, and 7. Steps 2-4 (playback and capture) require WASAPI integration which comes in Phase 1.

## Decision

**Use Piper TTS + whisper.cpp as the local audio stack.**

### Tools Verified

| Tool | Version | Purpose | Status |
|------|---------|---------|--------|
| Piper TTS | 1.2.0 | Text-to-speech generation | Working |
| whisper.cpp | 1.9.1 | Speech-to-text transcription | Working |
| ggml-tiny.en.bin | 77MB | Whisper English model | Working |
| en_US-amy-medium.onnx | 60MB | Piper English voice model | Working |

### Audio Spike Results

```
Input:  "Tell me about your experience with systems programming."
Output: "Tell me about your experience with systems programming."

WAV: 167,072 bytes, 22050 Hz, mono, 16-bit PCM
Transcription time: ~930ms
```

**Transcript matches source text exactly.**

### Key Findings

1. **Piper requires stdin input**, not `--text` flag (despite docs)
2. **Whisper uses `-otxt` and `-of` flags**, not `--output-format` and `--output-dir`
3. **Both tools are fast**: Piper generates 3.7s audio in ~30ms, whisper transcribes in ~930ms
4. **Local-only**: No network, no cloud, no API keys needed

## Consequences

### Positive

- Full audio stack runs locally on Windows
- No cloud dependencies or API costs
- Fast enough for real-time interview flow
- Simple integration via command-line execution

### Negative

- Piper model download is 60MB per voice
- Whisper model is 77MB
- WASAPI integration still needed for real mic capture

### Next Steps

1. **Phase 1**: Add WASAPI mic capture (real audio input)
2. **Phase 1**: Add rodio/symphonia for audio playback
3. **Phase 1**: Replace placeholder silence with real mic recording
4. **Phase 2**: Build full interview loop with Tauri commands

## Appendix: Audio Spike Binary

Location: `src-tauri/src/bin/audio_spike.rs`

Run with: `cargo run --bin audio-spike`
