//! ADR-0116 D2 — recovery-evidence protection and its lifecycle.
//!
//! # Why this module exists
//!
//! Recovery evidence (`*.CORRUPT-*`, `*RECOVERY*`, `*.ahead-*.bak`,
//! `healed-*.bak`, `*INDEXDESYNC*`, `.PRE-RESTORE-*`, …) is written as a
//! **sibling of the live DB inside a tenant home**, never into the snapshot
//! store. `plan_retention` therefore cannot see it at all: it operates on
//! [`crate::SnapshotRecord`]s, which `list_snapshots` builds only from
//! `snap-*` directories carrying a parseable `meta.json`.
//!
//! So the guarantee "recovery evidence is never pruned" was satisfied **by
//! accident** — the pruner is structurally blind to those files rather than
//! deliberately protective of them. Any future "clean up the tenant home"
//! helper would have met no guard at all, and one such helper already exists
//! ([`crate::recover`]'s orphan-sibling sweeper, which enumerates the tenant
//! home and unlinks by prefix).
//!
//! # The rule, and why it is inverted
//!
//! ADR-0116 rev 2 F3 measured the first-draft deny-list against the 101
//! evidence-shaped names actually on disk: **58 escaped case-sensitively**,
//! and **14 escaped even case-insensitively** — including all 9
//! `healed-*.bak` (the only surviving copies of pre-heal mirror state) and
//! the 24 MB `INDEXDESYNC-BACKUP`, the sole physical DB backup from the
//! 2026-08-03 index-desync incident. `healed-` and `INDEXDESYNC-` match none
//! of the five original patterns in any case.
//!
//! The rule is therefore **inverted to an allow-list**:
//!
//!   > Under a tenant home, anything whose name is not a KNOWN-LIVE name is
//!   > protected evidence.
//!
//! The live set is enumerable and stable; the evidence set is neither. The
//! named families survive as a **belt-and-braces second predicate** that
//! applies everywhere, not only under a tenant home — so a `*CORRUPT*` /
//! `*RECOVERY*` / `*DEFORK*` / `*PRE-*` / `healed-*` / `INDEXDESYNC*`
//! artefact is never removable by any caller, wherever it sits.
//!
//! Every match is **case-insensitive**. This exact bug class was closed once
//! already in this repo (the edition DB-guard escape that needed both walks
//! made case-insensitive); re-opening it here would re-open it in the one
//! place where the failure mode is permanent data loss.
//!
//! # Where the guard sits
//!
//! `retention::prune` calls [`is_protected_evidence`] — but `prune` only ever
//! touches the snapshot store, where no evidence lives, so a refusal there
//! alone protects nothing. **Every tenant-home helper must call it too**, via
//! [`guarded_remove`], and the ADR-0116 cut-gate check fails any
//! `remove_file` / `remove_dir_all` that reaches a tenant-home path without
//! going through this module.

use std::path::{Component, Path, PathBuf};

use time::OffsetDateTime;

use crate::{Result, SnapshotError};

/// Names that are part of the LIVE tenant state and are therefore **not**
/// evidence. Compared case-insensitively against the full file name.
///
/// This list is the primary predicate: anything under a tenant home that is
/// not on it is treated as protected evidence. Adding a new legitimate live
/// filename here is a deliberate act; forgetting to is the SAFE direction (a
/// live file is merely protected, never deleted), which is why the list is
/// allowed to lag rather than the other way round.
pub const LIVE_TENANT_NAMES: &[&str] = &[
    // The database and its durability siblings (ADR-0095/0110/0111).
    "aberp.duckdb",
    "aberp.duckdb.wal",
    "aberp.duckdb.audit.log",
    "aberp.duckdb.ckpt-ok",
    "aberp.duckdb.install-intent",
    // Tenant configuration + branding.
    "seller.toml",
    "seller.toml.example",
    "logo.png",
    "runtime.json",
    "tenants.toml",
    ".first-launch-acknowledged",
    // Loopback TLS material (regenerated, not evidence).
    "loopback.crt.pem",
    "loopback.key.pem",
    "loopback.fingerprint.sha256",
    // Live working directories.
    "ap-artifacts",
    "ncr-photos",
    "email-relay-attachments",
    "issued",
];

