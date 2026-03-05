# fzf Interface Design

ferret provides a CLI that outputs structured, fzf-friendly data for terminal-based fuzzy searching. This document specifies output formats, integration recipes, preview commands, shell functions, and editor integration.

## Design Principles

- **ferret is the data source; fzf is the UI.** ferret outputs lines to stdout with ANSI colors and structured delimiters. fzf handles filtering, selection, and preview orchestration.
- **Streaming first.** All commands stream results as they are found rather than buffering. This keeps fzf responsive even on large repos.
- **Delimiter consistency.** All modes use `:` as the primary field delimiter, matching grep/ripgrep conventions that editors and fzf already understand.
- **ANSI by default when stdout is a TTY.** Use `--color=always` to force colors (for piping into `fzf --ansi`), `--color=never` to disable.

---

## 1. CLI Output Modes

### 1.1 File Search Mode (`ferret files`)

Lists indexed files. Output format matches `fd` conventions.

```
FORMAT: <relative-path>
```

With `--color=always`:
- Directory components: dim
- Filename: bold
- Extension: cyan

```
$ ferret files --color=always
src/main.rs
src/index/mod.rs
src/index/trigram.rs
tests/integration_test.rs
```

Options:
- `--language <lang>` - filter by language (e.g., `rust`, `python`)
- `--path <glob>` - filter by path pattern
- `--limit <n>` - maximum results (default: unlimited, streams)
- `--sort <field>` - sort by `path`, `modified`, `size` (default: `path`)

### 1.2 Content Search Mode (`ferret search`)

Searches file contents using the index. Output format matches ripgrep's `--vimgrep` format.

```
FORMAT: <file>:<line>:<column>:<matched-line>
```

With `--color=always`:
- File path: magenta
- Line number: green
- Column number: green
- Match highlight: red bold

```
$ ferret search 'trigram' --color=always
src/index/trigram.rs:15:12:    pub fn build_trigram(text: &str) -> Vec<Trigram> {
src/index/trigram.rs:42:8:    let trigrams = extract_trigrams(&query);
src/index/mod.rs:7:5:use trigram::TrigramIndex;
```

Options:
- `--regex` - interpret query as regex (default: literal)
- `--case-sensitive` / `--ignore-case` / `--smart-case` (default: smart-case)
- `--language <lang>` - restrict to language
- `--path <glob>` - restrict to path pattern
- `--limit <n>` - maximum results (default: 1000 for interactive use)
- `--context <n>` - include n lines of context (for preview, not default output)
- `--stats` - print match count to stderr (does not interfere with stdout piping)

### 1.3 Symbol Search Mode (`ferret symbols`)

Searches indexed symbols (functions, types, constants, etc.).

```
FORMAT: <symbol-kind>:<symbol-name>:<file>:<line>
```

With `--color=always`:
- Symbol kind: yellow bold (fn, struct, enum, trait, const, type, impl, mod, etc.)
- Symbol name: white bold
- File path: magenta
- Line number: green

```
$ ferret symbols 'TrigramIndex' --color=always
struct:TrigramIndex:src/index/trigram.rs:8
impl:TrigramIndex:src/index/trigram.rs:20
fn:TrigramIndex::new:src/index/trigram.rs:21
fn:TrigramIndex::search:src/index/trigram.rs:35
```

Options:
- `--kind <kind>` - filter by symbol kind (`fn`, `struct`, `trait`, etc.)
- `--language <lang>` - restrict to language
- `--limit <n>` - maximum results
- `--definition-only` - only show definitions, not references

### 1.4 Preview Subcommand (`ferret preview`)

A dedicated subcommand that renders file content for fzf's preview window. This exists so that ferret can provide syntax-highlighted, line-centered previews without requiring bat as a dependency (though bat is preferred when available).

```
$ ferret preview <file> [--line <n>] [--context <n>]
```

- `--line <n>` - center the preview on this line
- `--context <n>` - number of context lines above/below (default: fills `$FZF_PREVIEW_LINES`)
- `--highlight-line <n>` - highlight a specific line (ANSI reverse video)

