# Helix-Git Cheat Sheet

## Git Status Picker (`Space+g+s`)

Open the git status picker to see all changed files with diff preview.

### File Status Labels
| Status | Meaning |
|--------|---------|
| staged | Fully staged for commit |
| unstaged | Not staged |
| partial | File has both staged and unstaged changes |
| untracked | New file not yet tracked by git |

Partially staged files appear once (yellow "partial" label). Untracked directories show individual files.

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
| `Enter` | Jump to selected line in workspace file (uses similarity matching for historical diffs) |
| `q/Esc` | Return to file picker |

### Hunk Actions
| Key | Action |
|-----|--------|
| `s` | Stage selected hunk (stays in view, shows dimmed with checkmark) |
| `u` | Unstage selected hunk (stays in view, removes checkmark) |
| `r` | Revert selected hunk (confirms first) |
| `R` | Reload file from disk |

Hunk staging uses git-generated patches internally for robustness, including correct behavior with partially staged files.

### Visual Features
- **Syntax highlighting** - Full syntax highlighting for all line types
- **Word-level diff** - Changed words highlighted with underline
- **Function context** - Hunk headers show nested parent scopes (e.g. `impl Foo { fn bar(`)
- **Line numbers** - Both old and new line numbers displayed
- **3-line box decoration** - Delta-style hunk headers
- **Staged hunk indicators** - Staged hunks shown dimmed with checkmark badge
- **Inline blame** - Bottom bar shows who last changed the selected line

### Partially Staged Files

Files with "partial" status show a unified diff (HEAD to workdir). Hunks that are already staged appear in muted/dimmed colors so you can distinguish staged from unstaged changes at a glance.

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

## Global Commands

| Command | Action |
|---------|--------|
| `Space+g+s` | Open git status picker |
| `Space+g+l` | Open git log browser |
| `Space+g+t` | Open git stash picker |
| `gv` | Open diff view for current file |
| `]d` | Go to next diff hunk |
| `[d` | Go to previous diff hunk |

---

## Git Log Browser (`Space+g+l`)

Browse commit history with diff preview.

| Key | Action |
|-----|--------|
| `j/k` | Navigate commits |
| `Enter` | Open commit file picker (shows changed files with diff preview) |
| `y` | Yank commit hash |
| `q/Esc` | Close |

Preview pane shows formatted commit info (hash, author, date, subject) above a color-coded file list: modified (yellow), added (green), deleted (red), renamed (cyan).

From the commit file picker, `Enter` opens the diff view for that file. From the diff view, `Enter` jumps to the corresponding line in the workspace file (uses similarity matching to find the best line in the current version).

---

## Inline Blame (in Diff View)

When viewing a diff (`gv` or from log/stash), the bottom of the view shows blame information for the currently selected line:

```
 a1b2c3d Alice, 2 hours ago • Fix parser edge case
```

- Automatically loaded when opening a diff view for a real file
- Shows: commit hash, author, relative date, and commit subject
- Updates as you navigate lines with j/k

---

## Git Stash (`Space+g+t`)

Manage git stashes with preview.

### Picker Actions
| Key | Action |
|-----|--------|
| `j/k` | Navigate stashes |
| `a` | Apply stash (keep in list) |
| `Enter`/`p` | Pop stash (apply + remove) |
| `d` | Drop stash (confirms first) |
| `q/Esc` | Close |

### Typed Commands
| Command | Action |
|---------|--------|
| `:stash-push [message]` | Stash current changes |
| `:stash-pop [index]` | Pop stash (default: stash@{0}) |

Preview pane shows a color-coded file list: modified (yellow), added (green), deleted (red), renamed (cyan). `Enter` on a stash opens a file picker for that stash's contents.

---

## Tips

1. **Stage specific changes**: Use `s` in diff view to stage individual hunks
2. **Review before commit**: Use the picker to review all staged changes
3. **Check whitespace**: Trailing whitespace is highlighted in red
4. **Navigate efficiently**: Use `J/K` to jump between hunks, `n/p` for files
5. **Partial staging**: Partially staged files show as "partial" with staged hunks dimmed in the diff
6. **Jump to source**: Press `Enter` in any diff view to jump to that line in the workspace file
7. **Browse history**: Use `Space+g+l` to explore commit log with formatted preview
8. **Blame a line**: Inline blame shows automatically in diff view
9. **Stash work**: Use `Space+g+t` to manage stashes, `:stash-push` to create
