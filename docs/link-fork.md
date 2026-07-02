# Codex-Link Fork

Codex-Link is an independent fork of OpenAI Codex CLI. The fork starts from the
official repository and keeps upstream history intact so fixes can be merged
forward, but it uses its own public review and release process.

## Why Fork

The upstream repository states that unsolicited code contributions are closed by
default. That is a reasonable product choice for OpenAI, but it is a poor fit
for a community-maintained command-line agent where users need fast local
repairs, transparent extension points, and reviewable patches.

Codex-Link exists to make those patches land in a normal open-source workflow.

## Principles

1. Keep upstream mergeable. Prefer small, isolated changes over broad rewrites.
2. Prove behavior at runtime. Favor smoke tests, logs, and reproducible commands
   over speculative refactors.
3. Do not grow `codex-rs/core` by default. Add focused crates or use existing
   extension crates unless core ownership is genuinely required.
4. Preserve user control. Configuration, providers, MCP, plugins, and local
   state should be inspectable and recoverable without hidden server behavior.
5. Keep distribution honest. Do not publish under upstream package names or imply
   OpenAI endorsement.

## Initial Scope

Milestone 0: fork hygiene

- Preserve upstream license and NOTICE attribution.
- Rename the local upstream remote to `upstream`.
- Document the fork workflow and local verification commands.
- Keep the first patch documentation-only.

Milestone 1: developer operability

- Add a repeatable local smoke script for build, help text, and non-interactive
  execution.
- Add diagnostics for common installation, plugin, MCP, provider, and state-db
  failures.
- Make failure output actionable enough to fix a broken local install without
  reading source.

Milestone 2: extension reliability

- Stabilize plugin and MCP health checks.
- Add clear contracts for local skills, connectors, and provider profiles.
- Prefer additive fork commands over invasive changes to existing upstream
  behavior.

Milestone 3: distribution

- Choose fork package names and binary names before the first public release.
- Publish signed release artifacts only after the local build path is
  reproducible.
- Keep release notes split into upstream syncs and Codex-Link changes.

## Upstream Sync Policy

Use `upstream` for OpenAI's repository and reserve `origin` for the public
Codex-Link fork:

```bash
git remote -v
git fetch upstream
git merge upstream/main
```

When a merge conflicts with Link changes, prefer moving Link behavior into
smaller files or modules instead of repeatedly editing high-churn upstream
files.

## Contribution Policy

Codex-Link should accept normal issue-driven pull requests once the public
repository exists. Until a formal governance file is added, every patch should:

- explain the user-visible problem,
- include the command used to verify the change,
- avoid unrelated formatting churn,
- preserve Apache-2.0 attribution and the upstream NOTICE content.
