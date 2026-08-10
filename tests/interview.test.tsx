import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, act, fireEvent } from "@testing-library/react";
import InterviewPage from "../app/interview/page";
import { INTERVIEW_QUESTIONS } from "../lib/interview-questions";
import { mockListenCallbacks } from "./setup";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

const mockAppConfig = {
  tool_dir: "/fake/tools",
  recordings_dir: "/fake/recordings",
  db_path: "/fake/interviews.db",
  readiness: {
    ready: true,
    issues: [],
  },
};

const mockAppConfigWithMissingTools = {
  tool_dir: "/fake/tools",
  recordings_dir: "/fake/recordings",
  db_path: "/fake/interviews.db",
  readiness: {
    ready: false,
    issues: [
      { code: "PIPER_BINARY_MISSING", message: "Piper binary not found", expected_path: "/fake/tools/piper/piper.exe" },
      { code: "WHISPER_BINARY_MISSING", message: "Whisper binary not found", expected_path: "/fake/tools/whisper/Release/main.exe" },
    ],
  },
};

function mockStoredRound(sessionId: string, roundIndex: number) {
  return {
    id: roundIndex + 1,
    session_id: sessionId,
    round_index: roundIndex,
    question: INTERVIEW_QUESTIONS[roundIndex] ?? `Unexpected question ${roundIndex}`,
    transcription: `Persisted answer ${roundIndex + 1}`,
    audio_path: `/tmp/persisted-round-${roundIndex}.wav`,
    sha256: `persisted-sha-${roundIndex}`,
    duration_ms: 4000,
    sample_rate: 16000,
    channels: 1,
    file_size_bytes: 100,
    created_at: "2026-01-01T00:01:00Z",
  };
}

