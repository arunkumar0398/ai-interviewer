import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
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

  it("renders three navigation cards", () => {
    render(<Home />);
    expect(screen.getByText("Start Interview")).toBeInTheDocument();
    expect(screen.getByText("Candidate View")).toBeInTheDocument();
    expect(screen.getByText("Dashboard")).toBeInTheDocument();
  });

  it("links to /interview", () => {
    render(<Home />);
    const link = screen.getByText("Start Interview").closest("a");
    expect(link).toHaveAttribute("href", "/interview");
  });

  it("links to /candidate", () => {
    render(<Home />);
    const link = screen.getByText("Candidate View").closest("a");
    expect(link).toHaveAttribute("href", "/candidate");
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

  it("renders Start Recording button in idle state", () => {
    render(<Home />);
    const btn = screen.getByRole("button", { name: /Start Recording/i });
    expect(btn).toBeInTheDocument();
  });

  it("does not show Recording indicator in idle state", () => {
    render(<Home />);
    expect(screen.queryByText("Recording...")).not.toBeInTheDocument();
  });

  it("does not show error state initially", () => {
    render(<Home />);
    expect(screen.queryByText(/Error/)).not.toBeInTheDocument();
  });
});
