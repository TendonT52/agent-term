# agent-term

A detached, observable subprocess runner for AI agents.

`agent-term` lets an AI agent run a long-lived command — a dev server,
a build watcher, a backend, a training job — and then read its output
later. The process keeps running between commands, doesn't get lost
when the shell exits, and can be inspected, searched, waited on, or
killed from any new shell session.

## The problem

AI agents talk to your computer through a shell. That works fine for
commands that start, finish, and print everything before exiting —
`ls`, `git log`, `cargo build`. It breaks the moment the command is a
**server**.

Ask an agent: *"start the dev server, open `/products`, and fix
whatever's wrong."*

The agent types `npm run dev`. Now what?

- If it waits for the command to finish, it waits forever. Dev servers
  don't finish.
- If it backgrounds the command with `&`, the agent can move on — but
  it can no longer see what the server is printing. So when
  `/products` returns a 500, the agent has nowhere to read the stack
  trace from. Its only option is to kill the server and run it again,
  hoping to catch the error this time.

The agent needs four things at once, and plain shell can't give it
all four:

1. **Start** a process without blocking on it.
2. **Read what it printed** — now, ten seconds from now, or tomorrow.
3. **Wait** for a specific line (e.g. `ready in 1.2s`) before continuing.
4. **Stop** it cleanly, without leaking it as an orphan.

That's what `agent-term` is for.

## What agent-term does

`agent-term spawn -- npm run dev` runs your command, but not as a
child of your shell. It hands the command to a small detached manager
that keeps running after your shell exits, and captures every line
the process prints to a log on disk.

From any shell session — the same one, the next one, or one tomorrow —
the agent talks to the manager through a few simple verbs:

| Verb | What it does |
| --- | --- |
| `spawn` | Start the process. Returns an id. |
| `wait` | Block until a line matches a regex, with a timeout. |
| `tail` | Read the last N lines of the log (always bounded). |
| `grep` | Search the log with surrounding context. |
| `summary` | One-shot health check: state, log size, recent errors. |
| `kill` | Stop the process cleanly. |
| `list` / `doctor` | See what's running, clean up stale state. |

Every read is bounded, so an agent investigating a 50 MB log doesn't
blow its context window. Cursors answer *"what's new since I last
looked?"* without re-reading the file. `summary` returns a small JSON
snapshot so the agent can decide what to investigate before reading a
single log line.

## Quick start

```bash
# Start a dev server in the background. Capture the id.
ID=$(agent-term spawn --name web --timestamps -- npm run dev)

# Block until it prints a readiness signal.
agent-term wait $ID --pattern "ready in" --timeout 60s

# Health check — is it still up? what does its log look like?
agent-term summary $ID

# Read the last 50 lines (bounded — your context window matters).
agent-term tail $ID --lines 50

# Find errors with surrounding context.
agent-term grep $ID --pattern '(?i)error|fail' --around 5 --limit 5

# What's new since I last looked? (cursor-driven polling)
agent-term tail $ID --since-cursor 18402 --json

# Stop it gracefully (TERM, 200 ms grace, then KILL).
agent-term kill $ID
```

The subprocess survives across CLI invocations and across shell
sessions. Logs are captured to a rotating on-disk file you can re-read
at any time.

## Installation

### Cargo (Rust)

```bash
cargo install agent-term
```

### From source

```bash
git clone https://github.com/TendonT52/agent-term
cd agent-term
cargo build --release
# binary at ./target/release/agent-term
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

Full reference: `agent-term --help`, `agent-term <cmd> --help`, or
the [skill docs](skill-data/core/SKILL.md).

## For AI agents

agent-term ships a Claude Code-compatible skill so an agent can drive
the tool from a single doc read. Two files:

- [`skills/agent-term/SKILL.md`](skills/agent-term/SKILL.md) —
  short discovery stub with trigger phrases for the skill matcher.
- [`skill-data/core/SKILL.md`](skill-data/core/SKILL.md) — the actual
  workflow guide (~600 lines). Covers the canonical loop, hard rules,
  scenario playbooks (npm / backend / docker / CI), JSON schemas, and
  troubleshooting.

Drop the `skills/agent-term/` directory into your Claude Code
skills path (`~/.config/anthropic/claude-code/skills/` or per-project
`.claude/skills/`) and the agent will discover it.

## Log navigation, the short version

The verbs that let an agent investigate a long, time-sensitive log
without burning its context window:

```bash
# Bounded reads — never tail without one of these on a log > 64 KiB.
agent-term tail $ID --lines 100
agent-term tail $ID --bytes 16K
agent-term tail $ID --head 50              # startup
agent-term tail $ID --reverse --lines 50   # newest-first (CI failures)

# Cursor-driven polling — read only the new bytes since last call.
RESP=$(agent-term tail $ID --since-cursor $CURSOR --json)
CURSOR=$(echo "$RESP" | jq -r .cursor)

# Time windows (requires spawn --timestamps).
agent-term tail $ID --since 5m
agent-term slice $ID --from "30s ago" --to now

# Regex with context.
agent-term grep $ID --pattern '(?i)error|fail' --around 5 --limit 5 --json

# Health snapshot — 200-token JSON, the right first call.
agent-term summary $ID --json
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
  `~/.agent-term/` — with one set of files per managed subprocess
  (`.pid`, `.version`, `.cmd`, `.meta`, `.sock`, `.log`). Discovery is
  filesystem-based; there is no central registry.
- Logs **rotate** at 10 MiB (configurable) and keep the last 3 segments
  by default. The `.log` file itself is preserved after `kill` so
  post-mortem inspection still works.


### Where state is stored

`agent-term` resolves its state directory in this order:

1. `$AGENT_TERM_STATE_DIR` — explicit override.
2. `$XDG_RUNTIME_DIR/agent-term` — Linux with a runtime dir.
3. `~/.agent-term` — the default.
4. `$TMPDIR/agent-term` — last-resort fallback.

## Environment variables

| Variable | Effect |
| --- | --- |
| `AGENT_TERM_STATE_DIR` | Override the state directory (handy for tests). |
| `AGENT_TERM_TIMESTAMPS=1` | Equivalent to `spawn --timestamps`. |
| `AGENT_TERM_IDLE_TIMEOUT_MS` | Per-daemon idle shutdown. `0` / unset = off. |
| `AGENT_TERM_LOG_SIZE` | Bytes per `.log` segment before rotation (default 10 MiB). |
| `AGENT_TERM_LOG_SEGMENTS` | Rotated segments to keep (default 3). |

## Status

What ships in v1.0.0:

- Detached daemon with PTY-attached child, Unix-socket IPC, signal handling.
- `spawn` / `list` / `status` / `kill` / `wait` / `tail` / `grep` /
  `slice` / `summary` / `doctor`.
- Log rotation, project scoping, name uniqueness, tag filtering.
- Cursor-based and time-window reads on the log.
- Version negotiation on daemon reuse, idle timeout, orphan-child detection
  in `doctor --fix`.
- `schemas/list-entry.schema.json` — formal JSON schema for `list --json` output.
- 94 unit tests, `cargo clippy -- -D warnings` clean.

Not yet in scope for v1.0:

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
  daemon design; `agent-term` is what you reach for when you want
  the same lifecycle properties for *any* subprocess, not just a browser.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
