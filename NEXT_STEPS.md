# Agentic Review Editor — Full Integration Plan

## Vision

Transform helix-git from a review-focused editor into a **review-focused agentic editor** where the user manually orchestrates AI agents through the same UI they use for code review. The editor becomes the swarm's control plane.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    HELIX (Rust)                       │
│                                                       │
│  ┌──────────┐  ┌──────────┐  ┌────────────────────┐  │
│  │ Diff View│  │ Status   │  │ Agent Overlay       │  │
│  │ (review) │  │ Picker   │  │ (chat + responses)  │  │
│  └────┬─────┘  └────┬─────┘  └────────┬───────────┘  │
│       │              │                 │               │
│  ┌────┴──────────────┴─────────────────┴────────────┐ │
│  │           OpenCode HTTP Client (Rust)             │ │
│  │  - REST calls (sessions, messages, agents)        │ │
│  │  - SSE listener (streaming responses, events)     │ │
│  │  - Permission handler (approve/reject edits)      │ │
│  └──────────────────────┬───────────────────────────┘ │
└─────────────────────────┼─────────────────────────────┘
                          │ HTTP (localhost:4096)
┌─────────────────────────┼─────────────────────────────┐
│              OPENCODE SERVER (Bun/TypeScript)           │
│                                                         │
│  ┌─────────────────────────────────────────────────┐   │
│  │  Agents: build, plan, explorer, sme, coder,     │   │
│  │          reviewer, critic, test_engineer, docs   │   │
│  │  Tools: edit, write, bash, grep, read, etc.     │   │
│  │  Plugins: opencode-swarm (multi-agent)          │   │
│  │  LLMs: Anthropic, OpenAI, Gemini, etc.          │   │
│  └─────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### Permission-as-Review
OpenCode config: `"edit": "ask"`. Every file edit pauses and emits a permission event with the full diff BEFORE writing. Helix shows the diff in our diff view. User approves → edit applied → appears as unstaged change. User rejects → edit discarded. The agent proposes, the human reviews.

### User-as-Architect
Instead of the swarm running autonomously, the USER is the orchestrator. They pick which agent to talk to, review each response, and decide the next step. The editor provides the UI for this orchestration — agent picker, response overlay, plan viewer, etc.

### Popup Overlay UI
All agent interactions happen in a popup overlay (like our diff view). Versatile for any screen size. Can show: streaming text responses, diffs for review, plans, agent picker.

---

## Phase 15: OpenCode Server Management

### Goal
Spawn and manage the opencode server as a child process from helix.

### Tasks
- 15.1: Add `helix-opencode` crate with HTTP client basics
  - 15.1.1: Create `helix-opencode/src/lib.rs` with server spawn/health check
  - 15.1.2: Add `reqwest` (or `ureq`) dependency for HTTP client
  - 15.1.3: Implement `spawn_server()` — runs `opencode serve`, waits for health check
  - 15.1.4: Implement `health_check()` — GET `/global/health`
  - 15.1.5: Implement `shutdown()` — kill child process gracefully

- 15.2: Server lifecycle integration
  - 15.2.1: Auto-spawn server on first agent command (lazy start)
  - 15.2.2: Status bar indicator: "AI: connected" / "AI: starting..." / "AI: offline"
  - 15.2.3: `:ai-start` and `:ai-stop` typed commands
  - 15.2.4: Auto-shutdown on helix exit

- 15.3: Configuration
  - 15.3.1: Add `[ai]` section to helix config (server port, auto-start, provider)
  - 15.3.2: Detect existing opencode server (check port before spawning)

---

## Phase 16: Agent Overlay UI

### Goal
Create a popup overlay for interacting with AI agents — streaming responses, scrollable output.

### Tasks
- 16.1: Agent overlay component
  - 16.1.1: Create `helix-term/src/ui/agent_overlay.rs` — full-screen overlay
  - 16.1.2: Render streaming text with markdown-like formatting
  - 16.1.3: Scrollable output (j/k to scroll, G for bottom)
  - 16.1.4: Input prompt at bottom (type message, Enter to send)
  - 16.1.5: Escape to close overlay

- 16.2: Basic chat flow
  - 16.2.1: Create session on first message
  - 16.2.2: POST `/session/:id/message` with streaming response
  - 16.2.3: Parse SSE events, render tokens as they arrive
  - 16.2.4: Show tool calls inline (e.g., "Reading file.rs...", "Searching for X...")
  - 16.2.5: Message history (scroll up to see previous messages)

- 16.3: Keybindings
  - 16.3.1: `Space+a` — Open agent overlay (or resume last session)
  - 16.3.2: `Space+A` — Open agent overlay with new session

---

## Phase 17: Permission-as-Review (Edit Approval)

### Goal
When the agent wants to edit a file, show the diff in our diff view for approval.

### Tasks
- 17.1: Permission event handling
  - 17.1.1: Listen for permission events on SSE stream
  - 17.1.2: Parse edit permission requests (extract file path, diff, before/after)
  - 17.1.3: When edit permission requested, push DiffView overlay with the proposed changes

- 17.2: Approval UI in DiffView
  - 17.2.1: Add "PROPOSED CHANGE" header to DiffView when showing agent edits
  - 17.2.2: `y` key — Approve edit (POST permission reply "once")
  - 17.2.3: `n` key — Reject edit (POST permission reply "reject")
  - 17.2.4: `Y` key — Approve all future edits to this file ("always")
  - 17.2.5: After approval, edit is applied → file appears as unstaged in status picker

