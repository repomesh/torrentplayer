# 0003: Frontend framework

- **Status:** Accepted
- **Date:** 2026-10-17

## Context

The UI has several screens (Library, Course, Player, Discover, Downloads, Settings), live
statistics updated about once per second, and a player overlay that must stay smooth while video
plays underneath. It runs in a system webview, so bundle size and runtime overhead matter, and
the strict CSP forbids inline scripts and `eval`.

## Decision

**Svelte 5 with TypeScript (strict), built by Vite.** No meta-framework (no SvelteKit): Tauri
serves a single-page app from bundled assets, so routing and server rendering are unnecessary.

Tooling: ESLint (`typescript-eslint` strict type-checked rules and `eslint-plugin-svelte`),
Prettier, `svelte-check`, and Vitest.

## Alternatives considered

- **React.** The largest ecosystem, but a bigger runtime, and frequent once-per-second updates
  need more care (memoization) to avoid re-rendering the overlay.
- **SolidJS.** Similar fine-grained reactivity and performance, but a smaller ecosystem and fewer
  contributors familiar with it.
- **Vanilla TypeScript.** Smallest footprint, but the number of screens and the amount of live
  state would mean building our own component and reactivity layer.

## Consequences

- Svelte's compiled, fine-grained updates suit live stats and smooth number transitions.
- Svelte escapes text by default; `{@html}` is banned in review to keep the CSP meaningful.
- TypeScript is pinned to 6.0 because `typescript-eslint` and `svelte-check` do not support
  TypeScript 7 yet.
