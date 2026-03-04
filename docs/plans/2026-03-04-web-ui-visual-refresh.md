# Web UI Visual Refresh

**Date:** 2026-03-04
**Goal:** Polish the web interface with micro-interactions, refined transitions, glassmorphism, and VS Code-inspired code styling. Pure CSS, no new dependencies.

## Design Decisions

- **Vibe:** Developer tool polish (VS Code / GitHub aesthetic)
- **Focus:** Micro-interactions and transitions, with visual refresh layered on
- **Dependencies:** Pure CSS only. No new JS libraries.
- **Constraint:** All animations must feel snappy (100-250ms max). Respect `prefers-reduced-motion`.

## Section 1: Motion Foundation

Add CSS custom properties to `:root` and `[data-theme="dark"]`:

```css
--duration-fast: 100ms;
--duration-normal: 200ms;
--duration-slow: 250ms;
--ease-out: cubic-bezier(0.16, 1, 0.3, 1);
```

Add `@media (prefers-reduced-motion: reduce)` that sets all durations to `0ms`.

**Staggered result fade-in:** Each `.file-result` gets `animation: fadeSlideIn 200ms var(--ease-out) both` with `animation-delay` via `:nth-child()` (0ms, 30ms, 60ms, ... up to ~10 items).

```css
@keyframes fadeSlideIn {
  from { opacity: 0; transform: translateY(6px); }
  to   { opacity: 1; transform: translateY(0); }
}
```

**htmx swap transitions:** Add opacity transition on `#results` using htmx's `htmx-settling` class.

## Section 2: Glassmorphism Header

Replace solid header background with frosted glass:

- **Light:** `background: rgba(240, 239, 237, 0.8)`
- **Dark:** `background: rgba(12, 10, 9, 0.8)`
- `backdrop-filter: blur(12px); -webkit-backdrop-filter: blur(12px)`
- Border: `border-bottom: 1px solid rgba(0,0,0,0.06)` (light) / `rgba(255,255,255,0.06)` (dark)
- Keep `position: sticky; top: 0; z-index: 100`

## Section 3: Card Depth & Hover States

**File result cards:**
- Add `border-radius: var(--radius)`, small margins, `--shadow-sm` at rest
- Hover: elevate to `--shadow`, `translateY(-1px)`, brighter border
- Transition: `var(--duration-fast)`

**Repo cards:**
- Same lift-on-hover pattern, colored left border stays

**Search input:**
- Add `--shadow-sm` at rest for inset field appearance

**Buttons:**
- `.mode-btn` and pagination: `transform: scale(1.02)` on hover
- Active: `scale(0.98)` for press feel
- Transition: `var(--duration-fast)`

## Section 4: Loading Skeleton (CSS Shimmer)

Replace spinner with skeleton placeholder cards during loading:

```css
@keyframes shimmer {
  0%   { background-position: -200% 0; }
  100% { background-position: 200% 0; }
}

.skeleton-card {
  background: linear-gradient(90deg,
    var(--bg-secondary) 25%,
    var(--bg-elevated) 50%,
    var(--bg-secondary) 75%);
  background-size: 200% 100%;
  animation: shimmer 1.5s ease infinite;
  border-radius: var(--radius);
}
```

Show 3-4 skeleton cards while htmx request is in flight; hide when real results arrive.

## Section 5: VS Code-style Code Blocks

**Search result code snippets:**
- Rounded corners: `border-radius: var(--radius)`
- Inner shadow: `box-shadow: inset 0 1px 3px rgba(0,0,0,0.04)` (light) / `rgba(0,0,0,0.2)` (dark)
- Line number left column: thin separator border
- Match highlight: add 3px left accent bar (`border-left: 3px solid var(--match-border)`) for visual scanning

**File preview code:**
- Same inner shadow treatment
- Subtle line hover highlight (very light background change on mouseover)

## Section 6: Gradient Accents

Subtle gradient treatments:

- **Active mode button:** `linear-gradient(135deg, var(--accent), var(--accent-hover))` instead of flat color
- **Status badge pulse:** `@keyframes pulse` (opacity 1 → 0.7 → 1, 2s) on green "ok" badge
- **Pagination current page:** gradient background matching active mode button

## Section 7: Smooth Expand/Collapse

- **Segment detail table:** `max-height` transition with `overflow: hidden` for CSS-only expand/collapse
- **Help overlay:** fade + scale entrance (`transform: scale(0.97)` → `scale(1)`, `opacity: 0` → `1`)
- **Quick-open modal:** same entrance animation
- **Backdrop:** separate fade-in for blur backdrop

## Implementation Order

1. Motion foundation (CSS variables, keyframes, reduced-motion)
2. Glassmorphism header
3. Card depth & hover states
4. Loading skeleton
5. VS Code code blocks
6. Gradient accents
7. Smooth expand/collapse

Each section is independently testable. All changes are in `style.css` except skeleton HTML (templates) and the htmx settling class wiring.