- 17.3: AI commit messages
  - 17.3.1: In status picker, `C` (capital) — generate commit message from staged diff
  - 17.3.2: Send staged diff to agent with prompt "Write a concise commit message"
  - 17.3.3: Show generated message in commit prompt (editable before confirming)
  - 17.3.4: Use the `plan` agent (read-only) for commit message generation

---

## Phase 18: Context-Aware Agent Interactions

### Goal
Send rich context to the agent from anywhere in the editor.

### Tasks
- 18.1: Context from Diff View
  - 18.1.1: `?` key in diff view — "Explain this hunk" (sends hunk content to agent)
  - 18.1.2: Agent response shown in overlay on top of diff view
  - 18.1.3: Include file path, function context, and surrounding code

- 18.2: Context from Log Browser
  - 18.2.1: `?` key on commit — "Summarize this commit" (sends commit diff to agent)
  - 18.2.2: `?` key on commit file — "Explain changes to this file"

- 18.3: Context from Status Picker
  - 18.3.1: `?` key — "Review my staged changes" (sends all staged diffs)
  - 18.3.2: Agent provides code review feedback in overlay

- 18.4: Context from Editor
  - 18.4.1: Visual selection + `Space+a` — sends selected code as context
  - 18.4.2: Current file + cursor position as default context

---

## Phase 19: Manual Swarm Orchestration

### Goal
The user manually picks which agent to use, becoming the swarm's architect.

### Tasks
- 19.1: Agent picker
  - 19.1.1: `Space+a+a` — Open agent picker (list all available agents)
  - 19.1.2: Show agent name, description, capabilities (read-only vs can-edit)
  - 19.1.3: Select agent → opens overlay with that agent's session
  - 19.1.4: Agents from swarm plugin: explorer, sme, coder, reviewer, critic, test_engineer, docs

- 19.2: Specialized agent workflows
  - 19.2.1: Explorer: "Analyze the auth module" → response in overlay
  - 19.2.2: SME: "What's the best approach for caching here?" → advice in overlay
  - 19.2.3: Planner: "Plan a feature for X" → generates plan, shown in overlay
  - 19.2.4: Critic: Send plan text → gets critique back
  - 19.2.5: Coder: "Implement task 1 from the plan" → edits via permission-as-review
  - 19.2.6: Reviewer: "Review the changes in src/auth.rs" → feedback in overlay
  - 19.2.7: Test Engineer: "Write tests for the auth module" → edits via permission-as-review

- 19.3: Plan viewer
  - 19.3.1: Display plan.md in a formatted overlay (phases, tasks, status)
  - 19.3.2: Navigate tasks, send individual tasks to coder
  - 19.3.3: Mark tasks complete/in-progress from the UI

- 19.4: Session management
  - 19.4.1: List active sessions (one per agent or shared)
  - 19.4.2: Resume previous session
  - 19.4.3: Fork session (branch a conversation)

---

## Phase 20: Polish & Advanced Features

### Tasks
- 20.1: Streaming diff preview — show proposed edits updating in real-time as agent types
- 20.2: Multi-file edit review — batch approve/reject when agent edits multiple files
- 20.3: Agent status in status bar — show what the agent is currently doing
- 20.4: Keyboard shortcuts cheatsheet for all agent commands
- 20.5: Background agents — fire-and-forget tasks that notify when done
- 20.6: MCP tool integration — agent can use external tools via MCP

---

## Keybinding Summary

```
Global:
  Space+a       Open agent overlay (resume last session)
  Space+A       Open agent overlay (new session)
  Space+a+a     Agent picker (choose which agent)

Agent Overlay:
  Enter         Send message
  j/k           Scroll response
  Escape        Close overlay
  Tab           Switch between agents

Status Picker:
  C             Generate AI commit message from staged diff
  ?             Ask agent to review staged changes

Diff View:
  ?             Ask agent to explain current hunk
  y/n           Approve/reject proposed edit (when reviewing agent changes)
  Y             Approve all edits to this file

Log Browser:
  ?             Ask agent to summarize commit

Editor:
  (visual) Space+a    Send selection as context to agent
```

---

## Technical Notes

### OpenCode API Endpoints Used
| Endpoint | Purpose |
|----------|---------|
| `GET /global/health` | Server health check |
| `GET /event` | SSE event stream (responses, permissions, tool calls) |
| `GET /agent` | List available agents |
| `POST /session` | Create new session |
| `POST /session/:id/message` | Send message (streaming) |
| `POST /session/:id/abort` | Cancel in-progress response |
| `GET /session/:id/diff` | Get file diffs from agent edits |
| `POST /permission/:id/reply` | Approve/reject edit permission |

### OpenCode Config for Helix Integration
```json
{
  "permission": {
    "edit": "ask"
  }
}
```
This ensures all file edits pause for approval, enabling the permission-as-review flow.

### Rust Dependencies Needed
- `reqwest` or `ureq` — HTTP client
- `eventsource-client` or custom SSE parser — for streaming events
- `serde` + `serde_json` — JSON serialization
- `tokio` — async runtime (helix already uses it)

## Status
Planning complete. Ready to begin Phase 15.
