# Contributing to meguri

Thanks for considering a contribution. meguri is a single-maintainer,
best-effort open-source project, but issues and PRs from humans are welcome
and get reviewed like any other contribution.

## Reporting bugs / requesting features

Just open a regular GitHub issue — describe the bug or the feature you want.
You don't need to think about meguri's own `meguri:*` labels; that's internal
bookkeeping the maintainer (or meguri itself) applies, not something
contributors are expected to touch. See [Labels](#labels) below for why an
unlabeled issue is exactly the right state for a fresh report.

Found a **security vulnerability**? Don't open a public issue — see
[SECURITY.md](SECURITY.md) instead.

## Submitting changes

Normal fork & PR flow:

1. Fork the repository, create a branch, commit your changes.
2. Open a pull request against `main`.
3. CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo nextest run` (+ `cargo test --doc`), and `cargo deny check` (see
   [.github/workflows/ci.yml](.github/workflows/ci.yml)) — make sure these
   pass locally first (see [Development environment](#development-environment)).

Branch names starting with `meguri/` are reserved for worktrees meguri's own
loops create — please use a normal branch name for your PR so the two don't
collide. Beyond that, there's nothing special about contributing here versus
any other Rust project: no CLA, no meguri-specific ritual.

## Labels

meguri's issue labels are its own internal state machine for the agent loops
(two axes — phase and ball; see the [Labels section of the
README](README.md#labels) and [ADR 0005](docs/adr/0005-issue-labels-two-axis-phase-and-ball.md)
for the full model). The one fact that matters for contributors:

**An unlabeled issue means exactly one thing — untriaged.** Your bug report
or feature request sits unlabeled until a maintainer decides what to do with
it; that's normal, not neglect. There's no separate issue tracker or process
for "issues meant to feed a loop" versus "issues from humans" — it's the same
tracker, and only a label (`meguri:plan` / `meguri:ready`) turns one into
loop input. Applying that label needs collaborator (write) access to the
repo, which is also why it's safe to describe a bug in as much detail as you
like: an agent only ever acts on an issue once a maintainer has explicitly
opted it in (see the [Security](README.md#security) section of the README
for the full reasoning — issue bodies are prompt input to an agent with real
shell access, so that label is a deliberate trust gate, not a formality).

So: open issues freely, and leave the `meguri:*` labels alone.

## Development environment

Prereqs: a Rust toolchain matching `rust-version` in `Cargo.toml` (currently
1.89+), `git`. `tmux` is needed for the integration tests to actually run
(they skip gracefully without it).

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
```

Two more test suites are gated behind environment variables because they
drive real external processes and are **not** run in CI:

```bash
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration; needs a live herdr socket
MEGURI_TEST_CLAUDE=1 cargo test     # + a real `claude` CLI e2e run in a real tmux pane
                                     # (needs `claude` + tmux on PATH, spends real Claude usage)
```

Before opening a PR, it's worth running what CI runs so review round-trips
don't get spent on formatting/lint noise:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo nextest run   # or: cargo test
cargo test --doc
cargo deny check     # advisories / crates.io source / licenses / bans (needs cargo-deny)
```

### Platform support

Core meguri (CLI, `watch`, all the agent loops) runs on **macOS and Linux**.
`meguri daemon install` (the `launchd`-based supervisor for surviving
logout/reboot) is **macOS-only** — other platforms get an explicit error
rather than a silent no-op; `meguri daemon start` (detached, no OS
supervision) works everywhere `tmux`/`herdr` does. Windows isn't supported.

## Documentation language policy

- **README** is bilingual: [README.md](README.md) (English) is canonical,
  [README.ja.md](README.ja.md) is the Japanese counterpart. If you edit one
  in a way that changes meaning, please update the other too (a PR that only
  updates one is still fine — the maintainer will catch up the other side).
- **Internal documentation** — ADRs (`docs/adr/`), specs (`docs/specs/`),
  ops notes (`docs/ops/`) — is written in **Japanese**, since that's the
  maintainer's working language and where meguri's own design decisions get
  recorded.
- Contributor-facing entry points (this file, `SECURITY.md`, issue/PR
  discussion) stay in **English** so they're accessible to outside
  contributors regardless of the internal docs' language. Feel free to open
  an issue or PR in Japanese too — it'll just be understood natively rather
  than needing translation.
