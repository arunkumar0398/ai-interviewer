"use client";

import { useState, useCallback, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { INTERVIEW_QUESTIONS } from "../../lib/interview-questions";

interface PhaseEventPayload {
  phase: string;
  question: string | null;
  duration_ms: number | null;
}

interface DeviceCheckResult {
  mic_available: boolean;
  mic_name: string | null;
  speaker_available: boolean;
  speaker_name: string | null;
  mic_test_ok: boolean;
  errors: string[];
}

interface AudioMetadata {
  file_path: string;
  sha256: string;
  duration_ms: number;
  sample_rate: number;
  channels: number;
  file_size_bytes: number;
}

interface InterviewRoundResult {
  metadata: AudioMetadata;
  transcription: string;
}

/** Serialized Rust db::InterviewRound returned by get_rounds. */
interface StoredInterviewRound {
  id: number;
  session_id: string;
  round_index: number;
  question: string;
  transcription: string;
  audio_path: string;
  sha256: string;
  duration_ms: number;
  sample_rate: number;
  channels: number;
  file_size_bytes: number;
  created_at: string;
}

function storedRoundToResult(round: StoredInterviewRound): InterviewRoundResult {
  return {
    metadata: {
      file_path: round.audio_path,
      sha256: round.sha256,
      duration_ms: round.duration_ms,
      sample_rate: round.sample_rate,
      channels: round.channels,
      file_size_bytes: round.file_size_bytes,
    },
    transcription: round.transcription,
  };
}

function hasContiguousRoundHistory(rounds: StoredInterviewRound[]): boolean {
  return (
    rounds.length < INTERVIEW_QUESTIONS.length &&
    rounds.every((round, index) => round.round_index === index)
  );
}

interface InterviewSession {
  id: string;
  candidate_name: string;
  started_at: string;
  completed_at: string | null;
  total_rounds: number;
}

interface ToolsStatus {
  piper: boolean;
  whisper: boolean;
  model: boolean;
}

interface AppConfig {
  tool_dir: string;
  recordings_dir: string;
  db_path: string;
  readiness: {
    ready: boolean;
    issues: Array<{ code: string; message: string; expected_path: string | null }>;
  };
}

type InterviewPhase =
  | "checking-tools"
  | "device-check"
  | "ready"
  | "speaking-question"
  | "settling"
  | "recording-answer"
  | "processing"
  | "showing-result"
  | "error";

/** Which operation failed and should be retried. */
type RetryTarget =
  | { kind: "round"; question: string }
  | { kind: "device-check" }
  | { kind: "readiness" }
  | { kind: "session-init" };

export default function InterviewPage() {
  const [phase, setPhase] = useState<InterviewPhase>("checking-tools");
  const [toolsStatus, setToolsStatus] = useState<ToolsStatus | null>(null);
  const [deviceResult, setDeviceResult] = useState<DeviceCheckResult | null>(
    null
  );
  const [currentQuestion, setCurrentQuestion] = useState("");
  const [currentRound, setCurrentRound] = useState(0);
  const [roundResults, setRoundResults] = useState<InterviewRoundResult[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [isStopping, setIsStopping] = useState(false);
  const [retryTarget, setRetryTarget] = useState<RetryTarget | null>(null);

  // When the Recruiter Dashboard created the session, it hands it off here via
  // ?session=<uuid> (P2-4: session-only — the candidate name stays in the DB,
  // never in the URL). The interview then continues THAT session instead of
  // creating a second, orphan one (P1-1). The handoff is read synchronously
  // (the query string is static for the page lifetime) and nothing derived
  // from it is rendered, so the static prerender is unaffected. No
  // setState-in-effect is needed.
  const handedOffSession = (() => {
    if (typeof window === "undefined") return null;
    return new URLSearchParams(window.location.search).get("session");
  })();

  // Stable session ID for the entire interview flow (one session across all
  // rounds): the Dashboard's session when handed off, otherwise a fresh one.
  const [sessionId] = useState(() => handedOffSession ?? crypto.randomUUID());
  const sessionIdRef = useRef(sessionId);
  const [candidateName] = useState("Candidate");

  // Backend-authoritative session lifecycle: tracks whether create_session
  // has been called and whether the session is ready for round execution. A
  // handed-off session was already created by the Dashboard, so it starts
  // VERIFYING (P2-1): the backend is asked whether the session exists and is
  // incomplete BEFORE any round flow. create_session is never called for a
  // handed-off session; invalid/missing/completed handoffs never reach the
  // round invoke.
  type SessionInitState =
    | "idle"
    | "creating"
    | "verifying"
    | "ready"
    | "error"
    | "completed";
  const [sessionInitState, setSessionInitState] = useState<SessionInitState>(
    handedOffSession ? "verifying" : "idle"
  );
  // Synchronous marker set by the handoff-verification effect BEFORE the
  // tools-check effect's async continuation can run, so a rejected handoff is
  // never overwritten by the independent tools/device-check phase flow.
  const handoffFailedRef = useRef(false);

  // Ref-based guard to prevent concurrent double-starts while session creation
  // or round startup is in progress. Must outlive async callbacks.
  const isStartingRound = useRef(false);

  // Keep ref in sync
  useEffect(() => {
    sessionIdRef.current = sessionId;
  }, [sessionId]);

  // Subscribe to backend phase events — drives UI transitions automatically
  useEffect(() => {
    const unlisten = listen<PhaseEventPayload>("interview-phase", (event) => {
      const { phase: backendPhase, question } = event.payload;

      // Map backend phase strings to frontend InterviewPhase
      switch (backendPhase) {
        case "speaking-question":
          setPhase("speaking-question");
          if (question) setCurrentQuestion(question);
          break;
        case "settling":
          setPhase("settling");
          break;
        case "recording-answer":
          setPhase("recording-answer");
          break;
        case "processing":
          setPhase("processing");
          break;
        case "complete":
          setPhase("showing-result");
          break;
        case "idle":
          setPhase("ready");
          break;
        case "error":
          // A stop-during-round is a controlled cancellation, not a failure:
          // return to ready (matching the invoke rejection handling below).
          if (question && question.toLowerCase().includes("stopped")) {
            setPhase("ready");
          } else {
            setPhase("error");
            if (question) setError(question);
          }
          break;
      }
    });

    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // Check tools on mount
  useEffect(() => {
    let cancelled = false;

    async function init() {
      try {
        const config = await invoke<AppConfig>("get_app_config");
        if (cancelled) return;
        // A rejected session handoff is authoritative — do not override its
        // error phase with the tools/device-check flow (P2-1).
        if (handoffFailedRef.current) return;

        if (config.readiness.ready) {
          setToolsStatus({ piper: true, whisper: true, model: true });
          setPhase("device-check");
        } else {
          const missing = config.readiness.issues
            .map((i) => i.message)
            .join(", ");
          setError(`Missing tools: ${missing}`);
          setRetryTarget({ kind: "readiness" });
          setPhase("error");
        }
      } catch (e) {
        if (cancelled) return;
        setError(String(e));
        setRetryTarget({ kind: "readiness" });
        setPhase("error");
      }
    }

    init();
    return () => { cancelled = true; };
  }, []);

  // P2-1: verify a Dashboard-handed-off session BEFORE any round flow. The
  // handoff is validated as a UUID, then checked against the DB: missing
  // session -> stale/invalid handoff error; completed session -> explicit
  // completed state; valid + incomplete -> ready. An invalid handoff is never
  // turned into a round retry (no RetryTarget is set).
  useEffect(() => {
    const sessionParam = handedOffSession;
    if (!sessionParam) return;
    let cancelled = false;

    async function verifyHandoff(sessionId: string) {
      const uuidRe =
        /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
      if (!uuidRe.test(sessionId)) {
        if (cancelled) return;
        handoffFailedRef.current = true;
        setSessionInitState("error");
        setPhase("error");
        setRetryTarget(null);
        setError(
          "Invalid session link: the session identifier is not a valid UUID. Please create a new session from the Dashboard."
        );
        return;
      }
      try {
        const session = await invoke<InterviewSession | null>("get_session", {
          sessionId,
        });
        if (cancelled) return;
        if (!session) {
          handoffFailedRef.current = true;
          setSessionInitState("error");
          setPhase("error");
          setRetryTarget(null);
          setError(
            "This session link is invalid or expired — no matching session was found. Please create a new session from the Dashboard."
          );
          return;
        }
        if (session.completed_at) {
          setSessionInitState("completed");
          return;
        }
        const rounds = await invoke<StoredInterviewRound[]>("get_rounds", {
          sessionId,
        });
        if (cancelled) return;
        if (!hasContiguousRoundHistory(rounds)) {
          handoffFailedRef.current = true;
          setSessionInitState("error");
          setPhase("error");
          setRetryTarget(null);
          setError(
            "This session has an inconsistent round history and cannot be resumed."
          );
          return;
        }
        setRoundResults(rounds.map(storedRoundToResult));
        setCurrentRound(rounds.length);
        setSessionInitState("ready");
      } catch (e) {
        if (cancelled) return;
        handoffFailedRef.current = true;
        setSessionInitState("error");
        setPhase("error");
        setRetryTarget(null);
        setError(`Failed to verify session: ${String(e)}`);
      }
    }

    verifyHandoff(sessionParam);
    return () => { cancelled = true; };
  }, [handedOffSession]);

  // P2-1: in-flight guard — a rapid double-click must not start a second
  // device check. The backend also rejects overlap via the shared recording
  // slot, but the UI should not even attempt it. The ref is synchronous (no
  // stale-closure window); the state disables the button visually.
  const deviceCheckRunning = useRef(false);
  const [deviceCheckBusy, setDeviceCheckBusy] = useState(false);

  // Run device check
  const handleDeviceCheck = useCallback(async () => {
    if (deviceCheckRunning.current) return;
    deviceCheckRunning.current = true;
    setDeviceCheckBusy(true);
    setPhase("device-check");
    try {
      const result = await invoke<DeviceCheckResult>("check_audio_devices");
      setDeviceResult(result);

      // Readiness requires mic AND speaker. A missing speaker means the
      // candidate cannot hear questions, so the device check must not pass.
      if (result.mic_available && result.mic_test_ok && result.speaker_available) {
        setPhase("ready");
      } else {
        setPhase("error");
        setError(
          result.errors.length > 0
            ? result.errors.join("; ")
            : "Device check failed"
        );
        setRetryTarget({ kind: "device-check" });
      }
    } catch (e) {
      setPhase("error");
      setError(String(e));
      setRetryTarget({ kind: "device-check" });
    } finally {
      deviceCheckRunning.current = false;
      setDeviceCheckBusy(false);
    }
  }, []);

  // Start interview round — let backend phase events drive the UI
  const handleStartRound = useCallback(async () => {
    if (currentRound >= INTERVIEW_QUESTIONS.length) {
      // All rounds done — the backend derived finality and completed the
      // session atomically on the final round.
      setPhase("showing-result");
      return;
    }

    // Prevent concurrent double-starts while session creation or round startup
    if (isStartingRound.current) return;
    // Never execute a round while session initialization is pending, being
    // verified, or failed (P2-1: a handed-off session must be verified first).
    if (
      sessionInitState === "creating" ||
      sessionInitState === "verifying" ||
      sessionInitState === "error" ||
      sessionInitState === "completed"
    )
      return;
    isStartingRound.current = true;

    try {
      // Backend-authoritative session lifecycle: create session before first
      // round — but only when no session was handed off by the Dashboard (the
      // Dashboard already created it). After create_session succeeds, proceed
      // immediately into round execution. The ref guard above prevents
      // re-entry.
      if (sessionInitState === "idle") {
        setSessionInitState("creating");
        try {
          await invoke("create_session", {
            sessionId: sessionIdRef.current,
            candidateName,
          });
          setSessionInitState("ready");
        } catch (e) {
          setSessionInitState("error");
          setPhase("error");
          setError(`Failed to create session: ${String(e)}`);
          setRetryTarget({ kind: "session-init" });
          return;
        }
      }

      const question = INTERVIEW_QUESTIONS[currentRound];
      setCurrentQuestion(question);
      setRetryTarget(null);

      // Finality is backend-derived (EXPECTED_ROUNDS) — never sent from here.
      const roundId = crypto.randomUUID();
      const result = await invoke<InterviewRoundResult>("run_interview_round", {
        question,
        sessionId: sessionIdRef.current,
        roundId,
        roundIndex: currentRound,
      });

      setRoundResults((prev) => [...prev, result]);
      setCurrentRound((prev) => prev + 1);
      setPhase("showing-result");
    } catch (e) {
      if (String(e).includes("stopped")) {
        setPhase("ready");
      } else {
        setPhase("error");
        setError(String(e));
        setRetryTarget({ kind: "round", question: INTERVIEW_QUESTIONS[currentRound] });
      }
    } finally {
      isStartingRound.current = false;
    }
  }, [currentRound, sessionInitState, candidateName]);

  // Retry handler — only redoes the failed operation
  const handleRetry = useCallback(async () => {
    if (!retryTarget) return;

    setError(null);
    setRetryTarget(null);

    switch (retryTarget.kind) {
      case "readiness": {
        // Re-run tools check
        try {
          const config = await invoke<AppConfig>("get_app_config");
          if (config.readiness.ready) {
            setToolsStatus({ piper: true, whisper: true, model: true });
            setPhase("device-check");
          } else {
            const missing = config.readiness.issues
              .map((i) => i.message)
              .join(", ");
            setError(`Missing tools: ${missing}`);
            setRetryTarget({ kind: "readiness" });
            setPhase("error");
          }
        } catch (e) {
          setError(String(e));
          setRetryTarget({ kind: "readiness" });
          setPhase("error");
        }
        break;
      }
      case "device-check": {
        // Re-run device check only
        await handleDeviceCheck();
        break;
      }
      case "session-init": {
        // Retry only create_session, reusing the stable sessionId. On success
        // transition to ready; the user then starts the first round.
        try {
          await invoke("create_session", {
            sessionId: sessionIdRef.current,
            candidateName,
          });
          setSessionInitState("ready");
          setPhase("ready");
        } catch (e) {
          setError(String(e));
          setRetryTarget({ kind: "session-init" });
          setPhase("error");
        }
        break;
      }
      case "round": {
        // Re-run the failed round via retry command. Finality is derived on
        // the backend from roundIndex.
        try {
          const result = await invoke<InterviewRoundResult>(
            "retry_interview_round",
            {
              question: retryTarget.question,
              sessionId: sessionIdRef.current,
              roundIndex: currentRound,
            }
          );
          setRoundResults((prev) => [...prev, result]);
          setCurrentRound((prev) => prev + 1);
          setPhase("showing-result");
        } catch (e) {
          setError(String(e));
          setRetryTarget({ kind: "round", question: retryTarget.question });
          setPhase("error");
        }
        break;
      }
    }
  }, [retryTarget, currentRound, handleDeviceCheck, candidateName]);

  // Stop current round
  const handleStop = useCallback(async () => {
    setIsStopping(true);
    try {
      await invoke("stop_interview_round");
    } catch (e) {
      console.error("Stop failed:", e);
    }
    setIsStopping(false);
  }, []);

  // Render based on phase
  return (
    <div className="flex flex-col min-h-screen bg-zinc-50 dark:bg-black font-sans">
      <main className="flex flex-1 flex-col items-center gap-6 py-12 px-6">
        <h1 className="text-3xl font-semibold tracking-tight text-black dark:text-zinc-50">
          AI Interviewer
        </h1>

        {/* Tools Check */}
        {phase === "checking-tools" && (
          <div className="text-zinc-600 dark:text-zinc-400">
            Checking tools installation...
          </div>
        )}

        {toolsStatus && (
          <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
            <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
              Tools Status
            </h2>
            <div className="space-y-2 text-sm">
              <StatusRow
                label="Piper TTS"
                ok={toolsStatus.piper}
              />
              <StatusRow
                label="Whisper"
                ok={toolsStatus.whisper}
              />
              <StatusRow
                label="Whisper Model"
                ok={toolsStatus.model}
              />
            </div>
          </div>
        )}

        {/* Handed-off session verification (P2-1) */}
        {sessionInitState === "verifying" && (
          <div className="text-zinc-600 dark:text-zinc-400">
            Verifying session...
          </div>
        )}

        {/* Handed-off session already completed (P2-1) */}
        {sessionInitState === "completed" && (
          <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
            <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
              Session Already Completed
            </h2>
            <p className="text-sm text-zinc-600 dark:text-zinc-400 mb-4">
              This interview session has already been completed. No further
              rounds can be started.
            </p>
            <a
              href="/dashboard"
              className="inline-block px-4 py-2 bg-blue-600 text-white rounded hover:bg-blue-700 text-sm"
            >
              Back to Dashboard
            </a>
          </div>
        )}

        {/* Device Check — shown only when the session can actually proceed
            (idle: direct /interview flow; ready: handoff verified or session
            created). Never during handoff verification, errors, or a
            completed session (P2-1). */}
        {(phase === "device-check" || phase === "ready") &&
          (sessionInitState === "idle" || sessionInitState === "ready") && (
          <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
            <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
              Device Check
            </h2>
            {deviceResult ? (
              <div className="space-y-2 text-sm">
                <StatusRow
                  label={`Microphone${deviceResult.mic_name ? ` (${deviceResult.mic_name})` : ""}`}
                  ok={deviceResult.mic_available && deviceResult.mic_test_ok}
                />
                <StatusRow
                  label={`Speaker${deviceResult.speaker_name ? ` (${deviceResult.speaker_name})` : ""}`}
                  ok={deviceResult.speaker_available}
                />
                {deviceResult.errors.length > 0 && (
                  <div className="text-red-600 text-xs mt-2">
                    {deviceResult.errors.join("; ")}
                  </div>
                )}
              </div>
            ) : (
              <button
                onClick={handleDeviceCheck}
                disabled={deviceCheckBusy}
                className="px-4 py-2 bg-blue-600 text-white rounded hover:bg-blue-700 disabled:opacity-50"
              >
                {deviceCheckBusy ? "Checking…" : "Check Devices"}
              </button>
            )}
          </div>
        )}

        {/* Interview Area */}
        {phase === "ready" && (
          <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
            <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
              Interview Progress
            </h2>
            <p className="text-sm text-zinc-600 dark:text-zinc-400 mb-4">
              Round {currentRound + 1} of {INTERVIEW_QUESTIONS.length}
            </p>
            <button
              onClick={handleStartRound}
              className="w-full px-6 py-3 bg-indigo-600 text-white rounded-lg hover:bg-indigo-700 font-medium"
            >
              {currentRound === 0 ? "Start Interview" : "Next Question"}
            </button>
          </div>
        )}

        {/* Active Interview */}
        {(phase === "speaking-question" ||
          phase === "settling" ||
          phase === "recording-answer") && (
          <div className="w-full max-w-md border-2 border-indigo-500 rounded-lg p-4 dark:border-indigo-400">
            <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
              Round {currentRound + 1}
            </h2>
            <p className="text-sm text-zinc-700 dark:text-zinc-300 mb-4 italic">
              &ldquo;{currentQuestion}&rdquo;
            </p>
            <div className="flex items-center gap-3 mb-4">
              {phase === "speaking-question" && (
                <>
                  <div className="w-3 h-3 rounded-full bg-blue-500 animate-pulse" />
                  <span className="text-sm text-blue-600 dark:text-blue-400">
                    Speaking question...
                  </span>
                </>
              )}
              {phase === "settling" && (
                <>
                  <div className="w-3 h-3 rounded-full bg-yellow-500" />
                  <span className="text-sm text-yellow-600 dark:text-yellow-400">
                    Settling...
                  </span>
                </>
              )}
              {phase === "recording-answer" && (
                <>
                  <div className="w-3 h-3 rounded-full bg-red-500 animate-pulse" />
                  <span className="text-sm text-red-600 dark:text-red-400">
                    Recording your answer...
                  </span>
                </>
              )}
            </div>
            <button
              onClick={handleStop}
              disabled={isStopping}
              className="px-4 py-2 bg-red-600 text-white rounded hover:bg-red-700 disabled:opacity-50 text-sm"
            >
              {isStopping ? "Stopping..." : "Stop Round"}
            </button>
          </div>
        )}

        {/* Processing */}
        {phase === "processing" && (
          <div className="text-zinc-600 dark:text-zinc-400">
            Processing answer...
          </div>
        )}

        {/* Show Result */}
        {phase === "showing-result" && roundResults.length > 0 && (
          <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
            <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
              Round {roundResults.length} Complete
            </h2>
            <div className="space-y-2 text-sm">
              <p className="text-zinc-600 dark:text-zinc-400">
                <span className="font-medium">Duration:</span>{" "}
                {roundResults[roundResults.length - 1].metadata.duration_ms}ms
              </p>
              <p className="text-zinc-600 dark:text-zinc-400">
                <span className="font-medium">SHA-256:</span>{" "}
                <span className="font-mono text-xs break-all">
                  {roundResults[roundResults.length - 1].metadata.sha256.slice(
                    0,
                    16
                  )}
                  ...
                </span>
              </p>
              <div className="mt-3 p-3 bg-zinc-50 dark:bg-zinc-900 rounded">
                <p className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-1">
                  Transcription:
                </p>
                <p className="text-sm text-zinc-700 dark:text-zinc-300 italic">
                  &ldquo;{roundResults[roundResults.length - 1].transcription ||
                    "(no speech detected)"}&rdquo;
                </p>
              </div>
            </div>
            {currentRound < INTERVIEW_QUESTIONS.length ? (
              <button
                onClick={handleStartRound}
                className="w-full mt-4 px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700"
              >
                Next Question
              </button>
            ) : (
              <button
                onClick={() => setPhase("showing-result")}
                className="w-full mt-4 px-4 py-2 bg-green-600 text-white rounded hover:bg-green-700"
              >
                View All Results
              </button>
            )}
          </div>
        )}

        {/* Error */}
        {phase === "error" && error && (
          <div className="w-full max-w-md p-4 bg-red-50 border border-red-200 rounded text-red-700 text-sm">
            <p className="font-medium mb-1">Error</p>
            <p>{error}</p>
            {retryTarget ? (
              <button
                onClick={handleRetry}
                className="mt-3 px-3 py-1 bg-red-600 text-white rounded text-xs hover:bg-red-700"
              >
                Retry
              </button>
            ) : (
              <a
                href="/dashboard"
                className="inline-block mt-3 px-3 py-1 bg-zinc-600 text-white rounded text-xs hover:bg-zinc-700"
              >
                Back to Dashboard
              </a>
            )}
          </div>
        )}

        {/* Results Summary */}
        {roundResults.length > 0 &&
          phase !== "speaking-question" &&
          phase !== "recording-answer" && (
            <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
              <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
                Interview Summary
              </h2>
              <div className="space-y-2">
                {roundResults.map((r, i) => (
                  <div
                    key={i}
                    className="p-2 bg-zinc-50 dark:bg-zinc-900 rounded text-sm"
                  >
                    <p className="font-medium text-zinc-700 dark:text-zinc-300">
                      Q{i + 1}: {INTERVIEW_QUESTIONS[i]}
                    </p>
                    <p className="text-zinc-500 dark:text-zinc-400 italic mt-1">
                      A: {r.transcription || "(no speech)"}
                    </p>
                  </div>
                ))}
              </div>
            </div>
          )}
      </main>
    </div>
  );
}

function StatusRow({ label, ok }: { label: string; ok: boolean }) {
  return (
    <div className="flex items-center gap-2">
      <div
        className={`w-2 h-2 rounded-full ${ok ? "bg-green-500" : "bg-red-500"}`}
      />
      <span className={ok ? "text-green-700 dark:text-green-400" : "text-red-700 dark:text-red-400"}>
        {label}
      </span>
      <span className="text-zinc-400 text-xs ml-auto">
        {ok ? "OK" : "FAIL"}
      </span>
    </div>
  );
}
