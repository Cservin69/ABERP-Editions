//! Snapshot store layout, sequence derivation, and listing.
//!
//! Layout: `<store>/snap-<seq>-<UTC-ts>/` where each directory holds a
//! DuckDB `EXPORT DATABASE` (schema.sql, load.sql, *.parquet) plus a
//! `meta.json` ([`crate::SnapshotMeta`]). A `*.partial` suffix marks an
//! in-progress export not yet finalized; those are ignored by listing and
//! sequence derivation.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::{Result, SnapshotError, SnapshotMeta};

/// Filename of the per-snapshot metadata sidecar.
pub(crate) const META_FILE: &str = "meta.json";

/// Suffix marking an export directory that is not yet finalized.
pub(crate) const PARTIAL_SUFFIX: &str = ".partial";

/// A finalized snapshot on disk: its directory plus parsed metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// Absolute path to the snapshot directory (`snap-<seq>-<ts>`).
    pub dir: PathBuf,
    /// Parsed `meta.json`.
    pub meta: SnapshotMeta,
}

impl SnapshotRecord {
    /// Age of the snapshot relative to `now` (UTC). Saturates at zero for
    /// clock skew where `created_at` is slightly in the future.
    pub fn age(&self, now: OffsetDateTime) -> time::Duration {
        let d = now - self.meta.created_at;
        if d.is_negative() {
            time::Duration::ZERO
        } else {
            d
        }
    }
}

/// `$HOME` (or `$USERPROFILE` on Windows) as a `PathBuf`. Used by the store
/// resolvers; no `dirs` dep. A missing/empty value is a loud error, never a
/// silent fallback to `/` or the cwd.
fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|p| !p.is_empty()))
        .map(PathBuf::from)
        .ok_or_else(|| {
            SnapshotError::io(
                PathBuf::from("$HOME"),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "neither HOME nor USERPROFILE is set",
                ),
            )
        })
}

/// Resolve `~/Documents/ABERP-snapshots/<tenant>/` — the FROZEN-PROD-shaped
/// store. Kept OUTSIDE the repo and OUTSIDE `~/.aberp/` so a tenant reset or
/// a restore never deletes the rollback copies (the S393/ADR-0082 posture).
///
/// The sawed-off editions tree does NOT use this for its own snapshots — it
/// uses [`edition_store_dir`] so Defense and Portable get disjoint stores
/// that can never share prod's. This resolver is retained as the
/// prod-shaped default (and the surface the prod-refusal guard names).
pub fn default_store_dir(tenant: &str) -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Documents")
        .join("ABERP-snapshots")
        .join(sanitise_tenant(tenant)))
}

/// Resolve the EDITION-SCOPED snapshot store
/// `~/Documents/ABERP-snapshots-<edition>/<tenant>/` (ADR-0093 §5).
///
/// `edition` is the lowercase edition segment (`"defense"` / `"portable"`),
/// which the binary derives from the COMPILE-TIME
/// `build_profile::edition_store_segment()` — never an env/launcher string
/// (FOUNDATION §5). The resulting store is:
///   - **provably disjoint from prod's** `~/Documents/ABERP-snapshots/`:
///     `ABERP-snapshots-defense` / `ABERP-snapshots-portable` are sibling
///     directories of `ABERP-snapshots`, so neither is nested under the
///     other — an editions build can never take, list, prune, or restore
///     from prod's store; and
///   - kept OUTSIDE `~/.aberp*` (the live DB roots) exactly like
///     [`default_store_dir`], so a tenant reset or a restore never deletes
///     the rollback copies (ADR-0082).
///
/// `edition` is sanitised identically to `tenant`, so it can never inject a
/// path separator or `..`.
pub fn edition_store_dir(edition: &str, tenant: &str) -> Result<PathBuf> {
    let seg = sanitise_tenant(edition);
    Ok(home_dir()?
        .join("Documents")
        .join(format!("ABERP-snapshots-{seg}"))
        .join(sanitise_tenant(tenant)))
}

