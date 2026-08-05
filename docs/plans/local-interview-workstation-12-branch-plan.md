# Local Interview Workstation — 12-Branch Implementation Plan

**Status:** Approved for implementation
**Supersedes:** 8-branch-plan.md
**Product direction:** Privacy-first local structured-interview workstation
**Base branch:** `main`
**Date:** August 5, 2026

---

## Product positioning

Frame this as a **privacy-first, local structured-interview workstation**, not a cloud recruiting platform clone. Recordings, transcripts, questions, and reports stay on the employer's machine. Structured evidence and explicit human review are required.

### Deferred scope (not in this plan)

- Remote candidate links
- Video avatars
- Coding sandbox
- ATS integrations
- Multi-user organizations
- Anti-cheating surveillance
- Fully autonomous hiring recommendations
- Adaptive AI follow-ups
- Resume and CV parsing
- Job-description-based personalization
- AI-generated interview plans
- Semantic scoring and numeric candidate ranking
- Cloud accounts and synchronization
- Team roles and permissions
- Email invitations and scheduling
- Bulk candidate import
- Video and screen recording
- Live coding and whiteboards
- Multilingual interviews
- Hosted model-provider marketplace
- Cross-device candidate sessions
- Mobile support

### Not deferred (must ship in beta)

- Local TTS and transcription
- Question snapshots
- Consent
- Evidence-linked transcripts
- Human review
- Recovery
- Recruiter notes
- Basic export

### Architectural principle

Provider extensibility may be deferred, but provider coupling must not be introduced. Define clean local interfaces for STT and TTS now, implementing only Piper and Whisper adapters.

---

## Tier model

| Tier | Purpose | Branches |
|------|---------|----------|
| **Tier 0 — Foundation** | Portable paths, domain schema, walking skeleton | 1–3 |
| **Tier 1 — Working interview loop** | Templates, wizard, consent, candidate runtime | 4–9 |
| **Tier 2 — Trust, recovery, safety** | Device negotiation, recovery, resume | 7, 11 |
| **Tier 3 — Presentation, expansion** | Sidebar, dashboard, polish, reports, export | 5, 6, 10, 12 |

---

## Session lifecycle states

```
draft
  ↓
consent_pending
  ↓
device_check
  ↓
ready
  ↓
active
  ↓
completed

Terminal or alternate states:
  paused
  withdrawn
  cancelled
  failed
```

A session cannot enter `active` until consent and required device checks are recorded.

---

## Recruiter-to-candidate architecture

### Hybrid sync model

- **SQLite:** authoritative and recoverable state
- **Tauri events:** immediate UI updates
- **Route parameter:** identifies the session (`session_id` in URL)

Do not use Tauri events alone (they disappear when windows close). Do not make React context the source of truth (it disappears on refresh).

### Domain commands (not raw CRUD)

Each command performs related writes in one SQLite transaction and emits an event only after commit.

```rust
create_interview(...)        → returns session_id
start_session(session_id)
submit_answer(session_id, session_question_id, ...)
pause_session(session_id)
resume_session(session_id)
withdraw_session(session_id)
complete_session(session_id)
get_session_snapshot(session_id)
```

### Candidate routing

```
/candidate/[sessionId]/consent
/candidate/[sessionId]/device-check
/candidate/[sessionId]/interview
/candidate/[sessionId]/complete

/sessions/[sessionId]/live
/sessions/[sessionId]/report
```

Keep `/interview` temporarily as a developer harness, then remove it once the session-based candidate route is complete.

### Candidate window operation

The candidate window opens on the same workstation. The recruiter hands keyboard/screen control to the candidate. Where two monitors are available, recruiter monitor and candidate window remain separate.

Future assisted-consent mode (for accessibility) would require:
```
consent_method = "candidate_self_service" | "assisted"
recorded_by
assistance_reason
notice_version
recorded_at
```

---

## Consent model

Candidate-facing self-service consent. The candidate:
1. Reads the recording and processing notice
2. Checks required boxes
3. Submits consent personally
4. Continues to device testing

The recruiter monitor shows only status: `Awaiting candidate consent`, `Consent recorded at [time]`, `Consent declined`, `Candidate withdrew`.

The recruiter must not silently check consent boxes on the candidate's behalf.

---

