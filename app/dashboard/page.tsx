"use client";

import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import Link from "next/link";

interface InterviewSession {
  id: string;
  candidate_name: string;
  started_at: string;
  completed_at: string | null;
  total_rounds: number;
}

interface InterviewRound {
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

const QUESTIONS = [
  "Tell me about yourself and your background.",
  "What is your experience with Rust or systems programming?",
  "Describe a challenging technical problem you solved recently.",
  "How do you approach debugging complex issues?",
  "What interests you about this role?",
];

export default function DashboardPage() {
  const [sessions, setSessions] = useState<InterviewSession[]>([]);
  const [selectedSession, setSelectedSession] = useState<string | null>(null);
  const [rounds, setRounds] = useState<InterviewRound[]>([]);
  const [candidateName, setCandidateName] = useState("");
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [dbReady, setDbReady] = useState(false);
  const [questions, setQuestions] = useState<string[]>(QUESTIONS);

  const loadSessions = async () => {
    try {
      const data = await invoke<InterviewSession[]>("get_sessions");
      setSessions(data);
    } catch (e) {
      console.error("Failed to load sessions:", e);
    }
  };

  useEffect(() => {
    const initDb = async () => {
      try {
        await invoke("get_app_config");
        setDbReady(true);
        await loadSessions();
      } catch (e) {
        console.error("Failed to init DB:", e);
      }
    };
    initDb();
  }, []);

  const loadRounds = async (sessionId: string) => {
    try {
      const data = await invoke<InterviewRound[]>("get_rounds", { sessionId });
      setRounds(data);
      setSelectedSession(sessionId);
    } catch (e) {
      console.error("Failed to load rounds:", e);
    }
  };

  const startNewSession = async () => {
    if (!candidateName.trim()) return;
    const sessionId = `session-${Date.now()}`;
    try {
      await invoke("create_session", {
        sessionId,
        candidateName: candidateName.trim(),
      });
      setActiveSessionId(sessionId);
      setCandidateName("");
      loadSessions();
    } catch (e) {
      console.error("Failed to create session:", e);
    }
  };

  const addQuestion = () => {
    setQuestions([...questions, ""]);
  };

  const updateQuestion = (index: number, value: string) => {
    const updated = [...questions];
    updated[index] = value;
    setQuestions(updated);
  };

  const removeQuestion = (index: number) => {
    if (questions.length <= 1) return;
    setQuestions(questions.filter((_, i) => i !== index));
  };

  return (
    <div className="min-h-screen bg-gray-50 p-6">
      <div className="max-w-6xl mx-auto">
        <div className="flex items-center justify-between mb-8">
          <h1 className="text-3xl font-bold text-gray-900">
            Recruiter Dashboard
          </h1>
          <Link
            href="/"
            className="text-blue-600 hover:text-blue-800 text-sm font-medium"
          >
            &larr; Back to Home
          </Link>
        </div>

        <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
          {/* Left: Start New Interview */}
          <div className="bg-white rounded-lg shadow p-6">
            <h2 className="text-lg font-semibold mb-4">New Interview</h2>
            <div className="space-y-4">
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">
                  Candidate Name
                </label>
                <input
                  type="text"
                  value={candidateName}
                  onChange={(e) => setCandidateName(e.target.value)}
                  placeholder="Enter candidate name"
                  className="w-full border rounded-lg px-3 py-2 text-sm"
                  disabled={!!activeSessionId}
                />
              </div>
              <button
                onClick={startNewSession}
                disabled={!dbReady || !candidateName.trim() || !!activeSessionId}
                className="w-full bg-blue-600 text-white rounded-lg px-4 py-2 text-sm font-medium hover:bg-blue-700 disabled:bg-gray-300"
              >
                {activeSessionId ? "Session Active" : "Start Interview"}
              </button>
              {activeSessionId && (
                <div className="bg-green-50 border border-green-200 rounded-lg p-3 text-sm">
                  <p className="font-medium text-green-800">Session Active</p>
                  <p className="text-green-600 text-xs mt-1">
                    ID: {activeSessionId.slice(0, 20)}...
                  </p>
                  <p className="text-xs text-gray-500 mt-1">
                    Go to{" "}
                    <Link href="/interview" className="text-blue-600 underline">
                      /interview
                    </Link>{" "}
                    to run the interview
                  </p>
                </div>
              )}
            </div>
          </div>

          {/* Center: Question Bank */}
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center justify-between mb-4">
              <h2 className="text-lg font-semibold">Question Bank</h2>
              <button
                onClick={addQuestion}
                className="text-blue-600 hover:text-blue-800 text-sm font-medium"
              >
                + Add
              </button>
            </div>
            <div className="space-y-3 max-h-96 overflow-y-auto">
              {questions.map((q, i) => (
                <div key={i} className="flex gap-2">
                  <span className="text-xs text-gray-400 mt-2 w-5">
                    {i + 1}.
                  </span>
                  <input
                    type="text"
                    value={q}
                    onChange={(e) => updateQuestion(i, e.target.value)}
                    className="flex-1 border rounded px-2 py-1 text-sm"
                    placeholder={`Question ${i + 1}`}
                  />
                  <button
                    onClick={() => removeQuestion(i)}
                    className="text-red-400 hover:text-red-600 text-sm px-1"
                    disabled={questions.length <= 1}
                  >
                    x
                  </button>
                </div>
              ))}
            </div>
          </div>

          {/* Right: Past Sessions */}
          <div className="bg-white rounded-lg shadow p-6">
            <h2 className="text-lg font-semibold mb-4">Past Sessions</h2>
            {sessions.length === 0 ? (
              <p className="text-gray-400 text-sm">No sessions yet</p>
            ) : (
              <div className="space-y-2 max-h-96 overflow-y-auto">
                {sessions.map((s) => (
                  <button
                    key={s.id}
                    onClick={() => loadRounds(s.id)}
                    className={`w-full text-left border rounded-lg p-3 text-sm transition ${
                      selectedSession === s.id
                        ? "border-blue-500 bg-blue-50"
                        : "border-gray-200 hover:border-gray-300"
                    }`}
                  >
                    <div className="flex justify-between">
                      <span className="font-medium">{s.candidate_name}</span>
                      <span
                        className={`text-xs px-2 py-0.5 rounded ${
                          s.completed_at
                            ? "bg-green-100 text-green-700"
                            : "bg-yellow-100 text-yellow-700"
                        }`}
                      >
                        {s.completed_at ? "Completed" : "In Progress"}
                      </span>
                    </div>
                    <div className="text-xs text-gray-500 mt-1">
                      {s.total_rounds} rounds &middot; {s.started_at}
                    </div>
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>

        {/* Bottom: Session Details */}
        {selectedSession && (
          <div className="mt-6 bg-white rounded-lg shadow p-6">
            <h2 className="text-lg font-semibold mb-4">
              Session Details:{" "}
              {sessions.find((s) => s.id === selectedSession)?.candidate_name}
            </h2>
            {rounds.length === 0 ? (
              <p className="text-gray-400 text-sm">No rounds recorded yet</p>
            ) : (
              <div className="space-y-4">
                {rounds.map((r) => (
                  <div key={r.id} className="border rounded-lg p-4">
                    <div className="flex justify-between items-start mb-2">
                      <div>
                        <span className="text-xs text-gray-400">
                          Round {r.round_index + 1}
                        </span>
                        <h3 className="font-medium text-gray-900">
                          {r.question}
                        </h3>
                      </div>
                      <span className="text-xs text-gray-500">
                        {(r.duration_ms / 1000).toFixed(1)}s &middot;{" "}
                        {r.file_size_bytes} bytes
                      </span>
                    </div>
                    <div className="bg-gray-50 rounded p-3 mb-2">
                      <p className="text-sm text-gray-700">
                        {r.transcription || (
                          <span className="text-gray-400 italic">
                            No transcription
                          </span>
                        )}
                      </p>
                    </div>
                    <div className="flex gap-4 text-xs text-gray-400">
                      <span>
                        SHA-256: {r.sha256.slice(0, 16)}...
                      </span>
                      <span>{r.sample_rate}Hz</span>
                      <span>{r.channels}ch</span>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