/// Sanitise a tenant so it can never escape the store dir (no `/`, `..`).
pub(crate) fn sanitise_tenant(tenant: &str) -> String {
    tenant
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build a finalized snapshot directory name `snap-<seq>-<UTC-ts>`. The
/// timestamp format matches S393 (`YYYYMMDD-HHMMSS`) so the two stores read
/// the same at a glance.
pub(crate) fn snapshot_dir_name(seq: u64, now: OffsetDateTime) -> Result<String> {
    use time::macros::format_description;
    const TS: &[time::format_description::FormatItem<'_>] =
        format_description!("[year][month][day]-[hour][minute][second]");
    let ts = now.format(TS).map_err(|e| SnapshotError::BadMeta {
        path: PathBuf::from("<timestamp>"),
        detail: format!("format snapshot timestamp: {e}"),
    })?;
    Ok(format!("snap-{seq}-{ts}"))
}

/// Parse the seq out of a finalized directory name `snap-<seq>-<ts>`.
/// Returns `None` for `*.partial` dirs and anything not matching.
pub(crate) fn seq_of_dir_name(name: &str) -> Option<u64> {
    if name.ends_with(PARTIAL_SUFFIX) {
        return None;
    }
    name.strip_prefix("snap-")
        .and_then(|rest| rest.split_once('-'))
        .and_then(|(seq, _ts)| seq.parse::<u64>().ok())
}

/// Next sequence number for the store: `max(existing seq) + 1`, or 1 for an
/// empty/absent store. Crashed `*.partial` dirs do not consume a seq.
pub(crate) fn next_seq(store_dir: &Path) -> Result<u64> {
    let mut max = 0u64;
    match std::fs::read_dir(store_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| SnapshotError::io(store_dir, e))?;
                if let Some(name) = entry.file_name().to_str() {
                    if let Some(seq) = seq_of_dir_name(name) {
                        max = max.max(seq);
                    }
                }
            }
        }
        // A not-yet-created store is simply empty → start at 1.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(SnapshotError::io(store_dir, e)),
    }
    Ok(max + 1)
}

/// Write `meta.json` into a snapshot directory.
pub(crate) fn write_meta(dir: &Path, meta: &SnapshotMeta) -> Result<()> {
    let path = dir.join(META_FILE);
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| SnapshotError::BadMeta {
        path: path.clone(),
        detail: format!("serialize meta.json: {e}"),
    })?;
    std::fs::write(&path, bytes).map_err(|e| SnapshotError::io(path, e))
}

/// Read `meta.json` from a snapshot directory.
pub(crate) fn read_meta(dir: &Path) -> Result<SnapshotMeta> {
    let path = dir.join(META_FILE);
    let bytes = std::fs::read(&path).map_err(|e| SnapshotError::io(path.clone(), e))?;
    serde_json::from_slice(&bytes).map_err(|e| SnapshotError::BadMeta {
        path,
        detail: format!("parse meta.json: {e}"),
    })
}

/// List finalized snapshots in `store_dir`, newest seq first. A directory
/// whose `meta.json` is missing/unreadable is **skipped with a warning**
/// rather than failing the whole list — one corrupt sidecar must not hide
/// every other rollback point (`[[fail-loud]]` without taking the operator
/// down). An absent store lists empty.
pub fn list_snapshots(store_dir: &Path) -> Result<Vec<SnapshotRecord>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(store_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(SnapshotError::io(store_dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| SnapshotError::io(store_dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if seq_of_dir_name(name).is_none() {
            continue;
        }
        let dir = entry.path();
        match read_meta(&dir) {
            Ok(meta) => out.push(SnapshotRecord { dir, meta }),
            Err(e) => tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "snapshot directory has unreadable meta.json — skipping it in the listing"
            ),
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.meta.seq));
    Ok(out)
}

/// ADR-0116 D3.3 / G6 — a snapshot's STABLE identity.
///
/// `seq` alone is not one. `next_seq` is `max(surviving seq) + 1`, so a
/// pruned seq is **recycled**: seq 24 names three different snapshots in
/// prod's ledger, two of which are the `validation_failed` pair. A
/// `seq`-addressed restore CLI is therefore ambiguous *by construction*, and
/// the ambiguity is worst exactly where it hurts most — around failed and
/// pruned snapshots.
///
/// The identity is the triple `(seq, created_at, source_db_sha256)`. It is
/// what `--dry-run` prints, what the audit payloads record, what `list`
/// shows, and what a selector must resolve to exactly one of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSelector {
    /// The operator-typed string, kept verbatim for error messages.
    pub raw: String,
}