## Question persistence model

### SQLite tables (not localStorage)

Use SQLite for templates and questions. localStorage is only for:
- An unsaved wizard draft
- The current wizard step
- Non-sensitive display preferences

### Session question snapshots

When an interview starts, copy the selected template questions into `session_questions`. This guarantees:
1. Editing a template tomorrow does not change an interview in progress
2. A completed report always shows exactly the question and rubric used

### Template ordering

Persist a `position` field. Initially provide accessible up/down controls; drag-and-drop can be added afterward.

---

## Schema design

### New and modified tables

```sql
-- Template library
interview_templates (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  role_title TEXT NOT NULL,
  description TEXT DEFAULT '',
  version INTEGER DEFAULT 1,
  created_at TEXT DEFAULT (datetime('now')),
  updated_at TEXT DEFAULT (datetime('now')),
  archived_at TEXT
);

template_questions (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL,
  position INTEGER NOT NULL,
  category TEXT NOT NULL DEFAULT 'general',
  competency TEXT DEFAULT '',
  prompt TEXT NOT NULL,
  time_limit_seconds INTEGER DEFAULT 120,
  required INTEGER DEFAULT 1,
  evaluation_criteria_json TEXT DEFAULT '{}',
  FOREIGN KEY (template_id) REFERENCES interview_templates(id) ON DELETE CASCADE
);

-- Sessions (extended)
sessions (
  id TEXT PRIMARY KEY,
  candidate_name TEXT NOT NULL DEFAULT '',
  candidate_email TEXT DEFAULT '',
  job_title TEXT DEFAULT '',
  template_id TEXT,
  template_version INTEGER,
  status TEXT NOT NULL DEFAULT 'draft',
  current_question_index INTEGER DEFAULT 0,
  consented_at TEXT,
  consent_method TEXT DEFAULT 'candidate_self_service',
  consent_notice_version TEXT DEFAULT '1.0',
  started_at TEXT DEFAULT (datetime('now')),
  completed_at TEXT,
  cancelled_at TEXT,
  total_rounds INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (template_id) REFERENCES interview_templates(id)
);

-- Session question snapshots
session_questions (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  source_question_id TEXT,
  position INTEGER NOT NULL,
  prompt TEXT NOT NULL,
  category TEXT NOT NULL DEFAULT 'general',
  competency TEXT DEFAULT '',
  time_limit_seconds INTEGER DEFAULT 120,
  required INTEGER DEFAULT 1,
  evaluation_criteria_json TEXT DEFAULT '{}',
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  FOREIGN KEY (source_question_id) REFERENCES template_questions(id)
);

-- Rounds (extended, keeping original table name)
rounds (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  session_question_id TEXT,
  round_index INTEGER NOT NULL,
  question TEXT NOT NULL,
  status TEXT DEFAULT 'pending',
  written_answer TEXT DEFAULT '',
  transcription TEXT DEFAULT '',
  audio_path TEXT DEFAULT '',
  sha256 TEXT DEFAULT '',
  duration_ms INTEGER DEFAULT 0,
  sample_rate INTEGER DEFAULT 16000,
  channels INTEGER DEFAULT 1,
  file_size_bytes INTEGER DEFAULT 0,
  started_at TEXT,
  completed_at TEXT,
  error_code TEXT,
  created_at TEXT DEFAULT (datetime('now')),
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
  FOREIGN KEY (session_question_id) REFERENCES session_questions(id)
);

-- Recruiter notes
recruiter_notes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  author TEXT DEFAULT 'recruiter',
  content TEXT NOT NULL,
  created_at TEXT DEFAULT (datetime('now')),
  updated_at TEXT DEFAULT (datetime('now')),
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

-- Session event journal
session_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  payload_json TEXT DEFAULT '{}',
  created_at TEXT DEFAULT (datetime('now')),
  FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
```

### Migration strategy

Use `PRAGMA user_version` for schema versions. Backward-compatible migration:

- v1: existing sessions + rounds (current state)
- v2: lifecycle columns + templates + session snapshots
- v3: consent + events + recruiter notes

Migration behavior:
1. Preserve all current `sessions` rows
2. Preserve all current `rounds` rows
3. Add new session columns with safe defaults
4. Add all new tables
5. Create `session_questions` snapshot for each legacy round using its stored question text
6. Link legacy rounds to synthesized session questions
7. Leave `template_id` nullable for migrated sessions

