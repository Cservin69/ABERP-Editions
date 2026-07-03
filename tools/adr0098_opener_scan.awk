# ADR-0098 C2 (+ R4 finding H) — shared comment/string/cfg(test)-aware live-DB
# opener scanner. Toolchain-free (awk only). Prints "LINE:fname:text" for each
# RUNTIME (outside #[cfg(test)]) independent live-tenant-DB opener:
#   Connection::open( / Connection::open_with_flags( / Ledger::open( /
#   DuckDbBillingStore::open( / Database::open( / *.append_reopen(
# R4 finding H (b) — ALIAS EVASION: a `use duckdb::Connection as C;` (or any
#   `use ... <OpenerType> as <Alias>;`) rename followed by `<Alias>::open(` is
#   now caught too. Aliases of Connection / Ledger / DuckDbBillingStore /
#   Database are tracked per file and matched as `<Alias>::open(_with_flags)?(`.
# EXCLUDED (the sanctioned shared-instance seams): open_in_memory, from_connection.
# Boot/allow-listed fn names may be passed via -v allow="fn1,fn2".
# Strings, // line comments and /* */ block comments are skipped so a banned
# token inside a doc-comment or string never trips the scan.
BEGIN{ depth=0; tdepth=-1; pending=0; inblk=0; instr=0; n_allow=split(allow,A,",") }
function is_allowed(name,   k){ for(k=1;k<=n_allow;k++) if(A[k]==name) return 1; return 0 }
{
  line=$0
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    fn=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",fn); fname=fn
  }
  st=line; sub(/^[ \t]+/,"",st)
  if (st ~ /^#\[cfg\(/ && st ~ /test/ && st !~ /not\(test\)/) pending=1
  was_in=(tdepth>=0)
  # Build a "code-only" version of the line (strip comments/strings) for matching.
  code=""; L=length(line)
  for(i=1;i<=L;i++){
    c=substr(line,i,1); d=substr(line,i,2)
    if(inblk){ if(d=="*/"){inblk=0;i++} ; continue }
    if(instr){ if(c=="\\"){i++;continue} ; if(c=="\""){instr=0} ; continue }
    if(d=="//"){ break }
    if(d=="/*"){ inblk=1;i++;continue }
    if(c=="\""){ instr=1; continue }
    code=code c
    if(c=="{"){ depth++; if(pending && tdepth<0){ tdepth=depth; pending=0 } }
    else if(c=="}"){ if(tdepth==depth) tdepth=-1; depth-- }
  }
  now_in=(tdepth>=0); intest = was_in || now_in
  # R4 (b): learn per-file aliases of the opener types from `use ... as X;`.
  # A `use` line is never itself an opener call, so we can learn even in test
  # regions (a test-only alias used only in test code is skipped below anyway).
  if (code ~ /(^|[^A-Za-z0-9_])use([^A-Za-z0-9_])/ \
      && match(code, /(Connection|Ledger|DuckDbBillingStore|Database)[ \t]+as[ \t]+[A-Za-z_][A-Za-z0-9_]*/)) {
    a=substr(code,RSTART,RLENGTH); sub(/.*as[ \t]+/,"",a); ALIAS[a]=1
  }
  if (!intest) {
    hit=0
    if ((code ~ /(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)\(/ \
         || code ~ /append_reopen[ \t]*\(/) \
        && code !~ /open_in_memory/ && code !~ /from_connection/) {
      hit=1
    }
    # R4 (b): aliased open — `<Alias>::open(` / `<Alias>::open_with_flags(`.
    if (!hit && code !~ /open_in_memory/ && code !~ /from_connection/) {
      for (a in ALIAS) {
        if (code ~ (a "::open(_with_flags)?[ \t]*\\(")) { hit=1; break }
      }
    }
    if (hit && !is_allowed(fname)) { t=line; sub(/^[ \t]+/,"",t); printf "%d:%s:%s\n",NR,fname,substr(t,1,76) }
  }
}
