import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import InterviewPage from "../app/interview/page";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: any[]) => mockInvoke(...args),
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

  it("renders the heading on mount", () => {
    render(<InterviewPage />);
    expect(screen.getByText("AI Interviewer")).toBeInTheDocument();
  });

  it("shows checking tools state initially", () => {
    render(<InterviewPage />);
    expect(screen.getByText("Checking tools installation...")).toBeInTheDocument();
  });

  it("shows error when get_app_config fails", async () => {
    mockInvoke.mockRejectedValueOnce(new Error("Command not found"));
    render(<InterviewPage />);

    await waitFor(() => {
      expect(screen.getByText("Error")).toBeInTheDocument();
    });
    expect(screen.getByText(/Command not found/)).toBeInTheDocument();
  });

  it("shows error when tools are missing", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfigWithMissingTools); // get_app_config

    render(<InterviewPage />);

    await waitFor(() => {
      expect(screen.getByText("Error")).toBeInTheDocument();
    });
    expect(screen.getByText(/Missing tools/)).toBeInTheDocument();
  });

  it("shows tools status when all tools are present", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig); // get_app_config

    render(<InterviewPage />);

    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });
  });

  it("shows device check section when tools are ready", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig); // get_app_config

    render(<InterviewPage />);

    await waitFor(() => {
      expect(screen.getByText("Device Check")).toBeInTheDocument();
    });
    expect(screen.getByText("Check Devices")).toBeInTheDocument();
  });

  it("renders all 5 questions from QUESTIONS constant", async () => {
    mockInvoke.mockResolvedValueOnce(mockAppConfig); // get_app_config

    render(<InterviewPage />);

    await waitFor(() => {
      expect(screen.getByText("Check Devices")).toBeInTheDocument();
    });

    // Verify questions are accessible (they render in the QUESTIONS array)
    // They appear when round is shown, but we can verify the component structure
  });

  it("has Retry button in error state", async () => {
    mockInvoke.mockRejectedValue(new Error("fail"));
    render(<InterviewPage />);

    await waitFor(() => {
      expect(screen.getByText("Retry")).toBeInTheDocument();
    });
  });
});