/// Render a snapshot's stable identity as a single human/machine token:
/// `<seq>@<created_at>#<sha8>`. Unambiguous across a recycled seq, and short
/// enough to paste into a restore command.
pub fn snapshot_identity(meta: &SnapshotMeta) -> String {
    let ts = meta
        .created_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| meta.created_at.unix_timestamp().to_string());
    let sha8: String = meta.source_db_sha256.chars().take(8).collect();
    format!("{}@{}#{}", meta.seq, ts, sha8)
}

/// Resolve `selector` against the store's snapshots and **refuse on
/// ambiguity** (ADR-0116 D3.3).
///
/// Accepted forms, tried in order — the first form that matches ANY record
/// decides, and if that form matches more than one the call REFUSES rather
/// than silently taking the newest:
///
///   1. the full directory name — `snap-42-20260615-143000` (always unique
///      on a filesystem, so this is the form to prefer);
///   2. a bare `seq`, matched EXACTLY — **ambiguous after a prune**, so this
///      refuses whenever two records share the seq;
///   3. the stable identity token from [`snapshot_identity`] — `42@<ts>#<sha8>`
///      — or any unambiguous prefix of it **that reaches the `@`**;
///   4. a timestamp / directory-name substring — for NON-numeric selectors
///      only (see below).
///
/// # ADR-0116 F3 — why the bare seq is tried BEFORE the identity prefix
///
/// The identity is `<seq>@<ts>#<sha8>`, so an identity *prefix* match on the
/// bare string `"2"` also matches seq **24**'s identity — and the identity form
/// used to be tried first. Two consequences, both reproduced:
///
///   * a store holding only seq 24 resolved `--snapshot 2` to **seq 24** and
///     overwrote the live database from a snapshot the operator never named.
///     Seq 2 does not exist; the correct answer is `NotFound`.
///   * a store holding seqs 2 **and** 24 REFUSED `--snapshot 2` as ambiguous,
///     even though seq 2 is unique — so the documented bare-seq form stopped
///     working the moment a store grew past seq 9.
///
/// Both are closed by ordering the exact form first and by requiring a prefix
/// to reach the `@` separator before it may match: a bare number is a seq, and
/// an identity prefix has to look like one.
///
/// The same reasoning ends the search there. A bare integer that names no seq
/// would otherwise fall through to the SUBSTRING form and match
/// `snap-24-20260615-143000` on the digit `2` — the identical defect one form
/// further down. **A selector that parses as an integer is a seq and only a
/// seq**; a date is addressed with the directory name or a hyphenated
/// fragment (`-20260615-`), both of which are non-numeric and still reach
/// form 4. Refusing a numeric selector that names nothing is the safe
/// direction: the alternative is overwriting a live database from a snapshot
/// the operator did not name.
///
/// A form that matches nothing falls through to the next; a form that matches
/// several returns [`SnapshotError::AmbiguousSelector`] naming every
/// candidate by its full identity, so the operator can retry with one.
pub fn resolve_selector(store_dir: &Path, selector: &str) -> Result<SnapshotRecord> {
    let records = list_snapshots(store_dir)?;
    resolve_selector_in(&records, selector)
}

