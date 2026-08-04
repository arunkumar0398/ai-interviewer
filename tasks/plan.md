# Implementation Plan: Audio Spike — Native Audio Roundtrip

## Overview

Prove the core audio loop works end-to-end on Windows: Piper TTS generates speech → playback → settling → mic capture → WAV on disk → whisper.cpp transcribes → verify transcript matches source text. This is the Phase 0 architecture spike from the build plan.

## Architecture Decisions

- **Piper TTS** for speech generation (local, offline, fast, v1.2.0 confirmed working)
- **whisper.cpp** for transcription (local, offline, v1.9.1 confirmed working)
- **hound** crate for WAV file I/O (Rust, already in Cargo.toml)
- **Placeholder mic capture** — record silence for now; real WASAPI capture comes in Phase 1
- **Single Rust binary** orchestrates the full loop via CLI subcommand

## Task List

### Task 1: Download whisper tiny model

**Description:** Download `ggml-tiny.en.bin` (~75MB) from HuggingFace into `ai-interviewer-tools/models/`. This is required for whisper transcription.

**Acceptance criteria:**
- [ ] File exists at `ai-interviewer-tools/models/ggml-tiny.en.bin`
- [ ] File size is approximately 75MB (not a truncated download)

**Verification:**
- [ ] `ls -la ai-interviewer-tools/models/ggml-tiny.en.bin` shows ~75MB

**Dependencies:** None

**Files likely touched:**
- `ai-interviewer-tools/models/ggml-tiny.en.bin` (downloaded)

**Estimated scope:** XS

---

### Task 2: Add audio spike Rust binary

**Description:** Create a standalone Rust binary (`audio-spike`) that orchestrates the full roundtrip: generate TTS WAV → play WAV → wait → capture (placeholder silence) → write WAV → transcribe with whisper → print result.

**Acceptance criteria:**
- [ ] `cargo build --bin audio-spike` succeeds
- [ ] Binary runs and produces output without panicking

**Verification:**
- [ ] `cargo build --bin audio-spike` compiles clean
- [ ] Running `cargo run --bin audio-spike` produces output

**Dependencies:** Task 1

**Files likely touched:**
- `src-tauri/Cargo.toml` (add [[bin]] section)
- `src-tauri/src/bin/audio_spike.rs` (new file)

**Estimated scope:** S

---

### Task 3: Run full roundtrip and verify

**Description:** Execute the audio spike binary and verify the complete loop: Piper generates WAV → whisper transcribes → transcript matches source text.

**Acceptance criteria:**
- [ ] Piper generates a valid WAV file
- [ ] Whisper produces a transcript from the WAV
- [ ] Transcript contains the original text (or close match)

**Verification:**
- [ ] Terminal output shows successful TTS generation
- [ ] Terminal output shows whisper transcription result
- [ ] Transcript is readable English text

**Dependencies:** Task 2

**Files likely touched:**
- None (execution only)

**Estimated scope:** XS

---

### Task 4: Commit and document results

**Description:** Commit the spike code and document findings as an ADR (Architecture Decision Record) in the repo.

**Acceptance criteria:**
- [ ] Git commit with descriptive message
- [ ] ADR document captures: what worked, what didn't, recommendation for production audio

**Verification:**
- [ ] `git log` shows the spike commit
- [ ] ADR file exists in `docs/` or `tasks/`

**Dependencies:** Task 3

**Files likely touched:**
- `docs/adr-001-audio-spike.md` (new file)

**Estimated scope:** S

---

## Checkpoint: After Task 3
- [ ] Full audio roundtrip works end-to-end
- [ ] Transcript matches source text
- [ ] Ready to document and commit

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Model download fails/times out | Medium | Retry with longer timeout, or use smaller model |
| Piper generates incompatible WAV | Medium | Check WAV spec (sample rate, channels) matches whisper expectations |
| Whisper transcription inaccurate | Low | Use tiny.en model (optimized for English), verify with known-good text |

## Open Questions

- None — all tools confirmed working, proceed with execution
