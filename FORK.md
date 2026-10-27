# rpi for delegate (fork notes)

This fork builds the `rpi` binary that the [delegate](https://github.com/zji996/AIA-skills) skill
runs as its colleague. It stays a thin patch stack on top of upstream
[bigfish1913/pi-rust](https://github.com/bigfish1913/pi-rust); everything else is upstream as is.

## Branches and tags

- `main` mirrors upstream `main`; nothing is committed to it here.
- `aia` is an upstream release tag plus the patches below, one commit each.
- Releases are tagged `v<upstream version>-aia.<n>` (for example `v0.9.4-aia.1`) and publish
  `rpi-<tag>-x86_64-unknown-linux-gnu.tar.gz` with its `.sha256`.

## Patches

| Patch | Why delegate needs it |
| --- | --- |
| The default system prompt lists only the active tools | `--tools read,grep,find,ls` (read-only runs) otherwise tells the model it can edit, write and track todos |
| `--fork <file>` takes a session file outside the session directory | delegate forks a copy of the previous round's session kept in the run directory (Pi accepts any session file) |
| `edit` matches like upstream and diagnoses like a compiler: candidate regions from the [pi-semantic-edit](https://github.com/k3-2o/pi-semantic-edit) (MIT) chain, plus `replaceAll` | Pi colleagues copy `oldText` from `read` output; the fork drops drift-tolerant matching and auto-expand (the model decides, not the tool) but keeps the chain for naming the closest real text, the differing lines and the exact text to copy |
| The `edit` display diff shows replaced lines old then new | the TUI diff printed the new text on both sides of a replaced line (upstream PR pending) |
| Tool schemas carry no developer notes | struct doc comments ("Mirrors TS `ReadToolInput`") were sent to the model as schema descriptions on every request |
| The default prompt states each guideline once | the plan and todo rules repeated each other, and bash/docs advice appeared in two or three places (2181 → 1375 characters) |

A patch that upstream would also want goes upstream as a PR; once merged it drops out of `aia`.

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
