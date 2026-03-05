# ferret: Risks, Tradeoffs, and Devil's Advocate Analysis

This document challenges the assumptions behind ferret - a local code indexing
service written in Rust with MCP server, web server, and fzf-compatible CLI
interfaces. The goal is to identify real risks early, prevent scope creep, and
ensure the project delivers genuine value over existing tools.

---

## 1. "Why Not Just Use X?"

### ripgrep + fzf

**The case against ferret:** ripgrep searches the Linux kernel (~1.1GB of
source) in under 100ms. For most repos (< 500MB of source), ripgrep + fzf
already provides sub-second interactive search with zero setup, zero daemon,
zero disk overhead, and zero stale-index risk. The combination is already
installed on most developer machines and requires no background process. For
symbol search, add universal-ctags and you get `ctags -R && fzf < tags`.

**What ferret would offer:** ripgrep rescans the entire file tree on every
query. For truly large monorepos (5GB+), queries that match rare patterns still
pay the full scan cost. An index inverts this: rare queries become fastest.
ferret could also provide structured results (symbol-aware search, language
filtering, file-type facets) that raw ripgrep+fzf cannot. The MCP interface
enables AI agents to search code without shelling out to ripgrep and parsing
unstructured output.

**Verdict:** For repos under ~1GB, ripgrep + fzf is genuinely hard to beat.
ferret only justifies itself for large repos, multi-repo search, or structured
query needs (symbols, language filtering). If the target user has a single
medium-sized repo, this project may be solving a problem they don't have.

### zoekt (Sourcegraph's fork)

**The case against ferret:** zoekt is a production-grade trigram-based code
search engine maintained by Sourcegraph, battle-tested on thousands of
repositories at scale. It has 1.4k+ stars, 105+ contributors, 1,881 commits,
and powers real code search infrastructure. It supports regex, boolean queries,
symbol search (with universal-ctags), file filtering, and has a web UI. It
already works.

**What ferret would offer:** zoekt is designed for server-side deployment
searching many repos, not as a lightweight local tool. It has no MCP interface,
no fzf-compatible CLI output, and running it locally means managing a Go service
that was designed for a different use case. zoekt's index files are optimized
for server workloads, not for a single developer's laptop. ferret could be
purpose-built for the local developer workflow.

**Verdict:** zoekt is the strongest competitor. If the goal is "just code
search," wrapping zoekt with an MCP adapter would be faster to ship. ferret
must differentiate on developer experience, local-first design, and interface
diversity (MCP + fzf).

### Sourcegraph

**The case against ferret:** Sourcegraph is the most feature-complete code
search platform. It supports regex, boolean operators, commit search, language
filtering, repository patterns, and more. It has VS Code and JetBrains
extensions. Sourcegraph also has Cody (their AI assistant) which likely has or
will have MCP capabilities for code search.

**What ferret would offer:** Sourcegraph requires an Enterprise plan for full
code search features. It is a heavy platform designed for organizations, not a
lightweight local tool. Self-hosting requires significant infrastructure.
Sourcegraph does not have a local single-binary mode suitable for individual
developers. ferret would be zero-config, zero-infrastructure, local-only.

**Verdict:** Different target audience. Sourcegraph is for teams with
infrastructure budgets. ferret is for individual developers who want local,
private, fast code search.

### ctags + fzf (symbol search)

**The case against ferret:** universal-ctags supports 100+ languages, generates
tag files that fzf can consume directly, and the combination has been solving
symbol navigation for decades. Editors (vim, emacs, VS Code) already consume
ctags output natively.

**What ferret would offer:** ctags only does symbol extraction, not full-text
search. The tag file must be manually regenerated (or hooked into file watchers
separately). ferret would unify full-text search and symbol search in one index
with automatic incremental updates. tree-sitter-based parsing would provide
more accurate symbol extraction than ctags' regex-based approach for supported
languages.

**Verdict:** ctags solves symbol search well but doesn't solve full-text code
search. The combination of ctags + ripgrep + fzf + manual glue scripts is what
ferret aims to replace with a single integrated tool.

### AI assistants (GitHub Copilot, Claude Code)

**The case against ferret:** Claude Code already has Grep and Glob tools.
Copilot has `@workspace` search. These AI assistants can search code, understand
context, and answer questions about codebases. They get better every month.

