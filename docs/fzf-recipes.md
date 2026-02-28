# fzf Recipes for indexrs

Copy-paste-ready shell functions, keybindings, and editor integrations for using indexrs with fzf.

**Prerequisites:**
- `indexrs` binary in `$PATH`
- [fzf](https://github.com/junegunn/fzf) 0.40+ installed
- [bat](https://github.com/sharkdp/bat) recommended for syntax-highlighted previews

---

## Shell Functions

Add these to `~/.zshrc`, `~/.bashrc`, or equivalent.

### `ixf` -- Interactive File Finder

Fuzzy-find an indexed file with syntax-highlighted preview.

```bash
# ixf - Interactive file finder
ixf() {
  local file
  file=$(indexrs files --color=always | fzf \
    --ansi \
    --scheme=path \
    --prompt 'file> ' \
    --header 'ENTER: open | CTRL-Y: copy path' \
    --preview 'bat --style=numbers,header --color=always -- {} 2>/dev/null || cat {}' \
    --preview-window 'right,60%,border-left' \
    --bind 'ctrl-y:execute-silent(echo -n {} | pbcopy)+abort') \
  && ${EDITOR:-vim} "$file"
}
```

### `ixg` -- Interactive Grep / Content Search

Type to search the index with live reload. All filtering is server-side.

```bash
# ixg - Interactive grep/content search
ixg() {
  local result
  result=$(indexrs search "${1:-}" --color=always --limit=1000 | fzf \
    --ansi \
    --disabled \
    --query "${1:-}" \
    --delimiter : \
    --prompt 'grep> ' \
    --header 'Type to search index | CTRL-R: regex | CTRL-Y: copy path' \
    --bind 'change:reload:indexrs search {q} --color=always --limit=1000 || true' \
    --bind 'ctrl-r:transform:[[ $FZF_PROMPT == "grep> " ]] && echo "change-prompt(regex> )+reload(indexrs search {q} --regex --color=always --limit=1000 || true)" || echo "change-prompt(grep> )+reload(indexrs search {q} --color=always --limit=1000 || true)"' \
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
```

Key details:
- `--disabled` turns off fzf's built-in fuzzy matching; all filtering is done server-side by indexrs
- `--delimiter :` lets fzf parse `file:line:col:content` fields
- `--preview-window '+{2}+3/2,~3'` scrolls preview to the matched line, centered
- `|| true` prevents fzf from showing an error when indexrs returns no results
- CTRL-R toggles between literal and regex mode

### `ixs` -- Interactive Symbol Search (Placeholder)

Search indexed symbols (functions, types, constants). Symbol indexing is planned for a future release; this recipe is ready to use once it ships.

```bash
# ixs - Interactive symbol search
ixs() {
  local result
  result=$(indexrs symbols "${1:-}" --color=always | fzf \
    --ansi \
    --disabled \
    --query "${1:-}" \
    --delimiter : \
    --prompt 'symbol> ' \
    --header 'Search symbols | CTRL-K: filter kind' \
    --with-nth '1,2' \
    --bind 'change:reload:indexrs symbols {q} --color=always || true' \
    --bind 'ctrl-k:transform:
      case $FZF_PROMPT in
        "symbol> ") echo "change-prompt(fn> )+reload(indexrs symbols {q} --kind=fn --color=always || true)" ;;
        "fn> ")     echo "change-prompt(struct> )+reload(indexrs symbols {q} --kind=struct --color=always || true)" ;;
        "struct> ") echo "change-prompt(trait> )+reload(indexrs symbols {q} --kind=trait --color=always || true)" ;;
        *)          echo "change-prompt(symbol> )+reload(indexrs symbols {q} --color=always || true)" ;;
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
```

### `ix` -- Combined Mode Switcher

Start in grep mode; switch between files, grep, and symbols with keybindings.

```bash
# ix - Combined mode (start in grep, switch with keybindings)
ix() {
  local result
  result=$(: | fzf \
    --ansi \
    --disabled \
    --delimiter : \
    --prompt 'grep> ' \
    --header $'CTRL-F: files | CTRL-G: grep | CTRL-S: symbols | CTRL-/: toggle preview\n' \
    --bind 'start:reload:indexrs search {q} --color=always --limit=1000 || true' \
    --bind 'change:reload:indexrs search {q} --color=always --limit=1000 || true' \
    --bind 'ctrl-f:unbind(change)+change-prompt(file> )+enable-search+reload(indexrs files --color=always)' \
    --bind 'ctrl-g:rebind(change)+change-prompt(grep> )+disable-search+reload(indexrs search {q} --color=always --limit=1000 || true)' \
    --bind 'ctrl-s:rebind(change)+change-prompt(symbol> )+disable-search+reload(indexrs symbols {q} --color=always || true)' \
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

Mode switching logic:
- **CTRL-G (grep)**: `disable-search` turns off fzf filtering, `rebind(change)` re-enables the reload-on-change binding. Typing queries indexrs.
- **CTRL-F (files)**: `enable-search` turns on fzf filtering, `unbind(change)` stops reloading. Typing filters the file list locally (fast).
- **CTRL-S (symbols)**: Same as grep mode -- server-side search via reload.

---

## Keybinding Integration

Bind `ix` to a key combo in the shell for instant access.

### Zsh

```bash
# Bind CTRL-X CTRL-F to ix
ix-widget() { LBUFFER+=$(ix); zle redisplay; }
zle -N ix-widget
bindkey '^X^F' ix-widget
```

### Bash

```bash
# Bind CTRL-X CTRL-F to ix
bind -x '"\C-x\C-f": ix'
```

---

## Tmux Popup Support

When inside tmux, use fzf's `--tmux` flag for floating popup windows:

```bash
ixf() {
  local fzf_opts=()
  [[ -n "$TMUX" ]] && fzf_opts+=(--tmux 'center,80%,70%')
  indexrs files --color=always | fzf "${fzf_opts[@]}" \
    --ansi --scheme=path \
    --preview 'bat --style=numbers,header --color=always -- {}' \
    --preview-window 'right,60%,border-left' \
  && ${EDITOR:-vim} "$file"
}
```

This pattern works with any of the shell functions above -- add the `fzf_opts` logic at the top and pass `"${fzf_opts[@]}"` to fzf.

---

## Vim / Neovim Integration

### fzf.vim Commands

Add to `~/.config/nvim/plugin/indexrs.vim` (or equivalent):

```vim
" File search via indexrs
command! IxFiles call fzf#run(fzf#wrap({
  \ 'source': 'indexrs files --color=always',
  \ 'options': ['--ansi', '--scheme=path',
  \             '--preview', 'bat --style=numbers,header --color=always -- {}',
  \             '--preview-window', 'right,60%,border-left'],
  \ }))

" Content search via indexrs with live reload
command! -nargs=* IxGrep call s:indexrs_grep(<q-args>)
function! s:indexrs_grep(query)
  let command_fmt = 'indexrs search %s --color=always --limit=1000 || true'
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

" Symbol search via indexrs
command! -nargs=* IxSymbols call s:indexrs_symbols(<q-args>)
function! s:indexrs_symbols(query)
  let command_fmt = 'indexrs symbols %s --color=always || true'
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

### telescope.nvim

indexrs can serve as a custom picker source for telescope.nvim. A `telescope-indexrs.nvim` plugin would call indexrs as a subprocess and feed results into telescope's pipeline. The key requirement is that indexrs outputs one result per line in a parseable format, which the CLI satisfies.

---

## VS Code Terminal Patterns

VS Code's integrated terminal supports fzf. Run the shell functions directly, or use the `--goto` flag for deeper integration:

```bash
# From VS Code terminal
ixg | xargs -I{} code --goto {}
```

The CLI output format (`file:line:col:content`) is directly compatible with VS Code's `--goto` flag.

---

## Editor Open Patterns

All recipes use fzf's `become()` or post-processing to open results. Here are patterns for different editors:

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