/// [`resolve_selector`] over an already-listed set — pure, so the
/// recycled-seq behaviour is testable without a filesystem.
pub fn resolve_selector_in(records: &[SnapshotRecord], selector: &str) -> Result<SnapshotRecord> {
    // (1) exact directory name.
    let by_dir: Vec<&SnapshotRecord> = records
        .iter()
        .filter(|r| r.dir.file_name().and_then(|n| n.to_str()) == Some(selector))
        .collect();
    if let Some(one) = pick_one(&by_dir, selector)? {
        return Ok(one);
    }

    // (2) a bare seq, EXACTLY — the recycled-identity trap. Two records CAN
    //     share it, and `pick_one` refuses rather than guessing. Tried before
    //     the identity prefix (ADR-0116 F3): `"2"` is a seq, not the first
    //     character of seq 24's identity.
    if let Ok(seq) = selector.parse::<u64>() {
        let by_seq: Vec<&SnapshotRecord> = records.iter().filter(|r| r.meta.seq == seq).collect();
        if let Some(one) = pick_one(&by_seq, selector)? {
            return Ok(one);
        }
        // A bare number that names no snapshot must NOT fall through to the
        // identity-prefix form, which would silently resolve it to a
        // higher-seq snapshot whose identity happens to start with those
        // digits. Nothing below can legitimately match a bare integer.
        return Err(SnapshotError::NotFound(selector.to_string()));
    }

    // (3) the stable identity token, or a prefix of it that REACHES the `@`.
    //     A prefix shorter than `<seq>@` is just a number with a different
    //     name, and matching it here is what let `--snapshot 2` resolve to
    //     seq 24 (ADR-0116 F3).
    let by_identity: Vec<&SnapshotRecord> = records
        .iter()
        .filter(|r| selector.contains('@') && snapshot_identity(&r.meta).starts_with(selector))
        .collect();
    if let Some(one) = pick_one(&by_identity, selector)? {
        return Ok(one);
    }

    // (4) a timestamp / directory-name substring.
    let by_substr: Vec<&SnapshotRecord> = records
        .iter()
        .filter(|r| {
            r.dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(selector))
        })
        .collect();
    if let Some(one) = pick_one(&by_substr, selector)? {
        return Ok(one);
    }

    Err(SnapshotError::NotFound(selector.to_string()))
}

/// `Ok(None)` = this form matched nothing, try the next. `Ok(Some(_))` = a
/// unique match. `Err(Ambiguous)` = REFUSE — never guess which snapshot the
/// operator meant when one of them is a failed snapshot and the other is a
/// good one.
fn pick_one(matches: &[&SnapshotRecord], selector: &str) -> Result<Option<SnapshotRecord>> {
    match matches {
        [] => Ok(None),
        [one] => Ok(Some((*one).clone())),
        many => Err(SnapshotError::AmbiguousSelector {
            selector: selector.to_string(),
            count: many.len(),
            candidates: many
                .iter()
                .map(|r| snapshot_identity(&r.meta))
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// Find a snapshot by seq (`"42"`), by exact directory name
/// (`"snap-42-20260615-143000"`), by the stable identity token, or by a
/// unique timestamp substring.
///
/// ADR-0116 D3.3 — this is now a thin alias for [`resolve_selector`], which
/// **refuses on ambiguity**. The previous implementation returned the first
/// seq match it found; after a prune recycles a seq that silently picked one
/// of several snapshots, which around the `validation_failed` pair meant
/// silently picking between a good snapshot and a broken one.
pub fn find_snapshot(store_dir: &Path, selector: &str) -> Result<SnapshotRecord> {
    resolve_selector(store_dir, selector)
}

/// Sum of regular-file sizes directly inside `dir` (the export is flat).
pub(crate) fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir).map_err(|e| SnapshotError::io(dir, e))? {
        let entry = entry.map_err(|e| SnapshotError::io(dir, e))?;
        let meta = entry.metadata().map_err(|e| SnapshotError::io(dir, e))?;
        if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}
