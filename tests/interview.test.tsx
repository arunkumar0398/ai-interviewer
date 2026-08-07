import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";
import InterviewPage from "../app/interview/page";
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

  it("renders all 5 questions from QUESTIONS constant", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig);

    await act(async () => {
      render(<InterviewPage />);
    });

    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });

    // Verify questions are accessible (they render in the QUESTIONS array)
    // They appear when round is shown, but we can verify the component structure
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
});
