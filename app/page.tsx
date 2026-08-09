"use client";

import { useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";

interface AudioTestResult {
  duration_ms: number;
  file_size_bytes: number;
}

export default function Home() {
  const [testState, setTestState] = useState<"idle" | "testing">("idle");
  const [result, setResult] = useState<AudioTestResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Bounded, self-cleaning microphone test (P1-2): the backend records to a
  // TEMP file, auto-stops after a hard 10s wall-clock bound, and deletes the
  // WAV before returning. No orphan persistent recording can be created and
  // no backend microphone operation can run unbounded — navigating away
  // cannot leave a stray capture running.
  const handleAudioTest = useCallback(async () => {
    setError(null);
    setResult(null);
    setTestState("testing");

    try {
      const testResult = await invoke<AudioTestResult>("run_audio_test");
      setResult(testResult);
    } catch (e) {
      setError(String(e));
    } finally {
      setTestState("idle");
    }
  }, []);

  return (
    <div className="flex flex-col min-h-screen bg-zinc-50 dark:bg-black font-sans">
      <main className="flex flex-1 flex-col items-center gap-8 py-16 px-8">
        <h1 className="text-4xl font-semibold tracking-tight text-black dark:text-zinc-50">
          AI Interviewer
        </h1>
        <p className="text-zinc-600 dark:text-zinc-400 max-w-md text-center">
          Automated interview platform with local audio capture, TTS, and whisper transcription.
        </p>

        {/* Navigation Cards */}
        <div className="grid grid-cols-1 md:grid-cols-3 gap-4 w-full max-w-2xl">
          <a
            href="/interview"
            className="block p-6 border rounded-lg dark:border-zinc-800 hover:border-indigo-500 dark:hover:border-indigo-400 transition-colors"
          >
            <h2 className="text-lg font-medium dark:text-zinc-200 mb-2">
              Start Interview
            </h2>
            <p className="text-sm text-zinc-500 dark:text-zinc-400">
              Device check, TTS playback, recording, and transcription.
            </p>
          </a>

          <a
            href="/candidate"
            className="block p-6 border rounded-lg dark:border-zinc-800 hover:border-indigo-500 dark:hover:border-indigo-400 transition-colors"
          >
            <h2 className="text-lg font-medium dark:text-zinc-200 mb-2">
              Candidate View
            </h2>
            <p className="text-sm text-zinc-500 dark:text-zinc-400">
              Restricted window for the interviewee.
            </p>
          </a>

          <a
            href="/dashboard"
            className="block p-6 border rounded-lg dark:border-zinc-800 hover:border-indigo-500 dark:hover:border-indigo-400 transition-colors"
          >
            <h2 className="text-lg font-medium dark:text-zinc-200 mb-2">
              Dashboard
            </h2>
            <p className="text-sm text-zinc-500 dark:text-zinc-400">
              View past sessions and start interviews.
            </p>
          </a>
        </div>

        {/* Audio Test */}
        <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
          <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
            Audio Test
          </h2>
          <p className="text-sm text-zinc-500 dark:text-zinc-400 mb-3">
            Records a short clip to verify the microphone. The recording is
            temporary and deleted automatically (max 10 seconds).
          </p>
          <div className="flex items-center gap-4">
            <button
              onClick={handleAudioTest}
              disabled={testState === "testing"}
              className={`px-6 py-3 rounded-full font-medium text-white transition-colors disabled:opacity-60 ${
                testState === "testing"
                  ? "bg-red-600 animate-pulse"
                  : "bg-indigo-600 hover:bg-indigo-700"
              }`}
            >
              {testState === "testing" ? "Testing…" : "Run Audio Test"}
            </button>
            {testState === "testing" && (
              <div className="flex items-center gap-2">
                <div className="w-3 h-3 rounded-full bg-red-500 animate-pulse" />
                <span className="text-sm text-zinc-600 dark:text-zinc-400">
                  Testing microphone… (auto-stops after 10s)
                </span>
              </div>
            )}
          </div>
          {result && (
            <div className="mt-3 text-sm text-zinc-600 dark:text-zinc-400">
              <p>Captured {(result.duration_ms / 1000).toFixed(1)}s of audio ({result.file_size_bytes} bytes)</p>
              <p className="text-xs text-zinc-400 mt-1">
                Temporary recording deleted — nothing was saved.
              </p>
            </div>
          )}
        </div>

        {error && (
          <div className="w-full max-w-md p-3 bg-red-50 border border-red-200 rounded text-red-700 text-sm">
            {error}
          </div>
        )}
      </main>
    </div>
  );
}
