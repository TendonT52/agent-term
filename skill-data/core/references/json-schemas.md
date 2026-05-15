# JSON envelopes

Every verb that accepts `--json` emits a stable envelope. This document
shows the shape; the formal JSON Schemas live in `schemas/` at the repo
root and are referenced inline.

## tail

### Cursor / bounded read (default mode)

```json
{
  "cursor_start": 17000,
  "cursor": 18402,
  "lines_emitted": 12,
  "bytes_emitted": 1402,
  "content": "..."
}
```

### Stale cursor (past EOF after a rotation)

```json
{
  "cursor_start": 999999,
  "cursor": 50000,
  "cursor_stale": true,
  "stale_reason": "cursor past EOF (rotation or truncation)",
  "lines_emitted": 0,
  "bytes_emitted": 0,
  "content": ""
}
```

Reset to the returned `cursor` and continue. No bytes are lost — the
previous content lives in `<id>.log.1`, `<id>.log.2`, etc.

### Time-window mode (with `--since` / `--until`)

```json
{
  "since_ms": 1700000000000,
  "until_ms": 1700000060000,
  "lines_emitted": 42,
  "bytes_emitted": 5301,
  "content": "..."
}
```

Note: this variant has no `cursor` field — time-window mode reads what
matches the window, not "everything since a byte offset". To resume
polling after a time-window call, use a fresh `--since-cursor $LAST`
call against the cursor mode.

Schema: `schemas/tail.schema.json`.

## wait

Three outcomes, distinguished by `matched` and `reason`:

```json
{ "matched": true,  "line": "READY", "elapsed_ms": 423 }
{ "matched": false, "reason": "timeout",        "elapsed_ms": 30000 }
{ "matched": false, "reason": "process_exited", "code": 1, "elapsed_ms": 187 }
{ "matched": false, "reason": "error",          "error": "invalid pattern: ..." }
```

Exit codes mirror this — 0 on match, 1 on timeout, 2 on process exit,
non-zero on argument or filesystem error.

Schema: `schemas/wait.schema.json`.

## grep

Blocks of matches with surrounding context. Overlapping windows are merged.

```json
{
  "hits": 2,
  "blocks": [
    {
      "start_line_no": 4,
      "end_line_no": 9,
      "matches": [
        { "line_no": 5, "timestamp_ms": 1700000000123 }
      ],
      "lines": [
        { "line_no": 4, "is_match": false, "content": "..." },
        { "line_no": 5, "is_match": true,  "content": "ERROR ..." },
        { "line_no": 6, "is_match": false, "content": "..." },
        { "line_no": 7, "is_match": false, "content": "..." },
        { "line_no": 8, "is_match": false, "content": "..." }
      ]
    }
  ]
}
```

- `end_line_no` is half-open — the block covers `[start_line_no, end_line_no)`,
  matching Python slicing convention.
- `timestamp_ms` is null on hits in non-timestamped logs.
- `lines` includes every context line; `matches` is the subset where
  `is_match=true`.

Schema: `schemas/grep.schema.json`.

## slice

### Cursor mode

```json
{
  "from_cursor": 17204,
  "to_cursor": 18012,
  "lines_emitted": 6,
  "bytes_emitted": 808,
  "content": "..."
}
```

### Time mode

```json
{
  "from_ms": 1700000030000,
  "to_ms":   1700000060000,
  "lines_emitted": 12,
  "bytes_emitted": 1402,
  "content": "..."
}
```

Schema: `schemas/slice.schema.json`.

## summary

```json
{
  "schema_version": 1,
  "id": "abc123def456",
  "name": "api",
  "project": "/Users/x/repo",
  "state": "running",
  "child_pid": 12345,
  "exit_code": null,
  "started_at": 1700000000,
  "uptime_ms": 145200,
  "log_bytes": 17204882,
  "log_lines": 92113,
  "segments": 2,
  "last_line_age_ms": 312,
  "tail_cursor": 17204882,
  "recent": {
    "since_ms": 60000,
    "lines_scanned": 218,
    "errors": 4,
    "warnings": 17,
    "mode": "time-window"
  }
}
```

- `state` is one of `"running"`, `"exited"`, `"unknown"`.
- `exit_code` is set only when `state == "exited"`.
- `recent.mode` is `"time-window"` when the log is timestamped (precise),
  `"tail-bytes"` when it falls back to scanning the most recent bytes.

Schema: `schemas/summary.schema.json`.

## list

```json
[
  {
    "schema_version": 1,
    "id": "abc123def456",
    "name": "api",
    "project": "/Users/x/repo",
    "tags": { "env": "staging" },
    "cmd": ["python", "api.py"],
    "cwd": "/Users/x/repo",
    "started_at": 1700000000,
    "started_by_pid": 12345,
    "log_path": "/Users/x/.agent-term/abc123def456.log",
    "state": "running",
    "child_pid": 12346,
    "exit_code": null,
    "uptime_ms": 145200
  }
]
```

Schema: `schemas/list-entry.schema.json` (single-entry schema; the
top-level value is an array of these).

## doctor

```json
{
  "live": [
    {
      "id": "abc123",
      "daemon_pid": 12345,
      "child_pid": 12346,
      "name": "api",
      "project": "/Users/x/repo"
    }
  ],
  "stale": [
    { "id": "def456", "reason": "daemon pid 9999 not alive" }
  ],
  "orphans": [
    { "id": "ghi789", "child_pid": 22001 }
  ],
  "warnings": [
    "≥ 10 short-lived daemons in the last hour; misuse heuristic triggered"
  ]
}
```

Schema: `schemas/doctor.schema.json`.

## status

The plain-text `status` verb prints a single-line summary; with `--json`:

```json
{ "state": "running", "child_pid": 12346, "code": null, "uptime_ms": 14520 }
```

Or, on an exited daemon still in its 2 s linger:

```json
{ "state": "exited", "child_pid": null, "code": 1, "uptime_ms": 14520 }
```

## Schema versioning

Schemas in `schemas/*.json` include `$id` URLs and pin to JSON Schema
Draft 2020-12. The `schema_version` field on each envelope is bumped on
backwards-incompatible changes; additive changes keep the version stable.
