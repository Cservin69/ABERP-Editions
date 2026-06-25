#!/usr/bin/env bash
#
# run_storefront_gate_probe.sh — S2 / ADR-0093 both-arms proof of the
# storefront-reach gate DECISION (tools/storefront_gate_decision_probe.rs).
#
# Compiles the pure decision twice — Portable arm (default) and Defense arm
# (--cfg defense_arm) — and runs each test binary. Exit 0 = both arms green.
# This is the sandbox-honest stand-in for the full `cargo test --workspace`
# + `cargo test --features production` that the repo CI build-proves (serve.rs
# is DuckDB/HTTP-backed and cannot build in the 45s/4GB sandbox).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROBE="$ROOT/tools/storefront_gate_decision_probe.rs"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/sf-gate-probe.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
echo "storefront-reach gate DECISION — both-arms rustc --test proof"
echo "probe: $PROBE"
fail=0

echo "[arm: Portable] rustc --test"
rustc --test --edition 2021 -o "$TMP/portable" "$PROBE"
"$TMP/portable" || fail=1

echo "[arm: Defense ] rustc --test --cfg defense_arm"
rustc --test --edition 2021 --cfg defense_arm -o "$TMP/defense" "$PROBE"
"$TMP/defense" || fail=1

echo
if [[ "$fail" -ne 0 ]]; then echo "STOREFRONT-GATE PROBE: ✗ FAILED"; exit 1; fi
echo "STOREFRONT-GATE PROBE: ✓ BOTH ARMS GREEN (Portable⇒refuse, Defense⇒allow)"
