/**
 * Single source of truth for the interview's fixed question set (P1-2).
 *
 * The Recruiter Dashboard displays exactly what /interview asks — there must
 * never be two divergent five-question lists. The count stays aligned with
 * the backend's `EXPECTED_ROUNDS` (5), and finality is backend-derived.
 *
 * Question customization is intentionally NOT wired up yet: it arrives with a
 * later template/session-snapshot feature. Nothing here is editable at
 * runtime in this version.
 */
export const INTERVIEW_QUESTIONS = [
  "Tell me about yourself and your background.",
  "What are your strengths and weaknesses?",
  "Why are you interested in this position?",
  "Describe a challenging project you worked on.",
  "Where do you see yourself in five years?",
] as const;