Legacy status backfill:
- `completed_at` is present → `completed`
- No `completed_at` and rounds exist → `paused`
- No `completed_at` and no rounds → `draft`

Provide an explicitly labeled **Reset development database** action for developers. Never make reset the migration strategy.

---

## 12-branch plan

### Branch 1: `fix/runtime-bootstrap-and-portable-paths`

**Priority:** Critical
**Tier:** 0 — Foundation

**Scope:**
- Remove machine-specific `TOOLS_DIR` constant from `lib.rs`
- Remove hardcoded interview output directory from frontend
- Create `AppPaths` struct resolved once during Tauri startup in managed state
- Resolve app data, model, and recording paths through Tauri APIs
- Initialize SQLite once during application bootstrap (not per-page)
- Add a startup readiness result
- Fix Piper binary layout (support canonical `resources/tools/piper/piper.exe` with fallback for current double-nested `piper/piper/piper.exe`)
- Frontend calls `get_app_config()` instead of hardcoding paths

**Rust-side configuration:**
```rust
pub struct AppPaths {
    pub app_data_dir: PathBuf,
    pub database_path: PathBuf,
    pub recordings_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub tools_dir: PathBuf,
    pub piper_binary: PathBuf,
    pub piper_model: PathBuf,
    pub whisper_binary: PathBuf,
    pub whisper_model: PathBuf,
}
```

**Frontend API:**
```typescript
interface AppConfig {
  databaseReady: boolean;
  recordingDirectoryReady: boolean;
  piperReady: boolean;
  whisperReady: boolean;
  modelReady: boolean;
  issues: AppConfigurationIssue[];
}
```

**Pre-step:** Merge `feat/phase-2-interview-loop` into `main` (9 commits ahead). Delete `feat/phase-2-interview-loop` after merge.

**Exit gate:** A clean machine can launch after configuring or installing model assets without source edits.

**Files to modify:**
- `src-tauri/src/lib.rs` — remove `TOOLS_DIR`, add `AppPaths` management
- `src-tauri/src/audio/capture.rs` — use managed paths
- `src-tauri/src/audio/playback.rs` — use managed paths
- `src-tauri/src/audio/tts_supervisor.rs` — use managed paths
- `src-tauri/tauri.conf.json` — ensure resource bundling config
- `app/interview/page.tsx` — call `get_app_config()` instead of hardcoding

---

### Branch 2: `feat/versioned-interview-schema`

**Priority:** Critical
**Tier:** 0 — Foundation

**Scope:**
- Add real migration versions using `PRAGMA user_version`
- Add all new tables (templates, session_questions, notes, events)
- Add explicit session and round statuses
- Enable foreign-key enforcement (`PRAGMA foreign_keys = ON`)
- Add unique and ordering constraints
- Backward-compatible migration from current schema
- Migration tests from current schema state

**Exit gate:** Existing databases migrate without losing recorded rounds. New tables are created and accessible.

**Files to modify:**
- `src-tauri/src/db.rs` — add migration logic, new tables, new structs
- `src-tauri/src/lib.rs` — expose new query commands

---

### Branch 3: `feat/session-orchestration-walking-skeleton`

**Priority:** Critical
**Tier:** 0 — Foundation

**Scope:**
- Create Rust session service module
- Add atomic domain-level Tauri commands (`create_interview`, `start_session`, `submit_answer`, `complete_session`, `get_session_snapshot`)
- Return `session_id` from interview creation
- Snapshot template questions into `session_questions` at interview start
- Persist every completed answer via `submit_answer`
- Emit typed Tauri events after commit (`session-state-changed`, `round-saved`, `session-completed`)
- End-to-end integration test

**This is the most important branch in the entire plan.**

**Exit gate:** A test can:
1. Create a persisted interview template
2. Create a session from it
3. Receive a `session_id`
4. Record consent
5. Pass device preflight
6. Start the session
7. Submit one answer
8. Terminate and restart the application
9. Resume and complete the session
10. Open the evidence report (basic, from Branch 2 schema data)

The restart between steps 7 and 9 is essential.

