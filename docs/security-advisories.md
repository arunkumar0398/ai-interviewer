# npm Security Advisory Triage

**Date:** 2026-08-08
**Command:** `npm audit` and `npm audit --omit=dev`
**Result:** 3 high-severity advisories, all transitive via `next@16.2.12`.

Both vulnerable packages are build/dev-time only. They do **not** reach the
packaged runtime: the portable artifact ships the statically exported frontend
(`out/`), the Tauri executable, the audio tools, and licenses — `node_modules`
is never packaged and no Node server runs on the workstation.

| Package | Advisory | Severity | Reaches packaged runtime? | Why non-runtime | Intended follow-up |
|---------|----------|----------|---------------------------|-----------------|--------------------|
| `postcss` (<=8.5.22, via `next`) | GHSA-qx2v-qp2m-jg93 (XSS via unescaped `</style>` in CSS stringify), GHSA-6g55-p6wh-862q / GHSA-r28c-9q8g-f849 / GHSA-fxqj-rqcc-2cmp (arbitrary file read via sourceMappingURL) | high | No | PostCSS processes only the project's first-party CSS during `next build` (via `@tailwindcss/postcss`). Build input is not attacker-controlled, and the compiled CSS is baked into `out/`. | Track upstream; bump to `postcss >= 8.5.22` (or Next 16.3.0) when Next's dependency range allows without a forced major upgrade. |
| `sharp` (<0.35.0, via `next`) | GHSA-f88m-g3jw-g9cj (libvips CVEs CVE-2026-33327, CVE-2026-33328, CVE-2026-35590, CVE-2026-35591) | high | No | sharp is only invoked by `next/image` on a Node server. This app uses no `next/image` components and ships a fully static export (`out/`) served by the Tauri webview — no image-optimization server exists. | Track upstream; align with Next's sharp version when the pinned Next range is upgraded. |

**Why `npm audit fix --force` was not run:** it forces `next@16.3.0`, outside
the project's stated dependency range (`next: 16.2.12`). Per remediation policy,
unrelated major upgrades are not introduced solely to zero the raw audit count.

Re-run `npm audit` after any dependency upgrade to confirm the advisories are
gone before release.
