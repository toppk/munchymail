# The CLI is the interface; see README.md. These are just conveniences.

set positional-arguments

_default:
    @just --list --unsorted

# Build the release binary (debug builds are much slower).
build:
    cargo build --release --quiet

# Run munchymail with any arguments, e.g. `just run report --by from:addr`.
run *args: build
    target/release/munchymail "$@"

# Format check, clippy with warnings as errors, tests.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --quiet
