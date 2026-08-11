import { describe, it, expect } from "vitest";
import { INTERVIEW_QUESTIONS } from "../lib/interview-questions";

// P1-2: one shared fixed question set. The Dashboard displays exactly what
// /interview asks, and the count stays aligned with the backend's
// EXPECTED_ROUNDS (5). No second hard-coded five-question list may exist.
describe("INTERVIEW_QUESTIONS shared source (P1-2)", () => {
  it("has exactly 5 questions, matching the backend EXPECTED_ROUNDS", () => {
    expect(INTERVIEW_QUESTIONS.length).toBe(5);
  });

  it("has no duplicate or blank entries", () => {
    const unique = new Set(INTERVIEW_QUESTIONS);
    expect(unique.size).toBe(5);
    for (const q of INTERVIEW_QUESTIONS) {
      expect(q.trim().length).toBeGreaterThan(0);
    }
  });
});
