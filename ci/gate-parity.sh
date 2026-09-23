#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# THE WORKFLOWS AND THE LOCAL GATES ARE ONE SET, CHECKED IN BOTH DIRECTIONS.
#
# Defense runs Actions-off: nothing in .github/workflows/ executes. A check
# that lives only in a workflow does not run at all, and it goes red without
# anybody seeing it. On 2026-09-23 that was the state of main (42c9423):
#
#   * cargo fmt, the workspace build, the workspace tests, the durability
#     e2e, the named integration tests, clippy, cargo-deny, the SPA build and
#     the Python extractor suite existed ONLY as inline workflow steps.
#   * cargo-deny was RED: RustSec withdrew eight advisories deny.toml still
#     ignored (advisory-not-detected), and under that, RUSTSEC-2026-0285 —
#     a TLS 1.3 handshake bug in rustls 0.23.40 — reached the NAV transport.
#   * the SPA type-check (svelte-check) and the SPA test suite (vitest, 1587
#     tests) were in package.json and run by NOTHING, workflow or local.
#
# Each of those is a ci/*.sh gate now, and the workflow step calls it: one
# producer. THIS gate keeps the class closed.
#
# # The gate set
#
#   * every executable, shebanged ci/*.sh (top level), and
#   * the toolchain-free families that predate ci/: tools/cut_gate_*.sh and
#     tools/lint_*.sh.
#
# # Direction 1 — every gate is run by a workflow step.
#
# # Direction 2 — every `run:` is exactly `bash <gate>` or `./<gate>`, or a
#   call to one of the WORKFLOW_ONLY scripts (runner setup / job plumbing,
#   pinned by hash below so a check cannot be appended to one quietly).
#   There is no "narrower inline run" exemption: the three durability e2e that
#   used to be one are a gate too. Nothing else on the line.
#
# # The workflow shape is held to a grammar, not parsed loosely
#
# A line parser only sees what the grammar lets through, so the grammar is
# the check (an adversarial review found each of these slipping a check past
# a looser version):
#   * steps are block mappings; keys at step level are name, id, uses, with,
#     env, run — nothing else (no shell:, working-directory:, if:,
#     continue-on-error:);
#   * `run:` sits at step level, has its value on the same line, is not a
#     block scalar and is not continued on the next line;
#   * no flow-style steps, no quoted or spaced `run` keys, no tabs;
#   * shell:, working-directory:, defaults:, continue-on-error: and BASH_ENV
#     appear nowhere in any workflow;
#   * `uses:` is one of USES_ALLOWED; env names are ENV_ALLOWED, and every
#     ENFORCE_* is "1".
#
# Run from anywhere. GATE_PARITY_ROOT overrides the tree (the probes use it).
# `--list` prints the gates in the order the workflows call them.
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="${GATE_PARITY_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

LIST=""; [ "${1:-}" = "--list" ] && LIST=1
if [ -n "$LIST" ]; then exec 3>&1 1>&2; else exec 3>&1; fi

GREEN=$'\033[32m'; RED=$'\033[31m'; DIM=$'\033[2m'; OFF=$'\033[0m'
ok()  { printf '  %s✓%s %s\n' "$GREEN" "$OFF" "$1"; }
bad() { printf '  %s✗%s %s\n' "$RED" "$OFF" "$1"; }
dim() { printf '    %s%s%s\n' "$DIM" "$1" "$OFF"; }

echo "gate-parity gate"

# sha256 of each exempt script. Editing one means editing this line too — in
# the same diff, in front of the reviewer.
WORKFLOW_ONLY=(
  "ci/workflow-only/tauri-system-deps.sh=0426c8fc8ca5b09adab6573788035bdd90ba325f3c6a98ad98ae58ad8b920a01"
  "ci/workflow-only/python-extractor-install.sh=1b17546dd1f5c44c1e32bbb188a3fa68c8bcf7a57a09760e2dc743458c437249"
  "ci/workflow-only/require-every-cut-gate-job.sh=f840a14c8a47ddf84be3d6d4721bae876b513d15d8cc759bd3aafb6a5fcb80a1"
)
USES_ALLOWED='^(actions/checkout|actions/setup-node|actions/setup-python|dtolnay/rust-toolchain|Swatinem/rust-cache|taiki-e/install-action)@'
ENV_ALLOWED='^(CARGO_TERM_COLOR|CARGO_NET_RETRY|RUSTUP_MAX_RETRIES|ABERP_EDITION_FEATURES|GATES|PROBES|PROBE_SHARD_INDEX|PROBE_SHARD_TOTAL|ENFORCE_[A-Z0-9_]+)$'