/// Code-owned TRANSIENT infixes: crash leftovers of operations this crate
/// performs, which are live-set members rather than evidence.
///
/// The allow-list inversion would otherwise freeze every crashed temp file
/// forever — a helper like [`crate::recover`]'s orphan sweeper would meet a
/// refusal on `aberp.duckdb.creating-<nanos>` and the tenant home would grow
/// without bound. These infixes satisfy the same test the named live files
/// do: **enumerable and stable**, because this crate is the only thing that
/// writes them and it writes them from named constants.
///
/// Safety of the ordering: [`is_protected_evidence`] evaluates
/// [`EVIDENCE_FRAGMENTS`] FIRST, so a name that is both transient-shaped and
/// evidence-shaped (`aberp.duckdb.CORRUPT-….creating-1`) is protected. A
/// transient infix can never un-protect an evidence artefact.
pub const LIVE_TRANSIENT_INFIXES: &[&str] = &[
    ".creating-", // recover.rs CREATING_INFIX — a half-built rebuild
    ".recover-",  // recover.rs RECOVER_INFIX — a half-built recovery
    ".restoring", // take.rs restore_into staging file
    ".partial",   // store.rs PARTIAL_SUFFIX — an unfinished EXPORT dir
    ".tmp.",      // tenant_registry.rs atomic-write staging
    // seller_toml_backup.rs writes `.{filename}.backup-{unix-secs}` beside
    // seller.toml and rotates the oldest away. Code-owned, enumerable, stable
    // — the same criterion the other transients meet. The LEADING DOT is what
    // makes it discriminating: real evidence spells it `-BACKUP-`
    // (`aberp.duckdb.CORRUPT-BACKUP-…`, `INDEXDESYNC-BACKUP-…`), which does not
    // contain `.backup-`, and both of those carry a family token that is
    // matched FIRST regardless.
    ".backup-",
];

/// Evidence name families — the belt-and-braces SECOND predicate, applied
/// everywhere (not only under a tenant home) and case-insensitively.
///
/// A name containing any of these fragments is NEVER removable, by any
/// caller, in any location. The list is deliberately broader than the
/// ADR's original five: F3 proved those five missed 14 real artefacts even
/// case-insensitively.
pub const EVIDENCE_FRAGMENTS: &[&str] = &[
    "corrupt",
    "recovery",
    "defork",
    "dedup",
    "spurious",
    "evidence",
    "healed-",
    "indexdesync",
    "ahead",
    "deepcorrupt",
    "pre-restore",
    "pre-recovery",
    "pre-dedup",
    "pre-defork",
    "pre-topup",
    "pre-reconcile",
    "pre-mirror-rebuild",
    "pre-upgrade",
    "keychain",
];

/// Evidence families matched as a **SUFFIX**, not a substring.
///
/// `.bak` used to live in [`EVIDENCE_FRAGMENTS`] as a plain substring, and
/// that was a real collision: `.seller.toml.backup-<ts>` CONTAINS `.bak`, so
/// the seller-config backup rotation was frozen by the evidence guard and
/// would have accumulated in the tenant home forever. Every real `.bak`
/// artefact on disk uses it as a suffix
/// (`…corrupt-<nanos>.bak`, `healed-*.bak`,
/// `…PRE-DEFORK-<ts>.bak`), and each also carries a substring family token
/// anyway — so anchoring costs nothing and removes the collision.
pub const EVIDENCE_SUFFIXES: &[&str] = &[".bak"];

/// Component naming the FROZEN prod line's out-of-tree physical backup store
/// `~/aberp-snapshots/` — **not** `~/Documents/ABERP-snapshots/`. It holds
/// the `*-keychain.zip` encrypted credential dumps (ADR-0116 G5) and is the
/// single most sensitive location the system writes to.
const PHYSICAL_BACKUP_COMPONENT: &str = "aberp-snapshots";

/// Prefix of the out-of-tree recovery directories
/// `~/Documents/ABERP-recovery-<tag>/` — the target of the seq-57 restore
/// recorded in prod's ledger.
const RECOVERY_DIR_PREFIX: &str = "aberp-recovery-";

