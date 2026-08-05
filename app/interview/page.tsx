"use client";

import { useState, useCallback, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";

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

interface ToolsStatus {
  piper: boolean;
  whisper: boolean;
  model: boolean;
}

interface AppConfig {
  tool_dir: string;
  recordings_dir: string;
  db_path: string;
  is_portable: boolean;
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

const QUESTIONS = [
  "Tell me about yourself and your background.",
  "What are your strengths and weaknesses?",
  "Why are you interested in this position?",
  "Describe a challenging project you worked on.",
  "Where do you see yourself in five years?",
];

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

  // Check tools on mount
  useEffect(() => {
    const checkTools = async () => {
      try {
        // Bootstrap readiness check — resolves paths, verifies tools exist
        const _config = await invoke<AppConfig>("get_app_config");
        const status = await invoke<ToolsStatus>("verify_tools_installation");
        setToolsStatus(status);

        if (status.piper && status.whisper && status.model) {
          setPhase("device-check");
        } else {
          setError(
            `Missing tools: ${!status.piper ? "Piper TTS " : ""}${!status.whisper ? "Whisper " : ""}${!status.model ? "Model " : ""}`
          );
          setPhase("error");
        }
      } catch (e) {
        setError(String(e));
        setPhase("error");
      }
    };
    checkTools();
  }, []);

  // Run device check
  const handleDeviceCheck = useCallback(async () => {
    setPhase("device-check");
    try {
      const result = await invoke<DeviceCheckResult>("check_audio_devices");
      setDeviceResult(result);

      if (result.mic_available && result.mic_test_ok) {
        setPhase("ready");
      } else {
        setPhase("error");
        setError(
          result.errors.length > 0
            ? result.errors.join("; ")
            : "Device check failed"
        );
      }
    } catch (e) {
      setPhase("error");
      setError(String(e));
    }
  }, []);

  // Start interview round
  const handleStartRound = useCallback(async () => {
    if (currentRound >= QUESTIONS.length) {
      setPhase("showing-result");
      return;
    }

    const question = QUESTIONS[currentRound];
    setCurrentQuestion(question);
    setPhase("speaking-question");

    try {
      const result = await invoke<InterviewRoundResult>("run_interview_round", {
        question,
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
      }
    }
  }, [currentRound]);

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

        {/* Device Check */}
        {(phase === "device-check" || phase === "ready") && (
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
                className="px-4 py-2 bg-blue-600 text-white rounded hover:bg-blue-700"
              >
                Check Devices
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
              Round {currentRound + 1} of {QUESTIONS.length}
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
            {currentRound < QUESTIONS.length ? (
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
            <button
              onClick={() => {
                setPhase("device-check");
                setError(null);
              }}
              className="mt-3 px-3 py-1 bg-red-600 text-white rounded text-xs hover:bg-red-700"
            >
              Retry
            </button>
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
                      Q{i + 1}: {QUESTIONS[i]}
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
