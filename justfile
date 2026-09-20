clippy_scope := "--workspace --all-targets --all-features --locked"

default:
    @just --list

fmt:
    cargo +nightly fmt --all
    dprint fmt

fix:
    cargo +stable clippy --fix {{clippy_scope}} --allow-dirty --allow-staged
    just fmt
    just lint

lint:
    dprint check
    cargo +nightly fmt --all -- --check
    cargo +stable clippy {{clippy_scope}} -- -D warnings
    just doc

test:
    cargo +stable test --workspace --all-features --locked

test-provider:
    cargo +stable test --package lmserve --locked --test cli provider_contract -- --ignored

build:
    cargo +stable build --workspace --all-targets --all-features --locked

install:
    cargo +stable install --path lmserve --locked

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
