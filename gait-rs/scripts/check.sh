#!/usr/bin/env bash
# gait checks: clippy, the telemetry lint, the OpenTelemetry dependents
# allowlist, no process::exit in CLI/binary sources, and the crates that must
# stay telemetry-free. Every cargo build goes through cargo-slot.
# Needs `concord-lint` installed at the rev pinned in Cargo.toml:
#   cargo-slot cargo install --git https://github.com/qtpi-bonding/concord-telemetry \
#     --rev <concord-telemetry rev> concord-lint
# and a checkout of concord-telemetry for the shared dependents script
# (CONCORD_TELEMETRY_DIR, default ~/Documents/my-libs/concord-telemetry).
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO_SLOT="${CARGO_SLOT:-$(command -v cargo-slot || echo "$PWD/../../../scripts/cargo-slot")}"
export CARGO_SLOT

# No clippy here: on the current toolchain `clippy -D warnings` fails on
# lints in generated code (gait-proto's pbjson output) and on async_trait fns,
# before any hand-written code. `make lint` keeps the clippy gate.
"$CARGO_SLOT" cargo check --workspace --all-targets
concord-lint --local fields.local.toml --baseline lint-baseline.txt crates/*/src crates/*/tests src
telemetry_dir="${CONCORD_TELEMETRY_DIR:-$HOME/Documents/my-libs/concord-telemetry}"
"$telemetry_dir/scripts/check-otel-dependents.sh" concord-telemetry

if grep -R --include='*.rs' -n 'process::exit' crates/gait-cli/src crates/gait-repl/src crates/gait-bin/src crates/gait-acp/src \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
    | grep -v 'no_process_exit_left_in_cli_crates' \
    | grep -v 'crates/gait-bin/src/lib.rs'; then
    echo "process::exit is forbidden in CLI and binary sources" >&2
    exit 1
fi

for crate in gait-types gait-proto gait-platform gait-sandbox gait-codemode-js gait-turn-runner; do
    if cargo tree -p "$crate" -e normal | grep -q concord-telemetry; then
        echo "$crate unexpectedly depends on concord-telemetry" >&2
        exit 1
    fi
done

# gait-min carries no telemetry crates.
if cargo tree -p gait-bin --no-default-features --features min -e normal | grep -qE 'concord-telemetry|opentelemetry'; then
    echo "gait-min unexpectedly depends on telemetry" >&2
    exit 1
fi
