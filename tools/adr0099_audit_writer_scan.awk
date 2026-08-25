# ADR-0099 R2 — AUDIT-APPEND PROVENANCE scanner (toolchain-free).
#
# WHY A SECOND SCANNER. `adr0099_write_fork_scan.awk` (CHECK 10M) fires on
# `opener AND append in the same fn`. That predicate has three blind spots the
# seq-2508 recurrence walked straight through:
#
#   B1 — `Handle::read()` is not an "opener". It hands back a full, WRITABLE
#        `Connection` (a `try_clone` of the shared instance) and takes the
#        writer mutex only for the duration of the clone. `db.read()` +
#        `append_in_tx` is therefore a second audit writer holding NEITHER the
#        handle writer mutex NOR `AUDIT_APPEND_LOCK` — a textbook seq fork —
#        and 10M cannot see it, because no name in its opener set appears.
#   B2 — SPLIT sites. A fn that appends on a `&Transaction` / `&mut Ledger`
#        PARAMETER has no opener of its own, so 10M scores it clean no matter
#        what its caller opened (the qc_inspection case, found by hand).
#   B3 — 10M is a BAN-list: it only fails on names it already knows. A NEW
#        provenance (a struct-field `Ledger`, a captured clone, a helper that
#        hands back a `Connection`) is silently clean.
#   B4 — 10M only counts appends to the audit TABLE. The audit ledger has a
#        second half — the `<db>.audit.log` MIRROR — and its writers
#        (`sync_mirror`, `ensure_consistent_with_db`, `replay_mirror_delta`)
#        were not in the append set at all. That is how the seq-2508 recurrence
#        got in: the snapshot daemon reconciled the mirror on its OWN
#        connection, and two `serve` routes followed their shared-Handle tx with
#        an independent `Ledger::open` + `sync_mirror`. A forked mirror refuses
#        the next boot exactly as a forked table does.
#
# This scanner inverts the predicate. It fires on the APPEND — the operation
# that actually forks the chain — and demands that EVERY runtime append site
# prove which serialization domain it is in. Unclassifiable ⇒ FAIL. There is no
# "not on the ban-list" escape.
#
# STATEMENT-scoped, not line-scoped. ADR-0105 F1 found a real `Connection::open`
# laundered past a LINE-scoped exclusion; and the real code splits
# `let mut conn = db` / `.write()?` across lines. Markers are therefore
# evaluated over a statement buffer that spans newlines and resets at `;{}`.
#
# Emitted record, one per runtime fn that appends:
#     <file>:<fname>:<verdict>@L<line>
# verdicts:
#   HANDLE_WRITE   the fn binds the shared writer (`let mut _ = ….write()`) and
#                  holds it across the append — the ONE serialized domain. OK.
#   WITH_LEDGER    the append is inside a `with_ledger(` closure, which holds
#                  the handle mutex AND `AUDIT_APPEND_LOCK`. OK.
#   LEDGER_LOCKED  the append is `Ledger::append`/`append_signed`/
#                  `append_reopen`, which take `AUDIT_APPEND_LOCK` across
#                  read-head → insert → commit themselves.
#   TX_PARAM       the fn appends on a caller-owned `&Transaction`/`&mut Ledger`
#                  parameter. Provenance is the CALLER's; inert on its own.
#   READ_CLONE     *** VIOLATION *** appends on a MUTABLE `.read()` clone (B1).
#   INDEP_OPENER   *** VIOLATION *** appends on an independent opener.
#   UNCLASSIFIED   *** VIOLATION *** appends with no provable provenance (B3).
#
# Provenance is tracked per BINDING and propagated through the derivations the
# real code uses, so `let ledger = Ledger::from_connection(db.read()?, ..)` used
# only for `verify_chain` is a reader (it never appears in a write statement)
# while `if let Ok(l) = Ledger::open(..)` followed by `l.sync_mirror(..)` is an
# INDEP_OPENER even though `l` is immutable.
#
# Comment/string/char-literal aware; `#[cfg(test)]` bodies are skipped.
# `-v allow="a,b"` skips fn names sanctioned by the residual ledger.
BEGIN{ depth=0; tdepth=-1; pending=0; inblk=0; instr=0; fn_depth=-1; fn_pending=0;
       n_allow=split(allow,A,","); n_taint=split(taint,T,",") }
