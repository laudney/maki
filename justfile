default:
    @just --list

build *ARGS:
    cargo build {{ARGS}}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
check *ARGS:
    cargo check --workspace --all-targets --features maki-ui/bench {{ARGS}}

run *ARGS:
    cargo run {{ARGS}}

install *ARGS:
    cargo install --locked --path . {{ARGS}}

test *ARGS:
    cargo nextest run --workspace {{ARGS}}

lint:
    cargo clippy --all --all-targets --features maki-ui/bench -- -D warnings

# Narrow it down, e.g. `just bench -p maki-ui --bench input`.
bench *ARGS:
    cargo bench --features maki-ui/bench {{ARGS}}

# Dev profile: builds fast, to see a bench run. Its numbers mean little.
bench-quick *ARGS:
    cargo bench --profile dev --features maki-ui/bench {{ARGS}}

lint-fix:
    cargo clippy --all --tests --fix

fmt-check:
    cargo fmt --all -- --check
    stylua --check plugins/

fmt:
    cargo fmt --all
    stylua plugins/

# Same command as CI. lute reads `plugins/.config.luau` from the working directory.
# Not part of `ci` yet: it needs the lute nightly pinned in `.github/workflows/rust.yml`.
check-lua:
    cd plugins && lute check $(find . -name '*.lua')

pylint:
    ruff check scripts/
    ty check scripts/

gen-docs:
    cargo run -p maki-docgen

gen-docs-check:
    cargo run -p maki-docgen -- --check

machete:
    cargo machete

# Crates the display path links must stay below the agent and providers
# Each line: a package and the crates it must not reach.
deps-check:
    #!/usr/bin/env bash
    set -euo pipefail
    status=0
    while IFS='|' read -r crate forbidden; do
        reached=$(cargo tree -p "$crate" -e normal --prefix none | awk '{print $1}' | sort -u)
        for dep in $forbidden; do
            if grep -qxF "$dep" <<<"$reached"; then
                echo "$crate must not depend on $dep" >&2
                status=1
            fi
        done
    done <<'EOF'
    maki-types|maki-agent maki-providers
    maki-wire|maki-agent maki-providers
    maki-lua-core|maki-agent maki-providers maki-interpreter maki-highlight maki-markdown crossterm ratatui
    maki-lua-ui|maki-agent maki-providers maki-interpreter maki-lua ratatui
    maki-host|maki-ui maki-lua-ui crossterm ratatui
    maki-acp-agent|maki-ui maki-lua-ui crossterm ratatui
    maki-acp-client|maki-agent maki-providers maki-lua maki-acp
    EOF
    exit $status

# Full CI check
ci: fmt-check lint pylint test gen-docs-check machete deps-check
