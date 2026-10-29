# rpi for delegate (fork notes)

This fork builds the `rpi` binary that the [delegate](https://github.com/zji996/AIA-skills) skill
runs as its colleague. It stays a thin patch stack on top of upstream
[bigfish1913/pi-rust](https://github.com/bigfish1913/pi-rust); everything else is upstream as is.

## Branches and tags

- `main` mirrors upstream `main`; nothing is committed to it here.
- `aia` is an upstream release tag (now `v0.12.0`) plus the patches below, one commit each.
- Releases are tagged `v<upstream version>-aia.<n>` (for example `v0.11.0-aia.1`) and publish
  `rpi-<tag>-x86_64-unknown-linux-gnu.tar.gz` and (from `v0.11.0-aia.3`)
  `rpi-<tag>-x86_64-pc-windows-msvc.zip`, each with its `.sha256`.

## Patches

| Patch | Why delegate needs it |
| --- | --- |
| `edit` matches like upstream and diagnoses like a compiler: candidate regions from the [pi-semantic-edit](https://github.com/k3-2o/pi-semantic-edit) (MIT) chain, plus `replaceAll` | Pi colleagues copy `oldText` from `read` output; the fork drops drift-tolerant matching and auto-expand (the model decides, not the tool) but keeps the chain for naming the closest real text, the differing lines and the exact text to copy. Only the bytes an edit matched are replaced: upstream rebuilds a normalized match's whole line from NFKC text, which turned untouched fullwidth （），into ASCII |
| `edit` merges the overlapping line rebuilds of one normalized `replaceAll`, and `splice` refuses invalid spans | a match ending inside an NFKC expansion (the `fi` of `deﬁne`) rebuilds its whole line per occurrence; overlapping rebuilds were silently skipped and then spliced out of bounds, so `edit` panicked instead of reporting an error. The overlapping rebuilds of one edit now merge into one region containing every occurrence, invalid splice spans become a normal `err: invalid replacement spans` failure, and the normalized byte→line lookup is built once per view instead of per occurrence |
| `bash` reads output while the command runs | upstream awaited the child before polling stdout/stderr, so output over the 64 KiB pipe buffer (a broad `rg docs/`) deadlocked until aborted; merged upstream as [#163](https://github.com/bigfish1913/pi-rust/pull/163) after v0.12.0, drops out with the next release |
| Tool schemas carry no developer notes | struct doc comments ("Mirrors TS `ReadToolInput`") were sent to the model as schema descriptions on every request |
| `anthropic-messages` models keep their `compat` from `models.json` ([#168](https://github.com/bigfish1913/pi-rust/pull/168)) | `compat` was parsed only for the OpenAI protocols, so `forceAdaptiveThinking` and the other Anthropic flags were dropped silently; a relay that rejects some `budget_tokens` values had no adaptive-thinking escape |
| `thinkingBudgets` from `settings.json` reaches the providers ([#169](https://github.com/bigfish1913/pi-rust/pull/169)) | the providers honoured `thinking_budgets` but nothing set it, so the per-level budgets could not be moved off a range a gateway rejects |
| `xhigh` and `max` get their own thinking budgets ([#170](https://github.com/bigfish1913/pi-rust/pull/170)) | budget-based thinking clamped both to `high`, so on models offering them they sent the `high` budget; now 32768 / 65536 by default, settable in `thinkingBudgets`, still `high` on models that don't offer them |
| Native `google-generative-ai` provider (`crates/rpi-ai/src/providers/google/`, registered in `crates/rpi-runtime/src/provider/factories.rs`) | `Api::GoogleGenerativeAi` existed as a name only, so a `models.json` entry with `api: "google-generative-ai"` resolved to `UnsupportedApi` and Gemini models were unusable. Ported from upstream's `api/google-generative-ai.ts` + `api/google-shared.ts` (streaming, thinking levels/budgets, function calls with thought-signature replay, usage/finish-reason mapping). Upstream already has this feature in TypeScript, so this is a port rather than a new patch — nothing to send upstream unless the Rust port is wanted there |

A patch that upstream would also want goes upstream as a PR; once merged it drops out of `aia`
(upstream 0.9.6 took the active-tools prompt and the display-diff fix; 0.11.0 took `--fork <file>`
outside the session directory, the once-only prompt guidelines and the session id as
`prompt_cache_key`, [#132](https://github.com/bigfish1913/pi-rust/pull/132)).

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
