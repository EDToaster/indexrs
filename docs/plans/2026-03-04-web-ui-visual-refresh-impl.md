# Web UI Visual Refresh — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add micro-interactions, glassmorphism header, card depth, loading skeletons, VS Code-style code blocks, gradient accents, and smooth expand/collapse to the ferret web UI.

**Architecture:** All changes are pure CSS in `ferret-indexer-web/static/style.css` with minimal HTML tweaks in askama templates and a few lines of JS in `app.js`. No new dependencies.

**Tech Stack:** CSS custom properties, CSS keyframe animations, htmx swap classes, askama templates

---

### Task 1: Motion Foundation — CSS Variables & Keyframes

**Files:**
- Modify: `ferret-indexer-web/static/style.css:1-34` (`:root` block)
- Modify: `ferret-indexer-web/static/style.css:37-62` (`[data-theme="dark"]` block)

**Step 1: Add motion variables to `:root`**

Add these after `--transition: 150ms ...;` (line 33):

```css
    --duration-fast: 100ms;
    --duration-normal: 200ms;
    --duration-slow: 250ms;
    --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
```

**Step 2: Add reduced-motion media query**

Add after the `[data-theme="dark"]` block (after line 62):

```css
@media (prefers-reduced-motion: reduce) {
    :root, [data-theme="dark"] {
        --duration-fast: 0ms;
        --duration-normal: 0ms;
        --duration-slow: 0ms;
        --transition: 0ms;
    }
    *, *::before, *::after {
        animation-duration: 0ms !important;
        transition-duration: 0ms !important;
    }
}
```

**Step 3: Add fadeSlideIn keyframe**

Add after the reduced-motion block:

```css
@keyframes fadeSlideIn {
    from { opacity: 0; transform: translateY(6px); }
    to   { opacity: 1; transform: translateY(0); }
}
```

**Step 4: Add staggered animation to file results**

Modify `.file-result` (line 167-170) to add animation:

```css
.file-result {
    border-bottom: 1px solid var(--border);
    transition: background var(--transition), box-shadow var(--transition), transform var(--duration-fast) var(--ease-out);
    animation: fadeSlideIn var(--duration-normal) var(--ease-out) both;
}
```

Then add stagger delays via `:nth-child`:

```css
.file-result:nth-child(2)  { animation-delay: 30ms; }
.file-result:nth-child(3)  { animation-delay: 60ms; }
.file-result:nth-child(4)  { animation-delay: 90ms; }
.file-result:nth-child(5)  { animation-delay: 120ms; }
.file-result:nth-child(6)  { animation-delay: 150ms; }
.file-result:nth-child(7)  { animation-delay: 180ms; }
.file-result:nth-child(8)  { animation-delay: 210ms; }
.file-result:nth-child(9)  { animation-delay: 240ms; }
.file-result:nth-child(10) { animation-delay: 270ms; }
```

Also add stagger for `.symbol-result`:

```css
.symbol-result {
    animation: fadeSlideIn var(--duration-normal) var(--ease-out) both;
}
.symbol-result:nth-child(2)  { animation-delay: 20ms; }
.symbol-result:nth-child(3)  { animation-delay: 40ms; }
.symbol-result:nth-child(4)  { animation-delay: 60ms; }
.symbol-result:nth-child(5)  { animation-delay: 80ms; }
.symbol-result:nth-child(6)  { animation-delay: 100ms; }
.symbol-result:nth-child(7)  { animation-delay: 120ms; }
.symbol-result:nth-child(8)  { animation-delay: 140ms; }
.symbol-result:nth-child(9)  { animation-delay: 160ms; }
.symbol-result:nth-child(10) { animation-delay: 180ms; }
```

**Step 5: Run cargo check to verify no build breakage**

Run: `cargo check -p ferret-indexer-web`
Expected: compiles (CSS is embedded, no Rust changes)

**Step 6: Commit**

```bash
git add ferret-indexer-web/static/style.css
git commit -m "feat(web): add motion foundation — CSS variables, keyframes, staggered fade-in"
```

---

### Task 2: Glassmorphism Header

**Files:**
- Modify: `ferret-indexer-web/static/style.css:96-104` (`.header` rule)

**Step 1: Add glassmorphism properties to `.header`**

Replace the current `.header` block with:

