# Host-handled state paths (`--host-state`)

Protocol of record for the `--host-state <prefix>` patch. A host that drives
`rpi --mode rpc` (mu desktop) can keep some project paths in its own memory — a
plan card, a question to the user, a timeline — and have the engine route the
model's `read`/`write`/`edit`/`ls` calls on those paths to the host instead of
the disk.

The flag is off by default: without it nothing in this document happens, the
engine's behaviour and `ready` payload are upstream's byte for byte (tests lock
both), and delegate's binary is unaffected.

## Startup

```sh
rpi --mode rpc --host-state .mu/state
```

- `--mode rpc` only. Any other mode fails at startup with
  `Invalid --host-state: --host-state requires --mode rpc` (exit code 2).
- The prefix is a directory relative to the working directory (mu passes
  `.mu/state`). An absolute path is accepted when it still points inside the
  working directory. The prefix must be a strict subdirectory of the working
  directory: `.`, `..`, and anything that normalizes outside it are rejected at
  startup.
- The tool list, tool names, schemas, descriptions and replay policies are
  unchanged. Only dispatch differs, so the provider prefix cache is unaffected.

## Which calls are forwarded

Each call's `path` argument is normalized **lexically** — the working directory
is prepended for a relative path, `.`/`..` segments are resolved, `/` and `\` are
both separators, a leading `@` is dropped — and then compared component-wise with
the normalized prefix. Nothing is read from or written to the disk to decide
this, so a state path that does not exist on disk still matches, and a `..` that
leaves the prefix is treated as an ordinary path outside it.

| Model writes | Inside `.mu/state`? | Forwarded as |
| --- | --- | --- |
| `.mu/state/plan.md` | yes | `plan.md` |
| `./.mu/state/ask/topic.md` | yes | `ask/topic.md` |
| `.mu\state\ask\topic.md` | yes | `ask/topic.md` |
| `/abs/project/.mu/state/plan.md` | yes | `plan.md` |
| `.mu/state` | yes | `` (empty: the prefix itself, as `ls` on it) |
| `.mu/state/../other/x.md` | no | disk, unchanged |
| `.mu/state-evil/x.md` | no | disk, unchanged |
| `README.md` | no | disk, unchanged |

Two lexical rules keep the match aligned with the real tools:

- the comparison is case-insensitive on Windows and macOS, because those
  filesystems are (`.MU/state` is routed, not written to disk);
- symlinks are not resolved. A state path reached through a symlink outside the
  prefix is a disk path, and a prefix reached through a symlink is matched only
  in the form that was written.

A forwarded call never touches the disk. Every other path runs the built-in tool
exactly as before, and an `ls` without a `path` argument lists the working
directory (not the host).

`edit` forwards one replacement per call. A call carrying more than one entry in
`edits` on a state path fails without reaching the host:
`Host state paths take one replacement per call: send each oldText/newText as its
own edit.` (Small state files make this a non-issue; it keeps the wire format
lossless.)

## Events (engine → host)

`ready` advertises the feature when the flag is given:

```json
{
  "type": "ready",
  "...": "the usual payload",
  "supportedCommands": ["...", "host_state_result", "host_state_config", "..."] ,
  "supportedEvents": [
    "session_entry_committed", "host_tool_call", "host_tool_cancelled",
    "host_state_call", "host_state_cancelled"
  ],
  "hostState": {"prefix": ".mu/state", "timeoutMs": 30000, "waitTimeoutMs": 30000}
}
```

### `host_state_call`

One model call on a state path.

| Field | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | `"host_state_call"` | always | |
| `requestId` | string | always | Settle it with `host_state_result`; unique per call |
| `toolCallId` | string | always | The model's tool call id, for UI correlation |
| `tool` | `"read"` \| `"write"` \| `"edit"` \| `"ls"` | always | Which built-in tool was called |
| `path` | string | always | Relative to the prefix, `/`-separated (`ask/topic.md`); `""` is the prefix itself |
| `content` | string | `write` only | The text the model wrote, verbatim |
| `oldText` | string | `edit` only | The replacement's original text, verbatim |
| `newText` | string | `edit` only | The replacement text, verbatim |
| `replaceAll` | bool | `edit`, when the model set it | Replace every occurrence |
| `offset` | number | `read`, when the model set it | First line to return (1-indexed) |
| `limit` | number | `read`/`ls`, when set | Maximum lines (`read`) or entries (`ls`) |

Optional arguments the model did not set are omitted, not sent as `null`.

```json
{"type":"host_state_call","requestId":"9f2…","toolCallId":"call_7","tool":"write","path":"ask/topic.md","content":"What next?\n"}
```

### `host_state_cancelled`

The engine no longer waits for that request: the run was aborted, the wait timed
out, or the RPC loop closed. The host should drop the call. A `host_state_result`
for a cancelled (or timed-out) `requestId` is rejected as unknown.

```json
{"type":"host_state_cancelled","requestId":"9f2…"}
```

## Commands (host → engine)

### `host_state_result`

The answer to one `host_state_call`.

| Field | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | `"host_state_result"` | always | |
| `requestId` | string | always | The `host_state_call` it settles |
| `text` | string | optional, `""` when absent | Returned to the model as the tool result, verbatim |
| `isError` | bool | optional, `false` when absent | `true` turns the call into a failed tool call carrying `text` |

The engine answers `{"type":"response","command":"host_state_result",...}` only
when the command carries an `id`; without one a rejection is written to stderr.
An unknown or already settled `requestId` is an error for the host
(`unknown host state request \`<id>\``), because its call is not waiting any
more.

