# Codex-Link Development

This document is the local development entrypoint for the Codex-Link fork.

## Repository Layout

- `codex-rs/`: Rust workspace and native CLI implementation.
- `codex-cli/`: npm package wrapper that dispatches to platform binaries.
- `docs/`: upstream docs plus Codex-Link fork notes.
- `justfile`: repository task runner. It defaults its working directory to
  `codex-rs/`.

## Remotes

The local checkout should treat OpenAI's repository as upstream:

```bash
git remote -v
# upstream  https://github.com/openai/codex.git (fetch)
# upstream  https://github.com/openai/codex.git (push)
```

After creating the public fork, add it as `origin`:

```bash
git remote add origin git@github.com:<owner>/Codex-Link.git
git push -u origin link/main
```

## Toolchain

Follow upstream's Rust toolchain setup in `docs/install.md`. The main commands
used by this fork are:

```bash
cargo install --locked just
cargo install --locked cargo-nextest
rustup component add rustfmt clippy
```

## Common Commands

Run these from the repository root:

```bash
just fmt-check
just fmt
just clippy -p codex-cli
just test -p codex-cli
cargo build --manifest-path codex-rs/Cargo.toml -p codex-cli
```

Use a narrower package when possible. Run the full test suite only when changes
touch shared crates such as `codex-core`, `codex-protocol`, config loading,
session persistence, MCP, plugin handling, or app-server contracts.

## First Smoke Check

After a successful build:

```bash
cargo run --manifest-path codex-rs/Cargo.toml --bin codex -- --help
cargo run --manifest-path codex-rs/Cargo.toml --bin codex -- exec --help
```

These checks avoid network calls and verify that the local binary starts.

## Patch Discipline

- Keep Link-specific behavior in new modules or docs when practical.
- Avoid editing high-churn upstream files unless the user-visible behavior
  requires it.
- If a file is modified for fork behavior, mention Codex-Link in the commit or
  nearby documentation so Apache-2.0 modified-file notice requirements are easy
  to audit.
- Do not publish npm packages under `@openai/*`.