```css
.header {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0 1.25rem;
    height: 42px;
    border-bottom: 1px solid rgba(0, 0, 0, 0.06);
    background: rgba(240, 239, 237, 0.8);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    position: sticky;
    top: 0;
    z-index: 50;
}
```

**Step 2: Add dark mode header override**

Add inside or after the `[data-theme="dark"]` section:

```css
[data-theme="dark"] .header {
    background: rgba(12, 10, 9, 0.8);
    border-bottom-color: rgba(255, 255, 255, 0.06);
}
```

**Step 3: Verify visually**

Run the web server and check both light/dark modes. Content should scroll behind the header with a frosted glass effect.

**Step 4: Commit**

```bash
git add ferret-indexer-web/static/style.css
git commit -m "feat(web): glassmorphism header with backdrop blur"
```

---

### Task 3: Card Depth & Hover States

**Files:**
- Modify: `ferret-indexer-web/static/style.css` — `.file-result`, `.file-header`, `.repo-card`, `.search-input`, `.mode-btn`, `.pagination button`

**Step 1: Add hover elevation to file result cards**

Update `.file-result` to include margin and card styling. Add after the existing `.file-result` rule:

```css
.file-result:hover {
    box-shadow: var(--shadow-sm);
}
```

**Step 2: Add hover elevation to file header**

The existing `.file-header:hover` (line 191-193) already changes background. Enhance it:

```css
.file-header:hover {
    background: var(--border);
    color: var(--accent);
}

.file-header:hover .path {
    text-decoration: underline;
    text-underline-offset: 2px;
}
```

**Step 3: Enhance repo card hover**

Update `.repo-card:hover` (line 635-639):

```css
.repo-card:hover {
    border-color: var(--border);
    border-left-color: var(--success);
    box-shadow: var(--shadow);
    transform: translateY(-1px);
}
```

Add transition for transform to `.repo-card` (line 626-633):

```css
.repo-card {
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    background: var(--bg-elevated);
    margin-bottom: 0.6rem;
    transition: border-color var(--transition), box-shadow var(--transition), transform var(--duration-fast) var(--ease-out);
    border-left: 3px solid var(--success);
}
```

**Step 4: Add inset shadow to search input**

Update `.search-input` (line 126-137) to add `box-shadow: var(--shadow-sm)`.

**Step 5: Add button press effect**

Update `.mode-btn` and `.pagination button`:

```css
.mode-btn:hover {
    color: var(--fg);
    border-color: var(--accent);
    transform: scale(1.03);
}

.mode-btn:active {
    transform: scale(0.97);
}

.pagination button:hover {
    background: var(--bg-secondary);
    border-color: var(--accent);
    color: var(--accent);
    transform: scale(1.03);
}

.pagination button:active {
    transform: scale(0.97);
}
```

**Step 6: Commit**

```bash
git add ferret-indexer-web/static/style.css
git commit -m "feat(web): card depth, hover elevation, and button press effects"
```

---

### Task 4: Loading Skeleton

**Files:**
- Modify: `ferret-indexer-web/static/style.css` (add skeleton styles)
- Modify: `ferret-indexer-web/templates/index.html` (add skeleton HTML)
- Modify: `ferret-indexer-web/static/app.js` (show/hide skeleton on htmx events)

**Step 1: Add shimmer keyframe and skeleton styles to CSS**

Add near the spinner styles (after line 527):

```css
/* Loading skeleton */
@keyframes shimmer {
    0%   { background-position: -200% 0; }
    100% { background-position: 200% 0; }
}

.skeleton {
    display: none;
}

.skeleton.active {
    display: block;
}

.skeleton-card {
    padding: 1.25rem;
    border-bottom: 1px solid var(--border);
}

.skeleton-header {
    height: 0.85rem;
    width: 55%;
    border-radius: 3px;
    background: linear-gradient(90deg, var(--bg-secondary) 25%, var(--bg-elevated) 50%, var(--bg-secondary) 75%);
    background-size: 200% 100%;
    animation: shimmer 1.5s ease infinite;
    margin-bottom: 0.65rem;
}

.skeleton-line {
    height: 0.7rem;
    border-radius: 3px;
    background: linear-gradient(90deg, var(--bg-secondary) 25%, var(--bg-elevated) 50%, var(--bg-secondary) 75%);
    background-size: 200% 100%;
    animation: shimmer 1.5s ease infinite;
    margin-bottom: 0.35rem;
}

.skeleton-line:nth-child(2) { width: 85%; animation-delay: 80ms; }
.skeleton-line:nth-child(3) { width: 70%; animation-delay: 160ms; }
.skeleton-line:nth-child(4) { width: 90%; animation-delay: 240ms; }
```

