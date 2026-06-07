default:
    just --list

check: fmt-check lint test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

build:
    cargo build

build-release:
    cargo build --release

test:
    cargo test

install: build-release
    ./target/release/servant install
