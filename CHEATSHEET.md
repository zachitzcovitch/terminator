# Helix-Git Cheat Sheet

## Git Status Picker (`Space+g`)

Open the git status picker to see all changed files with diff preview.

### Navigation
| Key | Action |
|-----|--------|
| `j/k` | Navigate files |
| `J/K` | Jump between staged/unstaged sections |
| `Enter` | Open diff view for selected file |
| `p` | Toggle preview pane |

### Staging Actions
| Key | Action |
|-----|--------|
| `s` | Stage selected file |
| `u` | Unstage selected file |
| `a` | Toggle stage/unstage |
| `S` | Stage all unstaged files |
| `U` | Unstage all staged files |

### Commit
| Key | Action |
|-----|--------|
| `c` | Commit staged changes (opens message dialog) |

---

## Diff View (`gv` or `Enter` from picker)

View the diff for the current file or selected file.

### Line Navigation
| Key | Action |
|-----|--------|
| `j/k` | Navigate lines |
| `J/K` | Navigate hunks |
| `n/p` | Next/previous file (when opened from picker) |
| `q/Esc` | Return to file picker |

### Hunk Actions
| Key | Action |
|-----|--------|
| `s` | Stage selected hunk |
| `r` | Revert selected hunk |
| `R` | Reload file from disk |

### Visual Features
- **Syntax highlighting** - Full syntax highlighting for all line types
- **Word-level diff** - Changed words highlighted with underline
- **Function context** - Hunk headers show containing function/class
- **Line numbers** - Both old and new line numbers displayed
- **3-line box decoration** - Delta-style hunk headers

---

## Whitespace & Error Highlighting

### Trailing Whitespace
- Trailing spaces/tabs shown with **red background** in added lines
- Helps catch whitespace errors before commit

### Indentation Warnings
- Tabs in space-indented files shown with **yellow background**
- Spaces in tab-indented files shown with **yellow background**
- Only shown in added lines and context lines

### Empty Line Markers
- Empty lines show `⏎` symbol with muted style
- Makes empty lines visible in diff

---

## File State Indicators

### Unsaved Changes
- `[modified]` badge shown in picker and diff view header
- Warns when file has unsaved buffer changes

### External Changes
- `⚠ File changed on disk` warning banner
- Buffer may be stale - press `R` to reload

### Revert Confirmation
- Prompts before reverting hunk on file with unsaved changes
- Default is No (safe default)

---

## Diff Preview for Partially Staged Files

When a file has both staged and unstaged changes (MM status):
- **Staged entry** shows: HEAD → INDEX (what will be committed)
- **Unstaged Entry** shows: INDEX → WORKDIR (uncommitted changes)

Each entry displays the correct diff independently.

---

## Global Commands

| Command | Action |
|---------|--------|
| `Space+g` | Open git status picker |
| `gv` | Open diff view for current file |
| `]d` | Go to next diff hunk |
| `[d` | Go to previous diff hunk |

---

## Tips

1. **Stage specific changes**: Use `s` in diff view to stage individual hunks
2. **Review before commit**: Use the picker to review all staged changes
3. **Check whitespace**: Trailing whitespace is highlighted in red
4. **Navigate efficiently**: Use `J/K` to jump between hunks, `n/p` for files
5. **Partial staging**: Files with both staged and unstaged changes appear twice