**What ferret would offer:** AI assistants' code search is a means to an end
(answering questions), not a standalone search tool. They are slow for
interactive browsing, cannot replace the tight feedback loop of fzf-style
interactive search, and their search is often imprecise. ferret via MCP would
actually make AI assistants better at code search by giving them a fast,
accurate index to query instead of slow filesystem scans.

**Verdict:** Not competitors - complementary. ferret as an MCP server would
improve AI assistant code search capabilities.

---

## 2. Scope Risks

### Three interfaces is a lot of surface area

Building an MCP server, a web server, and a fzf-compatible CLI means:
- Three different serialization formats (MCP JSON-RPC, HTTP/JSON/HTML, newline-delimited text)
- Three different error handling strategies
- Three sets of integration tests
- Three documentation surfaces
- Three potential security attack surfaces

For a solo project, maintaining three interfaces risks spreading effort thin
and delivering three mediocre interfaces instead of one excellent one.

**Risk level: HIGH.** Three interfaces tripling the surface area is the single
biggest scope risk.

### "Mimic GitHub code search" is enormous

GitHub code search supports:
- Exact string, regex, and boolean queries
- `language:`, `path:`, `repo:`, `org:`, `symbol:` qualifiers
- `NOT`, `OR` operators
- Regex with RE2 syntax
- Code navigation (go-to-definition, find-references)
- Result ranking by relevance

Full parity with GitHub code search is likely years of work. Partial parity
risks being worse than ripgrep for simple searches while not being good enough
for complex ones.

### Essential vs nice-to-have

| Feature | Priority | Justification |
|---|---|---|
| Substring/regex text search | Essential | Core value proposition |
| Incremental index updates | Essential | Without this, just use ripgrep |
| File path filtering | Essential | Basic usability |
| Language filtering | Essential | Straightforward with file extensions |
| Symbol search | Nice-to-have | Requires tree-sitter, large scope |
| Boolean operators (AND/OR/NOT) | Nice-to-have | Can be added incrementally |
| MCP server | Essential | Primary differentiator |
| fzf CLI | Essential | Developer UX, fast to build |
| Web server | Nice-to-have | Largest surface area, lowest priority |
| Result ranking | Nice-to-have | Simple ordering is fine for v0.1 |
| Multi-repo search | Nice-to-have | Single repo first |
| Code navigation | Cut | This is an IDE/LSP feature |

### Proposed realistic MVP scope

The MVP should be: **a single-repo trigram index with regex search, exposed via
MCP and fzf CLI.** No web server, no symbol search, no multi-repo, no code
navigation. See section 7 for the full MVP recommendation.

---

## 3. Technical Risks

### File watcher reliability

**Linux (inotify):**
- Default inotify watch limit is 8,192 per user (`/proc/sys/fs/inotify/max_user_watches`). A large repo with 50,000 directories would exceed this. Users would need to manually increase the limit (`sysctl fs.inotify.max_user_watches=524288`).
- Each inotify watch consumes ~1KB of kernel memory. 100,000 watches = ~100MB of kernel memory.
- inotify does not work across filesystem boundaries (NFS, FUSE, Docker volumes).
- Recursive watching requires one watch per directory - the notify crate handles this but it is slow to set up on large trees.

**macOS (FSEvents):**
- FSEvents operates at the directory level, not file level. It reports "something changed in this directory" and the application must rescan to find what changed. This introduces latency and potential races.
- FSEvents has a known coalescing behavior where rapid changes may be merged or delayed.
- FSEvents does not report which specific file changed in all cases, requiring directory rescans.
- The `notify` crate (v2.1+, 3.3k stars) handles FSEvents but its debouncing layer adds complexity.

**General file watcher issues:**
- Editors that use atomic saves (write to temp file, rename) may produce unexpected event sequences.
- Git operations (checkout, rebase, merge) can produce thousands of events in milliseconds.
- The watcher process must handle its own restart gracefully after crash/sleep.

**Mitigation:** Use `notify` with the debouncer. Implement a "full rescan" fallback triggered on error or after system sleep/wake. Document inotify limit requirements. Consider polling as a fallback mode.

### Index corruption during partial updates

If the indexer crashes mid-update, the index could be left in an inconsistent
state where:
- A file's old content is partially removed but new content not yet added
- The trigram posting lists reference deleted documents
- The file metadata disagrees with the trigram index

**Mitigation:** Use write-ahead logging or atomic file replacement for index
updates. Never modify the index in place - build a new segment and swap
atomically. Consider an append-only segment architecture (like Lucene) where
old segments are immutable and new data goes to new segments.