/// Prefix of the archive store this module writes to,
/// `~/Documents/ABERP-evidence/<tenant>/<incident>/`. Archived evidence is
/// evidence: it is protected in its new home too, so "release" can never
/// become "gone" via a second pass.
const ARCHIVE_DIR_PREFIX: &str = "aberp-evidence";

/// `true` if `path` sits inside a live tenant home
/// (`~/.aberp*/<tenant>/…` — prod's `.aberp` or an edition's
/// `.aberp-defense` / `.aberp-portable`).
///
/// HOME-independent by construction: it matches on the path SHAPE (a
/// component beginning `.aberp`, with at least a tenant segment and one more
/// component after it), so it is pure, total, and testable without touching
/// a real home directory.
pub fn path_is_under_tenant_home(path: &Path) -> bool {
    let comps: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    for (i, c) in comps.iter().enumerate() {
        let is_root = c
            .to_str()
            .is_some_and(|s| s.to_ascii_lowercase().starts_with(".aberp"));
        // The root itself is index i, the tenant i+1, the artefact i+2 —
        // so a path is "under a tenant home" once it has something inside
        // the tenant directory.
        if is_root && comps.len() >= i + 3 {
            return true;
        }
    }
    false
}

/// `true` if `path` sits under one of the out-of-tenant-home evidence roots
/// the ADR names explicitly: `~/aberp-snapshots/` (214 MB, keychain dumps),
/// `~/Documents/ABERP-recovery-*/` (57 MB), and this module's own archive
/// store `~/Documents/ABERP-evidence/`.
///
/// Governing one third of the footprint while claiming to govern all of it
/// is worse than governing none, because it reads as done (ADR-0116 D2.4).
pub fn path_is_under_evidence_root(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(s) => s.to_str().is_some_and(|s| {
            let lower = s.to_ascii_lowercase();
            lower == PHYSICAL_BACKUP_COMPONENT
                || lower.starts_with(RECOVERY_DIR_PREFIX)
                || lower.starts_with(ARCHIVE_DIR_PREFIX)
        }),
        _ => false,
    })
}

/// `true` if the file NAME alone marks this as recovery evidence, by the
/// belt-and-braces family predicate. Case-insensitive; location-independent.
pub fn name_is_evidence_shaped(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    EVIDENCE_FRAGMENTS.iter().any(|f| lower.contains(f))
        || EVIDENCE_SUFFIXES.iter().any(|f| lower.ends_with(f))
}

/// `true` if the file NAME is a known-live tenant artefact — either an exact
/// [`LIVE_TENANT_NAMES`] entry or a code-owned transient
/// ([`LIVE_TRANSIENT_INFIXES`]). Case-insensitive.
pub fn name_is_live(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    LIVE_TENANT_NAMES.iter().any(|n| *n == lower)
        || LIVE_TRANSIENT_INFIXES.iter().any(|i| lower.contains(i))
}

/// **The shared guard.** `true` if `path` is recovery evidence that must
/// NEVER be deleted by retention, by a tenant-home cleanup helper, or by any
/// other caller.
///
/// Two independent predicates, either of which protects:
///
///  1. **Allow-list inversion (primary).** Under a tenant home (or under one
///     of the named evidence roots), any name that is not in
///     [`LIVE_TENANT_NAMES`] is evidence.
///  2. **Family match (belt-and-braces).** Anywhere at all, a name containing
///     an [`EVIDENCE_FRAGMENTS`] token is evidence.
///
/// Pure and total — no IO, no clock, no `$HOME` read — so it is cheap enough
/// to call on every candidate path and testable against fixtured names.
pub fn is_protected_evidence(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        // A path with no final component (or a non-UTF-8 one) is not
        // something this guard can reason about. Refuse to bless it.
        return true;
    };
    // (2) — family match applies everywhere, including the snapshot store
    // and any side path an operator restored into.
    if name_is_evidence_shaped(name) {
        return true;
    }
    // (1) — allow-list inversion, scoped to the homes/roots where evidence
    // actually lives. Outside them the family predicate is the whole rule,
    // so the guard cannot accidentally freeze unrelated temp files.
    if path_is_under_tenant_home(path) || path_is_under_evidence_root(path) {
        return !name_is_live(name);
    }
    false
}

