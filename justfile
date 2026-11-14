# The CLI is the interface; see README.md. These are just conveniences.

set positional-arguments
# A git-ignored .env may set MUNCHYMAIL_PREFIX and MUNCHYMAIL_BINDIR for `install`.
set dotenv-load

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

# Run mmadm, the admin side of the same binary, e.g. `just adm origin list`.
adm *args: build
    ln -sf munchymail target/release/mmadm
    target/release/mmadm "$@"

# Build the documentation site into _site/ (needs pandoc); see site/theme/README.md.
site:
    site/build.sh _site

# Format check, clippy with warnings as errors, tests.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --quiet

# Install a release under $MUNCHYMAIL_PREFIX (see docs/install.md); --restart restarts the service.
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
    home="${MUNCHYMAIL_PREFIX:-$HOME/.local/lib/munchymail}"; home="${home/#\~/$HOME}"
    # Every deployment is a new version (a schema change, a new minor): see CHANGELOG.md.
    for r in "$home"/releases/"$version"-*; do
        if [ -e "$r" ] && [ "$(basename "$r")" != "$name" ]; then
            echo "version $version is already installed as $(basename "$r"); bump it in Cargo.toml first" >&2; exit 1
        fi
    done
    bin="${MUNCHYMAIL_BINDIR:-$HOME/.local/bin}"; bin="${bin/#\~/$HOME}"
    install -d "$home/releases/$name" "$bin" "$HOME/.config/systemd/user"
    install -m 0755 target/release/munchymail target/release/mbox-sort "$home/releases/$name/"
    ln -sfn munchymail "$home/releases/$name/mmadm"
    old=$(readlink "$home/current" || true)
    if [ -n "$old" ] && [ "$old" != "releases/$name" ]; then ln -sfn "$old" "$home/previous"; fi
    ln -sfn "releases/$name" "$home/current"
    for b in munchymail mmadm mbox-sort; do
        [ -L "$bin/$b" ] || [ -e "$bin/$b" ] || ln -s "$home/current/$b" "$bin/$b"
    done
    sed "s|@PREFIX@|$home|" contrib/munchymail.service > "$HOME/.config/systemd/user/munchymail.service.new"
    chmod 0644 "$HOME/.config/systemd/user/munchymail.service.new"
    mv "$HOME/.config/systemd/user/munchymail.service.new" "$HOME/.config/systemd/user/munchymail.service"
    if [ "$restart" = no ]; then
        echo "installed $name; to use it: just install --restart, or systemctl --user daemon-reload && systemctl --user restart munchymail"
        exit 0
    fi
    systemctl --user daemon-reload
    systemctl --user restart munchymail
    sleep 1
    systemctl --user is-active munchymail >/dev/null || { journalctl --user -u munchymail -n 20 --no-pager; exit 1; }
    echo "installed and restarted $("$bin/munchymail" --version)"

# Delete installed releases other than `current` and `previous`; --dry-run lists them.
prune *flags:
    #!/usr/bin/env bash
    set -euo pipefail
    dry=no
    for f in "$@"; do
        case "$f" in --dry-run) dry=yes ;; *) echo "unknown flag $f; only --dry-run" >&2; exit 2 ;; esac
    done
    home="${MUNCHYMAIL_PREFIX:-$HOME/.local/lib/munchymail}"; home="${home/#\~/$HOME}"
    cur=$(readlink "$home/current") || { echo "no $home/current; nothing pruned" >&2; exit 1; }
    prev=$(readlink "$home/previous" || true)
    for d in "$home"/releases/*/; do
        r="releases/$(basename "$d")"
        [ "$r" = "$cur" ] || [ "$r" = "$prev" ] && continue
        if [ "$dry" = yes ]; then echo "would delete $r"; else rm -r "$d"; echo "deleted $r"; fi
    done
