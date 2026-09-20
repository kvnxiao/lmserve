# Repository instructions

## 1. Load instructions for the task

Read [README.md](README.md) for implemented behavior, setup, and task commands.
Keep the primary CLI crate in `lmserve/` and target Linux and WSL2.

Before editing or reviewing, discover the available `*-rules` skills under
`.agents/skills/` and load every skill whose description covers a domain touched
by the task. Start with its `SKILL.md`, then read the references for the work
involved. Revisit skill selection when the scope expands.

| Domain | Skill entry point |
| --- | --- |
| Rust code, Cargo, tests, documentation, and tooling | [rust-rules](.agents/skills/rust-rules/SKILL.md) |
| GitHub Actions workflows | [github-actions-rules](.agents/skills/github-actions-rules/SKILL.md) |

For Rust CI changes, load both skills. Keep domain rules in their skills; keep
repository-specific instructions here.

## 2. Preserve application contracts

- Delegate ordinary Compose interpretation to an explicitly selected provider;
  validate `x-lmserve` metadata within the CLI.
- Admit at most one managed model in a user/runtime context. During a switch,
  confirm the old model has exited before launching another.
- Prepare images and model artifacts only through explicit update commands.
  Keep validation and planning free of deployment and download mutations.
- Distinguish accepted lifecycle work, container startup, and endpoint readiness.
  Use short-lived workers for accepted lifecycle operations.
- Record resource ownership and resolved image/artifact identities. Support
  inspection and shutdown when current configuration cannot be loaded.
- Preserve user configuration, companion volumes, unowned resources, and the
  last usable model when preparation fails.
- Pass subprocess arguments as separate values without evaluating host shell
  templates, and keep secrets out of diagnostic summaries.

## 3. Verify and report

Run `just --list` to select repository tasks. Before finishing, run the
`verify-changes` skill over the requested change set and follow its checks for
the change type. Use `just verify` for code and tooling changes; use the Rust
skill's MSRV checks when compatibility is affected. Report skipped or failed
checks and preserve unrelated user changes. Commit or push only when requested.
