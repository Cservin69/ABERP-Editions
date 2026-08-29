# ADR-0116 D2 — tenant-home removal scanner (cut-gate CHECK 11).
#
# Emits one record per filesystem-removal site in a Rust source file:
#
#   <file>:<fn>:<VERDICT>:<token>@L<n>
#
# VERDICTs:
#   GUARDED       the enclosing fn routes its removals through the ADR-0116 D2
#                 guard (`is_protected_evidence` / `guarded_remove` appear in
#                 the same fn body).
#   TENANT_HOME   the fn removes a path AND reaches a tenant-home path (it
#                 mentions a tenant-home token), with NO guard. **This is the
#                 class the gate freezes.** An unguarded unlink beside the live
#                 DB is permanent loss of the only record of a durability
#                 incident, and the ADR's own example — a "clean up the tenant
#                 home" helper meeting no guard — already existed in this tree
#                 (`recover::cleanup_siblings_with_infix`).
#   OTHER         a removal in a fn with no tenant-home reach — staging files,
#                 export dirs, temp scratch. Reported for corpus liveness only.
#
# ## Why a fn-scoped model, stated so nobody mistakes it for more
#
# The risk the ADR names is a helper that enumerates a tenant directory and
# unlinks by name or prefix. That shape puts the tenant-home token and the
# removal in the SAME fn body, which is what this can see. It is NOT a
# whole-program taint closure (CHECK 10N is that, for a different class), and
# it does not pretend to be. The gate carries a liveness floor so a scanner
# that stops classifying cannot read as "no violations".
#
# ## Structure detection is INDENT-based, not brace-counting
#
# The first cut counted braces and drifted: a `'{'` char literal or a raw
# string inside a 36k-line file desynchronised the depth, which silently ended
# the `#[cfg(test)] mod tests` block early and leaked two test fns into the
# TENANT_HOME class. That is the same failure mode as the ADR-0098 opener-scan
# char-literal bug already recorded in this repo — a scanner whose verdict can
# be flipped by editing an unrelated literal.
#
# `cargo fmt --all -- --check` is a REQUIRED CI step here, so rustfmt's layout
# is guaranteed: an item declared at indent N is closed by the first line whose
# stripped content is exactly `}` (or `};`) at indent N. Matching on that is
# both simpler and immune to literals.

function indent_of(line,   i, c, n) {
  n = 0
  for (i = 1; i <= length(line); i++) {
    c = substr(line, i, 1)
    if (c == " ") n++
    else if (c == "\t") n += 4
    else break
  }
  return n
}

function strip(line,   out, i, c, instr, inchar, prev) {
  out = ""; instr = 0; inchar = 0; prev = ""
  for (i = 1; i <= length(line); i++) {
    c = substr(line, i, 1)
    if (instr) {
      if (c == "\"" && prev != "\\") instr = 0
      prev = (c == "\\" && prev == "\\") ? "" : c
      continue
    }
    if (inchar) {
      if (c == "'" && prev != "\\") inchar = 0
      prev = (c == "\\" && prev == "\\") ? "" : c
      continue
    }
    if (c == "\"") { instr = 1; prev = c; continue }
    if (c == "/" && substr(line, i + 1, 1) == "/") break
    out = out c
    prev = c
  }
  return out
}

BEGIN {
  # Tokens that mean "this fn can reach a path inside a tenant home". Drawn
  # from the shapes that actually occur at removal sites in this tree.
  nth = split("db_path|mirror_path|tenant_home|tenant_dir|wal_sibling|marker_path|install_intent_path|.aberp|audit.log|CORRUPT|preserve_|evidence|sibling|live_db|.wal", TH, "|")
  ng  = split("is_protected_evidence|guarded_remove", GUARD, "|")
}

FNR == 1 {
  file = FILENAME
  fn_name = ""; fn_indent = -1; body = ""; hits = ""; in_test_fn = 0
  test_indent = -1; pending_test = 0
  inblock = 0
}

{
  raw = $0
  line = strip(raw)
  ind = indent_of(raw)
  trimmed = line; gsub(/^[ \t]+|[ \t]+$/, "", trimmed)

  # /* */ block comments.
  if (inblock) {
    if (index(line, "*/") > 0) { line = substr(line, index(line, "*/") + 2) }
    else next
    inblock = 0
  }
  while (index(line, "/*") > 0) {
    pre = substr(line, 1, index(line, "/*") - 1)
    rest = substr(line, index(line, "/*") + 2)
    if (index(rest, "*/") > 0) { line = pre substr(rest, index(rest, "*/") + 2) }
    else { line = pre; inblock = 1; break }
  }

  # ── cfg(test) scope, closed by indentation ──────────────────────────
  if (test_indent >= 0 && ind <= test_indent && trimmed ~ /^\}[;,]?$/) {
    test_indent = -1
  }
  if (line ~ /#\[cfg\(test\)\]/) { pending_test = 1; pending_indent = ind; next }
  if (pending_test) {
    # The attributed item opens here. Everything until its closing brace at
    # the same indent is test code.
    if (test_indent < 0) test_indent = pending_indent
    pending_test = 0
  }
  # A `mod tests`/`mod *_tests` block with no attribute is still test code.
  if (test_indent < 0 && trimmed ~ /^(pub[ \t]+)?mod[ \t]+[A-Za-z_0-9]*tests?[ \t]*\{/) {
    test_indent = ind
  }

  # ── fn scope, closed by indentation ─────────────────────────────────
  if (fn_name != "" && ind <= fn_indent && trimmed ~ /^\}[;,]?$/) {
    emit()
    fn_name = ""; fn_indent = -1
  }
  if (line ~ /(^|[^A-Za-z_0-9])fn[ \t]+[A-Za-z_][A-Za-z_0-9]*[ \t]*[(<]/) {
    if (fn_name != "") emit()
    m = line
    sub(/.*[^A-Za-z_0-9]fn[ \t]+/, "", m); sub(/^[ \t]*fn[ \t]+/, "", m)
    sub(/[ \t]*[(<].*$/, "", m)
    fn_name = m
    fn_indent = ind
    body = ""; hits = ""
    in_test_fn = (test_indent >= 0)
  }

  if (fn_name != "") {
    body = body " " line
    if (line ~ /fs::remove_(file|dir_all|dir)[ \t]*\(/) {
      tok = "remove_file"
      if (line ~ /remove_dir_all/) tok = "remove_dir_all"
      else if (line ~ /remove_dir[ \t]*\(/) tok = "remove_dir"
      hits = hits " " tok "@L" FNR
    }
  }
}

END { if (fn_name != "") emit() }

function emit(   i, guarded, reaches, verdict, n, parts) {
  if (hits == "" || in_test_fn) { body = ""; hits = ""; return }
  guarded = 0
  for (i = 1; i <= ng; i++) if (index(body, GUARD[i]) > 0) guarded = 1
  reaches = 0
  for (i = 1; i <= nth; i++) if (index(body, TH[i]) > 0) reaches = 1
  verdict = guarded ? "GUARDED" : (reaches ? "TENANT_HOME" : "OTHER")
  n = split(hits, parts, " ")
  for (i = 1; i <= n; i++) {
    if (parts[i] == "") continue
    print file ":" fn_name ":" verdict ":" parts[i]
  }
  body = ""; hits = ""
}
