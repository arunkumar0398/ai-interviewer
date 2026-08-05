import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import DashboardPage from "../app/dashboard/page";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: any[]) => mockInvoke(...args),
}));

describe("Dashboard Page", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Default: initDatabase succeeds, get_sessions returns empty
    mockInvoke.mockResolvedValue("/fake/appdir"); // get_app_dir
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

  it("renders Add button for questions", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("+ Add")).toBeInTheDocument();
    });
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

  it("shows 5 default questions in the question bank", async () => {
    mockInvoke.mockResolvedValue([]);
    render(<DashboardPage />);

    await waitFor(() => {
      expect(screen.getByText("Question Bank")).toBeInTheDocument();
    });

    // Check default questions are present as input values
    const inputs = screen.getAllByPlaceholderText(/Question \d/);
    expect(inputs.length).toBeGreaterThanOrEqual(5);
  });
});
