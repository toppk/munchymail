# Install

| What | Where |
|---|---|
| Releases | `$MUNCHYMAIL_PREFIX/releases/<version>-<shortsha>/{munchymail,mmadm,mbox-sort}` (`mmadm -> munchymail`) |
| Current, previous | `$MUNCHYMAIL_PREFIX/{current,previous} -> releases/…` (relative symlinks) |
| CLI | `$MUNCHYMAIL_BINDIR/{munchymail,mmadm,mbox-sort} -> $MUNCHYMAIL_PREFIX/current/…` |
| Config, credentials | `~/.config/munchymail/{config.toml,creds/}` |
| Data root | `~/.local/share/munchymail/`, mode 0700 (`--data`, `MUNCHYMAIL_DATA`) |
| Socket | `$XDG_RUNTIME_DIR/munchymail/serve.sock` (`MUNCHYMAIL_SOCKET`) |
| Unit | `~/.config/systemd/user/munchymail.service`, from `contrib/`; runs `mmadm serve` |

`MUNCHYMAIL_PREFIX` defaults to `~/.local/lib/munchymail` and
`MUNCHYMAIL_BINDIR` to `~/.local/bin`. Set them for your machine in a `.env`
next to the justfile (git-ignored), e.g.:

```
MUNCHYMAIL_PREFIX=~/opt/munchymail
MUNCHYMAIL_BINDIR=~/bin
```

The CLI is a symlink so it is always the same build as the service it talks
to. Cron, `ssh host cmd` and sandboxes may not have the bin directory on
`PATH`; call `munchymail` there by its full path.

Notes about one particular installation (its paths, accounts, history) don't
belong in this repository; keep them in a git-ignored `site/` or elsewhere.

## Installing a release

```
just install --restart            # needs a clean tree: releases are named by commit
munchymail --version              # 0.1.0-<commit>, the release's name
```

`just install` copies the binaries into a new release, moves `previous` to
the old `current`, points `current` at the new one, creates the CLI links if
missing and installs the unit with the prefix filled in. With `--restart` it then runs
`systemctl --user daemon-reload` and `restart munchymail`, and shows the
journal if the service doesn't come back. It never enables the unit and
never deletes old releases. Roll back by pointing `current` at
`previous`'s target and restarting. The first time, enable it:
`systemctl --user enable --now munchymail`.

The service stops at once on SIGTERM; writes are transactional, so an import
or OCR run loses only the file or chunk in progress. Restart it when idle.

## The unit

Hardened with systemd's sandboxing, minus what bwrap (which runs the OCR
and document tools) can't work under: `RestrictNamespaces` (it makes
namespaces), `ProtectKernelTunables` and `ProtectKernelLogs` (they overmount
parts of `/proc`, so the kernel refuses bwrap's fresh `/proc`), and with
`AF_NETLINK` allowed (it sets up loopback in its network namespace).
`MemoryDenyWriteExecute` is left out as untested with tesseract and
ocrmypdf. Before each OCR or extraction run munchymail starts the sandbox
once with `/usr/bin/true`; if that fails the run stops with an error and
records nothing, so a broken sandbox never marks attachments as failed. It may
read anything and writes only the data root; files `export -o` writes
elsewhere are sent to the CLI, which writes them. If `serve` ever refreshes
Gmail tokens itself, `~/.config/munchymail/creds` joins `ReadWritePaths`.

To check bwrap works under these settings before starting the unit (prints
`bwrap-ok`; needs no munchymail or data):

```
systemd-run --user --wait --pipe --collect -q \
  -p NoNewPrivileges=yes -p PrivateTmp=yes -p ProtectSystem=strict \
  -p ProtectHome=read-only -p ProtectKernelModules=yes -p ProtectControlGroups=yes \
  -p "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK" \
  -p LockPersonality=yes -p SystemCallArchitectures=native \
  bwrap --ro-bind /usr /usr --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
    --symlink usr/bin /bin --symlink usr/sbin /sbin --proc /proc --dev /dev \
    --tmpfs /tmp --unshare-all --die-with-parent --new-session --clearenv \
    --setenv PATH /usr/bin /usr/bin/true && echo bwrap-ok
```

## Development

`just run` and `cargo run` use the checkout's own `data/`, never the live
data root: a dev build may change the schema, and the live databases are the
service's. That data root differs from the service's, so those commands run
directly rather than through it.
