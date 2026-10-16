#!/usr/bin/env bash
# Runs the per-push CI checks (.github/workflows/ci.yml) locally, stopping at the first failure:
# formatting, the web console's types, tests and build, then clippy and tests for each feature
# combination CI uses. Flags add the slower jobs:
#   scripts/check.sh            # what most changes need
#   scripts/check.sh --msrv     # also the tests on the minimum supported Rust (rustup toolchain 1.89)
#   scripts/check.sh --postgres # also the store on PostgreSQL, at TURBOJET_HUB_POSTGRES_URL (a
#                               # database the tests may fill)
#   scripts/check.sh --deny     # also cargo-deny (`cargo install cargo-deny`)
#   scripts/check.sh --all      # all three
# The release build is left to CI.
set -euo pipefail
cd "$(dirname "$0")/.."

msrv=false
postgres=false
deny=false
for arg in "$@"; do
    case $arg in
        --msrv) msrv=true ;;
        --postgres) postgres=true ;;
        --deny) deny=true ;;
        --all) msrv=true; postgres=true; deny=true ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# The local Turbojet patch (CLAUDE.md) rewrites Cargo.lock, which --locked would refuse.
locked=--locked
[ -f .cargo/config.toml ] && locked=

step() { echo "== $*"; }

step fmt
cargo fmt --check

step web
(cd web && npm ci --silent && npm run --silent lint && npm test -- --run --silent && npm run --silent build)

# Keep in step with the test matrix in ci.yml.
for features in "" "--features tls" "--features gzip" "--features postgres" --all-features; do
    step "clippy and test (${features:-default})"
    # shellcheck disable=SC2086 # $features and $locked are zero to two words on purpose.
    cargo clippy --all-targets $features $locked --quiet -- -D warnings
    # shellcheck disable=SC2086
    cargo test $features $locked --quiet
done

if $postgres; then
    step postgres
    if [ -z "${TURBOJET_HUB_POSTGRES_URL:-}" ]; then
        echo "--postgres needs TURBOJET_HUB_POSTGRES_URL, e.g. postgres://user@localhost/turbojet_hub_test" >&2
        exit 2
    fi
    cargo test --features postgres $locked --quiet store::
fi
if $msrv; then
    step msrv
    cargo +1.89 test --features tls,gzip,postgres $locked --quiet
fi
if $deny; then
    step deny
    cargo deny check --hide-inclusion-graph
fi
step ok
