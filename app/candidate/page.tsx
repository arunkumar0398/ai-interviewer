"use client";

import { useState, useCallback } from "react";

type CandidatePhase = "waiting" | "listening" | "recording" | "done";

export default function CandidatePage() {
  const [phase, setPhase] = useState<CandidatePhase>("waiting");
  const [round] = useState(0);

  // Candidate can manually trigger "ready" to signal the recruiter
  const handleReady = useCallback(() => {
    setPhase("waiting");
  }, []);

  return (
    <div className="flex flex-col min-h-screen bg-white dark:bg-zinc-950 font-sans">
      <main className="flex flex-1 flex-col items-center justify-center gap-8 px-6">
        <h1 className="text-2xl font-semibold text-black dark:text-zinc-50">
          Interview Session
        </h1>

        {phase === "waiting" && (
          <div className="text-center">
            <div className="w-16 h-16 mx-auto mb-4 rounded-full bg-zinc-100 dark:bg-zinc-800 flex items-center justify-center">
              <svg
                className="w-8 h-8 text-zinc-400"
                fill="none"
                viewBox="0 0 24 24"
                stroke="currentColor"
              >
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth={2}
                  d="M19 11a7 7 0 01-7 7m0 0a7 7 0 01-7-7m7 7v4m0 0H8m4 0h4m-4-8a3 3 0 01-3-3V5a3 3 0 116 0v6a3 3 0 01-3 3z"
                />
              </svg>
            </div>
            <p className="text-zinc-600 dark:text-zinc-400">
              Waiting for the interviewer to begin...
            </p>
            <p className="text-sm text-zinc-400 dark:text-zinc-500 mt-2">
              Round {round + 1}
            </p>
          </div>
        )}

        {phase === "listening" && (
          <div className="text-center">
            <div className="w-16 h-16 mx-auto mb-4 rounded-full bg-blue-100 dark:bg-blue-900 flex items-center justify-center animate-pulse">
              <svg
                className="w-8 h-8 text-blue-500"
                fill="none"
                viewBox="0 0 24 24"
                stroke="currentColor"
              >
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth={2}
                  d="M15.536 8.464a5 5 0 010 7.072m2.828-9.9a9 9 0 010 12.728M5.586 15H4a1 1 0 01-1-1v-4a1 1 0 011-1h1.586l4.707-4.707C10.923 3.663 12 4.109 12 5v14c0 .891-1.077 1.337-1.707.707L5.586 15z"
                />
              </svg>
            </div>
            <p className="text-blue-600 dark:text-blue-400 font-medium">
              Listening to question...
            </p>
          </div>
        )}

        {phase === "recording" && (
          <div className="text-center">
            <div className="w-16 h-16 mx-auto mb-4 rounded-full bg-red-100 dark:bg-red-900 flex items-center justify-center">
              <div className="w-4 h-4 rounded-full bg-red-500 animate-pulse" />
            </div>
            <p className="text-red-600 dark:text-red-400 font-medium">
              Recording your answer...
            </p>
            <p className="text-sm text-zinc-400 mt-2">
              Speak clearly into your microphone
            </p>
          </div>
        )}

        {phase === "done" && (
          <div className="text-center">
            <div className="w-16 h-16 mx-auto mb-4 rounded-full bg-green-100 dark:bg-green-900 flex items-center justify-center">
              <svg
                className="w-8 h-8 text-green-500"
                fill="none"
                viewBox="0 0 24 24"
                stroke="currentColor"
              >
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth={2}
                  d="M5 13l4 4L19 7"
                />
              </svg>
            </div>
            <p className="text-green-600 dark:text-green-400 font-medium">
              Answer recorded
            </p>
            <button
              onClick={handleReady}
              className="mt-4 px-4 py-2 bg-zinc-200 dark:bg-zinc-700 text-zinc-700 dark:text-zinc-200 rounded hover:bg-zinc-300 dark:hover:bg-zinc-600"
            >
              Ready for next question
            </button>
          </div>
        )}
      </main>
    </div>
  );
}
