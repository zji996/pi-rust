# rpi for delegate and mu (fork notes)

This fork builds the `rpi` binary that the [delegate](https://github.com/zji996/AIA-skills) skill
runs as its colleague and that the mu desktop drives over `--mode rpc`. It stays a thin patch stack
on top of upstream [bigfish1913/pi-rust](https://github.com/bigfish1913/pi-rust); everything else is
upstream as is.

Tuning that only makes sense for mu (the prompt's opening identity, tool-result notes, error hints,
host hooks) may live here when it cannot be done from the host, under three rules: one small commit
per patch that leaves the main loop alone; the default behaviour stays byte-for-byte upstream's, and
mu turns the patch on through a setting or flag, so delegate's binary is unaffected; anything
upstream would also want goes upstream as a PR first.

## Branches and tags

- `main` mirrors upstream `main`; nothing is committed to it here.
- `aia` is an upstream release tag (now `v0.14.0`) plus the patches below, one commit each.
- Releases are tagged `v<upstream version>-aia.<n>` (for example `v0.11.0-aia.1`) and publish
  `rpi-<tag>-x86_64-unknown-linux-gnu.tar.gz` and (from `v0.11.0-aia.3`)
  `rpi-<tag>-x86_64-pc-windows-msvc.zip`, each with its `.sha256`.

## Patches

| Patch | Why delegate needs it |
| --- | --- |
| `edit` matches like upstream and diagnoses like a compiler: candidate regions from the [pi-semantic-edit](https://github.com/k3-2o/pi-semantic-edit) (MIT) chain, plus `replaceAll` | Pi colleagues copy `oldText` from `read` output; the fork drops drift-tolerant matching and auto-expand (the model decides, not the tool) but keeps the chain for naming the closest real text, the differing lines and the exact text to copy. Only the bytes an edit matched are replaced: upstream rebuilds a normalized match's whole line from NFKC text, which turned untouched fullwidth （），into ASCII |
| `edit` merges the overlapping line rebuilds of one normalized `replaceAll`, and `splice` refuses invalid spans | a match ending inside an NFKC expansion (the `fi` of `deﬁne`) rebuilds its whole line per occurrence; overlapping rebuilds were silently skipped and then spliced out of bounds, so `edit` panicked instead of reporting an error. The overlapping rebuilds of one edit now merge into one region containing every occurrence, invalid splice spans become a normal `err: invalid replacement spans` failure, and the normalized byte→line lookup is built once per view instead of per occurrence |
| Tool schemas carry no developer notes | struct doc comments ("Mirrors TS `ReadToolInput`") were sent to the model as schema descriptions on every request |
| Shell output decodes across reads and in the console code page ([#177](https://github.com/bigfish1913/pi-rust/pull/177)) | each 8 KiB read was decoded on its own, so a character split across reads became `U+FFFD`; on Windows `cmd`, `ipconfig`, Python and Windows PowerShell write GBK/936 on a Chinese system and came out garbled. Now incremental, per line: UTF-8 stays UTF-8, other lines use the console code page (`chcp`) |
| `get_messages` compaction rows carry `summary` (the recap text) | the summary existed only in the session file, so a client could mark a compaction point but not show what the model reads instead; mu desktop opens it from the timeline seam. General: to be offered upstream |
| Native `google-generative-ai` provider (`crates/rpi-ai/src/providers/google/`, registered in `crates/rpi-runtime/src/provider/factories.rs`) | `Api::GoogleGenerativeAi` existed as a name only, so a `models.json` entry with `api: "google-generative-ai"` resolved to `UnsupportedApi` and Gemini models were unusable. Ported from upstream's `api/google-generative-ai.ts` + `api/google-shared.ts` (streaming, thinking levels/budgets, function calls with thought-signature replay, usage/finish-reason mapping). Upstream already has this feature in TypeScript, so this is a port rather than a new patch — nothing to send upstream unless the Rust port is wanted there |
| Host-declared tools over the RPC channel (`--host-tools <file>`, `crates/rpi-cli/src/host_tools.rs`, the `host_tool_result` command and the `host_tool_call`/`host_tool_cancelled` events), PR #189 | the fork is also a host: mu desktop drives `rpi --mode rpc` and needs tools only it can implement (open a file in the editor, ask the user to pick a target). `--host-tools` declares them in a JSON file; a call becomes a `host_tool_call` event settled by a `host_tool_result` command, an abort answers it and emits `host_tool_cancelled`, and `ready` advertises the command, both events and `hostTools`. Declarations are validated at startup (rpc mode only, `^[A-Za-z0-9_-]{1,64}$`, unique, no built-in/extension collision, object parameters) and obey `--tools`/`--exclude-tools`/`--no-tools`. Backported from upstream 69b5df2; when upstream merges and releases it, drop this commit from `aia` |
| Host-handled state paths (`--host-state <prefix>`, `crates/rpi-cli/src/host_state.rs`, the `host_state_result`/`host_state_config` commands and the `host_state_call`/`host_state_cancelled` events) | mu keeps plugin state (`ask`, `plan`) in memory, not on disk, and the model has to reach it with the ordinary tools: writing `.mu/state/ask/<topic>.md` must create a card, not a file. With `--host-state .mu/state`, `read`/`write`/`edit`/`ls` calls whose `path` normalizes inside the prefix never touch the disk — the call becomes a `host_state_call` event settled by a `host_state_result` command, an abort (or a 30-second timeout, raised per path by `host_state_config`) emits `host_state_cancelled`, and the tool list, schemas and every other path stay byte for byte upstream's (tests lock `ready`). RPC mode only, off by default, so delegate's binary is unaffected. Protocol of record: [docs/host-state.md](docs/host-state.md) |
| `systemPromptIntro` (string) and `systemPromptGuidelines` (string array) in `settings.json` (`crates/rpi-cli/src/session.rs`) | mu desktop drives rpi over `--mode rpc` and the agent directory's `settings.json` is the only channel it writes, so the prompt's opening identity and mu's own rules have to be settings rather than a `--system-prompt` file mu would have to keep in sync. `systemPromptIntro` replaces the first paragraph of the built-in prompt, `systemPromptGuidelines` are appended to the end of `Guidelines` with the same trim/dedupe the core rules get; both unset, the prompt is upstream's byte for byte (a test locks the default text). `--debug-system-prompt` prints the result |
| `toolResultNotes` (bool) in `settings.json` (`crates/rpi-tools/src/tools/bash.rs`) | a bash result that fails reads as a prose sentence (`Command exited with code 7`) glued to the output, so mu's timeline cannot tell the model's failure note from the command's own output and the model re-reads the whole tail to find the reason. With the setting on, the status becomes a trailing bracket note — `[exit code N]`, `[terminated by signal N]` (needs the new `ShellOutput.signal`, since a signal kill collapses to exit 0), `[cancelled]` — while the output stays intact and the result is still an error. Off, the text is upstream's byte for byte |
| `toolErrorHints` (bool) in `settings.json` (`crates/rpi-tools/src/tools/error_hints.rs`, wrapped in at tool construction in `crates/rpi-cli/src/session.rs`) | a small model that hits `path not found` or an `edit` `oldText not found` diagnostic tends to repeat the same call; one English next-step line after the error (path → `Use ls or find to check the path.`, edit → `Read the file, copy the original text exactly with enough context, then retry edit.`, timeout, invalid arguments) breaks that loop. The hint is appended on its own line and the error text is untouched, because the `edit` diagnostic quotes text the model must copy exactly; an abort or a plain non-zero exit gets no hint. The wrap happens where the tools are built, not in the agent loop. Off, the messages are upstream's byte for byte. This one upstream may also want, as an opt-in |
| `childAgentPrompt` (bool) in `settings.json` (`crates/rpi-cli/src/child_agents.rs`) | the subagent prompt orders a small model ("Complete the task in at most N turns. Do not …") and never says what to do when the task needs a decision it cannot get, so mu's children either ask the user (there is nobody to answer) or invent one silently. With the setting on the prompt follows Norvane's `child.preamble.md`: the child shares the parent's working directory, cannot ask the user, writes a `Needs decision: …` line in its final report and stops, and the turn budget stays as information. Off, upstream's text is byte for byte |
| `readImageDraft` (bool) in `settings.json` (`crates/rpi-tools/src/tools/read.rs`, `crates/rpi-tools/src/image_processing.rs`, wired in `crates/rpi-cli/src/session.rs`) | every image a `read` returns is resent with the whole history on every later turn, and a 2000×2000 picture is roughly 5000 Anthropic tokens even when a glance was enough; Norvane's 0117 decision ("看图先看全图，再按框放大") has `read` show the whole picture first and zoom on request. With the setting on, `read` returns an image as a ~0.4 MP draft (`w*h <= 400_000`, only shrunk, never enlarged; an image already that small keeps upstream's bytes) and its result text ends with one line — `Image 4000×3000, shown as a 730×547 draft. For detail, read again with box=[left,top,right,bottom] (0-1000, relative to the whole image); box=[0,0,1000,1000] is the full image at full clarity.` — while `box` crops that region out of the original resolution and takes the ordinary inline limits (2000 px a side, byte cap, no upscale), naming the pixels it shows and saying when that is the original resolution. EXIF orientation is applied first, so the draft size and the box refer to the upright image. `box` only exists in the schema while the setting is on, and a malformed `box` (not four numbers, out of 0-1000, `left >= right`) or a `box` on a text file fails the call with one line saying what to send instead. Off, the schema, the description, the result text and the image bytes are upstream's byte for byte (a test locks the schema) |

A patch that upstream would also want goes upstream as a PR; once merged it drops out of `aia`
(upstream 0.9.6 took the active-tools prompt and the display-diff fix; 0.11.0 took `--fork <file>`
outside the session directory, the once-only prompt guidelines and the session id as
`prompt_cache_key`, [#132](https://github.com/bigfish1913/pi-rust/pull/132); 0.13.0 took the bash
pipe drain, [#163](https://github.com/bigfish1913/pi-rust/pull/163); 0.14.0 took `compat` on
`anthropic-messages` models, `thinkingBudgets` and the `xhigh`/`max` budgets,
[#168](https://github.com/bigfish1913/pi-rust/pull/168)–[#170](https://github.com/bigfish1913/pi-rust/pull/170)).

## Following upstream

```sh
git fetch upstream --tags
git rebase --onto v<new> v<old> aia     # replay the patch stack on the new release
cargo test -p rpi-tools -p rpi-cli
```

Follow upstream when a release fixes something delegate hits, not on every release.

## Building a release

```sh
CARGO_INCREMENTAL=0 cargo build --release -p rpi-cli
```

The binary is `target/release/rpi`; package it as `rpi-<tag>-x86_64-unknown-linux-gnu.tar.gz` with
a `.sha256` next to it.

Windows is built natively on the LAN Windows 11 test machine (MSVC toolchain), from a `git bundle`
of the tagged commit; no hosted CI. Run `cargo test --workspace --locked` there first, then the
same release build, and smoke `rpi.exe --mode rpc` with an isolated `RPI_CODING_AGENT_DIR`. The zip
holds `rpi.exe` only, like the Linux tarball holds `rpi` only.
