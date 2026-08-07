# ADR-0105 — TRANSITIVE (wrapper-aware) audit-ledger WRITE-FORK scanner.
#
# WHY THIS EXISTS (the CHECK 10M blind spot)
# ------------------------------------------
# tools/adr0099_write_fork_scan.awk (CHECK 10M) flags a runtime fn that contains
# BOTH an independent live-DB opener AND an audit append — but only when BOTH
# tokens appear in the SAME function body. That is a per-function, syntactic
# model, and it is BLIND to the most common real shape:
#
#     fn daemon_tick(..) {
#         let mut ledger = Ledger::open(..);      // <- opener here
#         write_event(&mut ledger, ..);           // <- append ONE call away
#     }
#     fn write_event(l: &mut Ledger, ..) { l.append_signed(..) }   // <- append here
#
# Neither fn trips 10M: `daemon_tick` has no append token, `write_event` has no
# opener token. The pre-fix aberp-mes writer (`write_mes_adapter_event`) was
# exactly this shape and scanned CLEAN while forking the chain in production.
# The ADR-0099 residual manifest already records a second instance of the same
# miss ("qc_inspection::record_manual_inspection — a SPLIT write-fork ... that
# the per-fn scanner did not flag"). It is a systematic hole, not a one-off.
#
# WHAT THIS ADDS — a whole-program taint closure over function DEFINITIONS
# -----------------------------------------------------------------------
#   seed   : the append PRIMITIVES — append_in_tx_signed (the ADR-0087
#            chokepoint every audit row funnels through) and append_reopen
#            (itself an open+append).
#   step   : a runtime fn DEFINITION whose body calls a tainted definition
#            becomes tainted (it can reach an audit append).
#   repeat : to a fixpoint, bounded by -v levels=N (default 12). Convergence is
#            asserted — a truncated closure is a HARNESS FAULT (exit 3), never a
#            silently weaker scan.
#
# Taint is per-DEFINITION, resolved by name only when that name has EXACTLY ONE
# definition in the scanned corpus. This precision is not optional: an earlier
# draft keyed taint on the bare NAME, which unioned the callees of all ~200 fns
# named `new` (and ~50 named `open`) into one bucket — one tainted `new`
# poisoned the whole corpus and every opener trivially reported `via=open`,
# because `Connection::open(` itself yields the callee token `open`.
#
# THE RESIDUAL BLIND SPOT IS REPORTED, NOT SWALLOWED
# --------------------------------------------------
# When a callee name is defined MORE than once and at least one of those
# definitions is tainted, this scanner cannot say which one is called. It does
# NOT guess (either way would be wrong): it emits an AMBIGUOUS record. Those are
# frozen in the manifest exactly like the others, so the unresolvable set can
# only shrink and a real fork can never hide inside it silently.
#
# OUTPUT (one record per offending runtime fn; line-number-free key so a benign
# shift never churns the frozen manifest — cf. CHECK 10k):
#   <file>:<fn>:<DIRECT|TRANSITIVE|AMBIGUOUS>:opener@L<n>:via=<callee>
#
# Comment/string/CHAR-LITERAL aware — the char-literal walk is carried over
# verbatim from the 10M scanner (ADR-0098 pinned a bug where a `'` inside a doc
# COMMENT flipped a scanner's parse; do not "simplify" it).
#
# Usage:
#   awk -v allow="a,b,c" [-v levels=12] -f adr0105_wrapper_fork_scan.awk FILES...
# `allow` is the SANCTIONED-fn list (pre-serve boot openers / separate-process
# CLI one-shots / the primitives themselves), matched on fn NAME as in 10M.
# NOTE: allow-listing suppresses the RECORD but never the TAINT — a sanctioned
# fn still propagates reachability to its callers.