Behavior:
- If `bat` is found in `$PATH`, delegate to `bat --style=numbers,header --color=always --highlight-line <n> <file>`.
- Otherwise, use built-in syntax highlighting via the `syntect` crate.
- Respects `$FZF_PREVIEW_LINES` and `$FZF_PREVIEW_COLUMNS` to size output.

---

## 2. fzf Integration Recipes

### 2.1 Basic File Search

Fuzzy-find an indexed file with syntax-highlighted preview:

```bash
ferret files --color=always | fzf \
  --ansi \
  --scheme=path \
  --preview 'bat --style=numbers,header --color=always -- {}' \
  --preview-window 'right,60%,border-left' \
  --bind 'enter:become(${EDITOR:-vim} {})'
```

### 2.2 Content Search with Live Reload

The key pattern: fzf starts with `--disabled` (no local filtering), and on every keystroke, reloads results from ferret. This delegates all searching to the index.

```bash
ferret search '' --color=always --limit=500 | fzf \
  --ansi \
  --disabled \
  --delimiter : \
  --prompt 'search> ' \
  --header 'CTRL-R: toggle regex | CTRL-F: switch to files | ENTER: open' \
  --bind 'change:reload:ferret search {q} --color=always --limit=500 || true' \
  --bind 'ctrl-r:transform-prompt:
    if [[ $FZF_PROMPT == "search> " ]]; then
      echo "regex> "
    else
      echo "search> "
    fi' \
  --bind 'ctrl-r:+reload:
    if [[ $FZF_PROMPT == "regex> " ]]; then
      ferret search {q} --regex --color=always --limit=500 || true
    else
      ferret search {q} --color=always --limit=500 || true
    fi' \
  --preview 'bat --style=numbers --color=always --highlight-line {2} -- {1}' \
  --preview-window 'right,60%,border-left,+{2}+3/2,~3' \
  --bind 'enter:become(${EDITOR:-vim} +{2} {1})'
```

Key details:
- `--disabled` turns off fzf's built-in fuzzy matching; all filtering is done server-side by ferret
- `--delimiter :` lets fzf parse `file:line:col:content` fields
- `--preview-window '+{2}+3/2,~3'` scrolls preview to the matched line, centered, with 3 fixed header lines
- `|| true` prevents fzf from showing error when ferret returns no results
- `--limit=500` caps results for responsiveness; the index returns the most relevant first

### 2.3 Symbol Search with Preview

```bash
ferret symbols '' --color=always | fzf \
  --ansi \
  --disabled \
  --delimiter : \
  --prompt 'symbol> ' \
  --header 'Search symbols | CTRL-K: kind filter' \
  --nth '1,2' \
  --with-nth '1,2' \
  --bind 'change:reload:ferret symbols {q} --color=always || true' \
  --preview 'bat --style=numbers --color=always --highlight-line {4} -- {3}' \
  --preview-window 'right,60%,border-left,+{4}+3/2,~3' \
  --bind 'enter:become(${EDITOR:-vim} +{4} {3})'
```

Note: `--with-nth '1,2'` shows only `kind:name` in the list (hiding file:line), while `{3}` and `{4}` in preview/enter still reference the full line.

### 2.4 Combined Mode Switching

Switch between files, content search, and symbols with keybindings:

```bash
: | fzf \
  --ansi \
  --disabled \
  --delimiter : \
  --prompt 'search> ' \
  --header $'CTRL-F: files | CTRL-G: grep | CTRL-S: symbols\n' \
  --bind 'start:reload:ferret search {q} --color=always --limit=500 || true' \
  --bind 'change:reload:ferret search {q} --color=always --limit=500 || true' \
  --bind 'ctrl-f:unbind(change)+change-prompt(files> )+enable-search+reload(ferret files --color=always)' \
  --bind 'ctrl-g:rebind(change)+change-prompt(search> )+disable-search+reload(ferret search {q} --color=always --limit=500 || true)' \
  --bind 'ctrl-s:rebind(change)+change-prompt(symbol> )+disable-search+reload(ferret symbols {q} --color=always || true)' \
  --preview 'ferret preview {1} --line {2} 2>/dev/null || bat --style=numbers --color=always -- {1} 2>/dev/null || echo "No preview"' \
  --preview-window 'right,60%,border-left' \
  --bind 'enter:become(${EDITOR:-vim} +{2} {1})'
```