function is_allowed(name,   k){ for(k=1;k<=n_allow;k++) if(A[k]==name) return 1; return 0 }
# `-v taint="f,g"` — treat a CALL to one of these fns as an append. The gate
# driver seeds this from the previous pass's TX_PARAM set and iterates to a
# fixpoint, so a fn that appends only by handing its connection to a helper is
# classified by ITS OWN provenance (blind spot B2 — the qc_inspection split
# write-fork, which had to be found by hand under 10M). Matching is by BARE
# NAME: a name collision pulls an extra fn into the scan, which is the
# fail-CLOSED direction (more sites classified, never fewer).
function calls_tainted(st,   k){
  # Bounded propagation: a call to a caller-owned-tx fn only makes THIS fn a
  # writer if it actually HANDS OVER a connection. Unbounded bare-name taint
  # walks up through every daemon supervisor that merely calls something that
  # calls something — technically fail-closed, but a gate that cries wolf gets
  # switched off, so the rule is "passes a conn/tx/ledger", not "calls".
  if (st !~ /(^|[^A-Za-z0-9_])(conn|connection|tx|txn|ledger|guard)([^A-Za-z0-9_]|$)/) return 0
  for(k=1;k<=n_taint;k++)
    if(T[k]!="" && st ~ ("(^|[^A-Za-z0-9_])" T[k] "[ \t]*\\(")) return 1
  return 0
}
# ── binding provenance ──────────────────────────────────────────────────────
# `prov[name]` records where a local binding's connection came from:
#   "W" shared-Handle write guard   "R" shared-Handle read clone
#   "I" independent opener          "P" a caller-owned PARAMETER (the fn is
#                                       inert on its own; the taint fixpoint is
#                                       what classifies its caller)
# and propagates through the derivations the real code uses
# (`conn.transaction()`, `Ledger::from_connection(conn, ...)`, `let mut c = c;`).
# A `mut`-based capability heuristic was tried first and is WRONG in both
# directions: `Ledger::sync_mirror(&self)` writes the mirror off an IMMUTABLE
# binding (the exact shape of the two `serve` routes R2 removed), while an
# immutable `Ledger::from_connection(db.read()?, ..)` used only for
# `verify_chain` writes nothing.
function worst_prov(st,   n,w){
  w=""
  for(n in prov){
    if(st ~ ("(^|[^A-Za-z0-9_.])" n "([^A-Za-z0-9_]|$)")){
      if(prov[n]=="I") return "I"
      if(prov[n]=="R") w="R"
      else if(prov[n]=="W" && w!="R") w="W"
      else if(prov[n]=="P" && w=="") w="P"
    }
  }
  return w
}
function note_worst(v){
  if(v=="I") saw_indep=1
  else if(v=="R") saw_read=1
  else if(v=="W") saw_write=1
  else if(v=="P") cur_txparam=1
}
# Evaluate the accumulated statement, then reset it.
function eval_stmt(   src,bind,inh,isw){
  if (stmt=="") { stmt_ln=0; return }
  isw=0
  # ── writes: the operations that actually extend the ledger ────────────────
  if (stmt ~ /append_reopen[ \t]*\(/)          { cur_lockedapi=1; isw=1 }
  if (stmt ~ /\.append(_signed)?[ \t]*\(/ \
      && stmt !~ /\.append[ \t]*\([ \t]*(true|false)[ \t]*\)/ \
      && stmt !~ /OpenOptions/)                  { cur_lockedapi=1; isw=1 }
  if (stmt ~ /append_in_tx(_signed)?[ \t]*\(/) { isw=1 }
  # B4 — the MIRROR half of the ledger. `sync_mirror` is internally atomic under
  # its own flock, but WHICH connection it reads is still a provenance question:
  # a stale non-shared instance mirrors a stale head.
  #
  # ROUND 6 — matched on the `sync_mirror` PREFIX, not a bare `sync_mirror(`.
  # Round 5 split the per-commit path out under a NEW public name,
  # `sync_mirror_lockstep`, and the narrow token stopped matching it: the
  # `aberp-db` WriteGuard `drop` that mirrors every committed write went from
  # `UNCLASSIFIED` to producing NO record at all, and 10P-2 reported it as a
  # residual that had "migrated off" — an invitation to delete a live entry
  # from the frozen manifest. A gate that goes quiet because a function was
  # RENAMED is the name-keyed-bypass class ADR-0111 R2 was bitten by; the
  # prefix form is what CHECK 10L-b already uses. Pinned by 10P-0.
  if (stmt ~ /sync_mirror[A-Za-z0-9_]*[ \t]*\(/ || stmt ~ /ensure_consistent_with_db[ \t]*\(/ \
      || stmt ~ /replay_mirror_delta[ \t]*\(/) isw=1
  if (n_taint>0 && calls_tainted(stmt)) isw=1
  if (stmt ~ /with_ledger[ \t]*\(/) cur_withledger=1

  # ── provenance of this statement ──────────────────────────────────────────
  # `.write()` / `.read()` with EMPTY parens is the aberp-db Handle api; io
  # `write(buf)` / `read(buf)` always take an argument, so this does not collide.
  src=""
  if (stmt ~ /\.write[ \t]*\([ \t]*\)/)      src="W"
  else if (stmt ~ /\.read[ \t]*\([ \t]*\)/)  src="R"
  else if (stmt ~ /Ledger::from_connection[ \t]*\(/) {
    # The SANCTIONED shared-instance seam: it wraps an already-open connection,
    # so it inherits that connection's provenance. Only an unrecognised source
    # makes it independent.
    inh=worst_prov(stmt); src=(inh!="" ? inh : "I")
    # `Ledger::from_connection(conn, ..)` where `conn` is a caller-owned
    # PARAMETER is the caller's provenance ("P"), never independent — the taint
    # fixpoint is what then classifies the caller (blind spot B2).
  }
  else if (stmt ~ /(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)[ \t]*\(/ \
           && stmt !~ /open_in_memory/) src="I"
  # Inheritance is only along CONNECTION-shaped derivations. Without this bound
  # `.read()` — which the aberp-db Handle shares with `RwLock::read()`, both
  # empty-paren — leaks a bogus "R" into every value computed from the guarded
  # data (`let login = match state.boot_state.read() { .. }` then
  # `Actor::from_local_cli(.., &login)`), and the shutdown audit writer reads as
  # a read-clone fork when it is on `db.write()`. Provenance is about
  # connections, so it travels only where a connection travels.
  else if (stmt ~ /\.(transaction|unchecked_transaction|try_clone|conn)[ \t]*\(/ \
           || stmt ~ /from_connection[ \t]*\(/ \
           || stmt ~ /^[ \t]*let[ \t]+(mut[ \t]+)?[A-Za-z0-9_]+[ \t]*=[ \t]*[A-Za-z0-9_]+[ \t]*$/) \
    src=worst_prov(stmt)
  else src=""

  # ── record the binding this statement introduces ──────────────────────────
  bind=""
  if (match(stmt,/(^|[^A-Za-z0-9_])let[ \t]+(mut[ \t]+)?[A-Za-z0-9_]+[ \t]*=/)) {
    bind=substr(stmt,RSTART,RLENGTH)
    sub(/^.*let[ \t]+/,"",bind); sub(/^mut[ \t]+/,"",bind); sub(/[ \t]*=$/,"",bind)
  } else if (match(stmt,/(^|[^A-Za-z0-9_])let[ \t]+(Ok|Some)\([ \t]*(mut[ \t]+)?[A-Za-z0-9_]+[ \t]*\)[ \t]*=/)) {
    # `if let Ok(ledger) = Ledger::open(..)` — an IMMUTABLE binding that can
    # still write the mirror. This arm is what makes the two removed `serve`
    # routes detectable at all.
    bind=substr(stmt,RSTART,RLENGTH)
    sub(/^.*\(/,"",bind); sub(/^[ \t]*mut[ \t]+/,"",bind); sub(/[ \t]*\).*$/,"",bind)
  }
  # `match db.write() { Ok(guard) => .. }`: the scrutinee statement ends at the
  # `{`, so the arm that names the guard is a SEPARATE statement. Carry the
  # scrutinee's provenance across to it.
  if (bind=="" && match(stmt,/^[ \t]*(Ok|Some)\([ \t]*(mut[ \t]+)?[A-Za-z0-9_]+[ \t]*\)[ \t]*=>/) \
      && block_src!="") {
    bind=substr(stmt,RSTART,RLENGTH)
    sub(/^[ \t]*(Ok|Some)\([ \t]*/,"",bind); sub(/^mut[ \t]+/,"",bind); sub(/[ \t]*\).*$/,"",bind)
    src=block_src
  }
  if (bind!="" && src!="") prov[bind]=src
  # Remember this statement's provenance for the block it may be opening.
  block_src=src

  if (isw) {
    if (!cur_app) { cur_app=1; cur_app_ln=stmt_ln }
    note_worst(src!="" ? src : worst_prov(stmt))
  }
  stmt=""; stmt_ln=0
}
function verdict(   ){
  # Order matters: a violation must never be masked by a benign co-occurrence.
  # A fn that writes through the shared guard AND through an independent opener
  # is an INDEP_OPENER — the safe write does not redeem the forking one.
  if (saw_indep)      return "INDEP_OPENER"
  if (saw_read)       return "READ_CLONE"
  if (saw_write)      return "HANDLE_WRITE"
  if (cur_withledger) return "WITH_LEDGER"
  # TX_PARAM outranks LEDGER_LOCKED deliberately. A fn that appends on a
  # `&mut Ledger` PARAMETER holds AUDIT_APPEND_LOCK, but that lock does not
  # exclude a handle-domain writer (ADR-0105's open residual), so the
  # load-bearing question is still what its CALLER opened. Ranking
  # LEDGER_LOCKED first masked exactly that: the audit-ledger SESSION api
  # (`heartbeat`, `open_service_session`, ...) scored LEDGER_LOCKED, so the taint
  # fixpoint never reached the DAP heartbeat daemon that drives it — blind spot
  # B2, reopened through the Ledger api.
  if (cur_txparam)    return "TX_PARAM"
  if (cur_lockedapi)  return "LEDGER_LOCKED"
  return "UNCLASSIFIED"
}
function flush(   v){
  eval_stmt()
  if (cur_fn!="" && cur_app && !is_allowed(cur_fn)) {
    v = verdict()
    printf "%s:%s:%s@L%d\n", cur_file, cur_fn, v, cur_app_ln
  }
  cur_app=0; cur_app_ln=0; saw_read=0; saw_write=0; saw_indep=0
  cur_withledger=0; cur_txparam=0; cur_lockedapi=0; stmt=""; stmt_ln=0
  delete prov
}
FNR==1 {
  # New file: close out the previous one (its record is attributed to
  # `cur_file`, captured at the fn declaration) and reset every positional
  # state. Without this the scanner may only be given one file per process.
  flush(); cur_fn=""; fn_depth=-1; fn_pending=0; sig_line=0
  depth=0; tdepth=-1; pending=0; inblk=0; instr=0
}
{
  line=$0
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    if (fn_depth<0 || depth<=fn_depth) {
      flush()
      f=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",f); cur_fn=f; fn_pending=1; sig_line=1
      cur_file=FILENAME
    }
  }
  st=line; sub(/^[ \t]+/,"",st)
  if (st ~ /^#\[cfg\(/ && st ~ /test/ && st !~ /not\(test\)/) pending=1
  was_in=(tdepth>=0)
  # Build the code-only view (strings / // and /* */ comments / char literals
  # stripped) and drive brace depth off it.
  code=""; L=length(line)
  for(i=1;i<=L;i++){
    c=substr(line,i,1); d=substr(line,i,2)
    if(inblk){ if(d=="*/"){inblk=0;i++} ; continue }
    if(instr){ if(c=="\\"){i++;continue} ; if(c=="\""){instr=0} ; continue }
    if(d=="//"){ break }
    if(d=="/*"){ inblk=1;i++;continue }
    if(c=="\""){ instr=1; continue }
    if(c=="'"){
       if(substr(line,i,3) ~ /^'\\.'/){ i+=2 }
       else if(substr(line,i+2,1)=="'"){ i+=2 }
       continue
    }
    code=code c
    if(c=="{"){
      depth++
      if(pending && tdepth<0){ tdepth=depth; pending=0 }
      if(fn_pending){ fn_depth=depth; fn_pending=0; in_sig=0 }
    } else if(c=="}"){
      if(tdepth==depth) tdepth=-1
      if(fn_depth>=0 && depth==fn_depth){ flush(); cur_fn=""; fn_depth=-1 }
      depth--
    }
  }
  now_in=(tdepth>=0); intest = was_in || now_in
  if (intest || cur_fn=="") { sig_line=0; next }
  # Signature: harvest the caller-owned-tx / ledger params, never markers. Both
  # while the signature is still open (`fn_pending`) AND on the declaration line
  # itself, because a one-line `fn h(tx: &Transaction) {` has already closed
  # `fn_pending` by the time we get here — the shape most helper fns have.
  if (fn_pending || sig_line) {
    if (code ~ /&(mut[ \t]+)?(duckdb::)?Transaction/ \
        || code ~ /&mut[ \t]+(aberp_audit_ledger::|audit_ledger::)?Ledger/ \
        || code ~ /&(mut[ \t]+)?(duckdb::)?Connection/ \
        || code ~ /:[ \t]*(duckdb::)?Connection[ ,)]/ \
        || code ~ /tx:[ \t]*&/) cur_txparam=1
    # Seed the PARAM binding with provenance "P" so a body that derives from it
    # (`Ledger::from_connection(conn, ..)`, `conn.transaction()`) is read as
    # caller-owned rather than independent.
    if (match(code,/[A-Za-z0-9_]+[ \t]*:[ \t]*&?[ \t]*(mut[ \t]+)?(duckdb::|aberp_audit_ledger::|audit_ledger::)?(Connection|Transaction|Ledger)([ \t]*<|[ \t]*,|[ \t]*\)|$)/)) {
      pn=substr(code,RSTART,RLENGTH); sub(/[ \t]*:.*$/,"",pn); sub(/^[ \t]*/,"",pn)
      if (pn!="") prov[pn]="P"
    }
    if (fn_pending) { sig_line=0; next }
    # One-line signature: strip it off before the statement scanner sees it, so
    # a default-arg-looking token in the params cannot be read as a statement.
    sub(/^.*\{/,"",code)
  }
  sig_line=0
  # Accumulate the statement buffer across newlines; `;` and braces end it.
  n=length(code)
  for(j=1;j<=n;j++){
    ch=substr(code,j,1)
    if(ch==";" || ch=="{" || ch=="}"){ eval_stmt() }
    else { if(stmt=="") stmt_ln=FNR; stmt = stmt ch }
  }
  # A newline inside a statement acts as whitespace.
  if (stmt!="") stmt = stmt " "
}
END{ flush() }
