---
name: core
description: Core agent-term usage guide. Read this before running any agent-term commands. Covers the spawn-and-observe workflow, capturing the daemon id, waiting for readiness with timeouts, reading logs in bounded slices, cursor-based polling for "what's new since I last looked", regex search with surrounding context, time-window reads on timestamped logs, the slice verb for explicit ranges, health summaries, project scoping, killing daemons, and troubleshooting common failures. Use when the user asks to run a long-lived process and observe it, tail or search a log, wait for a readiness line, find errors in long output, kill or signal a managed daemon, or investigate a hanging service.
allowed-tools: Bash(agent-term:*)
---

# agent-term core

Fast detached subprocess runner for AI agents. The daemon-per-process model
captures PTY output to a rotating log and gives you the primitives — bounded
reads, cursors, time windows, regex with context — to investigate long logs
without burning your context window.

Most everyday tasks (start a server, wait for ready, look at its output,
search for errors, kill it) are covered here. Deep dives, JSON envelope
shapes, scenario playbooks, and the complete command/flag reference live in
the bundled reference files — load them with `agent-term skills get core --full`,
or pull a single one when you actually need it:

- `references/scenarios.md` — playbooks for npm / backend / docker / CI / cross-session / polling.
- `references/commands.md` — every verb, every flag, every exit code, every env var.
- `references/json-schemas.md` — exact JSON envelope shape for every `--json` output.
- `references/troubleshooting.md` — what every error message means and how to recover.
- `references/safety.md` — what to spawn (and what not to), how to treat captured output.

## The core loop

```bash
ID=$(agent-term spawn --name api --timestamps -- python api.py)   # 1. start
agent-term wait $ID --pattern "listening on" --timeout 30s        # 2. wait for ready
agent-term tail $ID --lines 50                                    # 3. observe
# ... do work, e.g. curl localhost:8000/health ...
agent-term tail $ID --since-cursor $LAST_CURSOR                   # 4. re-observe
agent-term kill $ID                                               # 5. cleanup
```

The id from `spawn` is the handle for every subsequent verb. **Capture it
immediately**; don't try to find it later via `list` — racy and slow.

The daemon survives the CLI's exit. Spawn from one shell, observe from
another, kill from a third. That's the whole point.

## Check for an existing daemon before spawning

**Before every `spawn`, run `list` first.** A second `npm run dev` will
fight the first for port 5173; a second `docker compose logs -f api` just
duplicates the stream into your context. Daemons survive across CLI
invocations and shell sessions, so a previous agent run (or another
terminal) may already have one going.

```bash
agent-term list                          # current project
agent-term list --all                    # every project
agent-term list --json | jq '.[] | select(.name=="web")'   # specific name
```

Decision tree:

- **Match found, `state=running`**: reuse it. Capture its id, skip `spawn`,
  go straight to `wait` / `tail` / `grep`.
- **Match found, `state=exited`**: inspect (`tail $ID --lines 100`) to learn
  why it died, then `kill $ID` to drop the sidecar (it auto-clears after the
  2 s linger anyway) and spawn fresh.
- **No match**: spawn.

If the user asks you to "restart" something, that's `kill $ID` + `spawn` —
not a second `spawn` alongside the first.

## Starting a process

```bash
agent-term spawn -- node server.js                          # bare exec
agent-term spawn --name api -- python api.py                # labelled
agent-term spawn --timestamps -- bash run.sh                # opt in to timestamps
agent-term spawn --tag env=staging --tag svc=billing -- ... # annotations
agent-term spawn --project /repo/sub -- ...                 # explicit project scope
agent-term spawn --id my-daemon -- ...                      # explicit id
```

- **`--`** separates flags from the command. Always use it.
- **Shell text**: wrap in `sh -c '...'`. `agent-term spawn -- echo hi`
  runs the literal `echo` binary; `agent-term spawn -- sh -c 'echo hi; exit 7'`
  uses the shell.
- **`--timestamps`**: prepends `[<ms_since_epoch>] ` to each captured line.
  Opt this on for any process you'll later `tail --since` or `slice --from`.
  Default off so existing tooling that parses logs doesn't break.
- **Stdout**: a single line containing the id. Capture it: `ID=$(agent-term spawn -- ...)`.

## Waiting (read this)

Agents fail more often from bad waits than from anything else. `wait` is
the right primitive; raw polling loops are not.

