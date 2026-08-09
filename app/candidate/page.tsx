"use client";

// Candidate View is intentionally NOT advertised as functional (P2-2): a real
// restricted candidate window requires backend session synchronization and
// live interview-phase events, which belong to later work. Until then this
// route is honestly labeled as unavailable instead of faking a waiting/
// listening/recording state that has no backend behind it.
export default function CandidatePage() {
  return (
    <div className="flex flex-col min-h-screen bg-white dark:bg-zinc-950 font-sans">
      <main className="flex flex-1 flex-col items-center justify-center gap-6 px-6 text-center">
        <h1 className="text-2xl font-semibold text-black dark:text-zinc-50">
          Candidate View
        </h1>
        <p className="text-zinc-600 dark:text-zinc-400 max-w-md">
          This window is not available yet. The restricted candidate view with
          live interview synchronization is planned for a later release — run
          the interview from the main window for now.
        </p>
      </main>
    </div>
  );
}