BEGIN{
  depth=0; tdepth=-1; pending=0; inblk=0; instr=0; fn_depth=-1; fn_pending=0
  n_allow=split(allow,A,",")
  if (levels=="") levels=12
  ndef=0

  # The true append primitives. `append_in_tx`, `Ledger::append` and
  # `Ledger::append_signed` are deliberately NOT seeded — they are ordinary fns
  # in crates/audit-ledger whose bodies reach the chokepoint, so the closure
  # DERIVES them. Deriving rather than seeding keeps the seed honest: a fourth
  # primitive that bypassed the chokepoint would NOT be silently covered, and
  # the ADR-0105 census would have to be revisited.
  SEEDNAME["append_in_tx_signed"]=1
  SEEDNAME["append_reopen"]=1

  # Control flow / constructors / macros that `name(` also matches.
  split("if,while,for,match,return,fn,unsafe,in,else,loop,move,as,let,Some,Ok,Err,None,Box,Vec,String,format,println,eprintln,print,write,writeln,assert,assert_eq,assert_ne,panic,vec,dbg,todo,unimplemented,matches,cfg,derive", KW, ",")
  for (k in KW) KEYWORD[KW[k]]=1
}

function is_allowed(name,   k){ for(k=1;k<=n_allow;k++) if(A[k]==name) return 1; return 0 }

# Resolve callee `n` seen inside file `f` to a definition index, or 0 when it
# cannot be resolved unambiguously. Same-file wins (Rust module scoping);
# otherwise a corpus-unique name resolves; anything else is AMBIGUOUS.
function resolve(f, n){
  if (FCOUNT[f SUBSEP n]==1) return FIDX[f SUBSEP n]
  if (DEFCOUNT[n]==1)        return DEFIDX[n]
  return 0
}

