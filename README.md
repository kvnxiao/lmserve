# lmserve

`lmserve` prepares local language models and coordinates one managed model server with optional
companions through rootless Podman. Users own the Compose definitions, engine arguments, image
references, and model revisions. The CLI provides explicit preparation, background lifecycle
operations, readiness checks, and recorded shutdown.

Linux and WSL2 are the target platforms. Tests use fake host tools and local HTTP servers; the
provider contract test uses official `podman-compose` with a fake Podman executable. Live model
serving and Linux/WSL2 GPU deployments have not been verified.

## Use

Install the binary with `cargo install --path lmserve --locked`. Serving requires rootless Podman
**4.6 or newer**, `podman-compose` **1.5.0 or newer**, and prepared images and model files. Model
preparation additionally requires `hf`; authenticate with `hf` separately when the selected
repository requires access. NVIDIA deployments require a working host driver and Container Device
Interface (CDI) configuration. `nvidia-ctk` generates and inspects CDI devices. Selected image
builds may require additional provider or build-context tools.

Copy and customize [the Compose example](examples/compose.yaml) before running these commands from
its directory. Its image tags, model repositories, source ref, and NInfer command are placeholders
that must be replaced.

```sh
lmserve validate
lmserve plan update-images vllm
lmserve update-images vllm
lmserve update-models vllm
lmserve plan start vllm
lmserve start vllm
lmserve status vllm
lmserve health vllm
lmserve logs vllm
lmserve stop vllm
```

`start` returns an operation ID when work is accepted. Use `status` to distinguish container
startup, endpoint readiness, and failure. A successful submission does not mean the model is ready.
Updates run in the foreground; lifecycle commands do not download models, pull images, or build
images.

The global `--file PATH` selects a Compose file, defaulting to `./compose.yaml`. The global
`--provider PATH` selects the provider executable, defaulting to `podman-compose`. The provider must
report a numeric `major.minor.patch` release of 1.5.0 or newer. Relative Compose paths resolve from
the selected file's directory.

See [configuration and command reference](docs/usage.md),
[state, storage, and recovery](docs/operations.md), and [example setup](examples/README.md).

## Development

Install rustup, `just`, Bash, and `jq`, then run these commands from the repository root:

```sh
rustup toolchain install stable --profile minimal --component clippy
rustup toolchain install nightly --profile minimal --component rustfmt
cargo +stable install --locked cargo-audit cargo-machete dprint
just --list
just verify
just check-msrv lmserve
just check-msrv lmserve-test-tools
```

`rust-toolchain.toml` selects floating stable Rust for builds, tests, and Clippy. The workspace uses
edition 2024 and declares Rust 1.98.1 as its minimum supported version. `just check-msrv PACKAGE`
reads the member's resolved minimum version from Cargo metadata and installs that compiler for a
separate compatibility check.

Nightly rustfmt enforces one imported item per `use` statement, one sorted import group, comment
normalization, and Unix newlines. Configure editors to invoke `rustup run nightly rustfmt`.

[dprint](https://dprint.dev/plugins/) formats Markdown, YAML, JSON/JSONC, TOML, and HTML/XML markup,
including repository instructions and workflows. `dprint.json` preserves existing Markdown paragraph
wrapping and excludes the `.agents` directory, local untracked `SPEC.md`, generated Cargo lockfile,
and build output. Run `dprint config update` to update the formatter plugins, then `just fmt` to
apply their formatting.

The workspace manifest contains the Rust, Clippy, and rustdoc lint policy. Printing requires a
scoped expectation in CLI output functions. Test contexts permit `expect`, printing, indexing,
slicing, and explicit panic; `unwrap` remains denied.

- `just fmt` formats Rust source, markup, and configuration files.
- `just fix` applies Clippy fixes, runs `just fmt`, and runs strict lint checks.
- `just lint` checks formatting, Clippy, and rustdoc warnings.
- `just doc` builds documentation with warnings treated as errors.
- `just test` runs the workspace tests, including any doctests.
- `just test-provider` runs the provider contract test with externally installed `podman-compose`
  1.5.0 or newer and a fake Podman executable. It resolves the provider from `PATH`; set
  `LMSERVE_TEST_PROVIDER` to select an explicit executable path. It does not install tools, run
  containers, or use a GPU.
- `just dependencies` checks advisories and unused dependencies.
- `just build` builds all workspace targets.
- `just verify` runs lint checks, tests, dependency checks, and builds.

CI invokes the shared verification task and checks each member's minimum compiler version in a
separate job. Dependency auditing fetches the advisory database and requires network access.
Application tests isolate state, cache, configuration, and executable lookup in temporary
directories. They do not invoke a live runtime or download models. The provider contract test is
separate from `just verify`. Release builds retain line-table debug information for profilers and
backtraces.

Runtime tools are not required for the default build and test tasks. Cargo requirements use major
versions for stable crates and the current minor for crates below 1.0; `Cargo.lock` records the
exact dependency resolution.

The integration suite uses `assert_cmd` for CLI assertions and `escargot` to build the Rust fake
tools once per test process. Each test links those tools into an isolated executable directory. The
provider contract test requires the user's external provider installation; ordinary tests use only
native Rust executables.

## Workspace

- `Cargo.toml` defines members, shared package metadata, and lint policy.
- `lmserve/` contains the CLI binary crate.
- `lmserve-test-tools/` contains the unpublished fake-tool executable used by tests.
- `.agents/skills/` contains Rust and GitHub Actions development rules.
- `AGENTS.md` defines contributor instructions and application constraints.

Add future crates as root-level directories and list them in `workspace.members`. Commit
`Cargo.lock` with dependency changes.

## License

MIT. See [LICENSE](LICENSE).
