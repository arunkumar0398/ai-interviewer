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
    expect(screen.getByText("Interview Session")).toBeInTheDocument();
  });

  it("shows waiting state by default", () => {
    render(<CandidatePage />);
    expect(screen.getByText("Waiting for the interviewer to begin...")).toBeInTheDocument();
  });

  it("shows round number starting at 1", () => {
    render(<CandidatePage />);
    expect(screen.getByText("Round 1")).toBeInTheDocument();
  });

  it("does not show recording indicator in waiting state", () => {
    render(<CandidatePage />);
    expect(screen.queryByText("Recording your answer...")).not.toBeInTheDocument();
  });

  it("does not show listening indicator in waiting state", () => {
    render(<CandidatePage />);
    expect(screen.queryByText("Listening to question...")).not.toBeInTheDocument();
  });

  it("does not show done state initially", () => {
    render(<CandidatePage />);
    expect(screen.queryByText("Answer recorded")).not.toBeInTheDocument();
  });

  it("does not show error initially", () => {
    render(<CandidatePage />);
    expect(screen.queryByText(/Error/)).not.toBeInTheDocument();
  });

  it("does not show Ready button in waiting state", () => {
    render(<CandidatePage />);
    expect(screen.queryByText("Ready for next question")).not.toBeInTheDocument();
  });
});
