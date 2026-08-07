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
    DEFCOUNT[cur_fn]++; DEFIDX[cur_fn]=ndef
  }
  cur_open=0; cur_app=0; cur_open_ln=0; cur_calls=""; cur_handle=0
}

# Per-file reset — brace/comment state must not leak across files.
FNR==1 { flush(); cur_fn=""; depth=0; tdepth=-1; pending=0; inblk=0; instr=0; fn_depth=-1; fn_pending=0 }

{
  line=$0
  is_decl=0
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    if (fn_depth<0 || depth<=fn_depth) {
      flush()
      f=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",f); cur_fn=f; cur_file=FILENAME; fn_pending=1
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

  # INDEPENDENT live-DB opener? (identical token set to the 10M scanner —
  # open_in_memory / from_connection are the sanctioned shared-instance seams)
  if ((code ~ /(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)\(/ \
       || code ~ /append_reopen[ \t]*\(/) \
      && code !~ /open_in_memory/ && code !~ /from_connection/) {
    if(!cur_open){ cur_open=1; cur_open_ln=FNR }
  }
  # DIRECT append? (identical token set to the 10M scanner)
  if (code ~ /\.append(_signed)?[ \t]*\(/ || code ~ /append_in_tx(_signed)?[ \t]*\(/ \
      || code ~ /append_reopen[ \t]*\(/) {
    if(!cur_app){ cur_app=1 }
  }
  # SHARED-HANDLE ACQUIRE — `aberp_db::Handle::write()`, the one process-wide
  # audit serialization point. Empty parens discriminate it from `io::Write`
  # (`.write(&buf)`); an `RwLock::write()` would also match, but the barrier is
  # only ever CONSULTED on a definition that already reaches an append, so a
  # lock-only `.write()` in a non-appending fn is inert.
  if (code ~ /\.write\([ \t]*\)/) cur_handle=1
  if (!is_decl) cur_calls = cur_calls collect_calls(code)
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
        if(DEFCOUNT[C[i]]==1 && TD[DEFIDX[C[i]]] && !DHANDLE[DEFIDX[C[i]]]){ TD[r]=1; changed=1; break }
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

  # A name is a RESOLVABLE carrier iff it is a seed, or has exactly one
  # definition and that definition is tainted. ANYTAINT tracks names where SOME
  # definition is tainted — used only to report the ambiguous residue.
  for(r=1;r<=ndef;r++) if(TD[r] && !DHANDLE[r]) ANYTAINT[DNAME[r]]=1
  for(n in SEEDNAME){ TNAME[n]=1; ANYTAINT[n]=1 }
  for(r=1;r<=ndef;r++) if(TD[r] && !DHANDLE[r] && DEFCOUNT[DNAME[r]]==1) TNAME[DNAME[r]]=1
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
    c=split(DCALLS[r], C, " ")
    hit=""; amb=""
    for(i=1;i<=c;i++){
      if(C[i]==DNAME[r]) continue
      if(C[i] in TNAME){ hit=C[i]; break }
      if(amb=="" && (C[i] in ANYTAINT) && DEFCOUNT[C[i]]>1) amb=C[i]
    }
    if(hit!="")      printf "%s:%s:TRANSITIVE:opener@L%d:via=%s\n", DFILE[r], DNAME[r], DOPEN[r], hit
    else if(amb!="") printf "%s:%s:AMBIGUOUS:opener@L%d:via=%s\n",  DFILE[r], DNAME[r], DOPEN[r], amb
  }
}