shopt -s nullglob nocaseglob
workflows=(.github/workflows/*.yml .github/workflows/*.yaml)
shopt -u nocaseglob
[ "${#workflows[@]}" -gt 0 ] || { bad "no workflow files under .github/workflows/"; exit 1; }

fail=0

# ── the gate set ──────────────────────────────────────────────────────────
gates=()
for path in ci/*.sh; do
  if [ -x "$path" ] && head -1 "$path" | grep -q '^#!'; then gates+=("$path"); fi
done
for path in tools/cut_gate_*.sh tools/lint_*.sh; do gates+=("$path"); done
is_gate() { local g; for g in "${gates[@]}"; do [ "$g" = "$1" ] && return 0; done; return 1; }
is_workflow_only() { local w; for w in "${WORKFLOW_ONLY[@]}"; do [ "${w%%=*}" = "$1" ] && return 0; done; return 1; }

# ── ci/workflow-only: exactly the listed files, each at its pinned hash ───
for path in ci/workflow-only/* ci/workflow-only/.[!.]*; do
  if ! is_workflow_only "$path"; then
    bad "$path is in ci/workflow-only/ but not listed in WORKFLOW_ONLY."
    dim "That directory is exempt from both directions; a check put there would run nowhere."
    fail=1
  fi
done
for w in "${WORKFLOW_ONLY[@]}"; do
  f="${w%%=*}"; want="${w#*=}"
  if [ ! -f "$f" ]; then bad "WORKFLOW_ONLY names $f, which does not exist."; fail=1; continue; fi
  got="$(shasum -a 256 "$f" | cut -c1-64)"
  if [ "$got" != "$want" ]; then
    bad "$f changed (sha256 $got)."
    dim "It is exempt from parity, so it is pinned. If the change is setup, not a check,"
    dim "update its hash in WORKFLOW_ONLY in this file."
    fail=1
  fi
done

# ── the grammar, and the run: commands it admits ──────────────────────────
# awk emits "RUN<TAB>file<TAB>cmd" or "ERR<TAB>file:line<TAB>message".
parse() {
  awk -v Q="'" -v F="$1" -v USES="$USES_ALLOWED" -v ENVS="$ENV_ALLOWED" '
    function err(m) { printf "ERR\t%s:%d\t%s\n", F, NR, m }
    function ind(s) { match(s, /^ */); return RLENGTH }
    {
      sub(/\r$/, "")
      line = $0
      if (line ~ /^[ ]*\t/)                         { err("tab indentation"); next }
      if (line ~ /^[ ]*(#|$)/)                      { next }
      i = ind(line)

      if (in_run && i > run_i)                      { err("run: value continued on the next line"); next }
      in_run = 0
      if (in_env && i <= env_i) in_env = 0
      if (in_steps && i <= steps_i) { in_steps = 0; step_i = -1 }

      if (line ~ /(^|[^A-Za-z0-9_-])(shell|working-directory|defaults|continue-on-error)[ ]*:/) { err("forbidden key: " line); next }
      if (line ~ /BASH_ENV/)                        { err("BASH_ENV"); next }
      if (line ~ ("[\"" Q "]run[\"" Q "][ ]*:") || line ~ /(^|[ {,-])run[ ]+:/) { err("quoted or spaced run key"); next }
      if (line ~ /\{[^}]*run[ ]*:/ || line ~ /steps:[ ]*\[/) { err("flow-style step"); next }

      if (in_env) {
        e = line; sub(/^ */, "", e)
        name = e; sub(/[ ]*:.*/, "", name)
        val = e; sub(/^[^:]*:[ ]*/, "", val); gsub("[\"" Q "]", "", val); sub(/[ ]+#.*$/, "", val)
        if (name !~ ENVS)                           { err("env name not allowed: " name); next }
        if (name ~ /^ENFORCE_/ && val != "1")       { err(name " must be \"1\""); next }
        next
      }

      if (line ~ /^[ ]*steps:[ ]*$/)                { in_steps = 1; steps_i = i; step_i = -1; next }

      rest = line; eff = i
      if (in_steps && line ~ /^[ ]*- /) { step_i = i + 2; rest = substr(line, i + 3); eff = step_i }
      else sub(/^ */, "", rest)

      if (rest !~ /^[A-Za-z0-9_-]+:/) next
      key = rest; sub(/:.*/, "", key)
      val = rest; sub(/^[^:]*:[ ]*/, "", val)

      if (key == "env") {
        if (val != "")                              { err("inline env mapping"); next }
        in_env = 1; env_i = eff; next
      }
      if (key == "run") {
        if (!in_steps || eff != step_i)             { err("run: outside step level"); next }
        if (val == "" || val ~ /^[|>!&*]/)          { err("run: must be a single-line command on the same line"); next }
        sub(/[ ]+#.*$/, "", val); sub(/[ ]+$/, "", val)
        if (val ~ /^".*"$/ || val ~ ("^" Q ".*" Q "$")) val = substr(val, 2, length(val) - 2)
        printf "RUN\t%s\t%s\n", F, val
        in_run = 1; run_i = eff; next
      }
      if (in_steps && eff == step_i) {
        if (key !~ /^(name|id|uses|with|env|run)$/) { err("step key not allowed: " key); next }
        if (key == "uses" && val !~ USES)           { err("uses: not allowed: " val); next }
      }
    }
  ' "$1"
}

called=()
steps=0; gate_calls=0; plumbing=0; orphans=0; grammar=0
for wf in "${workflows[@]}"; do
  while IFS=$'\t' read -r kind where what; do
    if [ "$kind" = "ERR" ]; then
      [ "$grammar" -eq 0 ] && bad "workflow shape outside the grammar this gate can vouch for:"
      printf '        %s: %s\n' "$where" "$what"
      grammar=$((grammar + 1)); continue
    fi
    steps=$((steps + 1))
    cmd="$what"; script=""
    if [[ "$cmd" =~ ^(bash[[:space:]]+)?(\./)?((ci|tools)/[A-Za-z0-9_-]+(/[A-Za-z0-9_-]+)?\.sh)$ ]]; then
      script="${BASH_REMATCH[3]}"
    fi
    if [ -n "$script" ] && is_gate "$script"; then
      gate_calls=$((gate_calls + 1)); called+=("$script"); continue
    fi
    if [ -n "$script" ] && is_workflow_only "$script"; then
      plumbing=$((plumbing + 1)); continue
    fi
    [ "$orphans" -eq 0 ] && bad "workflow step(s) whose check exists nowhere else:"
    printf '        %s: %s\n' "$where" "$cmd"
    orphans=$((orphans + 1))
  done < <(parse "$wf")
done
[ "$grammar" = "0" ] || fail=1
if [ "$orphans" != "0" ]; then
  dim "Nothing in .github/workflows/ runs here. Move the check into ci/<name>.sh and"
  dim "make the step call it — nothing else on the line."
  fail=1
fi

# ── direction 1: every gate is called by some step ────────────────────────
unwired=0
for g in "${gates[@]}"; do
  found=""
  for c in "${called[@]+"${called[@]}"}"; do [ "$c" = "$g" ] && { found=1; break; }; done
  if [ -z "$found" ]; then
    [ "$unwired" -eq 0 ] && bad "gate(s) no workflow step runs:"
    printf '        %s\n' "$g"
    unwired=$((unwired + 1))
  fi
done
if [ "$unwired" != "0" ]; then
  dim "A mention in a comment does not count; a step-level \`run:\` must call it."
  fail=1
fi

# `--list`: the gates in workflow order, once each. run/local-gates.sh runs
# exactly this, so what a local run proves and what CI would prove are one set.
if [ -n "$LIST" ]; then
  [ "$fail" = "0" ] || { bad "refusing to list: the set is not in parity"; exit 1; }
  printf '%s\n' "${called[@]}" | awk '!seen[$0]++' >&3
  exit 0
fi

if [ "$fail" != "0" ]; then
  echo
  bad "gate-parity: FAILED"
  exit 1
fi
ok "${#gates[@]} gate(s), each run by a workflow step"
ok "$steps workflow step(s): $gate_calls call a gate, $plumbing are pinned plumbing, none is inline"
dim "a check added to a workflow and nowhere else cannot pass this."
