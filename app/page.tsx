"use client";

import { useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";

interface RecordingResult {
  path: string;
  duration_ms: number;
}

export default function Home() {
  const [recordingState, setRecordingState] = useState<"idle" | "recording" | "stopped">("idle");
  const [result, setResult] = useState<RecordingResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  const handleRecord = useCallback(async () => {
    setError(null);
    setResult(null);

    if (recordingState === "recording") {
      try {
        await invoke("stop_recording");
        setRecordingState("stopped");
      } catch (e) {
        setError(String(e));
      }
      return;
    }

    try {
      setRecordingState("recording");
      const outputPath = `recording_${Date.now()}.wav`;
      const result = await invoke<RecordingResult>("start_recording", {
        outputPath,
        sampleRate: 16000,
      });
      setResult(result);
      setRecordingState("stopped");
    } catch (e) {
      setError(String(e));
      setRecordingState("idle");
    }
  }, [recordingState]);

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
              View past sessions and manage questions.
            </p>
          </a>
        </div>

        {/* Audio Test */}
        <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
          <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
            Audio Test
          </h2>
          <div className="flex items-center gap-4">
            <button
              onClick={handleRecord}
              className={`px-6 py-3 rounded-full font-medium text-white transition-colors ${
                recordingState === "recording"
                  ? "bg-red-600 hover:bg-red-700 animate-pulse"
                  : "bg-indigo-600 hover:bg-indigo-700"
              }`}
            >
              {recordingState === "recording" ? "Stop Recording" : "Start Recording"}
            </button>
            {recordingState === "recording" && (
              <div className="flex items-center gap-2">
                <div className="w-3 h-3 rounded-full bg-red-500 animate-pulse" />
                <span className="text-sm text-zinc-600 dark:text-zinc-400">Recording...</span>
              </div>
            )}
          </div>
          {result && (
            <div className="mt-3 text-sm text-zinc-600 dark:text-zinc-400">
              <p>Saved: {result.path}</p>
              <p>Duration: {result.duration_ms}ms</p>
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
