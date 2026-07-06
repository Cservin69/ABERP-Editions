# ADR-0098 R7 — mirror-fork co-occurrence scanner (gate CHECK 10L-b). Toolchain-
# free (awk only). Prints "<file>|<fname>" once for every RUNTIME (outside
# #[cfg(test)]) function that contains BOTH:
#   (1) an INDEPENDENT live-DB opener — Connection::open(_with_flags)? /
#       Ledger::open / DuckDbBillingStore::open / Database::open / append_reopen
#       (open_in_memory & from_connection excluded: the sanctioned shared-instance
#        seams), AND
#   (2) a `.sync_mirror(` call.
# That pair IS the write-fork signature the 415/416 forensic named: a separate
# DuckDB instance, opened off the DB PATH, reads a STALE ledger head, re-assigns
# an already-used sequence, and then rewrites the mirror FROM ITS OWN VIEW. A
# pragma fence (disable_checkpoint_on_shutdown) does NOT stop that stale-head seq
# collision or the rogue sync_mirror — only routing the write through the ONE
# shared aberp_db::Handle does (its WriteGuard drop is the sole sanctioned
# post-commit sync_mirror). CHECK 10L-b freezes this set in
# tools/adr0098_r7_mirror_fork_sites.txt so it can only SHRINK (as residuals move
# onto the Handle), never grow, and so a migrated seam that REGROWS the pair goes
# RED. Comment/string/cfg(test)-aware. Scope/skip is set by the caller (same as
# CHECK 10i: apps/aberp/src + modules + crates, minus aberp-db / aberp-snapshot /
# the 7 C2-migrated files).
BEGIN{ depth=0; tdepth=-1; pending=0; inblk=0; instr=0; curfn=""; curfn_depth=-1 }
{
  line=$0
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    fn=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",fn); fname=fn; fdepth_pending=1
  }
  st=line; sub(/^[ \t]+/,"",st)
  if (st ~ /^#\[cfg\(/ && st ~ /test/ && st !~ /not\(test\)/) pending=1
  code=""; L=length(line)
  for(i=1;i<=L;i++){
    c=substr(line,i,1); d=substr(line,i,2)
    if(inblk){ if(d=="*/"){inblk=0;i++} ; continue }
    if(instr){ if(c=="\\"){i++;continue} ; if(c=="\""){instr=0} ; continue }
    if(d=="//"){ break }
    if(d=="/*"){ inblk=1;i++;continue }
    if(c=="\""){ instr=1; continue }
    code=code c
    if(c=="{"){ depth++; if(fdepth_pending){ curfn_depth=depth; curfn=fname; has_open=0; has_mirror=0; intest_fn=(tdepth>=0)||pending; fdepth_pending=0 }
                if(pending && tdepth<0){ tdepth=depth; pending=0 } }
    else if(c=="}"){
        if(depth==curfn_depth && curfn!=""){
           if(has_open && has_mirror && !intest_fn && (tdepth<0)) print FILENAME"|"curfn
           curfn=""; curfn_depth=-1
        }
        if(tdepth==depth) tdepth=-1; depth--
    }
  }
  intest=(tdepth>=0)
  if(!intest && curfn!=""){
    if((code ~ /(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)\(/ || code ~ /append_reopen[ \t]*\(/) && code !~ /open_in_memory/ && code !~ /from_connection/) has_open=1
    if(code ~ /sync_mirror[ \t]*\(/) has_mirror=1
  }
}