### Memory and disk usage for large repos

Trigram indexes are typically 2-5x the size of the source text (based on
livegrep's reported 3-5x ratio). For reference:

| Repo | Source size | Estimated index |
|---|---|---|
| Small project | 10 MB | 30-50 MB |
| Medium project | 100 MB | 300-500 MB |
| Large monorepo | 1 GB | 3-5 GB |
| Very large monorepo | 10 GB | 30-50 GB |

A 5 GB index on a developer laptop with a 256 GB SSD is 2% of total disk. A
50 GB index is 20% - this becomes a real problem. Memory-mapped access helps
with RAM but the disk footprint is the harder constraint.

**Mitigation:** Support configurable indexing scope (specific directories,
exclude patterns). Implement index compression (posting list compression can
reduce trigram index size by 40-60%). Provide `ferret status` showing disk
usage. Set hard limits with user-configurable caps.

### Stale index issues

The index can become stale when:
- The file watcher misses events (system sleep, overflow, crash)
- Files are modified on a network filesystem not visible to local watchers
- Git operations happen in rapid succession
- The watcher daemon is not running

A stale index is worse than no index because it returns confident-looking
wrong results. A developer trusting that a function "doesn't exist" because
the index says so, when really the index is stale, leads to real bugs.

**Mitigation:** Store file checksums/mtimes in the index. On query, optionally
spot-check a sample of results against the filesystem. Provide clear staleness
indicators in results. Support `ferret reindex` for manual full rebuild.
Consider showing "index last updated: 5 minutes ago" in results.

### Race conditions: query during reindex

If a query runs while the index is being updated:
- A document might be half-removed, causing phantom results
- A new file might be partially indexed, causing missed results
- Posting list iteration might see inconsistent state

**Mitigation:** Use reader-writer locking or MVCC (multi-version concurrency
control). Queries should always see a consistent snapshot. The simplest approach
is a read-write lock where writes block queries momentarily, but for large
updates this could cause visible latency. A segment-based architecture naturally
provides MVCC.

### Binary file handling and encoding detection

- Binary files (images, compiled objects, archives) must be detected and skipped
- Source files may be in various encodings (UTF-8, UTF-16, Latin-1, Shift-JIS)
- Files without BOM or explicit encoding markers require heuristic detection
- Indexing a 500 MB binary blob wastes disk and produces garbage results

**Mitigation:** Use the same heuristics as ripgrep (check for null bytes in
the first 8KB). Default to skipping binary files. Support `.ferretignore` for
explicit exclusions. For encoding, default to UTF-8 and fall back gracefully.

### Symlinks, submodules, sparse checkouts

- Symlinks can create cycles in directory traversal, causing infinite recursion
- Git submodules are separate repos that may or may not be initialized
- Sparse checkouts mean many files in the Git tree don't exist on disk
- Symlinks pointing outside the repo could expose unintended files to the index

**Mitigation:** Follow symlinks only one level deep by default, with
cycle detection. Skip uninitialized submodules. Only index files actually
present on disk. Respect `.gitignore` boundaries.

### .gitignore parsing edge cases

`.gitignore` has surprisingly complex semantics:
- Negation patterns (`!important.log`)
- Directory-only patterns (`build/`)
- Anchored vs unanchored patterns
- Nested `.gitignore` files in subdirectories
- Global gitignore (`~/.config/git/ignore`)
- `.git/info/exclude`

**Mitigation:** Use the `ignore` crate (from the ripgrep ecosystem) which
handles all of these correctly. Do not attempt to implement gitignore parsing
from scratch.

---

## 4. Ecosystem Risks (Rust-specific)

### MCP SDK maturity

The Rust MCP SDK (`rmcp`) is at version 0.16.0 (February 2026) with 389
commits and 143 contributors. It supports core MCP capabilities including
resources, prompts, tools, and notifications with tokio async runtime.

**Risk assessment: MODERATE.** The SDK is pre-1.0, meaning breaking API changes
are expected. However, it appears actively maintained and reasonably featured.
The MCP protocol itself is still evolving, so any SDK will be a moving target.

**Mitigation:** Pin the rmcp dependency version. Abstract the MCP layer behind
a trait so the transport can be replaced. Keep the MCP surface area minimal
(expose search as a tool, results as resources).

### tree-sitter Rust bindings stability

tree-sitter's Rust crate is at version 0.24 (also pre-1.0). However,
tree-sitter is used in production by editors like Zed, Helix, and Neovim, so
the Rust bindings are battle-tested despite the version number.

**Risk assessment: LOW-MODERATE.** The bindings are stable in practice.
The bigger risk is the per-language grammar crates - each language requires
a separate crate (tree-sitter-rust, tree-sitter-python, etc.), and these
vary in quality and maintenance. Supporting N languages means N grammar
dependencies.

**Mitigation:** Defer symbol search to post-MVP. When implemented, start with
3-5 well-maintained grammars (Rust, Python, TypeScript, Go, C). Load grammars
dynamically to avoid compile-time explosion.

### Compile times as dependencies grow

Rust compile times are a real concern. A typical dependency set for ferret:

| Dependency | Estimated compile contribution |
|---|---|
| tokio (full) | ~30s |
| axum (web server) | ~15s |
| rmcp (MCP SDK) | ~10s |
| tree-sitter + grammars | ~20s per grammar |
| serde + serde_json | ~10s |
| notify | ~5s |

A clean build could easily exceed 2-3 minutes. With 5 tree-sitter grammars,
add another 90 seconds. This is development friction, not a user-facing
problem, but it affects iteration speed.

**Mitigation:** Use cargo workspaces to isolate slow-building components.
Enable incremental compilation (default). Consider making the web server and
tree-sitter grammars optional features. Use `cargo-chef` or `sccache` in CI.

### Cross-platform file watching reliability

The `notify` crate is the standard but each platform backend has different
behavior:
- Linux inotify: event-level, but watch limits
- macOS FSEvents: directory-level, coalesced events
- Windows ReadDirectoryChangesW: different buffer overflow behavior
- BSD kqueue: per-file watches, expensive for large trees

Testing across all platforms is essential but difficult for a solo developer.

**Mitigation:** Target macOS and Linux first (the primary developer
populations). Use the notify crate's debouncer to normalize platform
differences. Have CI test on both platforms.

---

## 5. Operational Risks

### Daemon management

ferret as a service needs answers to:
- How does it start? (manual? login item? systemd/launchd?)
- How does it stop? (SIGTERM? graceful shutdown with index flush?)
- How does it restart after crash? (supervision? auto-restart?)
- How does it survive system sleep/wake? (re-establish watches?)
- How does it handle multiple instances? (pid file? socket lock?)

Getting daemon management right is a project in itself. Getting it wrong means
users have zombie ferret processes, orphaned lock files, or the service
silently not running when they think it is.

**Mitigation:** For MVP, do not run as a daemon. Start ferret on-demand: when
a query comes in (via CLI or MCP), start the indexer if not running, build/load
the index, and serve the query. Use a Unix domain socket for IPC. Only add
persistent daemon mode if on-demand startup is too slow (it might be fine for
repos under 1GB where indexing takes seconds).

### Port conflicts for web server

If the web server binds to a fixed port (e.g., 8080), it will conflict with
other development tools. If it uses a random port, clients need a discovery
mechanism.

**Mitigation:** For MVP, skip the web server entirely. For later versions, use
a Unix domain socket (no port conflicts) or write the chosen port to a
well-known file (`~/.ferret_index/port`).

### CPU and battery impact of file watching

Continuous file watching and re-indexing on every save can cause:
- Measurable CPU usage during rapid edit cycles
- Increased battery drain on laptops
- SSD write amplification from frequent index updates

A developer running `cargo watch` (which triggers rebuilds) alongside ferret
(which triggers reindexing) alongside their editor's LSP (which re-analyzes)
means three separate processes all responding to every file save.

**Mitigation:** Debounce aggressively (500ms-1s minimum). Only reindex changed
files, not the whole repo. Implement a "low power" mode that pauses watching
when on battery. Rate-limit reindexing (at most once per N seconds).

### Disk usage growth over time

If ferret maintains index segments over time without compaction, disk usage
will grow. Old index data for deleted files could accumulate.

**Mitigation:** Implement segment merging/compaction. Periodically rebuild the
index from scratch (e.g., weekly). Show disk usage in `ferret status`.

### Security: web server exposing code on a port

A web server serving source code on localhost is a potential security concern:
- Any process on the machine can access it
- If accidentally bound to 0.0.0.0, it's network-accessible
- Browser-based attacks (CSRF, DNS rebinding) could exfiltrate code
- In shared/multi-user systems, other users could access the code

**Mitigation:** Bind to 127.0.0.1 only (never 0.0.0.0). Use Unix domain
sockets instead of TCP where possible. For the web server, implement CORS
restrictions and consider authentication. For MVP, skip the web server.

---

## 6. Recommended Mitigations Summary

| Risk | Severity | Mitigation |
|---|---|---|
| Scope creep (3 interfaces) | HIGH | MVP with only MCP + CLI. Web server is post-v1. |
| File watcher reliability | HIGH | Use `notify` with debouncer. Full-rescan fallback. Document inotify limits. |
| Index corruption | HIGH | Segment-based architecture with atomic swaps. WAL for crash recovery. |
| Stale index | MEDIUM | Mtime verification. Staleness indicators. Manual reindex command. |
| Race conditions | MEDIUM | MVCC via immutable segments. Read-write lock as simpler alternative. |
| MCP SDK churn | MEDIUM | Pin version. Abstract behind trait. Minimal surface area. |
| Disk usage | MEDIUM | Posting list compression. Configurable scope. Usage reporting. |
| Memory usage | MEDIUM | Memory-mapped index files. Streaming query execution. |
| Daemon management | MEDIUM | On-demand startup for MVP. No persistent daemon initially. |
| CPU/battery impact | MEDIUM | Aggressive debouncing. Rate-limited reindexing. Low-power mode. |
| Binary file handling | LOW | Null-byte detection (ripgrep heuristic). `.ferretignore` support. |
| .gitignore parsing | LOW | Use the `ignore` crate. Do not reimplement. |
| Compile times | LOW | Cargo workspaces. Optional features for heavy deps. |
| Web server security | LOW | Defer web server. When built, bind localhost only. |
| tree-sitter stability | LOW | Defer symbol search. Start with few grammars. |

---

## 7. Honest MVP Recommendation

### What v0.1 should actually look like

**Cut ruthlessly.** The MVP should prove one thing: that a persistent trigram
index provides meaningfully faster code search than ripgrep for the user's
actual repo.

#### v0.1 scope: "Fast indexed search with MCP + CLI"

**Include:**
1. **Trigram index** for a single local repository
2. **Regex search** using the trigram index for candidate filtering, then
   regex verification (standard trigram search approach)
3. **Incremental updates** via file watcher (notify crate with debouncer)
4. **Path filtering** (glob patterns on file paths)
5. **Language filtering** (based on file extension, not parsing)
6. **fzf-compatible CLI** output (simple newline-delimited format, works with
   `ferret search "pattern" | fzf`)
7. **MCP server** with a single `search` tool (query string in, results out)
8. **Respect .gitignore** via the `ignore` crate
9. **On-demand startup** (no daemon - start when queried, keep running until
   idle timeout)

**Exclude from v0.1:**
- Web server (largest surface area, lowest priority for v0.1)
- Symbol search / tree-sitter (large scope, unclear value for v0.1)
- Multi-repo search (single repo is complex enough)
- Boolean query operators (can be added later)
- Result ranking (simple ordering by file path is fine)
- Code navigation (IDE/LSP territory, not a search tool)
- Persistent daemon with launchd/systemd integration

#### Success criteria for v0.1

ferret v0.1 is successful if:
1. For a repo of 500MB+ source, `ferret search` returns results noticeably
   faster than `rg` for the same pattern
2. The MCP tool works in Claude Code and returns useful, structured results
3. The index updates automatically when files change, without manual intervention
4. Disk usage is < 5x source size
5. Idle memory usage is < 100 MB
6. It does not noticeably impact battery life when idle

#### What comes after v0.1

- v0.2: Symbol search with tree-sitter (3-5 languages)
- v0.3: Web UI for interactive browsing
- v0.4: Multi-repo support
- v0.5: Boolean query operators, result ranking
- v1.0: Stable interfaces, daemon management, cross-platform testing

### The uncomfortable question

If v0.1 takes 2-3 months and ripgrep is "good enough" for 90% of use cases,
is the remaining 10% worth the investment? The honest answer: it depends on
how much time the developer spends in large repos and how much value the MCP
interface provides for AI-assisted development. The MCP angle is the strongest
unique value proposition - no existing tool provides a fast, local code search
index exposed via MCP. If AI-assisted development continues to grow (and it
will), this becomes increasingly valuable.

**The best reason to build ferret is not that ripgrep is slow. It is that AI
agents need structured, fast, local code search, and nothing provides that
today.**