/// Remove `path` **only** if it is not protected evidence.
///
/// This is the single sanctioned removal entrypoint for anything that can
/// reach a tenant home. A refusal is LOUD (house rule #12): silent protection
/// is how a stale plan becomes invisible, and an operator who cannot see the
/// refusal will reach for `rm` instead.
///
/// Returns `Ok(false)` when the path did not exist (the goal state "gone" is
/// already satisfied) and `Ok(true)` when something was removed.
pub fn guarded_remove(path: &Path) -> Result<bool> {
    if is_protected_evidence(path) {
        tracing::error!(
            path = %path.display(),
            "REFUSING to delete recovery evidence (ADR-0116 D2). This artefact is the only \
             record of a durability incident and is never removable by code. Use \
             `aberp evidence archive` if it must leave the live tenant home. \
             Magyarul: helyreállítási bizonyíték — a kód soha nem törli."
        );
        return Err(SnapshotError::EvidenceProtected(path.to_path_buf()));
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(SnapshotError::io(path, e)),
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| SnapshotError::io(path, e))?;
    } else {
        std::fs::remove_file(path).map_err(|e| SnapshotError::io(path, e))?;
    }
    Ok(true)
}

// ──────────────────────────────────────────────────────────────────────
// Evidence inventory + the tiered release policy (ADR-0116 D2)
// ──────────────────────────────────────────────────────────────────────

/// One evidence artefact found in a tenant home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceArtefact {
    pub path: PathBuf,
    pub name: String,
    /// Total bytes (recursive for a directory).
    pub byte_size: u64,
    /// Filesystem mtime, UTC. Falls back to the UNIX epoch when unreadable —
    /// which makes the artefact look OLD, so the age floor cannot release it
    /// on the strength of a missing timestamp: [`plan_evidence_release`]
    /// treats an unreadable mtime as ungroupable, hence protected.
    pub modified_at: OffsetDateTime,
    /// Whether the mtime was actually readable (see above).
    pub mtime_known: bool,
    /// Incident tag parsed from the filename, normalised to ISO
    /// (`20260705T184449Z`). `None` when the name carries no tag.
    pub incident_tag: Option<String>,
    /// Encrypted credential material (`*keychain*`). Never archived to a
    /// second location — for these, release means delete-in-place or nothing
    /// (ADR-0116 D2.4).
    pub is_credential_material: bool,
}

