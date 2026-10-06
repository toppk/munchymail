# Reports over the mail corpus. ORIGIN picks the origin (default inbox; empty = all).
# Dates are YYYY-MM-DD; pass "" to leave a bound open.

set positional-arguments

bin := "target/release/munchymail"
origin := env("ORIGIN", "inbox")

_default:
    @just --list --unsorted

# Build the release binary.
build:
    cargo build --release --quiet

# Import origins (all registered origins if none given).
import *labels: build
    {{bin}} import "$@"

# Registered origins and last import.
origins: build
    {{bin}} origin list

# Corpus counts.
stats: build
    {{bin}} stats

# Who sends the most mail, by address, with % of all mail in range.
senders since="" before="" n="40": build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} report --by from:addr ${o:+--origin "$o"} ${1:+--since "$1"} ${2:+--before "$2"} -n "$3"

# Same, grouped by sender domain.
domains since="" before="" n="40": build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} report --by from:domain ${o:+--origin "$o"} ${1:+--since "$1"} ${2:+--before "$2"} -n "$3"

# Every header one sender's mail carries (default X-headers) with its top values; useful ones sort first.
sender-headers from since="" before="" like="x-*" k="6": build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} profile --from "$1" --like "$4" ${o:+--origin "$o"} ${2:+--since "$2"} ${3:+--before "$3"} -k "$5"

# Free-form profile, e.g. `just profile --like 'list-*' --since 2026-01-01`.
profile *args: build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} profile ${o:+--origin "$o"} "$@"

# Group every sender by a header's value; % is of all mail in range.
by header since="" before="" n="40": build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} report --by from:addr --by "$1" ${o:+--origin "$o"} ${2:+--since "$2"} ${3:+--before "$3"} -n "$4"

# One sender (Sieve pattern) broken down by a header's value; % is of all mail in range.
sender-by from header since="" before="" n="40": build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} report --by from:addr --by "$2" --from "$1" ${o:+--origin "$o"} ${3:+--since "$3"} ${4:+--before "$4"} -n "$5"

# Free-form report, e.g. `just report --by list-id --since 2026-01-01`.
report *args: build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} report ${o:+--origin "$o"} "$@"

# Free-form search, e.g. `just search --from '*@github.com' --headers -n 3`.
search *args: build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    {{bin}} search ${o:+--origin "$o"} "$@"

# Most common values of one header.
top header *args: build
    #!/usr/bin/env bash
    set -euo pipefail
    o='{{origin}}'
    h="$1"; shift
    {{bin}} top "$h" ${o:+--origin "$o"} "$@"

# Header block of one message.
show id: build
    {{bin}} show "$1"

# Exact raw message, e.g. `just raw 123 > m.eml && sieve-test rules.sieve m.eml`.
raw id: build
    {{bin}} show --raw "$1"

# Every place a message was found.
where id: build
    {{bin}} show --where "$1"

# Ad-hoc SQL against the index.
sql query: build
    {{bin}} sql "$1"

# Format, lint and test.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --quiet