**Step 2: Add skeleton HTML to index.html**

In `ferret-indexer-web/templates/index.html`, inside the `.results-wrap` div, add before `<div id="results">`:

```html
        <div class="skeleton" id="search-skeleton">
            <div class="skeleton-card"><div class="skeleton-header"></div><div class="skeleton-line"></div><div class="skeleton-line"></div><div class="skeleton-line"></div></div>
            <div class="skeleton-card"><div class="skeleton-header"></div><div class="skeleton-line"></div><div class="skeleton-line"></div><div class="skeleton-line"></div></div>
            <div class="skeleton-card"><div class="skeleton-header"></div><div class="skeleton-line"></div><div class="skeleton-line"></div></div>
        </div>
```

**Step 3: Add JS to toggle skeleton on htmx request lifecycle**

In `app.js`, add these event listeners (before the closing `})();`):

```javascript
    // Show skeleton on search request start, hide on completion
    document.addEventListener("htmx:beforeRequest", function(e) {
        if (e.target && e.target.classList.contains("search-input")) {
            var skeleton = document.getElementById("search-skeleton");
            if (skeleton) skeleton.classList.add("active");
        }
    });

    document.addEventListener("htmx:afterSwap", function(e) {
        var skeleton = document.getElementById("search-skeleton");
        if (skeleton) skeleton.classList.remove("active");
    });
```

Note: there's already an `htmx:afterSwap` handler at line 211-216. Merge the skeleton hide into that existing handler instead of adding a duplicate.

**Step 4: Verify the build**

Run: `cargo check -p ferret-indexer-web`
Expected: compiles (template changes need askama to be happy)

**Step 5: Commit**

```bash
git add ferret-indexer-web/static/style.css ferret-indexer-web/templates/index.html ferret-indexer-web/static/app.js
git commit -m "feat(web): loading skeleton shimmer during search"
```

---

### Task 5: VS Code-style Code Blocks

**Files:**
- Modify: `ferret-indexer-web/static/style.css` — `.code-lines`, `.code-line--match`, `.code-line`, `.line-number`

**Step 1: Add rounded corners and inner shadow to code blocks**

Update `.code-lines` (line 220-225):

```css
.code-lines {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    overflow-x: auto;
    line-height: 1.45;
    border-radius: var(--radius);
    box-shadow: inset 0 1px 3px rgba(0, 0, 0, 0.04);
}

[data-theme="dark"] .code-lines {
    box-shadow: inset 0 1px 3px rgba(0, 0, 0, 0.2);
}
```

**Step 2: Add left accent bar to match lines**

Update `.code-line--match` (line 232-234):

```css
.code-line--match {
    background: var(--match-bg);
    border-left: 3px solid var(--match-border);
}
```

Adjust the line number padding for match lines to compensate for the 3px border:

```css
.code-line--match .line-number {
    color: var(--match-fg);
    border-right-color: var(--match-border);
}
```

**Step 3: Add line separator between line number and content**

The `.line-number` already has `border-right: 1px solid var(--border-subtle)`. Make it more distinct:

```css
.line-number {
    flex-shrink: 0;
    width: 4rem;
    padding: 0 0.6rem 0 0;
    text-align: right;
    color: var(--line-number-fg);
    background: var(--line-number-bg);
    user-select: none;
    border-right: 1px solid var(--border);
    font-variant-numeric: tabular-nums;
}
```

**Step 4: Add subtle line hover in file preview**

Add a new rule for file preview code lines:

```css
.file-preview-layout .code-line:hover {
    background: var(--selection-bg);
}
```

**Step 5: Commit**

```bash
git add ferret-indexer-web/static/style.css
git commit -m "feat(web): VS Code-style code blocks — rounded corners, inner shadow, match accent bar"
```

---

### Task 6: Gradient Accents

**Files:**
- Modify: `ferret-indexer-web/static/style.css` — `.mode-btn--active`, `.badge--ok`, `.pagination .current`

**Step 1: Gradient on active mode button**

Update `.mode-btn--active` (line 1082-1086):

```css
.mode-btn--active {
    background: linear-gradient(135deg, var(--accent), var(--accent-hover));
    color: #fff;
    border-color: var(--accent);
}

.mode-btn--active:hover {
    transform: none;
}
```

