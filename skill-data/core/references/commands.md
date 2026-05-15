# Command reference

Every verb, every flag, every exit code. For workflow examples see
`scenarios.md`; for JSON envelope shapes see `json-schemas.md`.

## `spawn` — start a managed subprocess

```
agent-term spawn [OPTIONS] -- <argv>...
```

| Flag | Meaning |
|------|---------|
| `--id <ID>` | Explicit id (1–64 chars, `[A-Za-z0-9_-]`). Default: random 12-hex. |
| `--name <NAME>` | Human-readable label, unique within the project. |
| `--project <PATH>` | Override the project scope. Default: canonicalised `$PWD`. |
| `--tag K=V` | Free-form annotations; repeatable. Filterable in `list`. |
| `--timestamps` | Prepend `[<ms_since_epoch>] ` to every captured line. |
| `-- <argv>...` | The command to run. Always present; always preceded by `--`. |

**Stdout**: a single line with the id.

**Exit codes**: 0 on success, 1 on argument or filesystem error.

Notes:

- Shell strings: wrap in `sh -c '...'`. `spawn -- echo hi` invokes the
  literal `echo` binary; `spawn -- sh -c 'echo hi; exit 7'` uses the shell.
- Two parallel `spawn --id foo` calls don't double-start. The loser
  piggybacks on the winner's daemon (spawn-race resolution).
- The child is PTY-attached so line-buffered programs (npm, python, node)
  flush as they would on a real terminal.
- `--timestamps` is a one-way decision per daemon; you cannot retrofit it.
  If you might need `tail --since` or `slice --from` later, opt in at spawn.

## `list` — show managed subprocesses

```
agent-term list [--project <PATH>] [--tag K=V]... [--all] [--json]
```

| Flag | Meaning |
|------|---------|
| `--project <PATH>` | Show daemons in a specific project (default: `$PWD`). |
| `--tag K=V` | Filter by tag; repeatable. AND semantics. |
| `--all` | Ignore project scope; show every daemon on this machine. |
| `--json` | Emit a JSON array. Schema: `schemas/list-entry.schema.json`. |

Side effect: `list` runs stale-sidecar cleanup. Dead daemons whose `.pid`
files no longer point to a live process get cleaned up as part of the scan.

## `status` — one-shot health check

```
agent-term status <ID>
```

Single IPC round trip. Prints `{state, child_pid, code?}` in text or JSON.
Cheaper than `summary` but carries no log-size or recent-error info.

## `tail` — read the log

```
agent-term tail <ID> [--lines N | --bytes N | --head N | --cursor POS | --since SPEC] [...]
```

| Flag | Meaning |
|------|---------|
| `--lines N` | Last N lines (line-aligned). |
| `--bytes N` | Last N bytes, line-aligned at the start. Suffix `K`/`M` accepted. |
| `--head N` | First N lines (startup investigation). |
| `--reverse` | Emit lines newest-first. |
| `--cursor POS` / `--since-cursor POS` | Read from byte offset POS to EOF. |
| `--since SPEC` / `--until SPEC` | Time-window filter. Requires `--timestamps` spawn. |
| `--strip-ansi` | Filter CSI escape sequences (colour codes, cursor moves). |
| `--keep-timestamps` | Preserve the `[<ms>] ` prefix in output (default: stripped). |
| `--grep PATTERN` | Sugar for `grep`; takes `--around`, `--limit`. |
| `--follow` | Stream new bytes as they appear. |
| `--json` | Emit a JSON envelope with `cursor`, `content`, `lines_emitted`, etc. |

**Never run `tail $ID` without one of `--lines`, `--bytes`, `--head`,
`--cursor`, or `--since`.** The default is "dump everything".

## `grep` — pattern-match the log

```
agent-term grep <ID> --pattern <REGEX> [...]
```

| Flag | Meaning |
|------|---------|
| `--pattern <REGEX>` | Regex (mutually exclusive with `--pattern-file`). |
| `--pattern-file <PATH>` | Read pattern from a file (for shell-hostile regexes). |
| `--around N` | Context lines, equivalent to `grep -C N`. Overlapping windows merge. |
| `--limit N` | Stop after N matches. Always set on long logs. |
| `--since SPEC` / `--until SPEC` | Time-window filter. Requires `--timestamps`. |
| `--strip-ansi` | Strip colour codes before matching. |
| `--match-full-line` | Match the regex against the full line, not the body. By default the timestamp prefix is stripped before matching. |
| `--multiline` | Multiline regex semantics (`^`/`$` honour internal line boundaries). |
| `--json` | Structured output: hits, blocks, line numbers, timestamps. |

## `slice` — explicit range read

```
agent-term slice <ID> (--from-cursor N --to-cursor N | --from SPEC --to SPEC) [...]
```

| Flag | Meaning |
|------|---------|
| `--from-cursor N` / `--to-cursor N` | Byte offsets. Half-open ranges allowed. |
| `--from SPEC` / `--to SPEC` | Time range. Requires `--timestamps` spawn. |
| `--strip-ansi` | Strip colour codes. |
| `--keep-timestamps` | Preserve `[<ms>] ` prefix in output. |
| `--json` | Structured envelope. |

Cursor selectors and time selectors are mutually exclusive.

## `wait` — block until a pattern matches

```
agent-term wait <ID> --pattern <REGEX> --timeout <DUR> [--multiline] [--json]
```

| Flag | Meaning |
|------|---------|
| `--pattern <REGEX>` | Regex to wait for. |
| `--pattern-file <PATH>` | Read regex from file. |
| `--timeout <DUR>` | Max wait. Suffixes: `ms`, `s` (default), `m`, `h`. Required in practice. |
| `--multiline` | Enable `(?m)` semantics. |
| `--json` | Emit JSON envelope. |

