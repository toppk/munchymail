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

# Install a release under ~/.local/lib/munchymail (see docs/install.md); doesn't restart the service.
install:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "$(git status --porcelain)" ]; then echo "commit first: releases are named by commit" >&2; exit 1; fi
    cargo build --release --quiet
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
    name="$version-$(git rev-parse --short HEAD)"
    home="$HOME/.local/lib/munchymail"
    install -d "$home/releases/$name" "$HOME/.local/bin" "$HOME/.config/systemd/user"
    install -m 0755 target/release/munchymail target/release/mbox-sort "$home/releases/$name/"
    old=$(readlink "$home/current" || true)
    if [ -n "$old" ] && [ "$old" != "releases/$name" ]; then ln -sfn "$old" "$home/previous"; fi
    ln -sfn "releases/$name" "$home/current"
    for b in munchymail mbox-sort; do
        [ -L "$HOME/.local/bin/$b" ] || [ -e "$HOME/.local/bin/$b" ] || ln -s "$home/current/$b" "$HOME/.local/bin/$b"
    done
    install -m 0644 contrib/munchymail.service "$HOME/.config/systemd/user/"
    echo "installed $name; to use it: systemctl --user daemon-reload && systemctl --user restart munchymail"