# Pull every called identifier out of a comment/string-stripped code line.
# Matches `foo(`, `.foo(` and `Type::foo(` (anchors on the name immediately
# followed by `(`, so `Ledger::open(` yields `open`).
function collect_calls(s,   tmp, nm, out){
  out=""; tmp=s
  while (match(tmp, /[A-Za-z_][A-Za-z0-9_]*[ \t]*\(/)) {
    nm=substr(tmp, RSTART, RLENGTH)
    sub(/[ \t]*\($/, "", nm)
    if (!(nm in KEYWORD)) out = out " " nm
    tmp = substr(tmp, RSTART+RLENGTH)
  }
  return out
}

# Close out the fn whose body just ended, recording ONE definition record.
function flush(   ){
  if (cur_fn!="") {
    ndef++
    # cur_file, not FILENAME: flush() also fires on the FNR==1 rule of the NEXT
    # file, where FILENAME has already advanced.
    DFILE[ndef]=cur_file; DNAME[ndef]=cur_fn
    DOPEN[ndef]=cur_open ? cur_open_ln : 0
    DAPP[ndef]=cur_app; DCALLS[ndef]=cur_calls; DHANDLE[ndef]=cur_handle
    DESCTO[ndef]=cur_escto; DRECV[ndef]=cur_recv
    DTEST[ndef]=cur_istest
    # A #[cfg(test)] fn must NOT enter the resolution index. Its body is skipped
    # (so it can never be tainted), but leaving its NAME in the index lets a test
    # helper shadow the runtime definition it shares a name with: same-file
    # resolution sees 2 and gives up, and — the real hazard — a name whose only
    # in-corpus definition is a test fn would resolve to an untainted record and
    # silently UNDER-report. Test defs are still recorded (so brace/flush
    # bookkeeping is unchanged) but are invisible to resolve().
    if (!cur_istest) { DEFCOUNT[cur_fn]++; DEFIDX[cur_fn]=ndef }
    # Same-FILE index. Rust resolves an unqualified call to the enclosing
    # module first, so a same-file definition is the callee with very high
    # confidence — this is what disambiguates the several private helpers that
    # share a name across files (`write_attempt_audit` exists in both
    # submit_invoice.rs and drain_submission_queue.rs, `write_check_performed_audit`
    # in both retry_submission.rs and drain_pending_retries.rs).
    if (!cur_istest) { FCOUNT[cur_file SUBSEP cur_fn]++; FIDX[cur_file SUBSEP cur_fn]=ndef }
  }
  cur_open=0; cur_app=0; cur_open_ln=0; cur_calls=""; cur_handle=0
  cur_openvars=""; cur_escto=""; cur_recv=""; pending_call=""; cdepth=0; cur_istest=0
}

# Per-file reset — brace/comment state must not leak across files.
FNR==1 { flush(); cur_fn=""; depth=0; tdepth=-1; pending=0; inblk=0; instr=0; fn_depth=-1; fn_pending=0 }

{
  line=$0
  is_decl=0; isopenline=0
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    if (fn_depth<0 || depth<=fn_depth) {
      flush()
      f=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",f); cur_fn=f; cur_file=FILENAME; fn_pending=1
      cur_istest=(tdepth>=0)   # declared inside a #[cfg(test)] block
      # The declaration line's own `name(` must NOT be collected as a callee —
      # it would make every fn appear to call itself and report `via=<itself>`
      # instead of the real wrapper.
      is_decl=1
    }
  }
  st=line; sub(/^[ \t]+/,"",st)
  if (st ~ /^#\[cfg\(/ && st ~ /test/ && st !~ /not\(test\)/) pending=1
  was_in=(tdepth>=0)

  # code-only view (strip strings / // and /* */ comments / char literals)
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
      if(fn_pending){ fn_depth=depth; fn_pending=0 }
    } else if(c=="}"){
      if(tdepth==depth) tdepth=-1
      if(fn_depth>=0 && depth==fn_depth){ flush(); cur_fn=""; fn_depth=-1 }
      depth--
    }
  }
  now_in=(tdepth>=0); intest = was_in || now_in
  if (intest || cur_fn=="") next

  # INDEPENDENT live-DB opener? (identical token set to the 10M scanner.
  # open_in_memory is the sanctioned shared-instance seam. `from_connection` is
  # deliberately NOT excluded — ADR-0105 F1: the exclusion was LINE-scoped, so
  # `Ledger::from_connection(Connection::open(p)?, ..)` laundered a real
  # independent opener past every scanner in the tree.)
  if ((code ~ /(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)\(/ \
       || code ~ /append_reopen[ \t]*\(/) \
      && code !~ /open_in_memory/) {
    if(!cur_open){ cur_open=1; cur_open_ln=FNR }
    # Bind the opened value's variable so we can ask the decisive question
    # later: does THIS connection actually reach the append? Two shapes cover
    # every opener in the tree — `let [mut] v = X::open(..)` and
    # `if let Ok([mut] v) = X::open(..)`.
    ov=""
    if (match(code, /if[ \t]+let[ \t]+Ok\([ \t]*(mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*[ \t]*\)/)) {
      ov=substr(code,RSTART,RLENGTH); sub(/.*Ok\([ \t]*(mut[ \t]+)?/,"",ov); sub(/[ \t]*\).*/,"",ov)
    } else if (match(code, /let[ \t]+(mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*[ \t]*=/)) {
      ov=substr(code,RSTART,RLENGTH); sub(/^let[ \t]+(mut[ \t]+)?/,"",ov); sub(/[ \t]*=.*/,"",ov)
    }
    if (ov!="") { cur_openvars = cur_openvars " " ov; isopenline=1 }
  }
  # The same two non-audit `.append(` shapes must also be kept out of the CALLEE
  # list, not just out of the direct-append test: `collect_calls` would otherwise
  # harvest the bare name `append` from `OpenOptions::append(true)`, and since
  # `Ledger::append` is the corpus's only definition of that name, it resolves —
  # silently tainting `mirror::sync_mirror` and every route that syncs the mirror.
  callcode=code
  gsub(/\.append[ \t]*\([ \t]*(true|false)[ \t]*\)/, "", callcode)
  gsub(/\.append[ \t]*\([ \t]*&mut /, "", callcode)
  linenames = collect_calls(callcode)
  if (!is_decl) cur_calls = cur_calls linenames

  # Does an opened value ESCAPE into a call (`&v` / `&mut v` / `f(.., v, ..)`),
  # or serve as the RECEIVER of a method (`v.foo(..)`)? Skip the binding line
  # itself so `let mut conn = Connection::open(..)` is not its own escape.
  #
  # The escape must be attributed to the SPECIFIC callee that receives it, not
  # merely recorded as "something escaped somewhere in this fn". Without that,
  # serve::pickup_quote_as_draft_request reports as a fork: it passes its
  # independently-opened ledger to a READ helper (count_prior_pickups) while its
  # append runs on the Handle guard — an escape and a tainted callee coexist in
  # the fn but are unrelated.
  if (cur_openvars!="" && !isopenline) {
    nov=split(cur_openvars, OV, " ")
    for(ovi=1; ovi<=nov; ovi++){
      v=OV[ovi]
      if (code ~ ("&[ \t]*(mut[ \t]+)?" v "[^A-Za-z0-9_]") \
          || code ~ ("[(,][ \t]*" v "[ \t]*[,)]")) {
        # Receivers of this escape: every callee named on this line, plus the
        # call still open from an earlier line (multi-line argument lists — the
        # `open_service_session_and_recover(\n &mut ledger,` shape).
        cur_escto = cur_escto " " linenames
        if (pending_call!="") cur_escto = cur_escto " " pending_call
      }
      rest=code
      while (match(rest, (v "\\.[A-Za-z_][A-Za-z0-9_]*[ \t]*\\("))) {
        m=substr(rest,RSTART,RLENGTH); sub(/^[^.]*\./,"",m); sub(/[ \t]*\($/,"",m)
        cur_recv = cur_recv " " m
        rest=substr(rest,RSTART+RLENGTH)
      }
    }
  }

  # Track the call whose argument list is still open, for the multi-line case.
  lp=code; rp=code
  nlp=gsub(/\(/,"(",lp); nrp=gsub(/\)/,")",rp)
  if (nlp>nrp) { nn=split(linenames, LN, " "); if (nn>0) pending_call=LN[nn]; cdepth += (nlp-nrp) }
  else { cdepth -= (nrp-nlp); if (cdepth<=0) { cdepth=0; pending_call="" } }
  # DIRECT append? Same token set as the 10M scanner, MINUS two non-audit `.append(`
  # shapes that would poison the TAINT closure (10M itself never needed the
  # distinction — it only ever asks about a single fn, never propagates):
  #   `.append(true)` / `.append(false)` — std::fs::OpenOptions::append. This is
  #      NOT hypothetical: it is how `audit_ledger::mirror::sync_mirror` opens the
  #      mirror sidecar, which made `sync_mirror` a tainted "audit appender" and
  #      spuriously flagged every route that syncs the mirror after a write.
  #   `.append(&mut ..)` — Vec::append (e.g. mes_manager.rs `handles.append(&mut spawned)`).
  # A real audit append is `.append(kind, payload, actor, idem)` / `.append_signed(..)`.
  if ((code ~ /\.append(_signed)?[ \t]*\(/ \
       && code !~ /\.append[ \t]*\([ \t]*(true|false)[ \t]*\)/ \
       && code !~ /\.append[ \t]*\([ \t]*&mut /) \
      || code ~ /append_in_tx(_signed)?[ \t]*\(/ \
      || code ~ /append_reopen[ \t]*\(/) {
    if(!cur_app){ cur_app=1 }
  }
  # SHARED-HANDLE ACQUIRE — `aberp_db::Handle::write()`, the one process-wide
  # audit serialization point. Empty parens discriminate it from `io::Write`
  # (`.write(&buf)`); an `RwLock::write()` would also match, but the barrier is
  # only ever CONSULTED on a definition that already reaches an append, so a
  # lock-only `.write()` in a non-appending fn is inert.
  if (code ~ /\.write\([ \t]*\)/) cur_handle=1
}

END{
  flush()

  # ── taint fixpoint over DEFINITIONS ──────────────────────────────────────
  for(r=1;r<=ndef;r++) if(DAPP[r]) TD[r]=1
  converged=0
  for(it=1; it<=levels; it++){
    changed=0
    for(r=1;r<=ndef;r++){
      if(TD[r]) continue
      c=split(DCALLS[r], C, " ")
      for(i=1;i<=c;i++){
        if(C[i]==DNAME[r]) continue                       # direct recursion
        if(C[i] in SEEDNAME){ TD[r]=1; changed=1; break }
        # A tainted callee that acquires the SHARED HANDLE is a serialization
        # BOUNDARY, not a carrier: it takes the one process-wide writer guard
        # itself, so whatever connection its caller happens to hold is
        # irrelevant to the audit chain. This is the exact post-ADR-0099 shape —
        # e.g. quality::create_ncr keeps an independent Connection::open for the
        # BUSINESS row (INSERT INTO ncrs, a frozen CHECK 10i/10k opener) while
        # its audit append goes through quality::append_event -> db.write().
        # Without this barrier every migrated seam reports as a fork.
        t=resolve(DFILE[r], C[i])
        if(t && TD[t] && !DHANDLE[t]){ TD[r]=1; changed=1; break }
      }
    }
    if(!changed){ converged=1; break }
  }
  # A non-converged closure is WEAKER than a converged one (more wrappers could
  # still become tainted). Fail loudly rather than under-report.
  if(!converged){
    printf("ADR-0105 SCANNER: taint closure did NOT converge in levels=%d — raise -v levels\n", levels) > "/dev/stderr"
    exit 3
  }

  # ANYTAINT tracks names where SOME definition can reach an append without
  # crossing a Handle barrier — used only to report the unresolvable residue.
  for(r=1;r<=ndef;r++) if(TD[r] && !DHANDLE[r]) ANYTAINT[DNAME[r]]=1
  for(n in SEEDNAME) ANYTAINT[n]=1
  # Barrier audit trail: every tainted definition that was treated as a
  # serialization boundary. Printed to stderr so the gate log shows exactly what
  # the scanner chose NOT to propagate — a silent barrier would be a blind spot
  # of the same kind this scanner exists to close.
  if (show_barriers=="1")
    for(r=1;r<=ndef;r++) if(TD[r] && DHANDLE[r])
      printf("ADR-0105 BARRIER (handle-routed, taint stops here): %s:%s\n", DFILE[r], DNAME[r]) > "/dev/stderr"

  for(r=1;r<=ndef;r++){
    if(!DOPEN[r]) continue
    if(is_allowed(DNAME[r])) continue
    if(DAPP[r]){
      printf "%s:%s:DIRECT:opener@L%d:via=-\n", DFILE[r], DNAME[r], DOPEN[r]
      continue
    }
    # REACHABILITY. A fork needs the INDEPENDENTLY-OPENED connection to be the
    # one that appends. Two ways that happens:
    #   • the opened value is PASSED to a tainted wrapper (`f(&mut conn, ..)`), or
    #   • a tainted method is invoked ON it (`ledger.append(..)` — already DIRECT,
    #     but a tainted non-primitive method would land here).
    # If the opened value only ever receives an UNtainted method, the opener is
    # not feeding the audit chain and this is not a fork. That is the dominant
    # benign shape in this tree: a post-commit `Ledger::open(..).sync_mirror(..)`
    # sitting in a fn whose audit append runs on a caller-supplied or
    # Handle-derived connection (retry_submission::perform_layer_2_check,
    # serve::create_stock_movement_request, serve::pickup_quote_as_draft_request).
    # Only the callees that actually RECEIVE the opened value (as an argument or
    # as a method receiver) can turn this opener into a fork.
    c=split(DESCTO[r] " " DRECV[r], C, " ")
    hit=""; amb=""
    for(i=1;i<=c;i++){
      if(C[i]==DNAME[r]) continue
      if(C[i] in SEEDNAME){ hit=C[i]; break }
      t=resolve(DFILE[r], C[i])
      if(t){ if(TD[t] && !DHANDLE[t]){ hit=C[i]; break } }
      else if(amb=="" && (C[i] in ANYTAINT)) amb=C[i]
    }
    if(hit!="")      printf "%s:%s:TRANSITIVE:opener@L%d:via=%s\n", DFILE[r], DNAME[r], DOPEN[r], hit
    else if(amb!="") printf "%s:%s:AMBIGUOUS:opener@L%d:via=%s\n",  DFILE[r], DNAME[r], DOPEN[r], amb
  }
}