### `host_state_config`

Optional, sent once after `ready` and before the first prompt. Every field is
optional; only the fields present change.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `type` | `"host_state_config"` | | |
| `timeoutMs` | number ≥ 1 | `30000` | How long the engine waits for a `host_state_result` |
| `waitTimeoutMs` | number ≥ 1 | `30000` | The longer limit for reads of a `waitPaths` path |
| `waitPaths` | string array | `[]` | Paths relative to the prefix whose calls may wait longer, e.g. `["ask"]` |

A `waitPaths` entry matches itself and everything under it (`ask` matches `ask`
and `ask/topic.md`, not `asks/x.md`). Paths are compared lexically after
normalization, so `"/ask/"` and `ask` are the same entry.

The response, when the command carries an `id`, echoes the applied settings:

```json
{"type":"response","id":3,"command":"host_state_config","success":true,
 "data":{"accepted":true,"advertisement":{"prefix":".mu/state","timeoutMs":1000,"waitTimeoutMs":5000}}}
```

## Timeouts and failures

- No answer within the limit: the call fails with one English line and the round
  trip continues, e.g.
  `host state read \`ask/topic.md\` got no answer within 30000 ms`, and
  `host_state_cancelled` is emitted for that request.
- An abort fails the call with
  `host state read \`ask/topic.md\` cancelled` and emits `host_state_cancelled`.
- The RPC loop ending (stdin EOF / `stop`) fails every in-flight call with
  `host disconnected`.
- `--host-state` is not enabled: `host_state_result`/`host_state_config` are not
  advertised, and either command is answered with the error
  `--host-state is not enabled`.

## Notes for the host

- `.mu/state/` is never on disk, so snapshots and `bash` do not see it; the model
  uses `read`/`write`/`edit`/`ls` only.
- Writes are commits: validate the format and answer with a one-line error
  (`isError: true`) that tells the model the exact shape to send instead.
- Reads may block for the user (`ask`); declare them through `waitPaths` and a
  `waitTimeoutMs` larger than `timeoutMs`, or they time out like any other call.
- A result arriving after a cancellation or a timeout is rejected; do not retry
  it with the same `requestId`.

## Where the patch lives

| File | What |
| --- | --- |
| `crates/rpi-cli/src/host_state.rs` | Bridge, path classification, tool wrapper, command payloads |
| `crates/rpi-cli/src/modes.rs` | `host_state_call`/`host_state_cancelled` relay, `host_state_result`/`host_state_config` commands |
| `crates/rpi-cli/src/rpc/protocol.rs` | `ready` advertisement, command variants |
| `crates/rpi-cli/src/session.rs` | `load` at startup, wrapping in `build_tools`, `/reload` |
| `crates/rpi-cli/src/args.rs`, `crates/rpi-cli/src/app.rs` | Flag, help text, startup error |

Follow-up work upstream will collide in `session.rs` (the `build_tools` signature
and the two wrap points) and in `protocol.rs` (`ready`'s signature). The rest is
a new module.
