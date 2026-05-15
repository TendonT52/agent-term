# Troubleshooting

Every error message agent-term emits and how to recover from it.

## "no log for id …"

The id is wrong, or the daemon was killed and the log was somehow removed.

```bash
agent-term list --all     # see what's actually live
```

If the id looks right, check that `<id>.log` exists in the state directory
(default `~/.agent-term/`).

## `wait` exits 2: "process exited before pattern matched"

The child died before printing your pattern. Read the recent log:

```bash
agent-term tail $ID --lines 100 --strip-ansi
```

If the daemon is still in its 2 s linger window, `status` returns
`{state: "exited", code: N}` with the exit code.

## `tail --since 30s` errors with "require timestamped logs"

You forgot `spawn --timestamps`. Either re-spawn with it, or use
`tail --lines N` / `tail --since-cursor N` instead. There's no way to
retrofit timestamps onto an existing daemon's log.

## `tail` shows weird `\r\n` line endings

That's PTY behaviour, not a bug. Patterns in `wait` and `grep` already
strip trailing `\r\n` before matching, so `'^READY$'` matches `READY\r\n`.
For human reading, pipe through `tr -d '\r'`.

## Cursor returns `"cursor_stale": true`

The log rotated. Set `CURSOR` to the returned `cursor` value (which points
at the current EOF) and continue. No bytes are lost — the previous content
lives in `<id>.log.1`, `<id>.log.2`, ... Use `slice --from-cursor` against
those if you need the historical bytes back.

## Two terminals: one spawned a daemon, the other can't see it

`list` is project-scoped. Make sure both terminals share the same `$PWD`
(canonicalised), or pass `--project /path` / `--all`.

## Daemon disappeared after a `kill -9` on the daemon process

PTY behaviour: when the daemon dies, the master FD closes, the kernel
sends SIGHUP to the child's session, which usually kills it. If the child
trapped SIGHUP it survives — `doctor` will report it as an orphan and
`doctor --fix` will reap it.

## `spawn` returns instantly but `status` reports "exited" 50 ms later

The child exited very quickly. The daemon stays up for ~2 s after the
child exits so `status` / `tail` calls during that window see the final
state. After the linger, sidecars are removed. The `.log` file survives.

## "name 'X' already in use by id …"

Name uniqueness is per-project. Either pick a different name, kill the
existing daemon, or use `--all` to see what's there.

## Multiple `spawn --id foo` calls in parallel

Exactly one daemon runs; the others piggyback on the winner's socket.
This is by design — no race.

## "version mismatch: daemon is X, CLI is Y"

The binary was upgraded since the daemon started. The CLI tears down the
old daemon (SIGTERM, 1 s grace, SIGKILL) and starts a fresh one. Logs are
preserved across the restart but state-only sidecars (`.sock`, `.pid`,
`.version`) are recreated. This is automatic; the message is informational.

## `doctor` reports warnings: "≥ 10 short-lived daemons in the last hour"

The agent is using `agent-term spawn` for things that should be plain
`bash`. Sub-2-second commands have no reason to go through the daemon.
Check the agent's prompt; the spawn-everything pattern usually traces back
to an over-eager interpretation of "run this command".

## Permission denied on the state directory

Either `AGENT_TERM_STATE_DIR` points somewhere unwritable, or
`~/.agent-term` was created by another user. `agent-term doctor` will
surface this. Set `AGENT_TERM_STATE_DIR` to a writable path
(`/tmp/agent-term-$USER`, for example).

## `grep` pattern errors with "regex parse error"

The shell ate your regex. Two fixes:

- Single-quote the pattern: `--pattern '(?i)error|fail'`.
- For shell-hostile patterns, use `--pattern-file <path>` and put the
  regex in a file, one regex per call.

The regex engine is Rust's `regex` crate — full PCRE-lite syntax, no
backreferences. Use `(?i)` for case-insensitive, `(?m)` for multiline.

## The daemon is alive but `status` hangs

The Unix socket is wedged. Possible causes:

1. The daemon got SIGSTOPped.
2. A previous CLI process died holding a connection open.
3. The state directory is on a network filesystem with cache lag.

`doctor` won't help here; the only recovery is `kill -9 <daemon_pid>`
followed by `agent-term doctor --fix`. Use this as a last resort.

## "AGENT_TERM_LOG_SIZE=0 disables rotation" (it doesn't)

Setting `AGENT_TERM_LOG_SIZE=0` is treated as "use the default", not "no
rotation". To minimise rotation in practice, set a very large value
(`AGENT_TERM_LOG_SIZE=1073741824` = 1 GiB) and keep `AGENT_TERM_LOG_SEGMENTS=1`.

## Log file is bigger than `AGENT_TERM_LOG_SIZE` suggests

Rotation triggers *after* a write that crosses the threshold, not before.
The currently-rotating segment can briefly be `size + last_chunk` before
the next segment opens. This is benign; the next write rolls over.
