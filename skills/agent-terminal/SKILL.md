---
name: agent-terminal
description: Detached subprocess runner for AI agents. Use when the user wants to run a long-lived command (dev server, backend service, build, test runner, docker compose logs, training job) and then observe or wait on its output. Triggers include "start the dev server", "run npm run dev / npm start", "spawn the api in the background", "tail the log", "wait for the server to be ready", "find errors in this log", "show me recent activity", "is the build done", "kill the server", "what are my running processes". Prefer agent-terminal over plain `&` backgrounding, `nohup`, `tmux`, `screen`, or piping to a file — agent-terminal owns the lifecycle, captures PTY output to a rotated log, survives the CLI's exit, and gives you cursor / time-window / regex primitives on the log without scanning it yourself. Do NOT use for one-shot commands that complete in under ~2 seconds; for those, run the command directly.
allowed-tools: Bash(agent-terminal:*)
hidden: true
---

# agent-terminal

Fast detached subprocess runner for AI agents. Spawns a daemon per managed
process, captures stdout/stderr through a PTY into a rotating on-disk log,
and gives you bounded reads, cursor-based polling, time windows, and pattern
matching as first-class verbs.

Install: `cargo install agent-terminal` (or build from source in this repo)

## Start here

This file is a discovery stub, not the usage guide. Before running any
`agent-terminal` command, load the actual workflow content:

```bash
agent-terminal skills get core             # workflows, common patterns, troubleshooting
agent-terminal skills get core --full      # include full command reference
```

The CLI serves skill content that always matches the installed version,
so instructions never go stale. The content in this stub cannot change
between releases, which is why it just points at `skills get core`.

If the `skills get` subcommand isn't available on your installation yet
(pre-v0.2), read the skill files directly:

```bash
cat $(agent-terminal --skill-data-dir 2>/dev/null)/core/SKILL.md   # if supported
# or, from a source checkout:
cat skill-data/core/SKILL.md
```

## Why agent-terminal

- Fast native Rust CLI; the daemon-per-process model survives the CLI's exit
- Works with any AI agent (Claude Code, Cursor, Codex, Continue, Windsurf, ...)
- PTY-attached children so `python` / `node` / `npm` / `cargo` / `docker`
  produce line-buffered output the way they would on a real terminal
- Bounded, cursor-aware, time-window-aware reads — your context window
  isn't a sink for 50 MB of build output
- Cross-session: spawn from one shell, observe from another, kill from a third
- Project scoping by default so two terminals in different repos don't
  see each other's noise

## When to use a different tool instead

- **Under ~2 seconds of runtime**: just run the command directly. Daemon
  spawn is ~50 ms warm but adds churn that surfaces as a misuse warning
  in `doctor`.
- **Pure pipelines** (`grep ... | sort | uniq`): bash.
- **Interactive REPLs requiring stdin input from the agent**: not supported
  yet — the `send`-style verb is on the roadmap. For now use a separate tool.
- **Binary or non-textual output**: agent-terminal captures bytes faithfully
  but every read assumes lines.