**Files to create:**
- `src-tauri/src/session/mod.rs` — session service
- `src-tauri/src/session/commands.rs` — Tauri commands
- `src-tauri/src/session/events.rs` — event types

**Files to modify:**
- `src-tauri/src/lib.rs` — register new commands
- `src-tauri/src/db.rs` — add session query helpers

---

### Branch 4: `feat/question-templates-and-bank`

**Priority:** High
**Tier:** 1 — Working interview loop

**Scope:**
- Template CRUD commands (create, read, update, archive, list)
- Question CRUD commands (create, read, update, delete, reorder)
- Fields: category, competency, criteria, estimated duration, time limit
- Save/load/version templates
- Reorder using persisted `position` field
- Archive rather than delete templates used by sessions
- Question bank page in frontend

**Exit gate:** A saved template survives restart and can create an immutable session snapshot.

**Dependencies:** Branch 2 (schema), Branch 3 (session orchestration)

**Files to create:**
- `src-tauri/src/templates/mod.rs`
- `src-tauri/src/templates/commands.rs`
- `components/QuestionBank.tsx`
- `app/question-bank/page.tsx`

---

### Branch 5: `feat/app-shell-and-overview-dashboard`

**Priority:** High
**Tier:** 3 — Presentation

**Scope:**
- Sidebar with navigation (Dashboard, New Interview, Question Bank, Sessions)
- App shell layout (sidebar + content area)
- System readiness display (tools, models, database)
- Recent sessions from SQLite (not hardcoded)
- Status badges based on real lifecycle values
- Actions: resume, continue, view report, cancel
- One clear primary action
- Window size 1400×900

**Exit gate:** Every dashboard row is derived from persisted data; no sample interview records are hardcoded.

**Dependencies:** Branch 1 (paths), Branch 2 (schema)

**Files to create:**
- `components/Sidebar.tsx`
- `components/Layout.tsx`
- `components/SystemReadiness.tsx`
- `components/RecentInterviews.tsx`

**Files to modify:**
- `app/layout.tsx` — use Layout wrapper
- `app/page.tsx` — redirect to `/dashboard`
- `app/dashboard/page.tsx` — rebuild with components
- `src-tauri/tauri.conf.json` — window size

---

### Branch 6: `feat/interview-creation-wizard`

**Priority:** High
**Tier:** 1 — Working interview loop

**Scope:**
- 5-step progress bar component
- Step 1 (Role): job title, role description
- Step 2 (Candidate): candidate name, email
- Step 3 (Template/Questions): select template or create questions, category badges, time estimates, evaluation criteria
- Step 4 (Review): summary, coverage analysis, question list
- Step 5 (Create/Start): final review, launch interview
- Autosave unfinished wizard draft to localStorage
- Validation per step
- Produce a real `session_id`
- Completing the wizard opens the exact candidate session it created

**Exit gate:** Completing the wizard opens the candidate window for that session.

**Dependencies:** Branch 4 (templates), Branch 5 (app shell)

**Files to create:**
- `app/interview/create/page.tsx`
- `components/wizard/StepRole.tsx`
- `components/wizard/StepCandidate.tsx`
- `components/wizard/StepQuestions.tsx`
- `components/wizard/StepReview.tsx`
- `components/wizard/StepStart.tsx`
- `components/ProgressBar.tsx`

---

### Branch 7: `fix/audio-device-negotiation`

**Priority:** High
**Tier 2 — Trust and recovery

**Scope:**
- Query microphone supported configurations via cpal
- Support `F32`, `I16`, and `U16` input formats
- Capture using a supported native rate
- Convert/resample to 16kHz for Whisper transcription
- Device selection dropdown
- Input level meter (real-time RMS display)
- Plain-language failure categories ("Microphone not found", "Permission denied", "Device in use")

**Exit gate:** Tests cover configuration selection, conversion, and errors. Physical smoke tests cover at least two microphones.

**Dependencies:** Branch 1 (paths)

**Files to modify:**
- `src-tauri/src/audio/capture.rs` — format negotiation, resampling
- `src-tauri/src/audio/devices.rs` — device enumeration, level meter
- Frontend interview and candidate pages — device selection UI

---

### Branch 8: `feat/consent-and-device-preflight`

**Priority:** High
**Tier 1 — Working interview loop

