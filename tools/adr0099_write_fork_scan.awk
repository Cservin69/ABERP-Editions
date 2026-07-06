# ADR-0099 — in-process runtime AUDIT-LEDGER WRITE-FORK scanner (toolchain-free).
#
# The seq-369/416/428/515 forks were NOT the narrow "opener + rogue sync_mirror"
# class CHECK 10L froze. The TRUE fork primitive is simpler and broader: ANY
# independent audit-ledger opener that then APPENDS on the live DB, inside the
# `serve` process, outside the ONE shared aberp_db::Handle. Two such openers off
# the same head both self-assign the next seq (snapshot daemon `snapshot.created`
# racing quote-intake — seq 515). A rogue `sync_mirror` is NOT required; 10L's
# model was too narrow, and 10i merely FROZE these openers instead of banning
# them. This scanner is the corrected model: it fails the build on the fork
# primitive itself.
#
# Prints one "LINE:fname:OPENER+APPEND" record per RUNTIME function (outside
# #[cfg(test)]) that contains BOTH:
#   • an INDEPENDENT live-DB opener — one of
#       Connection::open(_with_flags)? / Ledger::open / DuckDbBillingStore::open /
#       Database::open / append_reopen(               (open_in_memory &
#       from_connection are the sanctioned shared-instance seams, excluded), AND
#   • an AUDIT APPEND —
#       .append( / .append_signed( / append_in_tx( / append_in_tx_signed( /
#       append_reopen(                                (append_reopen is itself
#       an open+append, so it alone makes a fn a write-fork).
#
# Comment/string/char-literal aware (a token inside a doc-comment or string
# never trips it). Boot/CLI/allow-listed fn names are passed via -v allow="a,b".
# A fn on the allow-list is a SANCTIONED opener (pre-serve boot create/recover,
# or a separate-process CLI one-shot that has no Handle) and is skipped.
BEGIN{ depth=0; tdepth=-1; pending=0; inblk=0; instr=0; fn_depth=-1; fn_pending=0; n_allow=split(allow,A,",") }
function is_allowed(name,   k){ for(k=1;k<=n_allow;k++) if(A[k]==name) return 1; return 0 }
# Emit a pending record for the fn whose body we just closed.
function flush(   ){
  if (cur_fn!="" && cur_open && cur_app && !is_allowed(cur_fn)) {
    printf "%d:%s:opener@L%d+append@L%d\n", cur_open_ln, cur_fn, cur_open_ln, cur_app_ln
  }
  cur_open=0; cur_app=0; cur_open_ln=0; cur_app_ln=0
}
{
  line=$0
  # fn-name + body-brace tracking. A new top-level fn decl flushes the previous.
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    # Only treat as a NEW function when we are at (or above) the fn-body depth,
    # i.e. not a closure/nested item mid-body. Track the depth the fn body opens
    # at; when we return to it, the fn is done.
    if (fn_depth<0 || depth<=fn_depth) {
      flush()
      f=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",f); cur_fn=f; fn_pending=1
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
  # opener?
  if ((code ~ /(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)\(/ \
       || code ~ /append_reopen[ \t]*\(/) \
      && code !~ /open_in_memory/ && code !~ /from_connection/) {
    if(!cur_open){ cur_open=1; cur_open_ln=NR }
  }
  # append?
  if (code ~ /\.append(_signed)?[ \t]*\(/ || code ~ /append_in_tx(_signed)?[ \t]*\(/ \
      || code ~ /append_reopen[ \t]*\(/) {
    if(!cur_app){ cur_app=1; cur_app_ln=NR }
  }
}
END{ flush() }