**Exit codes**:

| Exit | Meaning |
|------|---------|
| 0    | Pattern matched. Stdout has the matching line (text) or a JSON envelope. |
| 1    | Timeout. The pattern never appeared. |
| 2    | Process exited before the pattern matched. The cause is in the log. |

`wait` reads the existing log first, then follows. Already-matched cases
return immediately.

## `summary` — health snapshot

```
agent-term summary <ID> [--recent-window SPEC] [--error-pattern REGEX] [--warning-pattern REGEX] [--json]
```

| Flag | Meaning |
|------|---------|
| `--recent-window SPEC` | Time window for the recent-errors count. Default: `60s`. |
| `--error-pattern REGEX` | Classifier for "errors". Default: `(?i)error\|fatal`. |
| `--warning-pattern REGEX` | Classifier for "warnings". Default: `(?i)warn`. |
| `--json` | Emit structured output. Schema: `schemas/summary.schema.json`. |

Schema fields: state, child_pid, exit_code, uptime_ms, log_bytes, log_lines,
segments, last_line_age_ms, tail_cursor, recent {since_ms, lines_scanned,
errors, warnings, mode}.

## `kill` — signal the daemon

```
agent-term kill <ID> [--signal <SIG>]
```

| Flag | Meaning |
|------|---------|
| `--signal <SIG>` | Default `TERM`. Accepted: `TERM`, `INT`, `HUP`, `KILL`, `USR1`, `USR2`, `QUIT`, `STOP`, `CONT`. `SIG`-prefix tolerated. |

Behaviour: SIGTERM to the child, 200 ms grace window, then SIGKILL if it
hasn't exited. The daemon lingers ~2 s after the child exits so `status` /
`tail` calls during that window see the final state, then sidecars are
removed.

## `doctor` — diagnose state-dir issues

```
agent-term doctor [--fix] [--json]
```

| Flag | Meaning |
|------|---------|
| `--fix` | Clean stale sidecars; SIGTERM-then-SIGKILL orphaned children. |
| `--json` | Structured output. Schema: `schemas/doctor.schema.json`. |

Reports:

- **live**: daemons currently running, with daemon pid + child pid.
- **stale**: sidecar bundles whose daemon process is gone.
- **orphans**: child processes whose parent daemon died (e.g. via `kill -9`)
  and which didn't take SIGHUP from the closing PTY.
- **warnings**: misuse heuristics — ≥ 10 short-lived daemons in the last
  hour suggests the agent is using `agent-term spawn` for things that
  should be plain `bash`.

Exit code: 0 if no problems or all fixed; 1 if there are issues and
`--fix` was not specified.

## `skills` — print bundled documentation

```
agent-term skills list
agent-term skills get <NAME> [--full]
```

Skills are embedded at compile time, so the content always matches the
installed binary. `--full` includes the reference files alongside the
main `SKILL.md`.

## Global flags

| Flag | Where it applies | Meaning |
|------|------------------|---------|
| `--json` | most read-verbs | Machine-readable output. |
| `--strip-ansi` | tail, grep, slice | Filter CSI escape sequences. |
| `--keep-timestamps` | tail, slice | Preserve the `[<ms>] ` prefix. |

## Environment variables

| Variable | Effect |
|---|---|
| `AGENT_TERM_STATE_DIR` | Override the state directory (handy for tests). |
| `AGENT_TERM_TIMESTAMPS=1` | Equivalent to `spawn --timestamps` (per-daemon). |
| `AGENT_TERM_IDLE_TIMEOUT_MS` | Per-daemon idle shutdown in ms. `0` / unset = off. |
| `AGENT_TERM_LOG_SIZE` | Bytes per `.log` segment before rotation. Default 10 MiB. |
| `AGENT_TERM_LOG_SEGMENTS` | Rotated segments to keep. Default 3. |

## State directory resolution

In precedence order:

1. `$AGENT_TERM_STATE_DIR` — explicit override.
2. `$XDG_RUNTIME_DIR/agent-term` — Linux with a runtime dir.
3. `~/.agent-term` — the default on macOS and Linux without XDG.
4. `$TMPDIR/agent-term` — last-resort fallback.

## Sidecar files

For each daemon `<id>`, the state directory contains:

| File | Purpose |
|------|---------|
| `<id>.pid` | Daemon process id. Used by liveness check (`kill -0`). |
| `<id>.version` | Daemon binary version. Mismatch triggers a restart. |
| `<id>.cmd` | The argv that was spawned, JSON-encoded. |
| `<id>.meta` | The metadata record (id, name, project, tags, timestamps, ...). |
| `<id>.sock` | Unix-domain socket the CLI talks to. |
| `<id>.log` | Append-only log of captured PTY output. |
| `<id>.log.1`, `<id>.log.2`, ... | Rotated segments. |

Logs persist after `kill` (sidecars are removed, the log is not), so
post-mortem inspection still works. `rm <id>.log*` if you don't want it.

## Time spec syntax

Used by `--since`, `--until`, `--from`, `--to`, `--recent-window`.

| Form | Meaning |
|------|---------|
| `Nms` | N milliseconds ago. |
| `Ns` | N seconds ago. |
| `Nm` | N minutes ago. |
| `Nh` | N hours ago. |
| `Nd` | N days ago. |
| `now` | The current wall-clock time. |
| `<integer>` | Absolute ms-since-epoch. |
| `<spec> ago` | Same as `<spec>`. Both `5m` and `5m ago` work; `5m ago` reads more naturally with `--to now`. |

All time-based selectors require `spawn --timestamps`. Without it, the
verb errors with `require timestamped logs; spawn with --timestamps`.
