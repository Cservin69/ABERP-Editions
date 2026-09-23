#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# PROVE ci/gate-parity.sh HAS TEETH, IN BOTH DIRECTIONS.
#
# Each probe copies the workflows, ci/ and tools/ into a scratch tree, applies
# one mutation, and runs the parity gate there (GATE_PARITY_ROOT). A RED probe
# must fail it; a GREEN probe — a check properly mirrored into a gate — must
# pass it. The red set is every way the hole was found to reopen, including
# each bypass the adversarial review of 2026-09-23 demonstrated against an
# earlier, looser version of the gate.
# ---------------------------------------------------------------------------
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "gate-parity probes"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/gate-parity-probes.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
T="$SCRATCH/t"; CI="$T/.github/workflows/ci.yml"

fresh() {
  rm -rf "$T"; mkdir -p "$T/.github"
  cp -R .github/workflows "$T/.github/workflows"
  cp -R ci tools "$T/"
}
add()  { printf '%b' "$1" >> "$CI"; }                   # append raw YAML to the last job
step() { add "\n      - name: probe\n        run: $1\n"; }
gate() { printf '#!/usr/bin/env bash\ntrue\n' > "$T/ci/$1"; chmod +x "$T/ci/$1"; }

pass=0; fail=0
expect() {
  local want="$1" desc="$2" got
  if GATE_PARITY_ROOT="$T" bash ci/gate-parity.sh >/dev/null 2>&1; then got=green; else got=red; fi
  if [ "$got" = "$want" ]; then printf '  ✓ %-5s %s\n' "$want" "$desc"; pass=$((pass + 1))
  else printf '  ✗ wanted %s, got %s: %s\n' "$want" "$got" "$desc"; fail=$((fail + 1)); fi
}

fresh;                                            expect green "the tree as committed"
# inline checks, the original hole
fresh; step "cargo fmt --all -- --check";         expect red "cargo fmt re-added inline"
fresh; step 'cargo build --workspace --locked --all-targets ${{ matrix.features }}'; expect red "cargo build re-added inline"
fresh; step "cargo test -p aberp-db --test handle_concurrency_e2e --locked -- --nocapture"; expect red "a 'narrower' cargo test -p inline"
fresh; step "cargo test -p aberp-portal-relay --test nginx_differential -- --ignored"; expect red "an --ignored test inline"
fresh; step "npm run check";                      expect red "an npm check inline"
fresh; step "bash tools/snapshot-prod.sh";        expect red "a tools/ script that is not a gate"
# smuggling behind a gate call
fresh; step "./ci/rust-formatting.sh && cargo clippy -- -D warnings"; expect red "&& after a gate call"
fresh; step "./ci/rust-formatting.sh & cargo fmt --check"; expect red "& after a gate call"
fresh; step "FOO=1 ./ci/rust-formatting.sh";      expect red "env-prefixed gate call"
fresh; step "./ci/../ci/rust-formatting.sh";      expect red "a ../ path"
# YAML forms a line parser misses
fresh; add "\n      - name: probe\n        run: |\n          cargo deny check\n";                expect red "multi-line block restored"
fresh; add "\n      - name: probe\n        run: >-\n          cargo fmt --check\n";               expect red "folded block"
fresh; add "\n      - name: probe\n        run:\n          cargo fmt --all -- --check\n";      expect red "run: value on the next line"
fresh; add "\n      - name: probe\n        run: ./ci/clippy.sh\n          && cargo fmt --check\n"; expect red "plain scalar continued on the next line"
fresh; add "\n      - { name: probe, run: cargo fmt --all -- --check }\n";                    expect red "flow-style step"
fresh; add "\n      - name: probe\n        \"run\": cargo fmt --check\n";                       expect red "quoted run key"
fresh; add "\n      - name: probe\n        run : cargo fmt --check\n";                         expect red "spaced run key"
fresh; add "\n      - run: python -m pytest\n";                                                 expect red "list-item shorthand '- run:'"
fresh; add "\n\t- name: probe\n";                                                               expect red "tab indentation"
# the other step keys
fresh; add "\n      - name: probe\n        shell: bash -c \"cargo fmt --check && bash {0}\"\n        run: ./ci/clippy.sh\n"; expect red "shell: wrapping a gate call"
fresh; add "\n      - name: probe\n        working-directory: apps/aberp-ui\n        run: ./ci/clippy.sh\n";  expect red "working-directory: on a gate step"
fresh; add "\n      - name: probe\n        env:\n          BASH_ENV: tools/x.sh\n        run: ./ci/clippy.sh\n"; expect red "BASH_ENV"
fresh; add "\n      - name: probe\n        env:\n          ENFORCE_DURABLE_ACK: \"0\"\n        run: ./ci/clippy.sh\n"; expect red "an ENFORCE_* switched off"
fresh; add "\n      - name: probe\n        if: false\n        run: ./ci/clippy.sh\n";                       expect red "step-level if:"
fresh; add "\n      - name: probe\n        continue-on-error: true\n        run: ./ci/clippy.sh\n";       expect red "continue-on-error:"
fresh; add "\n      - name: probe\n        uses: actions-rs/clippy-check@v1\n";                             expect red "a check run by an action"
fresh; add "\n      - name: probe\n        uses: ./.github/actions/lint\n";                                 expect red "a local composite action"
# direction 1
fresh; gate new-check.sh;                                                           expect red "a new gate no workflow calls"
fresh; sed -i.bak '/run: \.\/ci\/clippy\.sh/d' "$CI"; rm -f "$CI.bak";              expect red "a gate's step removed (comments still mention it)"
fresh; sed -i.bak '/run: \.\/ci\/clippy\.sh/d' "$CI"; rm -f "$CI.bak"
       add "\n      - name: probe\n        uses: taiki-e/install-action@v2\n        with:\n          run: ./ci/clippy.sh\n"; expect red "a gate 'called' from inside with:"
# the exempt directory
fresh; printf '#!/usr/bin/env bash\ncargo fmt --check\n' > "$T/ci/workflow-only/fmt.sh"; chmod +x "$T/ci/workflow-only/fmt.sh"
       step "./ci/workflow-only/fmt.sh";                                            expect red "a check hidden in ci/workflow-only/"
fresh; echo 'cargo fmt --all -- --check' >> "$T/ci/workflow-only/tauri-system-deps.sh"; expect red "a check appended to a pinned plumbing script"
# properly mirrored: stays green
fresh; gate new-check.sh; step "./ci/new-check.sh";                                 expect green "a new gate, wired"
fresh; step "bash tools/lint_svg_wellformed.sh";                                    expect green "an existing gate called a second time"
fresh; add "\n      - name: probe\n        env:\n          ENFORCE_DURABLE_ACK: \"1\"\n        run: bash tools/cut_gate_durable_ack.sh\n"; expect green "a gate step with an allowed env"

echo
if [ "$fail" -ne 0 ]; then echo "GATE-PARITY PROBES: ✗ $fail of $((pass + fail)) did not hold"; exit 1; fi
echo "GATE-PARITY PROBES: ✓ all $pass hold — the parity gate has teeth"