```bash
agent-term wait $ID --pattern 'listening on :8000'              --timeout 30s
agent-term wait $ID --pattern '^READY$'                         --timeout 5s
agent-term wait $ID --pattern 'Uvicorn running on http://[^ ]+' --timeout 60s
agent-term wait $ID --pattern 'ready in [0-9]+ ms' --strip-ansi --timeout 30s   # Vite/Next/Cargo
```

**Always pass `--timeout`** unless you have a hard proof that the pattern
will appear. An unbounded wait will hang the whole agent loop.

**Dev servers colour their output.** Vite prints `ready in \x1b[1m3688\x1b[22m ms`
— ANSI escapes split the literal between `in` and the digits, so a regex
like `ready in [0-9]+ ms` never matches the raw bytes. Pass `--strip-ansi`
whenever the pattern targets phrases a TTY-aware process might colour
(Vite, Next, Cargo, npm, Rich/colorama Python loggers). If you forget,
`wait` detects ANSI escapes in the log on timeout and appends a hint:
`log contains ANSI escapes — retry with --strip-ansi`. Don't ignore it.

**Timestamped spawns**: by default `wait`'s pattern is matched against the
post-prefix body, so `^READY$` keeps working when you flip
`spawn --timestamps` on. Pass `--match-full-line` if you actually want the
`[<ms>] ` prefix in the haystack (e.g. to assert "the line arrived after
ms=T").

**Match the terminal signal, not the activity.** A bound port, a printed
URL, or a framework's final ready line — never an alternation of generic
English verbs.

- **Bad**: `'(?i)listening|started|server|ready'` — fires on
  `Starting backend server...`, on any "server" banner, on any "ready to
  accept connections soon" log line. Generic verbs trigger before the
  socket is actually bound.
- **Good**: `'listening on (0\.0\.0\.0|127\.0\.0\.1|\[::\]):\d+'` —
  only matches once the process has bound a real port.
- **Good**: `'ready in \d+\s*ms'`, `'^READY$'`,
  `'http://127\.0\.0\.1:\d+'` — anchored with structure the process only
  emits at the final ready state.

If you're not sure which line is terminal, spawn with `--timestamps`, let
the service start once by hand, `tail` it, and copy the last line of the
startup banner verbatim into your pattern.

**Branch on the exit code**:

| Exit | Meaning |
|------|---------|
| 0    | Pattern matched. |
| 1    | Timeout. The pattern never appeared. |
| 2    | Process exited before the pattern matched. The cause is in the log. |

```bash
if agent-term wait $ID --pattern READY --timeout 30s; then
  echo "ready"
elif [ $? -eq 2 ]; then
  echo "child died — investigate the log:"
  agent-term tail $ID --lines 100
else
  echo "timed out"
fi
```

`wait` reads the existing log first, then follows — already-matched cases
return immediately.

## Reading a log (bounded, always bounded)

```bash
agent-term tail $ID --lines 100                  # last 100 lines
agent-term tail $ID --bytes 16K                  # last 16 KiB, line-aligned
agent-term tail $ID --head 50                    # first 50 lines (startup)
agent-term tail $ID --reverse --lines 50         # newest-first
agent-term tail $ID --lines 200 --strip-ansi     # filter CSI escapes
```

**Never run `tail $ID` without a bound** on a log you haven't sized. The
default is "dump everything", and "everything" can be 50 MB. Call `summary`
first if you don't know.

### Cursor mode — for "what's new since I looked?"

The killer primitive for polling. Every read with `--json` emits a `cursor`
field; pass it back on the next call.

```bash
# 1. First read: capture the cursor.
agent-term tail $ID --json --lines 50 > /tmp/r.json
CURSOR=$(jq -r .cursor /tmp/r.json)
cat /tmp/r.json | jq -r .content      # the bytes we got

# 2. Next read: only the new bytes since CURSOR.
agent-term tail $ID --json --since-cursor $CURSOR > /tmp/r.json
CURSOR=$(jq -r .cursor /tmp/r.json)
```

Stale cursor (past EOF after a rotation):

```bash
agent-term tail $ID --json --cursor 999999
# {"cursor_stale": true, "cursor": 50000, "content": "", ...}
```

Reset to the returned `cursor` and continue. No bytes are lost — the
previous content lives in the rotated segment files (`<id>.log.1`, ...).

### Time-window mode (requires `--timestamps` spawn)

```bash
agent-term tail $ID --since 5m                    # last 5 minutes
agent-term tail $ID --since "30s ago" --until now # 30s ago to now
agent-term tail $ID --since 500ms                 # last 500 ms
```

`--since` / `--until` **combine** with `--lines` / `--bytes` / `--head` /
`--reverse`. The time window picks candidates, the cap narrows them:

```bash
agent-term tail $ID --since 30s --lines 5        # last 5 lines from last 30s
agent-term tail $ID --since 5m --reverse --lines 3 # same, newest-first
agent-term tail $ID --since 1m --head 1          # first line in the last minute
agent-term tail $ID --since 30s --reverse        # full window, newest-first
```

`--cursor` and `--follow` remain mutex with the time selectors (different
access patterns).

Without `--timestamps` at spawn time, time selectors error with a clear
message. Time specs: `Nms` / `Ns` / `Nm` / `Nh` / `Nd`, the literal `now`,
or an integer ms-since-epoch.

## Searching a log

`grep` is regex + surrounding context + time-window filter in one verb.

```bash
agent-term grep $ID --pattern '(?i)error|fail' --around 5 --limit 5
agent-term grep $ID --pattern '^Exception' --around 30 --limit 1
agent-term grep $ID --pattern 'ERROR' --since 5m --around 10 --json
```

- **`--around N`** ≈ `grep -C N`. Overlapping context windows merge.
- **`--limit N`**: stop after N matches. Always use it on long logs.
- **`--since` / `--until`**: time-window filter (requires `--timestamps`).
- **`--json`** is the most LLM-friendly format on this verb. Schema in
  `references/json-schemas.md`.

**Killer combo**: `grep $ID --pattern '(?i)error' --around 10 --limit 3 --since 5m --json` —
"the three most recent errors in the last 5 minutes, with 10 lines of
context, machine-readable". Replaces five `tail | grep | head` calls.

`tail --grep PATTERN` is sugar for `grep` and accepts the same `--around`/`--limit`.

## Reading an explicit range: `slice`

When you have *two* bounds — both ends known — use `slice`:

```bash
agent-term slice $ID --from-cursor 17204 --to-cursor 18012
agent-term slice $ID --from "30s ago" --to now              # time mode
```

Time selectors and cursor selectors are mutually exclusive.

## At-a-glance health: `summary`

```bash
agent-term summary $ID
agent-term summary $ID --json
```

Returns: process state, child pid, uptime, log size in bytes and lines,
rotated segment count, last-line age, tail cursor, recent error and
warning counts. Cheap (one IPC roundtrip). **Start every investigation
here** — it tells you which other verb to reach for.

## Listing daemons

```bash
agent-term list                              # current project only
agent-term list --all                        # every daemon, every project
agent-term list --tag env=staging            # filter by tag
agent-term list --json                       # machine-readable
```

**Project scoping**: `list` defaults to `$PWD` (canonicalised). Two terminals
in different repos don't see each other's daemons. Use `--all` or
`--project /path` to break out.

## Status, kill, signal

```bash
agent-term status $ID                        # {state, child_pid, code?}
agent-term kill $ID                          # SIGTERM, 200 ms grace, then SIGKILL
agent-term kill $ID --signal HUP             # other signals: INT, USR1, KILL, ...
```

`kill` is graceful. **Don't reach for `--signal KILL` reflexively**; let the
child clean up.

### When *not* to kill

A long-lived, reusable daemon (dev server, db, compose log stream, queue
worker) should **outlive your turn**. Don't `kill` it just because you
finished using it. The next turn — or another agent on the same project —
should find it via `list` and reuse it. Killing it forces a 3–10 s
cold-start on every reuse and, on shared projects, can stomp on a sibling
session that's actively using it.

Kill only when:

- **You spawned it for a one-shot job** (a test run, a build) that has now
  exited or whose output you've consumed.
- **It's wedged** (`state=exited`, hung, or the log shows a fatal error).
- **The user explicitly asks to stop it.**

### Restart in place (do this for a fresh process)

If a reusable daemon needs to come back fresh — config change, port
collision, hung internal state, picking up new code that doesn't hot-reload —
**restart it in place**: `kill` the existing id, then `spawn` again with the
same `--name` (and same flags). Same name → callers find it by name on the
next `list`. **Do not spawn a second one alongside.**

```bash
# Restart-in-place for a dev server you found via list.
ID=$(agent-term list --json | jq -r '.[] | select(.name=="web") | .id')
agent-term kill "$ID"
ID=$(agent-term spawn --name web --timestamps -- npm run dev)
agent-term wait "$ID" --pattern 'ready in [0-9]+ ms' --timeout 60s
```

This is the *only* sanctioned way to get a fresh process for a named role.
A duplicate `spawn` without `kill` will either fail (name collision) or
fight the original for the port.

## Hard rules

These encode failure modes other agents have hit. Treat them as binding.

- **R0.** **`list` before every `spawn`.** Reuse a running daemon if one
  already serves the same role (dev server, db, compose log stream). A
  duplicate `spawn` of a port-binding service either fails outright or
  silently doubles the work. Restart = `kill` + `spawn` *with the same
  `--name`*, not two spawns.
- **R0.5.** **Don't kill reusable daemons at end-of-task.** Dev servers, dbs,
  compose log streams stay alive across turns and across sibling sessions —
  that's the whole point of the detached daemon model. Only `kill` for
  one-shot jobs that have served their purpose, wedged processes, or when
  the user asks. For a fresh process: restart-in-place (same name).
- **R1.** **Never unbounded `tail`.** On any log > 64 KiB, use `--lines N`,
  `--bytes N`, or `--since-cursor`. Call `summary` first if you don't know
  the size.
- **R2.** **Always pass `--timeout` to `wait`.** Unbounded waits hang.
- **R2.5.** **Readiness patterns match the terminal signal, not the
  activity.** A bound port, a printed URL, or a framework's final ready
  line. Never alternate generic English verbs
  (`starting|started|server|ready|listening`) — `server` matches
  "Starting backend server...". Anchor with structure: `listening on :\d+`,
  `ready in \d+\s*ms`, `^READY$`, `http://127\.0\.0\.1:\d+`.
- **R3.** **Capture the id from `spawn`** into a variable immediately.
  Don't rediscover via `list`.
- **R4.** **Branch on `wait`'s exit code.** 0 / 1 / 2 mean different things;
  don't assume 0.
- **R5.** **Time selectors require `--timestamps` at spawn time.** If you
  might need `tail --since` later, spawn with `--timestamps`.
- **R6.** **Default to `--strip-ansi`** for LLM-consumed output from
  dev servers, CI, or anything with colour.
- **R7.** **Poll with `--since-cursor`, not loop-and-tail.** Cursor-driven
  loops scale; bounded re-reads don't.
- **R8.** **`kill` is graceful.** Don't reach for `--signal KILL` first.
- **R9.** **`list` is project-scoped.** Use `--all` or `--project /path`
  to break out.
- **R10.** **Sub-2-second commands**: run them in bash. Don't spawn.

## Diagnosing install issues: `doctor`

If a command fails unexpectedly (stale daemons, orphaned children after a
`kill -9` of the daemon, accumulating short-lived daemons, version
mismatches after `upgrade`), run `doctor`:

```bash
agent-term doctor                # scan; exit 1 if issues found
agent-term doctor --fix          # clean stale sidecars, reap orphans
agent-term doctor --json         # structured output (schema: schemas/doctor.schema.json)
```

## Global flags and env vars

```bash
--json                                  # most read-verbs accept this
--strip-ansi                            # tail, grep, slice — filter CSI escapes

# Env vars (user-facing)
AGENT_TERM_STATE_DIR                # where sidecars live; default $XDG_RUNTIME_DIR/agent-term or ~/.agent-term
AGENT_TERM_TIMESTAMPS=1             # equivalent to spawn --timestamps
AGENT_TERM_IDLE_TIMEOUT_MS=120000   # per-daemon idle shutdown (0/unset = off)
AGENT_TERM_LOG_SIZE=10485760        # rotation size; default 10 MiB
AGENT_TERM_LOG_SEGMENTS=3           # rotated segments to keep; default 3
```

`AGENT_TERM_STATE_DIR` is the most useful one in practice — set it
in tests to isolate, or in CI to share state across steps.

## When to load another skill

None at v0.1 — `core` is everything. The roadmap includes `send` (stdin
injection), `process` (resource accounting), and `providers` (remote/cloud).

## Full reference

For deeper detail, load the bundled reference files:

```bash
agent-term skills get core --full          # everything below, concatenated
```

What's in each file:

- **`references/scenarios.md`** — Concrete playbooks: npm / Vite / Next.js
  dev servers, backend incident investigation, docker compose with one
  daemon per service, CI failure triage, cross-session observation,
  cursor-driven polling loops.
- **`references/commands.md`** — Exhaustive command/flag reference: every
  verb, every flag, every default, every exit code.
- **`references/json-schemas.md`** — JSON envelope shapes for every verb's
  `--json` output. Pointers to formal JSON Schema files in `schemas/`.
- **`references/troubleshooting.md`** — Every error message agent-term
  emits and how to recover from it.
- **`references/safety.md`** — What's safe to spawn, how to treat captured
  output, lifecycle and persistence rules.
