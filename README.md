# agent-terminal

A detached, observable subprocess runner for AI agents.

`agent-terminal` is a small Rust CLI that lets an AI agent (or any script)
launch long-running commands — dev servers, build watchers, training jobs,
background workers — and then come back later, from any shell session, to
read their output, search it, wait for a specific line to appear, or kill
them cleanly. The processes you start outlive the agent's turn; they are
not attached to a terminal, do not get orphaned when the agent exits, and
clean up after themselves.

## Why

AI agents that run shell commands have a hard time with anything that
doesn't exit. Start `npm run dev` in a normal shell and one of two
things happens:

1. The agent waits for it to finish — and it never finishes.
2. The agent backgrounds it with `&` — and it dies, or worse, leaks,
   when the agent's shell session ends.

Neither is acceptable when the agent needs to *use* the thing it just
started (hit the dev server, tail the build log, wait for "compiled
successfully" before running tests).

`agent-terminal` solves this the same way a process supervisor does: the
subprocess is owned by a separate, detached manager — not by the agent's
shell. The agent talks to the manager through a tiny CLI: spawn, wait,
tail, grep, summary, kill.

## Quick start

```bash
# Start a dev server in the background. Capture the id.
ID=$(agent-terminal spawn --name web --timestamps -- npm run dev)

# Block until it prints a readiness signal.
agent-terminal wait $ID --pattern "ready in" --timeout 60s

# Health check — is it still up? what does its log look like?
agent-terminal summary $ID

# Read the last 50 lines (bounded — your context window matters).
agent-terminal tail $ID --lines 50

# Find errors with surrounding context.
agent-terminal grep $ID --pattern '(?i)error|fail' --around 5 --limit 5

# What's new since I last looked? (cursor-driven polling)
agent-terminal tail $ID --since-cursor 18402 --json

# Stop it gracefully (TERM, 200 ms grace, then KILL).
agent-terminal kill $ID
```

The subprocess survives across CLI invocations and across shell
sessions. Logs are captured to a rotating on-disk file you can re-read
at any time.

## Installation

### Cargo (Rust)

```bash
cargo install agent-terminal
```

### From source

```bash
git clone https://github.com/TendonT52/agent-terminal
cd agent-terminal
cargo build --release
# binary at ./target/release/agent-terminal
# or install to ~/.cargo/bin:
cargo install --path .
```

### Requirements

- **Rust 1.75+** when building from source.
- **Unix-like OS** (macOS, Linux). Windows is not supported — the design
  relies on Unix-domain sockets, `setsid`, and PTY allocation via
  `portable-pty`'s Unix path.

## Commands

| Command   | What it does                                                                  |
| --------- | ----------------------------------------------------------------------------- |
| `spawn`   | Start a command as a detached, managed subprocess. Returns the id on stdout. |
| `list`    | Show managed subprocesses (project-scoped by default).                       |
| `status`  | One IPC round-trip: `{state, child_pid, code}`.                              |
| `tail`    | Read the log. `--lines / --bytes / --head / --reverse / --cursor / --since`. |
| `grep`    | Pattern-match the log with surrounding context and time-window filters.      |
| `slice`   | Read an explicit byte or time range (`--from-cursor` / `--from`).            |
| `wait`    | Block until a regex matches. Exit 0 / 1 / 2 = match / timeout / process exited. |
| `summary` | At-a-glance health snapshot: size, lines, last-line age, recent errors.      |
| `kill`    | Send a signal (`TERM` by default) and tear down sidecars.                    |
| `doctor`  | Diagnose state dir: live / stale / orphans / misuse heuristic. `--fix` cleans. |

Full reference: `agent-terminal --help`, `agent-terminal <cmd> --help`, or
the [skill docs](skill-data/core/SKILL.md).

## For AI agents

agent-terminal ships a Claude Code-compatible skill so an agent can drive
the tool from a single doc read. Two files:

- [`skills/agent-terminal/SKILL.md`](skills/agent-terminal/SKILL.md) —
  short discovery stub with trigger phrases for the skill matcher.
- [`skill-data/core/SKILL.md`](skill-data/core/SKILL.md) — the actual
  workflow guide (~600 lines). Covers the canonical loop, hard rules,
  scenario playbooks (npm / backend / docker / CI), JSON schemas, and
  troubleshooting.

Drop the `skills/agent-terminal/` directory into your Claude Code
skills path (`~/.config/anthropic/claude-code/skills/` or per-project
`.claude/skills/`) and the agent will discover it.

## Log navigation, the short version

The verbs that let an agent investigate a long, time-sensitive log
without burning its context window:

```bash
# Bounded reads — never tail without one of these on a log > 64 KiB.
agent-terminal tail $ID --lines 100
agent-terminal tail $ID --bytes 16K
agent-terminal tail $ID --head 50              # startup
agent-terminal tail $ID --reverse --lines 50   # newest-first (CI failures)

# Cursor-driven polling — read only the new bytes since last call.
RESP=$(agent-terminal tail $ID --since-cursor $CURSOR --json)
CURSOR=$(echo "$RESP" | jq -r .cursor)

# Time windows (requires spawn --timestamps).
agent-terminal tail $ID --since 5m
agent-terminal slice $ID --from "30s ago" --to now

# Regex with context.
agent-terminal grep $ID --pattern '(?i)error|fail' --around 5 --limit 5 --json

# Health snapshot — 200-token JSON, the right first call.
agent-terminal summary $ID --json
```

See [`skill-data/core/SKILL.md`](skill-data/core/SKILL.md) for the full
agent-facing guide with scenario playbooks (npm / backend / docker / CI),
hard rules, and JSON schemas.

## How it works

The design is a deliberate copy of the daemon-and-sidecar pattern used
by [`agent-browser`](https://github.com/vercel-labs/agent-browser):

- The CLI you type is **short-lived**. It parses your arguments, talks
  to a long-lived manager over a Unix socket, prints the reply, and exits.
- The manager is **detached** (`PPID = 1`, no controlling terminal, stdio
  redirected to `/dev/null`) so it survives the shell session that
  spawned it and never becomes an orphan.
- The child is **PTY-attached** so line-buffered programs (npm, python,
  node) produce output the way they do on a real terminal.
- State lives in a **sidecar directory** — by default
  `~/.agent-terminal/` — with one set of files per managed subprocess
  (`.pid`, `.version`, `.cmd`, `.meta`, `.sock`, `.log`). Discovery is
  filesystem-based; there is no central registry.
- Logs **rotate** at 10 MiB (configurable) and keep the last 3 segments
  by default. The `.log` file itself is preserved after `kill` so
  post-mortem inspection still works.


### Where state is stored

`agent-terminal` resolves its state directory in this order:

1. `$AGENT_TERMINAL_STATE_DIR` — explicit override.
2. `$XDG_RUNTIME_DIR/agent-terminal` — Linux with a runtime dir.
3. `~/.agent-terminal` — the default.
4. `$TMPDIR/agent-terminal` — last-resort fallback.

## Environment variables

| Variable | Effect |
| --- | --- |
| `AGENT_TERMINAL_STATE_DIR` | Override the state directory (handy for tests). |
| `AGENT_TERMINAL_TIMESTAMPS=1` | Equivalent to `spawn --timestamps`. |
| `AGENT_TERMINAL_IDLE_TIMEOUT_MS` | Per-daemon idle shutdown. `0` / unset = off. |
| `AGENT_TERMINAL_LOG_SIZE` | Bytes per `.log` segment before rotation (default 10 MiB). |
| `AGENT_TERMINAL_LOG_SEGMENTS` | Rotated segments to keep (default 3). |

## Status

What ships in v0.1.0:

- Detached daemon with PTY-attached child, Unix-socket IPC, signal handling.
- `spawn` / `list` / `status` / `kill` / `wait` / `tail` / `grep` /
  `slice` / `summary` / `doctor`.
- Log rotation, project scoping, name uniqueness, tag filtering.
- Cursor-based and time-window reads on the log.
- Version negotiation on daemon reuse, idle timeout, orphan-child detection
  in `doctor --fix`.
- `schemas/list-entry.schema.json` — formal JSON schema for `list --json` output.
- 94 unit tests, `cargo clippy -- -D warnings` clean.

Not yet in scope for v0.1:

- Windows support.
- Stdin / keystroke injection into the child (the `send` verb).
- Pre-built binaries / Homebrew / npm wrapper.
- Cloud / remote-host providers.

## Contributing

This is an early-stage project. Bug reports, design feedback, and PRs
welcome. The repo layout:

```
src/         Rust source — one module per verb (tail, grep, slice, …)
schemas/     JSON Schemas for --json output (e.g. list-entry.schema.json)
skills/      Claude Code skill stub (discovery + trigger phrases)
skill-data/  Full agent-facing workflow guide
```

Build, test, lint:

```bash
cargo build
cargo test                              # 94 unit tests
cargo clippy --all-targets -- -D warnings
```

## Related projects

- [`agent-browser`](https://github.com/vercel-labs/agent-browser) — the
  sibling tool for driving a real Chrome from an agent. Same detached-
  daemon design; `agent-terminal` is what you reach for when you want
  the same lifecycle properties for *any* subprocess, not just a browser.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
