import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import DashboardPage from "../app/dashboard/page";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

describe("Dashboard Page", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Default: get_app_config succeeds (DB is initialized at startup)
    mockInvoke.mockResolvedValue({
      tool_dir: "/fake/tools",
      recordings_dir: "/fake/recordings",
      db_path: "/fake/interviewer.db",
      is_portable: false,
    });
  });

  it("renders the heading", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Recruiter Dashboard")).toBeInTheDocument();
    });
  });

  it("renders New Interview section", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("New Interview")).toBeInTheDocument();
    });
  });

  it("renders Question Bank section", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Question Bank")).toBeInTheDocument();
    });
  });

  it("renders Past Sessions section", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Past Sessions")).toBeInTheDocument();
    });
  });

  it("shows 'No sessions yet' when no sessions exist", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("No sessions yet")).toBeInTheDocument();
    });
  });

  it("renders candidate name input", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByPlaceholderText("Enter candidate name")).toBeInTheDocument();
    });
  });

  it("does not render question editing controls (read-only bank)", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Question Bank")).toBeInTheDocument();
    });
    expect(screen.queryByText("+ Add")).not.toBeInTheDocument();
    expect(screen.queryByPlaceholderText(/Question \d/)).not.toBeInTheDocument();
  });

  it("shows Start Interview button disabled initially", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      const btn = screen.getByText("Start Interview");
      expect(btn).toBeDisabled();
    });
  });

  it("shows Back to Home link", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      const link = screen.getByText(/Back to Home/);
      expect(link).toHaveAttribute("href", "/");
    });
  });

  it("shows session cards when sessions exist", async () => {
    mockInvoke.mockResolvedValue([
      {
        id: "session-1",
        candidate_name: "Alice",
        started_at: "2026-01-01T10:00:00",
        completed_at: "2026-01-01T10:30:00",
        total_rounds: 5,
      },
    ]);

    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Alice")).toBeInTheDocument();
      expect(screen.getByText("Completed")).toBeInTheDocument();
    });
  });

  it("shows 5 default questions read-only in the question bank", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Question Bank")).toBeInTheDocument();
    });

    // Default questions are listed read-only.
    for (const q of [
      "Tell me about yourself and your background.",
      "What is your experience with Rust or systems programming?",
      "Describe a challenging technical problem you solved recently.",
      "How do you approach debugging complex issues?",
      "What interests you about this role?",
    ]) {
      expect(screen.getByText(q)).toBeInTheDocument();
    }
  });

  it("hands the created session id and candidate name to /interview", async () => {
    mockInvoke.mockImplementation((cmd: string) => {
      if (cmd === "get_app_config") {
        return Promise.resolve({
          tool_dir: "/fake/tools",
          recordings_dir: "/fake/recordings",
          db_path: "/fake/interviewer.db",
          is_portable: false,
        });
      }
      if (cmd === "get_sessions") {
        return Promise.resolve([]);
      }
      if (cmd === "create_session") {
        return Promise.resolve();
      }
      return Promise.reject(new Error(`Unexpected command: ${cmd}`));
    });

    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("New Interview")).toBeInTheDocument();
    });

    fireEvent.change(screen.getByPlaceholderText("Enter candidate name"), {
      target: { value: "Alice" },
    });
    fireEvent.click(screen.getByText("Start Interview"));

    // Session created, then the handoff link carries id + name.
    await waitFor(() => {
      const link = screen.getByText("/interview").closest("a");
      expect(link).toHaveAttribute(
        "href",
        expect.stringMatching(/^\/interview\?session=[0-9a-f-]+&name=Alice$/)
      );
    });
    const createCall = mockInvoke.mock.calls.find(([cmd]) => cmd === "create_session");
    expect(createCall).toBeDefined();
    const args = createCall![1] as { sessionId: string; candidateName: string };
    expect(args.candidateName).toBe("Alice");
    const link = screen.getByText("/interview").closest("a");
    expect(link!.getAttribute("href")).toContain(`session=${args.sessionId}`);
  });
});
