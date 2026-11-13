# The CLI is the interface; see README.md. These are just conveniences.

set positional-arguments

# Dev runs use the checkout's data/, never the live data root.
export MUNCHYMAIL_DATA := env("MUNCHYMAIL_DATA", justfile_directory() / "data")

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

# Install a release under ~/.local/lib/munchymail (see docs/install.md); --restart restarts the service.
install *flags:
    #!/usr/bin/env bash
    set -euo pipefail
    restart=no
    for f in "$@"; do
        case "$f" in --restart) restart=yes ;; *) echo "unknown flag $f; only --restart" >&2; exit 2 ;; esac
    done
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
    if [ "$restart" = no ]; then
        echo "installed $name; to use it: just install --restart, or systemctl --user daemon-reload && systemctl --user restart munchymail"
        exit 0
    fi
    systemctl --user daemon-reload
    systemctl --user restart munchymail
    sleep 1
    systemctl --user is-active munchymail >/dev/null || { journalctl --user -u munchymail -n 20 --no-pager; exit 1; }
    echo "installed and restarted $("$HOME/.local/bin/munchymail" --version)"