**Step 2: Pulse animation on status badge**

Add a pulse keyframe:

```css
@keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.7; }
}

.badge--ok {
    background: rgba(5, 150, 105, 0.1);
    color: var(--success);
    border: 1px solid rgba(5, 150, 105, 0.2);
    animation: pulse 2s ease-in-out infinite;
}
```

**Step 3: Gradient on pagination current page**

Update `.pagination .current` (line 312-318):

```css
.pagination .current {
    background: linear-gradient(135deg, var(--accent), var(--accent-hover));
    color: #fff;
    border-color: var(--accent);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
}
```

**Step 4: Commit**

```bash
git add ferret-indexer-web/static/style.css
git commit -m "feat(web): gradient accents on active states, status badge pulse"
```

---

### Task 7: Smooth Expand/Collapse & Modal Animations

**Files:**
- Modify: `ferret-indexer-web/static/style.css` — `.help-overlay`, `.help-content`, `.quickopen-overlay`, `.quickopen-modal`, `.segments-table-wrap`
- Modify: `ferret-indexer-web/static/app.js` — toggle help/quickopen with animation classes

**Step 1: Add modal entrance animations**

Add keyframes:

```css
@keyframes modalFadeIn {
    from { opacity: 0; }
    to   { opacity: 1; }
}

@keyframes modalScaleIn {
    from { opacity: 0; transform: scale(0.97) translateY(-4px); }
    to   { opacity: 1; transform: scale(1) translateY(0); }
}
```

**Step 2: Apply to help overlay**

Update `.help-overlay.visible` (line 436-438):

```css
.help-overlay.visible {
    display: flex;
    animation: modalFadeIn var(--duration-normal) var(--ease-out);
}

.help-overlay.visible .help-content {
    animation: modalScaleIn var(--duration-slow) var(--ease-out);
}
```

**Step 3: Apply to quick-open overlay**

Update `.quickopen-overlay.visible` (line 1316-1318):

```css
.quickopen-overlay.visible {
    display: flex;
    animation: modalFadeIn var(--duration-normal) var(--ease-out);
}

.quickopen-overlay.visible .quickopen-modal {
    animation: modalScaleIn var(--duration-slow) var(--ease-out);
}
```

**Step 4: Smooth expand/collapse for segment tables**

Update `.segments-table-wrap` (line 1016-1019). Since CSS `max-height` transitions need a fixed max value, use a grid-based approach:

```css
.segments-table-wrap {
    display: grid;
    grid-template-rows: 0fr;
    transition: grid-template-rows var(--duration-slow) var(--ease-out);
    overflow: hidden;
}

.segments-table-wrap.expanded {
    grid-template-rows: 1fr;
}

.segments-table-wrap > * {
    overflow: hidden;
}
```

**Step 5: Update app.js toggle logic**

Update the `data-toggle` click handler (line 229-234) to use the `.expanded` class instead of `display: none`:

```javascript
    document.addEventListener("click", function(e) {
        var toggle = e.target.closest("[data-toggle]");
        if (!toggle) return;
        var target = document.getElementById(toggle.getAttribute("data-toggle"));
        if (target) target.classList.toggle("expanded");
    });
```

**Step 6: Update repos.html template**

In the repos template, remove the inline `style="display:none"` from the segments table wrap and ensure it starts without the `expanded` class (which it will by default since we removed the display:none approach).

**Step 7: Commit**

```bash
git add ferret-indexer-web/static/style.css ferret-indexer-web/static/app.js ferret-indexer-web/templates/repos.html
git commit -m "feat(web): smooth modal animations and expand/collapse transitions"
```

---

### Task 8: Final Polish & Visual QA

**Files:**
- Modify: `ferret-indexer-web/static/style.css` (any tweaks)

**Step 1: Run clippy and format check**

Run: `cargo clippy --workspace -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: Both pass

**Step 2: Run full test suite**

Run: `cargo test --workspace`
Expected: All pass

**Step 3: Visual QA with Playwright**

Start the server and visually verify:
- Light mode: header blur, card hover, match bars, skeleton shimmer, modal animations
- Dark mode: all of the above
- `prefers-reduced-motion`: no animations visible

**Step 4: Final commit if any tweaks needed**

```bash
git add -A
git commit -m "fix(web): visual polish tweaks from QA"
```