Mode switching logic:
- **CTRL-G (grep)**: `disable-search` turns off fzf filtering, `rebind(change)` re-enables the reload-on-change binding. Typing queries ferret.
- **CTRL-F (files)**: `enable-search` turns on fzf filtering, `unbind(change)` stops reloading. Typing filters the file list locally (fast).
- **CTRL-S (symbols)**: Same as grep mode -- server-side search via reload.

### 2.5 Opening Results in Editors

All recipes above use `become()` to replace the fzf process with an editor. Here are patterns for different editors:

```bash
# Vim/Neovim: open at line
--bind 'enter:become(vim +{2} {1})'

# VS Code: open at file:line:column
--bind 'enter:become(code --goto {1}:{2}:{3})'

# Emacs: open at line
--bind 'enter:become(emacsclient -n +{2} {1})'

# Helix: open at file:line
--bind 'enter:become(hx {1}:{2})'

# Multi-select: open all selected files
--bind 'ctrl-o:become(${EDITOR:-vim} $(echo {+1} | tr " " "\n"))'
```

---

## 3. Preview Commands

### 3.1 File Preview with Syntax Highlighting

Primary strategy: delegate to `bat` when available.

```bash
# File preview (for file search mode)
bat --style=numbers,header --color=always -- {file}

# Content search preview (centered on match line)
bat --style=numbers,header --color=always --highlight-line {line} -- {file}

# Fallback when bat is unavailable
ferret preview {file} --line {line}
```

The `ferret preview` fallback uses syntect for highlighting and formats output for terminal display. It adds line numbers and highlights the target line with reverse video.

### 3.2 Context Preview Around Matched Lines

For content search, the preview window should center on the matched line:

```bash
--preview-window '+{2}+3/2,~3'
```

