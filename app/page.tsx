"use client";

import { useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";

type RecordingState = "idle" | "recording" | "stopped";

interface RecordingResult {
  path: string;
  duration_ms: number;
}

export default function Home() {
  const [recordingState, setRecordingState] = useState<RecordingState>("idle");
  const [result, setResult] = useState<RecordingResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ttsText, setTtsText] = useState("");
  const [ttsPath, setTtsPath] = useState<string | null>(null);

  const handleRecord = useCallback(async () => {
    setError(null);
    setResult(null);

    if (recordingState === "recording") {
      // Stop recording
      try {
        await invoke("stop_recording");
        setRecordingState("stopped");
      } catch (e) {
        setError(String(e));
      }
      return;
    }

    // Start recording
    try {
      setRecordingState("recording");
      // TODO: Use app_data_dir() for stable output path in production
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

  const handleGenerateTts = useCallback(async () => {
    if (!ttsText.trim()) return;
    setError(null);
    try {
      const outputPath = `tts_${Date.now()}.wav`;
      const path = await invoke<string>("generate_tts", {
        text: ttsText,
        outputPath,
      });
      setTtsPath(path);
    } catch (e) {
      setError(String(e));
    }
  }, [ttsText]);

  const handlePlayTts = useCallback(async () => {
    if (!ttsPath) return;
    setError(null);
    try {
      await invoke("play_audio", { filePath: ttsPath });
    } catch (e) {
      setError(String(e));
    }
  }, [ttsPath]);

  return (
    <div className="flex flex-col flex-1 items-center justify-center bg-zinc-50 font-sans dark:bg-black">
      <main className="flex flex-1 w-full max-w-3xl flex-col items-center gap-8 py-16 px-8 bg-white dark:bg-black">
        <h1 className="text-3xl font-semibold tracking-tight text-black dark:text-zinc-50">
          AI Interviewer
        </h1>
        <p className="text-zinc-600 dark:text-zinc-400">
          Audio capture and playback test
        </p>

        {/* TTS Section */}
        <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
          <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
            Text-to-Speech
          </h2>
          <textarea
            value={ttsText}
            onChange={(e) => setTtsText(e.target.value)}
            placeholder="Enter text to speak..."
            className="w-full p-2 border rounded dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100 mb-3"
            rows={3}
          />
          <div className="flex gap-2">
            <button
              onClick={handleGenerateTts}
              disabled={!ttsText.trim()}
              className="px-4 py-2 bg-blue-600 text-white rounded hover:bg-blue-700 disabled:opacity-50"
            >
              Generate TTS
            </button>
            <button
              onClick={handlePlayTts}
              disabled={!ttsPath}
              className="px-4 py-2 bg-green-600 text-white rounded hover:bg-green-700 disabled:opacity-50"
            >
              Play
            </button>
          </div>
          {ttsPath && (
            <p className="text-sm text-zinc-500 mt-2 break-all">
              Generated: {ttsPath}
            </p>
          )}
        </div>

        {/* Recording Section */}
        <div className="w-full max-w-md border rounded-lg p-4 dark:border-zinc-800">
          <h2 className="text-lg font-medium mb-3 dark:text-zinc-200">
            Microphone Recording
          </h2>

          <div className="flex items-center gap-4 mb-4">
            <button
              onClick={handleRecord}
              className={`px-6 py-3 rounded-full font-medium text-white transition-colors ${
                recordingState === "recording"
                  ? "bg-red-600 hover:bg-red-700 animate-pulse"
                  : "bg-indigo-600 hover:bg-indigo-700"
              }`}
            >
              {recordingState === "recording"
                ? "Stop Recording"
                : "Start Recording"}
            </button>

            {recordingState === "recording" && (
              <div className="flex items-center gap-2">
                <div
                  className="w-3 h-3 rounded-full bg-red-500 animate-pulse"
                />
                <span className="text-sm text-zinc-600 dark:text-zinc-400">
                  Recording...
                </span>
              </div>
            )}
          </div>

          {result && (
            <div className="text-sm text-zinc-600 dark:text-zinc-400">
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
