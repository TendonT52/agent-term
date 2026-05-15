# Scenario playbooks

Concrete recipes for the situations agents hit most. Each one is a
self-contained shell snippet. Adapt the names and patterns; keep the
structure.

## npm / Vite / Next.js dev server

The most common case: bring up a dev server, wait for "ready", incrementally
re-read after each edit.

```bash
ID=$(agent-term spawn --name web --timestamps -- npm run dev)
agent-term wait $ID --pattern 'ready in [0-9]+\s*ms' --timeout 60s

# After editing a file:
agent-term grep $ID --pattern '(?i)error|✘|✗' --since 30s --around 5 --strip-ansi

# Incremental check ("what's new since last time"):
RESP=$(agent-term tail $ID --since-cursor $LAST --json)
LAST=$(echo "$RESP" | jq -r .cursor)
echo "$RESP" | jq -r .content
```

Vite, Next.js, Remix, SvelteKit, Astro all match the same shape. The
readiness pattern differs:

| Tool | Readiness pattern |
|---|---|
| Vite | `ready in [0-9]+\s*ms` |
| Next.js (dev) | `ready (started server\|in [0-9]+)` |
| Remix | `Remix App Server started` |
| Webpack | `compiled successfully` |
| Rollup | `created .* in [0-9]+ms` |

## Backend service: readiness patterns

Match the terminal "socket is bound" line, not the activity banner.
`Starting server...` will fire any `(?i)server` alternation before the
port is actually open — see R2.5 in the core skill.

| Server | Readiness pattern |
|---|---|
| Uvicorn (FastAPI/Starlette) | `Uvicorn running on http://[^ ]+` |
| Gunicorn | `Listening at: http://[^ ]+` |
| Hypercorn | `Running on http://[^ ]+` |
| Flask dev server | `Running on http://127\.0\.0\.1:\d+` |
| Django runserver | `Starting development server at http://[^ ]+/` |
| Rails (Puma) | `Listening on (tcp\|http)://[^ ]+` |
| Express / Node | `listening on (port \|:)?\d+` (varies — anchor on the bound port) |
| Go `net/http` | custom; print and match e.g. `^server bound on :8080$` |
| Spring Boot | `Tomcat started on port\(s\): \d+` |
| Postgres (in compose logs) | `database system is ready to accept connections` |
| Redis | `Ready to accept connections` |
| MySQL | `ready for connections.* port: \d+` |

```bash
ID=$(agent-term spawn --name api --timestamps -- uvicorn app:api --port 8000)
agent-term wait $ID --pattern 'Uvicorn running on http://[^ ]+' --timeout 30s
```

If a server doesn't print its bound port distinctly, add one log line at
the end of your own startup code that does (e.g. `print(f"server bound on :{port}", flush=True)`)
and match that — much cheaper than fighting framework banners.

## Backend service incident investigation

You have a running service. Something is wrong. Triage:

```bash
agent-term summary $ID --recent-window 5m
# → tells you how many errors fired, when the last line was, etc.

agent-term grep $ID --pattern '^ERROR|^Exception|^Traceback' \
                --around 30 --limit 3 --since 5m --json

# Drill into a specific moment:
agent-term slice $ID --from "14:31:50 ago" --to "14:32:30 ago" --json
```

The `summary` call is cheap (~200 tokens). Run it first to decide whether
you need to read deeply at all. If `recent.errors == 0` and
`last_line_age_ms > 60000` the daemon is probably idle, not broken.

## Docker Compose (one daemon per service)

Multi-stream is "many daemons, one per service" — not one combined daemon.
Naming after the service makes the verbs read naturally.

```bash
agent-term spawn --name api -- docker compose logs -f api
agent-term spawn --name db  -- docker compose logs -f db
agent-term spawn --name web -- docker compose logs -f web

# Per-service investigation:
agent-term grep db --pattern '(?i)migration|FATAL' --around 10
agent-term tail api --since 5m
```

Names are unique within a project, so `agent-term tail api` resolves to
the same daemon as `agent-term tail $ID` once you've spawned it. The id
is canonical; the name is convenient.

## CI / build pipeline

Build outputs are bimodal: 50 MB of noise, errors near the end. Read
end-to-start.

```bash
# 1. Is the failure visible right at the bottom?
agent-term tail $ID --reverse --lines 200 --strip-ansi

# 2. First failure block with surrounding context:
agent-term grep $ID --pattern '(?i)^(error|fatal|fail)' \
                --around 15 --limit 1 --strip-ansi --json
```

For test runners specifically (pytest, jest, vitest, cargo test), look
for the summary line near the end:

```bash
agent-term grep $ID --pattern '([0-9]+) (passed|failed|skipped)' \
                --reverse --limit 1 --json
```

## Cross-session: spawn here, observe there

```bash
# Terminal A:
ID=$(agent-term spawn --name dev --timestamps -- npm run dev)
echo $ID > /tmp/dev.id

# Terminal B, possibly hours later:
ID=$(cat /tmp/dev.id)
agent-term summary $ID                       # is it still alive?
agent-term tail $ID --since 1m               # what did it just print?
```

`list` from any shell in the same project shows the same daemons. The
state directory (`~/.agent-term/`) is shared.

## Cursor-driven polling (the right way to watch growth)

```bash
CURSOR=0
while sleep 1; do
  RESP=$(agent-term tail $ID --since-cursor $CURSOR --json)
  CURSOR=$(echo "$RESP" | jq -r .cursor)
  CONTENT=$(echo "$RESP" | jq -r .content)
  [ -n "$CONTENT" ] && echo "$CONTENT"

  # Optional: bail out if the daemon died
  agent-term status $ID 2>/dev/null | grep -q running || break
done
```

The cursor caps memory and CPU per call to "exactly the new bytes". Do
**not** write a `while :; do tail; sleep; done` loop without `--since-cursor` —
it scales O(log_size × poll_count).

If `cursor_stale: true` ever comes back, just set `CURSOR` to the returned
`cursor` value and keep going.

## Long-running training / batch jobs

For multi-hour jobs the polling loop is the same, but the readiness
"signal" is harder. Three useful checkpoints:

```bash
# 1. Did it crash before the first epoch?
agent-term wait $ID --pattern 'epoch 1|step [0-9]+' --timeout 5m

# 2. Periodic progress check from a polling loop:
agent-term tail $ID --reverse --lines 20 --strip-ansi

# 3. Is it actually still writing, or stalled?
agent-term summary $ID --json | jq '.last_line_age_ms'
# > 300000 (5 minutes since last output) → probably hung
```

## Watching a config or filesystem watcher

Lots of tools print "watching for changes" once at startup, then nothing
until something happens. The right primitive is `tail --since-cursor`
gated on `status`:

```bash
# Trigger the change
touch src/index.ts

# Read what the watcher emitted in response
agent-term tail $ID --since-cursor $LAST --json | jq -r .content
```

Avoid `wait` for this — there's no fixed pattern to match. Use cursor
polling and inspect the diff yourself.