This means:
- `+{2}`: start scrolling at the value in field 2 (line number)
- `+3`: offset by 3 (compensate for 3 fixed header lines)
- `/2`: divide by 2 to center vertically in the preview window
- `~3`: keep the top 3 lines (bat's header) as a fixed header

### 3.3 Symbol Definition Preview

For symbol search, show the symbol definition with surrounding context:

```bash
--preview 'bat --style=numbers --color=always --highlight-line {4} --line-range={4}:+50 -- {3}'
```

This shows ~50 lines starting from the symbol definition, which typically captures the full function/struct body.

---

## 4. Shell Integration

### 4.1 Shell Functions

Add to `~/.zshrc`, `~/.bashrc`, or equivalent:

```bash
# ixf - Interactive file finder
ixf() {
  local file
  file=$(ferret files --color=always | fzf \
    --ansi \
    --scheme=path \
    --prompt 'file> ' \
    --header 'ENTER: open | CTRL-Y: copy path' \
    --preview 'bat --style=numbers,header --color=always -- {} 2>/dev/null || cat {}' \
    --preview-window 'right,60%,border-left' \
    --bind 'ctrl-y:execute-silent(echo -n {} | pbcopy)+abort') \
  && ${EDITOR:-vim} "$file"
}

# ixg - Interactive grep/content search
ixg() {
  local result
  result=$(ferret search "${1:-}" --color=always --limit=1000 | fzf \
    --ansi \
    --disabled \
    --query "${1:-}" \
    --delimiter : \
    --prompt 'grep> ' \
    --header 'Type to search index | CTRL-R: regex | CTRL-Y: copy path' \
    --bind 'change:reload:ferret search {q} --color=always --limit=1000 || true' \
    --bind 'ctrl-r:transform:[[ $FZF_PROMPT == "grep> " ]] && echo "change-prompt(regex> )+reload(ferret search {q} --regex --color=always --limit=1000 || true)" || echo "change-prompt(grep> )+reload(ferret search {q} --color=always --limit=1000 || true)"' \
    --bind 'ctrl-y:execute-silent(echo -n {1} | pbcopy)+abort' \
    --preview 'bat --style=numbers --color=always --highlight-line {2} -- {1} 2>/dev/null' \
    --preview-window 'right,60%,border-left,+{2}+3/2,~3')
  if [[ -n "$result" ]]; then
    local file line
    file=$(echo "$result" | cut -d: -f1)
    line=$(echo "$result" | cut -d: -f2)
    ${EDITOR:-vim} "+$line" "$file"
  fi
}

# ixs - Interactive symbol search
ixs() {
  local result
  result=$(ferret symbols "${1:-}" --color=always | fzf \
    --ansi \
    --disabled \
    --query "${1:-}" \
    --delimiter : \
    --prompt 'symbol> ' \
    --header 'Search symbols | CTRL-K: filter kind' \
    --with-nth '1,2' \
    --bind 'change:reload:ferret symbols {q} --color=always || true' \
    --bind 'ctrl-k:transform:
      case $FZF_PROMPT in
        "symbol> ") echo "change-prompt(fn> )+reload(ferret symbols {q} --kind=fn --color=always || true)" ;;
        "fn> ")     echo "change-prompt(struct> )+reload(ferret symbols {q} --kind=struct --color=always || true)" ;;
        "struct> ") echo "change-prompt(trait> )+reload(ferret symbols {q} --kind=trait --color=always || true)" ;;
        *)          echo "change-prompt(symbol> )+reload(ferret symbols {q} --color=always || true)" ;;
      esac' \
    --preview 'bat --style=numbers --color=always --highlight-line {4} -- {3} 2>/dev/null' \
    --preview-window 'right,60%,border-left,+{4}+3/2,~3')
  if [[ -n "$result" ]]; then
    local file line
    file=$(echo "$result" | cut -d: -f3)
    line=$(echo "$result" | cut -d: -f4)
    ${EDITOR:-vim} "+$line" "$file"
  fi
}

# ix - Combined mode (start in grep, switch with keybindings)
ix() {
  local result
  result=$(: | fzf \
    --ansi \
    --disabled \
    --delimiter : \
    --prompt 'grep> ' \
    --header $'CTRL-F: files | CTRL-G: grep | CTRL-S: symbols | CTRL-/: toggle preview\n' \
    --bind 'start:reload:ferret search {q} --color=always --limit=1000 || true' \
    --bind 'change:reload:ferret search {q} --color=always --limit=1000 || true' \
    --bind 'ctrl-f:unbind(change)+change-prompt(file> )+enable-search+reload(ferret files --color=always)' \
    --bind 'ctrl-g:rebind(change)+change-prompt(grep> )+disable-search+reload(ferret search {q} --color=always --limit=1000 || true)' \
    --bind 'ctrl-s:rebind(change)+change-prompt(symbol> )+disable-search+reload(ferret symbols {q} --color=always || true)' \
    --preview '[[ -n {2} ]] && bat --style=numbers --color=always --highlight-line {2} -- {1} 2>/dev/null || bat --style=numbers,header --color=always -- {} 2>/dev/null' \
    --preview-window 'right,60%,border-left')
  if [[ -n "$result" ]]; then
    local file line
    file=$(echo "$result" | cut -d: -f1)
    line=$(echo "$result" | cut -d: -f2)
    if [[ "$line" =~ ^[0-9]+$ ]]; then
      ${EDITOR:-vim} "+$line" "$file"
    else
      ${EDITOR:-vim} "$result"
    fi
  fi
}
```

### 4.2 Keybinding Integration

Bind `ix` to a key combo in the shell for instant access:

```bash
# Zsh: bind CTRL-X CTRL-F to ix
ix-widget() { LBUFFER+=$(ix); zle redisplay; }
zle -N ix-widget
bindkey '^X^F' ix-widget

# Bash: bind CTRL-X CTRL-F to ix
bind -x '"\C-x\C-f": ix'
```

### 4.3 Tmux Integration

When inside tmux, use fzf's `--tmux` for popup windows:

```bash
ixf() {
  local fzf_opts=()
  [[ -n "$TMUX" ]] && fzf_opts+=(--tmux 'center,80%,70%')
  ferret files --color=always | fzf "${fzf_opts[@]}" \
    --ansi --scheme=path \
    --preview 'bat --style=numbers,header --color=always -- {}' \
    --preview-window 'right,60%,border-left' \
  && ${EDITOR:-vim} "$file"
}
```

---

## 5. Editor Integration

### 5.1 Vim/Neovim

**With fzf.vim** -- add custom commands backed by ferret:

```vim
" ~/.config/nvim/plugin/ferret.vim

" File search via ferret
command! IxFiles call fzf#run(fzf#wrap({
  \ 'source': 'ferret files --color=always',
  \ 'options': ['--ansi', '--scheme=path',
  \             '--preview', 'bat --style=numbers,header --color=always -- {}',
  \             '--preview-window', 'right,60%,border-left'],
  \ }))

" Content search via ferret with live reload
command! -nargs=* IxGrep call s:ferret_grep(<q-args>)
function! s:ferret_grep(query)
  let command_fmt = 'ferret search %s --color=always --limit=1000 || true'
  let initial_command = printf(command_fmt, shellescape(a:query))
  let reload_command = printf(command_fmt, '{q}')
  let spec = {
    \ 'source': initial_command,
    \ 'options': ['--ansi', '--disabled', '--query', a:query,
    \             '--delimiter', ':',
    \             '--bind', 'change:reload:'.reload_command,
    \             '--preview', 'bat --style=numbers --color=always --highlight-line {2} -- {1}',
    \             '--preview-window', 'right,60%,border-left,+{2}+3/2,~3'],
    \ }
  call fzf#run(fzf#wrap(spec))
endfunction

" Symbol search via ferret
command! -nargs=* IxSymbols call s:ferret_symbols(<q-args>)
function! s:ferret_symbols(query)
  let command_fmt = 'ferret symbols %s --color=always || true'
  let initial_command = printf(command_fmt, shellescape(a:query))
  let reload_command = printf(command_fmt, '{q}')
  let spec = {
    \ 'source': initial_command,
    \ 'options': ['--ansi', '--disabled', '--query', a:query,
    \             '--delimiter', ':',
    \             '--with-nth', '1,2',
    \             '--bind', 'change:reload:'.reload_command,
    \             '--preview', 'bat --style=numbers --color=always --highlight-line {4} -- {3}',
    \             '--preview-window', 'right,60%,border-left,+{4}+3/2,~3'],
    \ }
  call fzf#run(fzf#wrap(spec))
endfunction

" Keybindings
nnoremap <leader>if :IxFiles<CR>
nnoremap <leader>ig :IxGrep<CR>
nnoremap <leader>is :IxSymbols<CR>
nnoremap <leader>iw :IxGrep <C-R><C-W><CR>
```

**With telescope.nvim** -- ferret can serve as a custom picker source. A `telescope-ferret.nvim` plugin would call ferret as a subprocess and feed results into telescope's pipeline. The key requirement is that ferret outputs one result per line in a parseable format, which this design satisfies.

### 5.2 VS Code Terminal

VS Code's integrated terminal supports fzf. Users can run the shell functions directly. For deeper integration, a VS Code extension could:

1. Spawn `ferret search` as a subprocess
2. Pipe results into VS Code's QuickPick UI
3. Open files at the selected location

The CLI output format (`file:line:col:content`) is directly compatible with VS Code's `--goto` flag:

```bash
# From VS Code terminal
ixg | xargs -I{} code --goto {}
```

### 5.3 Emacs

```elisp
;; Using consult + ferret
(defun ferret-search ()
  "Search code using ferret with fzf in a terminal buffer."
  (interactive)
  (let ((default-directory (project-root (project-current t))))
    (compilation-start
     (format "ferret search '%s' --color=never"
             (read-string "Search: "))
     'grep-mode)))

;; Or use fzf.el if available
(defun ferret-fzf ()
  "Interactive file search with ferret + fzf."
  (interactive)
  (let ((process-environment
         (cons (format "FZF_DEFAULT_COMMAND=ferret files") process-environment)))
    (fzf/start (project-root (project-current t)))))
```

---

## 6. Performance

### 6.1 Streaming Results

ferret writes results to stdout as they are found, without buffering the full result set. This means fzf starts showing results immediately. Implementation detail: use `BufWriter` with explicit `flush()` after each line, or use line-buffered stdout (default for TTY, must be forced when piping).

```rust
// Force line-buffered output when piping to fzf
use std::io::{self, Write, BufWriter};

let stdout = io::stdout();
let mut writer = BufWriter::new(stdout.lock());
for result in search_results {
    writeln!(writer, "{}", result)?;
    writer.flush()?; // Flush per line for streaming
}
```

Note: flushing per-line has overhead. An alternative is to flush every N lines (e.g., 64) or use a timer-based flush. For interactive use, per-line flush up to ~1000 results then switch to batch flush is a reasonable heuristic.

### 6.2 Result Limits

Default limits for interactive use prevent overwhelming fzf:
- `ferret search`: 1000 results by default (`--limit`)
- `ferret symbols`: 500 results by default
- `ferret files`: no limit (file lists are typically manageable)

Users can override with `--limit=0` for unlimited results.

### 6.3 Debouncing on Reload

fzf fires `change` events on every keystroke. The `reload` action spawns a new ferret process each time, killing the previous one. This is fine because:

1. fzf handles process lifecycle -- it kills the old process when a new reload fires
2. ferret should handle `SIGPIPE`/broken pipe gracefully (exit immediately, no error message)
3. The index is in-memory, so query startup is fast (sub-millisecond to first result)

If additional debouncing is needed, fzf does not provide built-in debounce, but the shell function can wrap the reload command:

```bash
# fzf's natural behavior: rapid reloads kill previous processes
# ferret should exit cleanly on SIGPIPE/broken pipe
--bind 'change:reload:ferret search {q} --color=always --limit=500 || true'
```

No explicit debounce is needed if ferret responds in <50ms, which is the target for indexed searches.

### 6.4 Startup Time

For the shell functions to feel instant:
- ferret should connect to the daemon via Unix socket (not re-index on every invocation)
- If the daemon is not running, `ferret search` should start it in the background and return results from a cold index build
- Target: <100ms to first result for common queries against a warm daemon

### 6.5 SIGPIPE Handling

When fzf kills a reload process or the user pipes output to `head`, ferret must handle SIGPIPE without printing error messages:

```rust
// In main()
unsafe {
    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
}
```

---

## 7. CLI Subcommand Summary

```
ferret files [OPTIONS]              List indexed files
ferret search <QUERY> [OPTIONS]     Search file contents
ferret symbols [QUERY] [OPTIONS]    Search symbols
ferret preview <FILE> [OPTIONS]     Render file preview for fzf

Global options:
  --color <WHEN>      Color output: auto, always, never (default: auto)
  --repo <PATH>       Repository root (default: git root or cwd)
  --limit <N>         Maximum results (0 for unlimited)
```

## 8. Output Format Specification

All output modes produce one result per line. ANSI escape codes are included only when color is enabled. Stripping ANSI codes from any output line must produce a valid parseable result.

| Mode    | Format                              | Delimiter | fzf fields            |
|---------|-------------------------------------|-----------|-----------------------|
| files   | `<path>`                            | N/A       | `{}`=path             |
| search  | `<file>:<line>:<col>:<content>`     | `:`       | `{1}`=file, `{2}`=line, `{3}`=col, `{4..}`=content |
| symbols | `<kind>:<name>:<file>:<line>`       | `:`       | `{1}`=kind, `{2}`=name, `{3}`=file, `{4}`=line |

This format is intentionally compatible with existing tooling (grep, ripgrep, compiler errors) so that editor goto-file-line integrations work out of the box.
