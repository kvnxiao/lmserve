# lmserve

`lmserve` is a Rust CLI project for preparing and serving local language models
with rootless Podman on Linux and WSL2. The planned CLI selects one model from
user-owned Compose definitions and coordinates its server and optional companions.

The repository currently contains a buildable workspace skeleton. The binary
reports that commands are not implemented and exits with failure.

## Development

Install rustup, `just`, Bash, and `jq`, then run these commands from the repository
root:

```sh
rustup toolchain install stable --profile minimal --component clippy
rustup toolchain install nightly --profile minimal --component rustfmt
cargo +stable install --locked cargo-audit cargo-machete
just --list
just verify
just check-msrv lmserve
```

`rust-toolchain.toml` selects floating stable Rust for builds, tests, and Clippy.
The workspace uses edition 2024 and declares Rust 1.98.1 as its minimum supported
version. `just check-msrv PACKAGE` reads the member's resolved minimum version from
Cargo metadata and installs that compiler for a separate compatibility check.

Nightly rustfmt enforces one imported item per `use` statement, one sorted import
group, comment normalization, and Unix newlines. The rust-analyzer settings in
`.vscode/settings.json` select nightly formatting. Other editors must invoke
`rustup run nightly rustfmt`.

The workspace manifest contains the Rust, Clippy, and rustdoc lint policy. Printing
requires a scoped expectation in CLI output functions. Test contexts permit
`expect`, printing, indexing, slicing, and explicit panic; `unwrap` remains denied.

- `just fix` applies Clippy fixes, formats Rust source, and runs strict lint checks.
- `just lint` checks formatting, Clippy, and rustdoc warnings.
- `just doc` builds documentation with warnings treated as errors.
- `just test` runs the workspace tests, including any doctests.
- `just dependencies` checks advisories and unused dependencies.
- `just build` builds all workspace targets.
- `just verify` runs lint checks, tests, dependency checks, and builds.

CI invokes the shared verification task and checks each member's minimum compiler
version in a separate job. Dependency auditing fetches the advisory database and
requires network access. The test harness currently contains no application tests.
Release builds retain line-table debug information for profilers and backtraces.

Podman, a Compose provider, `hf`, and NVIDIA tooling are intended runtime
dependencies. They are not required to build or check this skeleton. Serving and
WSL2 compatibility have not been tested.

## Workspace

- `Cargo.toml` defines members, shared package metadata, and lint policy.
- `lmserve/` contains the CLI binary crate.
- `.agents/skills/` contains Rust and GitHub Actions development rules.
- `AGENTS.md` defines contributor instructions and application constraints.

Add future crates as root-level directories and list them in `workspace.members`.
Commit `Cargo.lock` with dependency changes.

## License

MIT. See [LICENSE](LICENSE).
