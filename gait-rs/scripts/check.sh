#!/usr/bin/env bash
# gait checks: clippy, the telemetry lint, the OpenTelemetry dependents
# allowlist, no process::exit in CLI/binary sources, and the crates that must
# stay telemetry-free. Every cargo build goes through cargo-slot.
# The linter is installed on demand at the Cargo.lock rev (see below).
# The shared check scripts come from the concord-telemetry source cargo already
# resolved (found through cargo metadata); CONCORD_TELEMETRY_DIR overrides it.
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO_SLOT="${CARGO_SLOT:-$(command -v cargo-slot || echo "$PWD/../../../scripts/cargo-slot")}"
export CARGO_SLOT

# No clippy here: on the current toolchain `clippy -D warnings` fails on
# lints in generated code (gait-proto's pbjson output) and on async_trait fns,
# before any hand-written code. `make lint` keeps the clippy gate.
"$CARGO_SLOT" cargo check --workspace --all-targets
# The linter is keyed on the concord-lint (else concord-telemetry) rev in
# Cargo.lock: the lookup script ships with concord-telemetry, is found through
# cargo metadata, and installs that rev into ~/.cache/concord-lint/<rev>.
# CONCORD_LINT=/path overrides it (no rev check).
ct_manifest=""
if [ -z "${CONCORD_LINT:-}" ] || [ -z "${CONCORD_TELEMETRY_DIR:-}" ]; then
    ct_manifest="$("$CARGO_SLOT" cargo metadata --format-version 1 --locked |
        python3 -c 'import json,sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "concord-telemetry"))')"
fi
# The directory holding the shared scripts/: the checkout root, one above the
# concord-telemetry crate.
telemetry_dir="${CONCORD_TELEMETRY_DIR:-$(cd "$(dirname "$ct_manifest")/.." && pwd)}"
lint="${CONCORD_LINT:-}"
if [ -z "$lint" ]; then
    for_lock="$telemetry_dir/scripts/concord-lint-for-lock.sh"
    [ -x "$for_lock" ] || { echo "concord-lint-for-lock.sh not found at $for_lock" >&2; exit 1; }
    lint="$HOME/.cache/concord-lint/$("$for_lock" Cargo.lock)/bin/concord-lint"
fi
"$lint" --local fields.local.toml --baseline lint-baseline.txt crates/*/src crates/*/tests src
[ -x "$telemetry_dir/scripts/check-otel-dependents.sh" ] || {
    echo "$telemetry_dir/scripts/check-otel-dependents.sh is not an executable file; point CONCORD_TELEMETRY_DIR at a concord-telemetry checkout" >&2
    exit 1
}
"$telemetry_dir/scripts/check-otel-dependents.sh" -C . concord-telemetry
# The default gait-bin build exports telemetry.
"$telemetry_dir/scripts/check-has-telemetry.sh" -p gait-bin

if grep -R --include='*.rs' -n 'process::exit' crates/gait-cli/src crates/gait-repl/src crates/gait-bin/src crates/gait-acp/src \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
    | grep -v 'no_process_exit_left_in_cli_crates' \
    | grep -v 'crates/gait-bin/src/lib.rs'; then
    echo "process::exit is forbidden in CLI and binary sources" >&2
    exit 1
fi

no_telemetry=()
for crate in gait-types gait-proto gait-platform gait-sandbox gait-codemode-js gait-turn-runner; do
    no_telemetry+=(-p "$crate")
done
"$telemetry_dir/scripts/check-no-telemetry.sh" "${no_telemetry[@]}"

# gait-min carries no telemetry crates.
if cargo tree -p gait-bin --no-default-features --features min -e normal | grep -qE 'concord-telemetry|opentelemetry'; then
    echo "gait-min unexpectedly depends on telemetry" >&2
    exit 1
fi
