import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import Home from "../app/page";

// Mock next/link since it uses router internally
vi.mock("next/link", () => ({
  default: ({ children, href, ...props }: { children: React.ReactNode; href: string; [key: string]: unknown }) => (
    <a href={href} {...props}>
      {children}
    </a>
  ),
}));

// Mock Tauri invoke
const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

describe("Home Page", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders the main heading", () => {
    render(<Home />);
    expect(screen.getByText("AI Interviewer")).toBeInTheDocument();
  });

  it("renders the description paragraph", () => {
    render(<Home />);
    expect(
      screen.getByText(/Automated interview platform/)
    ).toBeInTheDocument();
  });

  it("renders the navigation cards (Candidate View is not advertised)", () => {
    render(<Home />);
    expect(screen.getByText("Start Interview")).toBeInTheDocument();
    expect(screen.getByText("Dashboard")).toBeInTheDocument();
    // P2-2: the Candidate View is a static placeholder without backend
    // synchronization — it must NOT be advertised as a functional window.
    expect(screen.queryByText("Candidate View")).not.toBeInTheDocument();
    expect(screen.queryByText("Restricted window for the interviewee")).not.toBeInTheDocument();
  });

  it("links to /interview", () => {
    render(<Home />);
    const link = screen.getByText("Start Interview").closest("a");
    expect(link).toHaveAttribute("href", "/interview");
  });

  it("links to /dashboard", () => {
    render(<Home />);
    const link = screen.getByText("Dashboard").closest("a");
    expect(link).toHaveAttribute("href", "/dashboard");
  });

  it("renders the Audio Test section", () => {
    render(<Home />);
    expect(screen.getByText("Audio Test")).toBeInTheDocument();
  });

  it("renders Run Audio Test button in idle state", () => {
    render(<Home />);
    const btn = screen.getByRole("button", { name: /Run Audio Test/i });
    expect(btn).toBeInTheDocument();
  });

  it("does not show testing indicator in idle state", () => {
    render(<Home />);
    expect(screen.queryByText(/Testing microphone/)).not.toBeInTheDocument();
  });

  it("runs a bounded temporary audio test — no persistent recording path is shown", async () => {
    mockInvoke.mockResolvedValue({ duration_ms: 3200, file_size_bytes: 102400 });

    render(<Home />);
    const btn = screen.getByRole("button", { name: /Run Audio Test/i });
    fireEvent.click(btn);

    // The bounded test result is shown (no saved-file path, no orphan record).
    expect(await screen.findByText(/Captured 3\.2s of audio/)).toBeInTheDocument();
    expect(mockInvoke).toHaveBeenCalledWith("run_audio_test");
    expect(mockInvoke).not.toHaveBeenCalledWith("start_recording");
  });

  it("shows an error when the audio test fails", async () => {
    mockInvoke.mockRejectedValue(new Error("No input device found"));

    render(<Home />);
    const btn = screen.getByRole("button", { name: /Run Audio Test/i });
    fireEvent.click(btn);

    expect(await screen.findByText(/No input device found/)).toBeInTheDocument();
  });

  it("does not show error state initially", () => {
    render(<Home />);
    expect(screen.queryByText(/Error/)).not.toBeInTheDocument();
  });
});