**Scope:**
- Privacy and recording notice (candidate-facing, readable wording)
- Explicit consent records with timestamp and notice version
- Three consent checkboxes: recording consent, transcription consent, data retention acknowledgment
- Microphone selection and level test
- Speaker playback test
- Retry, change device, continue with written answers, or withdraw options
- Retention summary
- Session lifecycle enforcement: cannot enter `active` without consent + device check

**Exit gate:** A session cannot enter `active` without the required consent and a recorded preflight outcome.

**Dependencies:** Branch 2 (schema), Branch 3 (session orchestration), Branch 7 (device negotiation)

**Files to create:**
- `app/candidate/[sessionId]/consent/page.tsx`
- `app/candidate/[sessionId]/device-check/page.tsx`
- `components/ConsentCheckboxes.tsx`
- `components/DeviceTest.tsx`

---

### Branch 9: `feat/candidate-interview-runtime`

**Priority:** High
**Tier 1 — Working interview loop

**Scope:**
- Session-driven state machine (reads from `session_id`, not hardcoded questions)
- Question display and TTS playback
- Recording with start/stop
- Processing state with transcription
- Answer review (transcript display)
- Written-answer alternative (4000 char limit)
- Replay question button
- Pause/resume session
- Withdraw from session
- Microphone problem handling
- Current question, question number and total
- Text states: "Playing question", "Recording", "Processing"
- Recording timer
- Basic microphone-level indication
- Answer-saved confirmation
- Pause, replay, withdraw, and mic-problem controls
- Plain error and retry messages
- Correct focus movement
- Button labels and keyboard operation
- Non-color status indicators

**Exit gate:** No candidate state is based on a separate local question list or manually triggered display state. All states driven by `session_id` and SQLite.

**Dependencies:** Branch 3 (session orchestration), Branch 8 (consent)

**Files to create:**
- `app/candidate/[sessionId]/interview/page.tsx`
- `app/candidate/[sessionId]/complete/page.tsx`

**Files to modify:**
- `app/candidate/page.tsx` — redirect to session-based route

---

### Branch 10: `feat/candidate-experience-polish-and-a11y-audit`

**Priority:** Medium
**Tier 3 — Presentation

**Scope:**
- Dark theme for candidate pages (full treatment, not just bg color)
- Refined visual hierarchy
- Animated microphone visualization (canvas-based, using `CaptureEvent::Level`)
- Recording indicator animation
- Remaining-question and time guidance
- Clear autosave status
- Responsive sizing
- Transitions between states
- Empty and edge-state presentation
- Full accessibility audit
- Screen-reader live-region refinement
- Reduced-motion behavior
- Visual regression testing

**Exit gate:** Candidate UI tests cover keyboard operation and all lifecycle states.

**Dependencies:** Branch 9 (candidate runtime)

**Files to modify:**
- `app/candidate/[sessionId]/interview/page.tsx`
- `app/candidate/[sessionId]/complete/page.tsx`
- `app/globals.css` — dark theme variables

**Files to create:**
- `components/MicVisualization.tsx`
- `components/RecordingIndicator.tsx`

---

### Branch 11: `feat/session-recovery-and-resume`

**Priority:** High
**Tier 2 — Trust and recovery

**Scope:**
- Persist current question index and round phase in SQLite
- Interrupted-round handling (partial recordings, incomplete transcriptions)
- Startup recovery screen ("Session was interrupted. Resume or discard?")
- Resume or discard partial recording
- Idempotent answer submission (prevent duplicate saves)
- Periodic transcript/metadata checkpointing
- Session event journal (every state transition logged)
- Recovery-focused integration tests

**Exit gate:** Force-closing the app during an interview loses at most the current incomplete answer, not the whole session.

**Dependencies:** Branch 3 (session orchestration), Branch 9 (candidate runtime)

**Files to create:**
- `app/candidate/[sessionId]/recovery/page.tsx`
- `src-tauri/src/session/recovery.rs`

**Files to modify:**
- `src-tauri/src/session/mod.rs` — checkpointing logic
- `src-tauri/src/db.rs` — event journal queries

---

### Branch 12: `feat/evidence-report-notes-and-export`

**Priority:** High
**Tier: 1 — Working interview loop (basic) + 3 — Presentation (full)

