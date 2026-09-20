clippy_scope := "--workspace --all-targets --all-features --locked"

default:
    @just --list

fix:
    cargo +stable clippy --fix {{clippy_scope}} --allow-dirty --allow-staged
    cargo +nightly fmt --all
    just lint

lint:
    cargo +nightly fmt --all -- --check
    cargo +stable clippy {{clippy_scope}} -- -D warnings
    just doc

test:
    cargo +stable test --workspace --all-features --locked

test-provider:
    uv run --no-project --with podman-compose==1.5.0 python -c 'import os, shutil; os.environ["LMSERVE_TEST_PROVIDER"] = shutil.which("podman-compose"); os.execvp("rustup", ["rustup", "run", "stable", "cargo", "test", "--locked", "--test", "cli", "provider_contract", "--", "--ignored"])'

build:
    cargo +stable build --workspace --all-targets --all-features --locked

doc:
    RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings" cargo +stable doc --workspace --all-features --no-deps --locked

dependencies:
    cargo +stable audit
    cargo +stable machete

verify: lint test dependencies build

[positional-arguments]
check-msrv package:
    #!/usr/bin/env bash
    set -euo pipefail
    metadata=$(cargo +stable metadata --no-deps --format-version 1 --locked)
    msrv=$(jq -er --arg name "$1" '
        .workspace_members as $members
        | .packages[]
        | select(.id as $id | $members | index($id))
        | select(.name == $name)
        | .rust_version // error("selected package must declare rust-version")
    ' <<< "$metadata")
    rustup toolchain install "$msrv" --profile minimal
    cargo +"$msrv" check --package "$1" --all-targets --all-features --locked
