import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
import CandidatePage from "../app/candidate/page";

const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

describe("Candidate Page", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders the heading", () => {
    render(<CandidatePage />);
    expect(screen.getByText("Candidate View")).toBeInTheDocument();
  });

  it("honestly labels the window as not available yet (P2-2)", () => {
    render(<CandidatePage />);
    expect(
      screen.getByText(/This window is not available yet/)
    ).toBeInTheDocument();
    expect(screen.getByText(/planned for a later release/)).toBeInTheDocument();
  });

  it("does not fake functional interview states", () => {
    render(<CandidatePage />);
    expect(screen.queryByText("Waiting for the interviewer to begin...")).not.toBeInTheDocument();
    expect(screen.queryByText("Recording your answer...")).not.toBeInTheDocument();
    expect(screen.queryByText("Listening to question...")).not.toBeInTheDocument();
    expect(screen.queryByText("Round 1")).not.toBeInTheDocument();
    // No Tauri invoke calls either — the placeholder is purely static.
    expect(mockInvoke).not.toHaveBeenCalled();
  });
});