/// Enumerate the evidence artefacts directly inside `tenant_home`.
///
/// Non-recursive by design: evidence is written as a SIBLING of the live DB,
/// and the live working directories (`ncr-photos`, `ap-artifacts`, …) are
/// allow-listed rather than walked. An absent directory lists empty.
pub fn list_evidence(tenant_home: &Path) -> Result<Vec<EvidenceArtefact>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(tenant_home) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(SnapshotError::io(tenant_home, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| SnapshotError::io(tenant_home, e))?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name_is_live(&name) {
            continue;
        }
        if !is_protected_evidence(&path) {
            continue;
        }
        let meta = entry.metadata().map_err(|e| SnapshotError::io(&path, e))?;
        let (modified_at, mtime_known) = match meta.modified() {
            Ok(t) => (OffsetDateTime::from(t), true),
            Err(_) => (OffsetDateTime::UNIX_EPOCH, false),
        };
        let byte_size = if meta.is_dir() {
            dir_size_recursive(&path)
        } else {
            meta.len()
        };
        out.push(EvidenceArtefact {
            incident_tag: normalise_incident_tag(&name, modified_at),
            is_credential_material: name.to_ascii_lowercase().contains("keychain"),
            path,
            name,
            byte_size,
            modified_at,
            mtime_known,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn dir_size_recursive(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return total;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size_recursive(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// Extract an incident tag from an evidence filename, **normalising the
/// nanosecond-epoch form to ISO** so the two coexisting tag formats group
/// together (ADR-0116 D2, "Never delete the only artefact of an incident").
///
/// Two mutually incompatible formats are on disk:
///
///   - ISO — `aberp.duckdb.CORRUPT-20260705T184449Z` (and its `.wal` sibling)
///   - nanosecond epoch — `aberp.duckdb.audit.log.corrupt-1783315209649645000.bak`
///
/// Under tag-only keying the nanosecond form shares a tag string with
/// *nothing*, including the ISO-tagged `.CORRUPT-` file from the same
/// incident — so every nanosecond-tagged artefact becomes a singleton
/// incident, permanently protected by the "only artefact" rule. Safe, but
/// **technically correct and operationally inert**: the policy would never
/// release the 22 `corrupt-*.bak` + 9 `healed-*.bak` that are most of the
/// growth. Normalising at parse time is what makes the policy able to act.
pub fn normalise_incident_tag(name: &str, mtime: OffsetDateTime) -> Option<String> {
    // ISO form: <14 chars>T<6 chars>Z, anywhere in the name.
    if let Some(tag) = find_iso_tag(name) {
        return Some(tag);
    }
    // Nanosecond-epoch form: a 16+ digit run. Convert via the epoch, then
    // format as ISO so it keys with the ISO artefacts of the same incident.
    if let Some(nanos) = find_nanos_run(name) {
        if let Ok(dt) = OffsetDateTime::from_unix_timestamp_nanos(nanos as i128) {
            return Some(format_iso_tag(dt));
        }
    }
    // A bare `_evidence-20260627` / `_recovery-20260629` date-only directory.
    if let Some(tag) = find_date_only_tag(name) {
        return Some(tag);
    }
    // No tag in the name at all. Fall back to the mtime — but ONLY as a
    // grouping hint; `plan_evidence_release` still refuses to release a
    // singleton, and an artefact whose mtime was unreadable is ungroupable.
    let _ = mtime;
    None
}

fn find_iso_tag(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    for start in 0..bytes.len() {
        if start + 16 > bytes.len() {
            break;
        }
        let win = &name[start..start + 16];
        let w = win.as_bytes();
        let shaped = w[..8].iter().all(u8::is_ascii_digit)
            && w[8] == b'T'
            && w[9..15].iter().all(u8::is_ascii_digit)
            && w[15] == b'Z';
        if shaped {
            return Some(win.to_string());
        }
    }
    None
}

fn find_date_only_tag(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    for start in 0..bytes.len() {
        if start + 8 > bytes.len() {
            break;
        }
        let win = &name[start..start + 8];
        if !win.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        // Reject a run that is part of a longer digit sequence (a nanosecond
        // tag would otherwise be shredded into a bogus date).
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let after_ok = start + 8 == bytes.len() || !bytes[start + 8].is_ascii_digit();
        if before_ok && after_ok && win.starts_with("20") {
            return Some(format!("{win}T000000Z"));
        }
    }
    None
}

fn find_nanos_run(name: &str) -> Option<u64> {
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // 19 digits is a nanosecond epoch in this era; 16+ is the safe floor
        // (a 2026 nanosecond stamp is 19 digits, a millisecond one 13).
        if i - start >= 16 {
            if let Ok(n) = name[start..i].parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn format_iso_tag(dt: OffsetDateTime) -> String {
    use time::macros::format_description;
    const TS: &[time::format_description::FormatItem<'_>] =
        format_description!("[year][month][day]T[hour][minute][second]Z");
    dt.format(TS)
        .unwrap_or_else(|_| dt.unix_timestamp().to_string())
}

/// Why an artefact is being kept rather than released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainReason {
    /// Younger than the age floor.
    WithinAgeFloor,
    /// Belongs to one of the N most recent distinct incidents.
    RecentIncident,
    /// The only artefact of its incident — never released.
    OnlyArtefactOfIncident,
    /// No incident tag and no usable mtime: ungroupable ⇒ protected.
    Ungroupable,
    /// Encrypted credential material. Never archived to a less-protected
    /// location; release for these means delete-in-place or nothing, and
    /// this command never deletes in place.
    CredentialMaterial,
}

/// One artefact's disposition under the tiered release policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceDisposition {
    pub artefact: EvidenceArtefact,
    /// `None` ⇒ releasable (archive-then-remove). `Some(_)` ⇒ retained.
    pub retained_because: Option<RetainReason>,
}

/// The tiered evidence policy's knobs. Nothing is ever deleted without an
/// explicit operator command; these bound what such a command may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidencePolicy {
    /// Never auto-release anything younger than this. ADR default: 90 days.
    pub age_floor_days: i64,
    /// Never release anything belonging to the N most recent distinct
    /// incidents, whichever set is larger. ADR default: 3.
    pub recent_incidents: usize,
}

impl Default for EvidencePolicy {
    fn default() -> Self {
        EvidencePolicy {
            age_floor_days: 90,
            recent_incidents: 3,
        }
    }
}

/// Decide, for each artefact, whether it may be released. **Pure** — no IO,
/// no clock read (the caller passes `now`), so the whole policy is
/// exhaustively testable.
///
/// Three independent floors, plus `release != delete` at the call site:
///
///  1. nothing younger than `age_floor_days`;
///  2. nothing in the `recent_incidents` most recent distinct incidents;
///  3. never the ONLY artefact of an incident — and **ungroupable ⇒
///     protected**, so an artefact whose incident cannot be established is
///     kept rather than guessed at.
///
/// Credential material (`*keychain*`) is retained unconditionally: the
/// archive step would copy encrypted NAV credentials and an SMTP password
/// into a second, less-protected location, which is strictly worse than
/// leaving them where they are.
pub fn plan_evidence_release(
    artefacts: &[EvidenceArtefact],
    policy: &EvidencePolicy,
    now: OffsetDateTime,
) -> Vec<EvidenceDisposition> {
    use std::collections::{BTreeMap, BTreeSet};

    // Group by (incident tag). Untagged artefacts are their own singleton
    // and fall to `Ungroupable` below.
    let mut per_incident: BTreeMap<&str, usize> = BTreeMap::new();
    for a in artefacts {
        if let Some(tag) = a.incident_tag.as_deref() {
            *per_incident.entry(tag).or_insert(0) += 1;
        }
    }
    // The N most recent distinct incidents, by tag (ISO tags sort
    // chronologically because every tag is normalised to a fixed-width ISO
    // form at parse time — a `20260705T…` and a `20260705T…` from a
    // nanosecond stamp compare identically).
    let recent: BTreeSet<&str> = per_incident
        .keys()
        .rev()
        .take(policy.recent_incidents)
        .copied()
        .collect();

    let cutoff = now - time::Duration::days(policy.age_floor_days);

    artefacts
        .iter()
        .map(|a| {
            let reason = if a.is_credential_material {
                Some(RetainReason::CredentialMaterial)
            } else if !a.mtime_known || a.incident_tag.is_none() {
                Some(RetainReason::Ungroupable)
            } else if a.modified_at >= cutoff {
                Some(RetainReason::WithinAgeFloor)
            } else if a
                .incident_tag
                .as_deref()
                .is_some_and(|t| recent.contains(t))
            {
                Some(RetainReason::RecentIncident)
            } else if a
                .incident_tag
                .as_deref()
                .and_then(|t| per_incident.get(t))
                .copied()
                .unwrap_or(0)
                <= 1
            {
                Some(RetainReason::OnlyArtefactOfIncident)
            } else {
                None
            };
            EvidenceDisposition {
                artefact: a.clone(),
                retained_because: reason,
            }
        })
        .collect()
}

/// Result of archiving one artefact out of the live tenant home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedArtefact {
    pub from: PathBuf,
    pub to: PathBuf,
    pub byte_size: u64,
    pub sha256: String,
}

/// **Archive-then-remove.** Copy `artefact` into
/// `<archive_root>/<tenant>/<incident-tag>/`, verify the copy byte-for-byte
/// by SHA-256, and only then unlink the original — so "pruned" never means
/// "gone".
///
/// The copy is **verbatim, not compressed**. ADR-0116 D2 says "(compressed)";
/// this implementation deliberately does not compress. Two reasons, both
/// conservative: a compression crate would be a new supply-chain dependency
/// (ADR-0007) added for a Phase-2 convenience, and verbatim bytes plus a
/// SHA-256 that the caller can re-check are strictly stronger for forensic
/// evidence than a re-encoded copy whose integrity depends on a decoder.
/// Recorded as drift in the ADR-0116 implementation notes.
///
/// The unlink goes through the *ordinary* filesystem call rather than
/// [`guarded_remove`], because the artefact IS protected evidence — the guard
/// would refuse it, correctly. That is why this function is the only place in
/// the tree allowed to unlink evidence, why it is reachable solely from an
/// explicit operator command, and why it refuses to run until the archived
/// copy's hash matches.
pub fn archive_then_remove(
    artefact: &EvidenceArtefact,
    archive_root: &Path,
    tenant: &str,
) -> Result<ArchivedArtefact> {
    if artefact.is_credential_material {
        return Err(SnapshotError::RestoreRefused(format!(
            "refusing to archive credential material {}: encrypted NAV/SMTP credentials are \
             never copied to a second location (ADR-0116 D2)",
            artefact.path.display()
        )));
    }
    let tag = artefact
        .incident_tag
        .clone()
        .unwrap_or_else(|| "untagged".to_string());
    let dest_dir = archive_root
        .join(crate::store::sanitise_tenant(tenant))
        .join(crate::store::sanitise_tenant(&tag));
    std::fs::create_dir_all(&dest_dir).map_err(|e| SnapshotError::io(&dest_dir, e))?;
    let dest = dest_dir.join(&artefact.name);

    let meta = std::fs::symlink_metadata(&artefact.path)
        .map_err(|e| SnapshotError::io(&artefact.path, e))?;
    if meta.is_dir() {
        return Err(SnapshotError::RestoreRefused(format!(
            "refusing to archive directory {}: only regular evidence FILES are archived; a \
             directory-shaped artefact is inspected and released by hand",
            artefact.path.display()
        )));
    }

    let bytes = std::fs::read(&artefact.path).map_err(|e| SnapshotError::io(&artefact.path, e))?;
    std::fs::write(&dest, &bytes).map_err(|e| SnapshotError::io(&dest, e))?;

    // Verify the archived copy from DISK, not from the buffer we just wrote —
    // a write that silently short-wrote would otherwise verify against its
    // own source and pass.
    let written = std::fs::read(&dest).map_err(|e| SnapshotError::io(&dest, e))?;
    let src_sha = sha256_bytes(&bytes);
    let dst_sha = sha256_bytes(&written);
    if src_sha != dst_sha {
        // Leave the original in place. An unverifiable archive is not a
        // release; it is a failed copy.
        let _ = std::fs::remove_file(&dest);
        return Err(SnapshotError::RestoreRefused(format!(
            "archived copy of {} does not match the original (src {src_sha} != dst {dst_sha}) — \
             the original is left untouched",
            artefact.path.display()
        )));
    }

    // fsync the archived copy and its directory BEFORE unlinking the
    // original: the ordering invariant is that the surviving copy must be
    // durable before the record it replaces is destroyed. A power cut between
    // the two leaves both copies, never neither.
    fsync_path(&dest)?;
    fsync_dir(&dest_dir);

    std::fs::remove_file(&artefact.path).map_err(|e| SnapshotError::io(&artefact.path, e))?;
    tracing::warn!(
        from = %artefact.path.display(),
        to = %dest.display(),
        sha256 = %dst_sha,
        bytes = artefact.byte_size,
        "ADR-0116 D2 — recovery evidence ARCHIVED out of the live tenant home (verified copy \
         first, then unlink). The artefact still exists; it has moved."
    );
    Ok(ArchivedArtefact {
        from: artefact.path.clone(),
        to: dest,
        byte_size: artefact.byte_size,
        sha256: dst_sha,
    })
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn fsync_path(path: &Path) -> Result<()> {
    let f = std::fs::File::open(path).map_err(|e| SnapshotError::io(path, e))?;
    f.sync_all().map_err(|e| SnapshotError::io(path, e))
}

fn fsync_dir(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}