describe("Interview Page", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders the heading on mount", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);
    await act(async () => {
      render(<InterviewPage />);
    });
    expect(screen.getByText("AI Interviewer")).toBeInTheDocument();
  });

  it("shows checking tools state initially", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);
    await act(async () => {
      render(<InterviewPage />);
    });
    // After mount, should transition to device-check since tools are ready
    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });
  });

  it("shows error when get_app_config fails", async () => {
    mockInvoke.mockRejectedValueOnce(new Error("Command not found"));
    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Error")).toBeInTheDocument();
    });
    expect(screen.getByText(/Command not found/)).toBeInTheDocument();
  });

  it("shows error when tools are missing", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfigWithMissingTools);

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Error")).toBeInTheDocument();
    });
    expect(screen.getByText(/Missing tools/)).toBeInTheDocument();
  });

  it("shows tools status when all tools are present", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });
  });

  it("shows device check section when tools are ready", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });
    expect(screen.getByText("Check Devices")).toBeInTheDocument();
  });

  it("reads questions from the single shared INTERVIEW_QUESTIONS source (P1-2)", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });

    // The page drives its rounds from the shared constant — exactly 5,
    // matching the backend EXPECTED_ROUNDS.
    expect(INTERVIEW_QUESTIONS.length).toBe(5);
  });

  it("has Retry button in error state", async () => {
    mockInvoke.mockRejectedValue(new Error("fail"));
    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Retry")).toBeInTheDocument();
    });
  });

  it("responds to interview-phase events and updates UI", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    // Wait for component to reach device-check phase
    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });

    // Simulate backend emitting a "speaking-question" phase event
    const callback = mockListenCallbacks.get("interview-phase");
    expect(callback).toBeDefined();

    act(() => {
      callback!({
        payload: {
          phase: "speaking-question",
          question: "Tell me about yourself",
        },
      });
    });

    await waitFor(() => {
      const matches = screen.getAllByText((_, node) =>
        node?.textContent?.includes("Tell me about yourself") ?? false
      );
      expect(matches.length).toBeGreaterThanOrEqual(1);
    });
  });

  it("responds to settling phase event and shows settling state", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    // Wait for component to reach device-check phase
    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });

    // Simulate backend emitting a "settling" phase event
    const callback = mockListenCallbacks.get("interview-phase");
    expect(callback).toBeDefined();

    act(() => {
      callback!({
        payload: {
          phase: "settling",
          duration_ms: 1500,
        },
      });
    });

    // The component should show settling state (no specific text, but phase changes)
    // We can verify the phase change by checking that the UI doesn't show other states
    await waitFor(() => {
      // Component should not be in device-check or error state
      expect(screen.queryByText("Device Check")).not.toBeInTheDocument();
    });
  });

  it("responds to recording-answer phase event and shows recording state", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    // Wait for component to reach device-check phase
    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });

    // Simulate backend emitting a "recording-answer" phase event
    const callback = mockListenCallbacks.get("interview-phase");
    expect(callback).toBeDefined();

    act(() => {
      callback!({
        payload: {
          phase: "recording-answer",
        },
      });
    });

    // The component should show recording state
    await waitFor(() => {
      // Component should not be in device-check or error state
      expect(screen.queryByText("Device Check")).not.toBeInTheDocument();
    });
  });

  it("recovers from create_session failure via Retry with the same session UUID", async () => {
    let createSessionCalls = 0;
    let capturedSessionId: string | null = null;
    let roundCalls = 0;

    mockInvoke.mockImplementation(
      (cmd: string, args?: Record<string, unknown>) => {
        switch (cmd) {
          case "get_app_config":
            return Promise.resolve(mockAppConfig);
          case "check_audio_devices":
            return Promise.resolve({
              mic_available: true,
              mic_name: "Mic",
              speaker_available: true,
              speaker_name: "Speaker",
              mic_test_ok: true,
              errors: [],
            });
          case "create_session": {
            capturedSessionId = args?.sessionId as string;
            createSessionCalls += 1;
            // First attempt fails, the retry succeeds.
            if (createSessionCalls === 1) {
              return Promise.reject(new Error("DB locked"));
            }
            return Promise.resolve();
          }
          case "run_interview_round": {
            roundCalls += 1;
            expect(args?.sessionId).toBe(capturedSessionId);
            return Promise.resolve({
              metadata: {
                file_path: "/tmp/round.wav",
                sha256: "abc123",
                duration_ms: 5000,
                sample_rate: 16000,
                channels: 1,
                file_size_bytes: 100,
              },
              transcription: "My answer",
            });
          }
          default:
            return Promise.reject(new Error(`Unexpected command: ${cmd}`));
        }
      }
    );

    await act(async () => {
      render(<InterviewPage />);
    });

    // Tools ready -> device check screen.
    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Check Devices"));

    // Device check passes -> ready to start.
    await waitFor(() => {
      expect(screen.getByText("Start Interview")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Start Interview"));

    // create_session fails -> error phase with Retry.
    await waitFor(() => {
      expect(screen.getByText("Retry")).toBeInTheDocument();
    });
    expect(createSessionCalls).toBe(1);

    // Retry retries ONLY create_session, preserving the stable sessionId.
    fireEvent.click(screen.getByText("Retry"));
    await waitFor(() => {
      expect(screen.getByText("Start Interview")).toBeInTheDocument();
    });
    expect(createSessionCalls).toBe(2);

    // First round can now execute exactly once, with the same session UUID.
    fireEvent.click(screen.getByText("Start Interview"));
    await waitFor(() => {
      expect(screen.getByText("Round 1 Complete")).toBeInTheDocument();
    });
    expect(roundCalls).toBe(1);
    expect(capturedSessionId).toBeTruthy();
  });

  it("does not become ready when speaker is missing (P2-5)", async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "check_audio_devices":
          return Promise.resolve({
            mic_available: true,
            mic_name: "Mic",
            speaker_available: false,
            speaker_name: null,
            mic_test_ok: true,
            errors: ["No speaker/headphone detected"],
          });
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Check Devices"));

    // Mic is OK but the speaker is missing -> NOT ready; error + Retry shown.
    await waitFor(() => {
      expect(screen.getByText("Retry")).toBeInTheDocument();
    });
    expect(screen.getByText(/No speaker\/headphone detected/)).toBeInTheDocument();
    expect(screen.queryByText("Start Interview")).not.toBeInTheDocument();
  });

  it("does not pass isFinal to run_interview_round (P1-2)", async () => {
    let capturedArgs: Record<string, unknown> | null = null;
    mockInvoke.mockImplementation((cmd: string, args?: Record<string, unknown>) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "check_audio_devices":
          return Promise.resolve({
            mic_available: true,
            mic_name: "Mic",
            speaker_available: true,
            speaker_name: "Speaker",
            mic_test_ok: true,
            errors: [],
          });
        case "create_session":
          return Promise.resolve();
        case "run_interview_round": {
          capturedArgs = args ?? null;
          return Promise.resolve({
            metadata: {
              file_path: "/tmp/round.wav",
              sha256: "abc123",
              duration_ms: 5000,
              sample_rate: 16000,
              channels: 1,
              file_size_bytes: 100,
            },
            transcription: "My answer",
          });
        }
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });
    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Check Devices"));
    await waitFor(() => {
      expect(screen.getByText("Start Interview")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Start Interview"));
    await waitFor(() => {
      expect(screen.getByText("Round 1 Complete")).toBeInTheDocument();
    });

    expect(capturedArgs).not.toBeNull();
    expect(capturedArgs).not.toHaveProperty("isFinal");
  });

  it("responds to processing phase event and shows processing state", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    // Wait for component to reach device-check phase
    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });

    // Simulate backend emitting a "processing" phase event
    const callback = mockListenCallbacks.get("interview-phase");
    expect(callback).toBeDefined();

    act(() => {
      callback!({
        payload: {
          phase: "processing",
        },
      });
    });

    // The component should show processing state
    await waitFor(() => {
      // Component should not be in device-check or error state
      expect(screen.queryByText("Device Check")).not.toBeInTheDocument();
    });
  });

  it("adopts the Dashboard session from the URL — no second create_session (P1-1, P2-4)", async () => {
    const handedOffSession = "11111111-2222-3333-4444-555555555555";
    // Simulate the Dashboard's session-only handoff link: /interview?session=<uuid>
    window.history.replaceState({}, "", `/interview?session=${handedOffSession}`);

    let createSessionCalls = 0;
    let getSessionCalls = 0;
    let getRoundsCalls = 0;
    let roundSessionId: string | null = null;
    mockInvoke.mockImplementation((cmd: string, args?: Record<string, unknown>) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "check_audio_devices":
          return Promise.resolve({
            mic_available: true,
            mic_name: "Mic",
            speaker_available: true,
            speaker_name: "Speaker",
            mic_test_ok: true,
            errors: [],
          });
        case "get_session": {
          getSessionCalls += 1;
          return Promise.resolve({
            id: handedOffSession,
            candidate_name: "Alice",
            started_at: "2026-01-01T00:00:00Z",
            completed_at: null,
            total_rounds: 0,
          });
        }
        case "get_rounds":
          getRoundsCalls += 1;
          expect(args?.sessionId).toBe(handedOffSession);
          return Promise.resolve([]);
        case "create_session":
          createSessionCalls += 1;
          return Promise.resolve();
        case "run_interview_round": {
          roundSessionId = (args?.sessionId as string) ?? null;
          return Promise.resolve({
            metadata: {
              file_path: "/tmp/round.wav",
              sha256: "abc123",
              duration_ms: 5000,
              sample_rate: 16000,
              channels: 1,
              file_size_bytes: 100,
            },
            transcription: "My answer",
          });
        }
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    // The handed-off session is verified against the backend first (P2-1).
    await waitFor(() => {
      expect(getSessionCalls).toBe(1);
    });
    expect(getRoundsCalls).toBe(1);
    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Check Devices"));

    // Ready — the handed-off session was created by the Dashboard, so the
    // Interview page must NOT call create_session again.
    await waitFor(() => {
      expect(screen.getByText("Start Interview")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("Start Interview"));

    await waitFor(() => {
      expect(screen.getByText("Round 1 Complete")).toBeInTheDocument();
    });
    expect(createSessionCalls).toBe(0);
    expect(roundSessionId).toBe(handedOffSession);

    // Restore the URL for other tests.
    window.history.replaceState({}, "", "/");
  });

  it.each([1, 2, 3, 4])(
    "resumes a handed-off session at persisted round %i",
    async (persistedCount) => {
      const handedOffSession = "22222222-3333-4444-5555-666666666666";
      window.history.replaceState({}, "", `/interview?session=${handedOffSession}`);
      let runRoundArgs: Record<string, unknown> | null = null;
      const storedRounds = Array.from({ length: persistedCount }, (_, roundIndex) =>
        mockStoredRound(handedOffSession, roundIndex)
      );

      mockInvoke.mockImplementation((cmd: string, args?: Record<string, unknown>) => {
        switch (cmd) {
          case "get_app_config":
            return Promise.resolve(mockAppConfig);
          case "get_session":
            return Promise.resolve({
              id: handedOffSession,
              candidate_name: "Alice",
              started_at: "2026-01-01T00:00:00Z",
              completed_at: null,
              total_rounds: 0,
            });
          case "get_rounds":
            return Promise.resolve(storedRounds);
          case "check_audio_devices":
            return Promise.resolve({
              mic_available: true,
              mic_name: "Mic",
              speaker_available: true,
              speaker_name: "Speaker",
              mic_test_ok: true,
              errors: [],
            });
          case "run_interview_round":
            runRoundArgs = args ?? null;
            return Promise.resolve({
              metadata: {
                file_path: "/tmp/round.wav",
                sha256: "next-sha",
                duration_ms: 5000,
                sample_rate: 16000,
                channels: 1,
                file_size_bytes: 100,
              },
              transcription: "Next answer",
            });
          default:
            return Promise.reject(new Error(`Unexpected command: ${cmd}`));
        }
      });

      await act(async () => {
        render(<InterviewPage />);
      });

      await waitFor(() => {
        expect(screen.getByText("Check Devices")).toBeInTheDocument();
      });
      fireEvent.click(screen.getByText("Check Devices"));
      await waitFor(() => {
        expect(screen.getByText("Next Question")).toBeInTheDocument();
      });
      fireEvent.click(screen.getByText("Next Question"));

      await waitFor(() => {
        expect(runRoundArgs).not.toBeNull();
      });
      expect(runRoundArgs).toMatchObject({
        sessionId: handedOffSession,
        roundIndex: persistedCount,
        question: INTERVIEW_QUESTIONS[persistedCount],
      });

      window.history.replaceState({}, "", "/");
    }
  );

  it("rejects a handed-off session with a gap in its persisted round history", async () => {
    const handedOffSession = "33333333-4444-5555-6666-777777777777";
    window.history.replaceState({}, "", `/interview?session=${handedOffSession}`);
    let deviceCheckCalls = 0;
    let roundCalls = 0;

    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "get_session":
          return Promise.resolve({
            id: handedOffSession,
            candidate_name: "Alice",
            started_at: "2026-01-01T00:00:00Z",
            completed_at: null,
            total_rounds: 2,
          });
        case "get_rounds":
          return Promise.resolve(
            [0, 2].map((roundIndex) => mockStoredRound(handedOffSession, roundIndex))
          );
        case "check_audio_devices":
          deviceCheckCalls += 1;
          return Promise.resolve({});
        case "run_interview_round":
          roundCalls += 1;
          return Promise.resolve({});
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(
        screen.getByText("This session has an inconsistent round history and cannot be resumed.")
      ).toBeInTheDocument();
    });
    expect(screen.queryByText("Check Devices")).not.toBeInTheDocument();
    expect(screen.queryByText("Start Interview")).not.toBeInTheDocument();
    expect(screen.queryByText("Retry")).not.toBeInTheDocument();
    expect(deviceCheckCalls).toBe(0);
    expect(roundCalls).toBe(0);

    window.history.replaceState({}, "", "/");
  });

  it("rejects an incomplete handed-off session that already has every interview round", async () => {
    const handedOffSession = "44444444-5555-6666-7777-888888888888";
    window.history.replaceState({}, "", `/interview?session=${handedOffSession}`);
    let deviceCheckCalls = 0;
    let roundCalls = 0;

    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "get_session":
          return Promise.resolve({
            id: handedOffSession,
            candidate_name: "Alice",
            started_at: "2026-01-01T00:00:00Z",
            completed_at: null,
            total_rounds: INTERVIEW_QUESTIONS.length,
          });
        case "get_rounds":
          return Promise.resolve(
            INTERVIEW_QUESTIONS.map((_, roundIndex) =>
              mockStoredRound(handedOffSession, roundIndex)
            )
          );
        case "check_audio_devices":
          deviceCheckCalls += 1;
          return Promise.resolve({});
        case "run_interview_round":
          roundCalls += 1;
          return Promise.resolve({});
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(
        screen.getByText("This session has an inconsistent round history and cannot be resumed.")
      ).toBeInTheDocument();
    });
    expect(screen.queryByText("Round 6 of 5")).not.toBeInTheDocument();
    expect(screen.queryByText("Check Devices")).not.toBeInTheDocument();
    expect(deviceCheckCalls).toBe(0);
    expect(roundCalls).toBe(0);

    window.history.replaceState({}, "", "/");
  });

  it("rejects a handed-off session with more persisted rounds than the interview allows", async () => {
    const handedOffSession = "44444444-5555-6666-7777-888888888888";
    window.history.replaceState({}, "", `/interview?session=${handedOffSession}`);
    let deviceCheckCalls = 0;
    let roundCalls = 0;

    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "get_session":
          return Promise.resolve({
            id: handedOffSession,
            candidate_name: "Alice",
            started_at: "2026-01-01T00:00:00Z",
            completed_at: null,
            total_rounds: 5,
          });
        case "get_rounds":
          return Promise.resolve(
            Array.from({ length: 6 }, (_, roundIndex) =>
              mockStoredRound(handedOffSession, roundIndex)
            )
          );
        case "check_audio_devices":
          deviceCheckCalls += 1;
          return Promise.resolve({});
        case "run_interview_round":
          roundCalls += 1;
          return Promise.resolve({});
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(
        screen.getByText("This session has an inconsistent round history and cannot be resumed.")
      ).toBeInTheDocument();
    });
    expect(screen.queryByText("Check Devices")).not.toBeInTheDocument();
    expect(screen.queryByText("Start Interview")).not.toBeInTheDocument();
    expect(screen.queryByText("Retry")).not.toBeInTheDocument();
    expect(deviceCheckCalls).toBe(0);
    expect(roundCalls).toBe(0);

    window.history.replaceState({}, "", "/");
  });

  it("rejects an invalid-UUID session handoff before any round flow (P2-1)", async () => {
    // Not a UUID — must fail verification without any backend round call.
    window.history.replaceState({}, "", `/interview?session=not-a-uuid`);

    let roundCalls = 0;
    let getSessionCalls = 0;
    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "get_session":
          getSessionCalls += 1;
          return Promise.resolve(null);
        case "run_interview_round":
          roundCalls += 1;
          return Promise.resolve({
            metadata: {
              file_path: "/tmp/round.wav",
              sha256: "abc123",
              duration_ms: 5000,
              sample_rate: 16000,
              channels: 1,
              file_size_bytes: 100,
            },
            transcription: "My answer",
          });
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(
        screen.getByText(/not a valid UUID/)
      ).toBeInTheDocument();
    });
    expect(getSessionCalls).toBe(0);
    expect(roundCalls).toBe(0);
    expect(screen.queryByText("Check Devices")).not.toBeInTheDocument();

    window.history.replaceState({}, "", "/");
  });

  it("rejects a missing session handoff without a round retry (P2-1)", async () => {
    const staleSession = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    window.history.replaceState({}, "", `/interview?session=${staleSession}`);

    let roundCalls = 0;
    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "get_session":
          return Promise.resolve(null);
        case "run_interview_round":
          roundCalls += 1;
          return Promise.resolve({
            metadata: {
              file_path: "/tmp/round.wav",
              sha256: "abc123",
              duration_ms: 5000,
              sample_rate: 16000,
              channels: 1,
              file_size_bytes: 100,
            },
            transcription: "My answer",
          });
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(
        screen.getByText(/invalid or expired/)
      ).toBeInTheDocument();
    });
    expect(roundCalls).toBe(0);
    // A stale handoff is NOT turned into a round retry.
    expect(screen.queryByText("Retry")).not.toBeInTheDocument();

    window.history.replaceState({}, "", "/");
  });

  it("blocks rounds for a completed session handoff (P2-1)", async () => {
    const doneSession = "11111111-2222-3333-4444-555555555555";
    window.history.replaceState({}, "", `/interview?session=${doneSession}`);

    let roundCalls = 0;
    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "get_app_config":
          return Promise.resolve(mockAppConfig);
        case "get_session":
          return Promise.resolve({
            id: doneSession,
            candidate_name: "Alice",
            started_at: "2026-01-01T00:00:00Z",
            completed_at: "2026-01-01T00:10:00Z",
            total_rounds: 5,
          });
        case "run_interview_round":
          roundCalls += 1;
          return Promise.resolve({
            metadata: {
              file_path: "/tmp/round.wav",
              sha256: "abc123",
              duration_ms: 5000,
              sample_rate: 16000,
              channels: 1,
              file_size_bytes: 100,
            },
            transcription: "My answer",
          });
        default:
          return Promise.reject(new Error(`Unexpected command: ${cmd}`));
      }
    });

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(
        screen.getByText("Session Already Completed")
      ).toBeInTheDocument();
    });
    expect(roundCalls).toBe(0);
    expect(screen.queryByText("Start Interview")).not.toBeInTheDocument();

    window.history.replaceState({}, "", "/");
  });

  it("returns to ready on a stopped round error event (P2-3)", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });

    const callback = mockListenCallbacks.get("interview-phase");
    expect(callback).toBeDefined();

    // A stop during the round is a controlled cancellation, not an error.
    act(() => {
      callback!({
        payload: {
          phase: "error",
          question: "Interview stopped during TTS",
        },
      });
    });

    await waitFor(() => {
      expect(screen.getByText("Start Interview")).toBeInTheDocument();
    });
    expect(screen.queryByText("Error")).not.toBeInTheDocument();
  });
});