**Scope — Basic (walking skeleton completion):**
- Session detail page at `/sessions/[sessionId]/report`
- Candidate name, session status, timestamps
- Question-by-question evidence list
- Transcripts and written answers
- Audio metadata (path, sha256, duration)
- Consent status and timestamp

**Scope — Full (after basic works):**
- Tabbed interface: Summary, Evidence, Audio, Transcripts, Notes
- Audio playback controls
- Recruiter notes section (stored in SQLite `recruiter_notes` table)
- Rubric coverage display
- Human-review status workflow
- PDF and CSV export
- Data provenance: question, answer, transcript, timestamps, model/version
- No automatic final employment decision — recruiter must explicitly review and finalize

**Exit gate:** Every AI observation links to source evidence. The recruiter must explicitly review and finalize the report.

**Dependencies:** Branch 2 (schema), Branch 3 (session orchestration)

**Files to create:**
- `app/sessions/[sessionId]/report/page.tsx`
- `components/report/TabSummary.tsx`
- `components/report/TabEvidence.tsx`
- `components/report/TabTranscripts.tsx`
- `components/report/TabNotes.tsx`
- `components/report/AudioPlayer.tsx`
- `components/report/RecruiterNotes.tsx`

---

## Execution order and dependencies

```
Branch 1 (paths) ──────────────────────┬──→ Branch 7 (device negotiation)
                                       │
Branch 2 (schema) ──┬──→ Branch 3 (walking skeleton) ──┬──→ Branch 8 (consent)
                    │                                    │
                    ├──→ Branch 4 (templates) ──→ Branch 6 (wizard)
                    │
                    └──→ Branch 12 (report, basic)
                    
Branch 5 (app shell) ──→ Branch 6 (wizard)
                    
Branch 3 ──→ Branch 9 (candidate runtime) ──┬──→ Branch 10 (polish + a11y)
                                            │
                                            └──→ Branch 11 (recovery)
```

### Parallelizable

After Branch 2 completes:
- Branch 5 (app shell) can proceed in parallel with Branch 3
- Branch 12 (basic report) can proceed in parallel with Branch 3

After Branch 3 completes:
- Branches 4, 8, 9, 12 (full) can proceed in parallel

### Merge strategy

Each branch is merged into `main` after its exit gate passes. Do not stack all 12 branches on one long-lived feature branch.

---

## Architectural acceptance test

The following 10-step scenario must pass as an automated integration test after Branch 3:

```
1. Create a persisted interview template with 3 questions
2. Create a session from that template
3. Receive a session_id
4. Record consent (candidate self-service)
5. Pass device preflight
6. Start the session
7. Submit one answer (audio + transcription persisted)
8. Terminate and restart the application
9. Resume and complete the session (remaining answers)
10. Open the evidence report (questions, transcripts, timestamps, consent status visible)
```

Steps 7–9 are the critical recovery test. Without the restart, the test proves persistence but not recovery.

---

## Current codebase state (starting point)

The `main` branch currently contains:

- Phase 0: Audio spike (Piper TTS + Whisper verified)
- Phase 1: cpal audio I/O (capture + playback)
- Phase 2: Interview state machine, crash-safe recording, SQLite persistence, frontend UI, 47 Rust tests + 37 frontend tests + smoke test

The `feat/phase-2-interview-loop` branch has 9 additional commits (dashboard, DB persistence, path fixes, icon, automation tests) that need to be merged into `main` as a pre-step.

---

## Notes

- **Window size:** 1400×900
- **Theme:** Light for recruiter, Dark for candidate
- **Data storage:** SQLite (all production data), localStorage (wizard draft only)
- **Question reorder:** Position field, up/down controls first, drag-and-drop later
- **Consent:** Candidate self-service, auditable, required before active recording
- **Reports:** Evidence-linked, human-review-required, no auto-hiring decisions
- **Export:** Basic (text/JSON) in beta, PDF/CSV in full release
- **Piper layout:** Canonical `resources/tools/piper/piper.exe` with fallback for current double-nested path
- **Recovery:** Force-close loses at most the current incomplete answer

---

*This plan supersedes the 8-branch-plan.md. The original plan treated consent, reports, and recovery as later UI work while the interview lifecycle was disconnected. This plan makes the data layer and session lifecycle the foundation.*
