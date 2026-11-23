# to11y tasks. Run `just` to list them.

# List the recipes.
default:
    @just --list

# Build the debug binary.
build:
    cargo build

# Build the static release binary for this machine.
release-build:
    cargo build --release --locked --target x86_64-unknown-linux-musl

# Run every test.
test:
    cargo test --locked

# Format the code.
fmt:
    cargo fmt --all

# Lint with every warning as an error.
lint:
    cargo clippy --all-targets --locked -- -D warnings

# Check the oldest supported Rust. Dev-dependencies may need a newer one.
msrv:
    cargo +1.89 check --locked

# The gate to pass before handing off: format, lint, test.
check:
    cargo fmt --all --check
    just lint
    just test

# Check token totals on this machine's logs against ccusage and Factory's
# own totals. Prints totals only. Takes a ccusage binary, then optionally
# the number of days (1, 7, or 30) and the UTC end date.
oracle-tokens ccusage *args: build
    python3 -I scripts/oracle_tokens.py {{ccusage}} {{args}}

# Package the crate and build it as crates.io would, without publishing.
publish-check:
    cargo publish --dry-run --locked

# Tag a release and push the tag, which starts the release workflow. The
# version must match Cargo.toml, as in `just release 0.1.0`.
release version: check
    #!/usr/bin/env sh
    set -eu
    current=$(cargo pkgid | sed 's/.*[#@]//')
    if [ "$current" != "{{version}}" ]; then
        echo "Cargo.toml says $current, not {{version}}" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
        echo "commit or stash your changes first" >&2
        exit 1
    fi
    git tag -a "v{{version}}" -m "to11y {{version}}"
    git push origin "v{{version}}"
