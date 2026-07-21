//! S433 — multi-tenant registry + switch-on-restart hint.
//!
//! Before S433 a tenant was nothing but the `--tenant` CLI string: the
//! launchers set `ABERP_TENANT`, the Tauri shell forwarded it as
//! `--tenant <slug>`, and every per-tenant artifact (DuckDB at
//! `~/.aberp/<slug>/aberp.duckdb`, keychain namespace `aberp.nav.<slug>`,
//! seller config at `~/.aberp/<slug>/seller.toml`) was keyed off that
//! string. There was no way to *enumerate* tenants, no operator-facing
//! CRUD, and switching tenants meant editing a launcher env var by hand.
//!
//! This module adds the missing menu side: a single registry file
//! `~/.aberp/tenants.toml` listing every tenant (slug + display name +
//! `Active`/`Archived` state + creation stamp), plus the switch
//! mechanism. Switching is deliberately **restart-based**, never a live
//! in-process swap: swapping the DuckDB handle, NAV credentials, and
//! keychain namespace out from under in-flight daemons mid-process is
//! exactly the class of footgun [[trust-code-not-operator]] /
//! [[hulye-biztos]] tell us to design out. Instead the switch writes a
//! one-shot hint file `~/.aberp/next_tenant`; the next boot consumes it
//! (honor-once), overriding the tenant + DuckDB path for that boot only.
//!
//! Why hand-rolled TOML and not the `toml` crate: the workspace carries
//! no `toml` dependency — `seller.toml` is hand-parsed for its
//! multi-writer section-preservation discipline. `tenants.toml` is
//! *single-writer* (only this module writes it), so the preservation
//! concern doesn't apply, but matching the codebase's zero-`toml`-dep
//! convention (rule 11) keeps the dependency surface flat. The schema is
//! a fixed 4-field array-of-tables, trivially serialised + parsed by the
//! line walker below.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_db::HandleArc;
use anyhow::{anyhow, Context, Result};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

/// Registry file name under `~/.aberp/`.
pub const REGISTRY_FILENAME: &str = "tenants.toml";
/// One-shot switch hint file name under `~/.aberp/`.
pub const NEXT_TENANT_FILENAME: &str = "next_tenant";
/// Per-tenant DuckDB file name under `~/.aberp/<slug>/`.
pub const TENANT_DB_FILENAME: &str = "aberp.duckdb";

/// Lifecycle state of a tenant in the registry.
///
/// - `Active` — a real operator tenant in the working pool.
/// - `Archived` — soft-deleted (still on disk, hidden from the active
///   pool, refused as a switch target until restored).
/// - `Demo` (S433) — the bundled `demo` safety-net tenant seeded on a
///   fresh install. Bootable + switchable like Active, but it can NEVER
///   be archived ([[trust-code-not-operator]]) and shows a DEMO badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantState {
    Active,
    Archived,
    Demo,
}

impl TenantState {
    /// Storage token written to / read from `tenants.toml`.
    pub fn as_token(self) -> &'static str {
        match self {
            TenantState::Active => "active",
            TenantState::Archived => "archived",
            TenantState::Demo => "demo",
        }
    }

    fn from_token(s: &str) -> Result<Self> {
        match s {
            "active" => Ok(TenantState::Active),
            "archived" => Ok(TenantState::Archived),
            "demo" => Ok(TenantState::Demo),
            other => Err(anyhow!("unknown tenant state token {other:?}")),
        }
    }
}

/// S433 — the bundled demo tenant's slug + display name. Seeded on a
/// fresh install so a new operator lands in a usable, populated system
/// instead of an empty NeedsSetup wall.
pub const DEMO_SLUG: &str = "demo";
pub const DEMO_DISPLAY_NAME: &str = "Demo Tenant";

/// One registry row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantEntry {
    pub slug: String,
    pub display_name: String,
    pub state: TenantState,
    /// RFC3339 UTC creation stamp.
    pub created_at: String,
    /// S434 — the per-tenant "NAV synchron" switch. `true` (default) runs
    /// the Hungarian NAV pipeline: the boot path loads NAV credentials,
    /// the §169 seller gate is enforced, invoices submit to NAV. `false`
    /// (international operators) skips NAV entirely — boot goes straight
    /// to Ready, seller tax is optional, invoices are stored LOCAL ONLY.
    ///
    /// BACKWARD COMPAT: a `tenants.toml` row written before S434 carries
    /// no `nav_enabled` key; [`PartialEntry::finish`] defaults the MISSING
    /// field to `true` so existing single-tenant HU installs keep their
    /// current NAV behaviour. Only the bundled demo tenant defaults
    /// `false` (see [`TenantRegistry::add_demo`]).
    pub nav_enabled: bool,

    /// S441 (ADR-0086/0087/0088) — the per-tenant DÁP/QES audit-chain
    /// switch. `false` (default) keeps the existing unsigned hash chain.
    /// `true` (Defense operators) opens a signed, NETLOCK-timestamp-anchored
    /// audit chain: a service session at boot, operator login via DÁP, and
    /// heartbeat anchors. BACKWARD COMPAT: a row written before S441 carries
    /// no `dap_enabled` key; [`PartialEntry::finish`] defaults the MISSING
    /// field to `false` (existing installs keep their unsigned chain).
    pub dap_enabled: bool,

    /// S441 (ADR-0087) — heartbeat anchor cadence in seconds (default 900 =
    /// 15 min). Only consulted when `dap_enabled`. Missing key → 900.
    pub audit_anchor_heartbeat_seconds: u64,

    /// S443 (ADR-0092) — the per-tenant QC probe calibration-stale window in
    /// seconds (default 86400 = 24h). A probe whose last calibration is older
    /// than this records its measurement with `calibration_stale` and raises a
    /// warning instead of an NCR. Operators can tighten it (e.g. 28800 = 8h
    /// per-shift) or relax it (e.g. 604800 = 7d for low-volume work).
    /// BACKWARD COMPAT: a row written before S443 carries no key →
    /// [`PartialEntry::finish`] defaults the MISSING field to 86400.
    pub qc_calibration_stale_window_seconds: u64,
}

/// Typed errors for the state-transition invariants. Routes map these to
/// HTTP status codes; the variants are the [[trust-code-not-operator]]
/// guards expressed in the type system so no operator action can reach an
/// unsafe registry state.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TenantRegistryError {
    #[error("slug {0:?} is invalid: tenant slugs must be 1–64 chars of [A-Za-z0-9_-]")]
    InvalidSlug(String),
    #[error("display name must be 1–120 chars with no control characters")]
    InvalidDisplayName,
    #[error("a tenant with slug {0:?} already exists")]
    SlugTaken(String),
    #[error("no tenant with slug {0:?}")]
    NotFound(String),
    #[error("cannot archive the currently-running tenant {0:?} — switch to another tenant first")]
    CannotArchiveRunning(String),
    #[error("cannot archive {0:?} — it is the only Active tenant; at least one must stay Active")]
    CannotArchiveOnlyActive(String),
    #[error("cannot archive the demo tenant {0:?} — it is the bundled safety net")]
    CannotArchiveDemo(String),
}

/// In-memory view of `tenants.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantRegistry {
    pub tenants: Vec<TenantEntry>,
    /// S434 — operator preference: once a real (Active, non-demo) tenant
    /// exists the operator can hide the bundled demo from the default
    /// Tenants view (the demo stays unarchivable per the S433 invariant —
    /// just hidden). Stored as a top-level `hide_demo = true` line at the
    /// head of `tenants.toml`; absent → `false`.
    pub hide_demo: bool,
}

/// Validate a tenant slug. Restricted to `[A-Za-z0-9_-]{1,64}` so it is
/// safe as a single filesystem path component (`~/.aberp/<slug>/`) and a
/// keychain service suffix — no traversal, no separators, no spaces.
pub fn validate_slug(slug: &str) -> Result<(), TenantRegistryError> {
    let ok = !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(TenantRegistryError::InvalidSlug(slug.to_string()))
    }
}

fn validate_display_name(name: &str) -> Result<(), TenantRegistryError> {
    let ok =
        !name.is_empty() && name.chars().count() <= 120 && !name.chars().any(|c| c.is_control());
    if ok {
        Ok(())
    } else {
        Err(TenantRegistryError::InvalidDisplayName)
    }
}

impl TenantRegistry {
    pub fn find(&self, slug: &str) -> Option<&TenantEntry> {
        self.tenants.iter().find(|t| t.slug == slug)
    }

    /// A slug names a tenant that exists AND is Active. Used for the
    /// "keep ≥1 Active" archive guard (Demo does NOT count as Active).
    pub fn is_active(&self, slug: &str) -> bool {
        matches!(
            self.find(slug),
            Some(TenantEntry {
                state: TenantState::Active,
                ..
            })
        )
    }

    /// A slug names a bootable tenant (Active OR Demo) — the valid switch
    /// targets and the gate the boot-hint consumer checks. Archived
    /// tenants are not bootable until restored.
    pub fn is_bootable(&self, slug: &str) -> bool {
        matches!(
            self.find(slug),
            Some(TenantEntry {
                state: TenantState::Active | TenantState::Demo,
                ..
            })
        )
    }

    fn active_count(&self) -> usize {
        self.tenants
            .iter()
            .filter(|t| t.state == TenantState::Active)
            .count()
    }

    /// Append a new Active tenant. Pure: caller supplies `now` so tests
    /// are deterministic. Errors if the slug is invalid or already taken.
    pub fn add(
        &mut self,
        slug: &str,
        display_name: &str,
        now: OffsetDateTime,
    ) -> Result<TenantEntry, TenantRegistryError> {
        validate_slug(slug)?;
        validate_display_name(display_name)?;
        if self.find(slug).is_some() {
            return Err(TenantRegistryError::SlugTaken(slug.to_string()));
        }
        let entry = TenantEntry {
            slug: slug.to_string(),
            display_name: display_name.to_string(),
            state: TenantState::Active,
            created_at: now
                .format(&Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()),
            // New operator tenants default to NAV-on (HU §169). The
            // operator flips it off from the Tenants screen for an
            // international tenant.
            nav_enabled: true,
            // S441 — DÁP/QES audit chain is OPT-IN per tenant (Defense line).
            dap_enabled: false,
            audit_anchor_heartbeat_seconds: 900,
            qc_calibration_stale_window_seconds: 86400,
        };
        self.tenants.push(entry.clone());
        Ok(entry)
    }

    /// Append the bundled demo tenant (state `Demo`). Errors only if a
    /// `demo` slug already exists (idempotency guard).
    pub fn add_demo(&mut self, now: OffsetDateTime) -> Result<TenantEntry, TenantRegistryError> {
        if self.find(DEMO_SLUG).is_some() {
            return Err(TenantRegistryError::SlugTaken(DEMO_SLUG.to_string()));
        }
        let entry = TenantEntry {
            slug: DEMO_SLUG.to_string(),
            display_name: DEMO_DISPLAY_NAME.to_string(),
            state: TenantState::Demo,
            created_at: now
                .format(&Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()),
            // S434 — demo is the NAV-off sandbox: an international operator
            // can drive it end-to-end without any Hungarian NAV setup.
            nav_enabled: false,
            // S441 — demo never runs the DÁP/QES audit chain.
            dap_enabled: false,
            audit_anchor_heartbeat_seconds: 900,
            qc_calibration_stale_window_seconds: 86400,
        };
        self.tenants.push(entry.clone());
        Ok(entry)
    }

    /// S434 — flip a tenant's `nav_enabled` flag. Returns the PRIOR value
    /// (for the `TenantNavToggled` audit's old/new pair). Errors if the
    /// slug is absent. A no-op flip (new == old) still succeeds and
    /// returns `old`; the caller decides whether to skip the audit.
    pub fn set_nav_enabled(
        &mut self,
        slug: &str,
        enabled: bool,
    ) -> Result<bool, TenantRegistryError> {
        let t = self
            .tenants
            .iter_mut()
            .find(|t| t.slug == slug)
            .ok_or_else(|| TenantRegistryError::NotFound(slug.to_string()))?;
        let old = t.nav_enabled;
        t.nav_enabled = enabled;
        Ok(old)
    }

    /// S443 — set a tenant's QC calibration-stale window (seconds). Returns
    /// the PRIOR value. Errors if the slug is absent.
    pub fn set_qc_calibration_stale_window_seconds(
        &mut self,
        slug: &str,
        seconds: u64,
    ) -> Result<u64, TenantRegistryError> {
        let t = self
            .tenants
            .iter_mut()
            .find(|t| t.slug == slug)
            .ok_or_else(|| TenantRegistryError::NotFound(slug.to_string()))?;
        let old = t.qc_calibration_stale_window_seconds;
        t.qc_calibration_stale_window_seconds = seconds;
        Ok(old)
    }

    /// S434 — does the registry hold at least one real (Active, non-demo)
    /// tenant? Gates the operator's "hide demo" preference: hiding demo is
    /// only meaningful once a real tenant exists to fall back to.
    pub fn has_real_tenant(&self) -> bool {
        self.tenants
            .iter()
            .any(|t| t.state == TenantState::Active && t.slug != DEMO_SLUG)
    }

    /// Soft-delete a tenant. Refuses the two unsafe cases in code:
    /// archiving the running tenant, or archiving the last Active one.
    pub fn archive(&mut self, slug: &str, running_slug: &str) -> Result<(), TenantRegistryError> {
        if slug == running_slug {
            return Err(TenantRegistryError::CannotArchiveRunning(slug.to_string()));
        }
        let entry_state = self.find(slug).map(|t| t.state);
        let Some(state) = entry_state else {
            return Err(TenantRegistryError::NotFound(slug.to_string()));
        };
        // The bundled demo tenant is the safety net — never archivable.
        if state == TenantState::Demo {
            return Err(TenantRegistryError::CannotArchiveDemo(slug.to_string()));
        }
        let is_active = state == TenantState::Active;
        // Only block the only-Active case when the target is itself
        // Active (archiving an already-Archived tenant is a no-op error
        // path, not an only-Active concern).
        if is_active && self.active_count() <= 1 {
            return Err(TenantRegistryError::CannotArchiveOnlyActive(
                slug.to_string(),
            ));
        }
        for t in &mut self.tenants {
            if t.slug == slug {
                t.state = TenantState::Archived;
            }
        }
        Ok(())
    }

    /// Flip an Archived tenant back to Active. Errors only if absent
    /// (restoring an already-Active tenant is idempotent).
    pub fn restore(&mut self, slug: &str) -> Result<(), TenantRegistryError> {
        if self.find(slug).is_none() {
            return Err(TenantRegistryError::NotFound(slug.to_string()));
        }
        for t in &mut self.tenants {
            if t.slug == slug {
                t.state = TenantState::Active;
            }
        }
        Ok(())
    }

    /// Serialise to the `tenants.toml` body. Deterministic: entries in
    /// vector order, fields in a fixed order.
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# ABERP tenant registry — managed by the Tenants admin screen (S433).\n\
             # Do not hand-edit while ABERP is running.\n",
        );
        // S434 — top-level operator preference, written before any
        // [[tenant]] block. Omitted when false so an unchanged registry
        // stays byte-identical to the S433 form (the parser defaults it).
        if self.hide_demo {
            out.push_str("hide_demo = true\n");
        }
        for t in &self.tenants {
            out.push_str("\n[[tenant]]\n");
            out.push_str(&format!("slug = {}\n", quote(&t.slug)));
            out.push_str(&format!("display_name = {}\n", quote(&t.display_name)));
            out.push_str(&format!("state = {}\n", quote(t.state.as_token())));
            out.push_str(&format!("created_at = {}\n", quote(&t.created_at)));
            // S434 — bare bool token (no quotes): a fixed-vocab `true`/
            // `false` the line walker parses without the string unquoter.
            out.push_str(&format!("nav_enabled = {}\n", t.nav_enabled));
            // S441 — omit the DÁP keys when at their defaults so a
            // non-Defense registry stays byte-identical to the S434 form
            // (the parser defaults missing keys to false / 900).
            if t.dap_enabled {
                out.push_str("dap_enabled = true\n");
            }
            if t.audit_anchor_heartbeat_seconds != 900 {
                out.push_str(&format!(
                    "audit_anchor_heartbeat_seconds = {}\n",
                    t.audit_anchor_heartbeat_seconds
                ));
            }
            // S443 — omit when at the 24h default (byte-identical to pre-S443).
            if t.qc_calibration_stale_window_seconds != 86400 {
                out.push_str(&format!(
                    "qc_calibration_stale_window_seconds = {}\n",
                    t.qc_calibration_stale_window_seconds
                ));
            }
        }
        out
    }

    /// Parse a `tenants.toml` body. Tolerant of comments + blank lines.
    /// A row is complete once `[[tenant]]` is seen; missing fields error
    /// loud (rule 12) rather than defaulting silently.
    pub fn parse_toml(body: &str) -> Result<Self> {
        let mut tenants: Vec<TenantEntry> = Vec::new();
        let mut hide_demo = false;
        let mut cur: Option<PartialEntry> = None;
        for (lineno, raw) in body.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[tenant]]" {
                if let Some(p) = cur.take() {
                    tenants.push(p.finish(lineno)?);
                }
                cur = Some(PartialEntry::default());
                continue;
            }
            let (key, val_raw) = line.split_once('=').ok_or_else(|| {
                anyhow!(
                    "tenants.toml line {} not `key = value`: {line:?}",
                    lineno + 1
                )
            })?;
            let key = key.trim();
            let val_raw = val_raw.trim();
            // S434 — top-level (pre-`[[tenant]]`) operator-preference keys
            // carry bare values, not quoted strings.
            if cur.is_none() {
                match key {
                    "hide_demo" => {
                        hide_demo = parse_bool(val_raw).with_context(|| {
                            format!("tenants.toml line {} hide_demo", lineno + 1)
                        })?;
                        continue;
                    }
                    other => {
                        return Err(anyhow!(
                            "tenants.toml unknown top-level key {other:?} at line {} \
                             (before any [[tenant]])",
                            lineno + 1
                        ))
                    }
                }
            }
            let p = cur
                .as_mut()
                .ok_or_else(|| anyhow!("tenants.toml line {} before any [[tenant]]", lineno + 1))?;
            match key {
                // S434 — bare bool, not a quoted string.
                "nav_enabled" => {
                    p.nav_enabled =
                        Some(parse_bool(val_raw).with_context(|| {
                            format!("tenants.toml line {} nav_enabled", lineno + 1)
                        })?)
                }
                // S441 — DÁP/QES audit-chain keys. Bare bool / bare integer.
                "dap_enabled" => {
                    p.dap_enabled =
                        Some(parse_bool(val_raw).with_context(|| {
                            format!("tenants.toml line {} dap_enabled", lineno + 1)
                        })?)
                }
                "audit_anchor_heartbeat_seconds" => {
                    p.audit_anchor_heartbeat_seconds =
                        Some(val_raw.trim().parse::<u64>().with_context(|| {
                            format!(
                                "tenants.toml line {} audit_anchor_heartbeat_seconds",
                                lineno + 1
                            )
                        })?)
                }
                // S443 — QC probe calibration-stale window (bare integer secs).
                "qc_calibration_stale_window_seconds" => {
                    p.qc_calibration_stale_window_seconds =
                        Some(val_raw.trim().parse::<u64>().with_context(|| {
                            format!(
                                "tenants.toml line {} qc_calibration_stale_window_seconds",
                                lineno + 1
                            )
                        })?)
                }
                "slug" | "display_name" | "state" | "created_at" => {
                    let val = unquote(val_raw)
                        .with_context(|| format!("tenants.toml line {} value", lineno + 1))?;
                    match key {
                        "slug" => p.slug = Some(val),
                        "display_name" => p.display_name = Some(val),
                        "state" => p.state = Some(val),
                        "created_at" => p.created_at = Some(val),
                        _ => unreachable!(),
                    }
                }
                other => {
                    return Err(anyhow!(
                        "tenants.toml unknown key {other:?} at line {}",
                        lineno + 1
                    ))
                }
            }
        }
        if let Some(p) = cur.take() {
            tenants.push(p.finish(body.lines().count())?);
        }
        Ok(TenantRegistry { tenants, hide_demo })
    }

    /// Read the registry from `path`. A missing file is an empty registry
    /// (first boot of this version) — not an error.
    pub fn read_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(body) => Self::parse_toml(&body)
                .with_context(|| format!("parse tenant registry at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read tenant registry at {}", path.display())),
        }
    }

    /// Atomically write the registry to `path` (tempfile + fsync +
    /// rename, 0600). Either the full new body lands or `path` is
    /// untouched — no half-written registry (rule 12).
    pub fn write_to(&self, path: &Path) -> Result<()> {
        write_atomic(path, self.to_toml().as_bytes())
    }
}

#[derive(Default)]
struct PartialEntry {
    slug: Option<String>,
    display_name: Option<String>,
    state: Option<String>,
    created_at: Option<String>,
    nav_enabled: Option<bool>,
    dap_enabled: Option<bool>,
    audit_anchor_heartbeat_seconds: Option<u64>,
    qc_calibration_stale_window_seconds: Option<u64>,
}

impl PartialEntry {
    fn finish(self, lineno: usize) -> Result<TenantEntry> {
        let slug = self
            .slug
            .ok_or_else(|| anyhow!("tenants.toml [[tenant]] near line {lineno} missing slug"))?;
        let display_name = self
            .display_name
            .ok_or_else(|| anyhow!("tenants.toml tenant {slug:?} missing display_name"))?;
        let state = TenantState::from_token(
            self.state
                .as_deref()
                .ok_or_else(|| anyhow!("tenants.toml tenant {slug:?} missing state"))?,
        )?;
        let created_at = self
            .created_at
            .ok_or_else(|| anyhow!("tenants.toml tenant {slug:?} missing created_at"))?;
        Ok(TenantEntry {
            slug,
            display_name,
            state,
            created_at,
            // S434 BACKWARD COMPAT: this is the ONE field that defaults
            // instead of erroring loud — a pre-S434 row has no
            // `nav_enabled` key and MUST keep its current NAV-on behaviour.
            nav_enabled: self.nav_enabled.unwrap_or(true),
            // S441 BACKWARD COMPAT: a pre-S441 row has no DÁP keys → the
            // unsigned chain (false) + the 15-min default.
            dap_enabled: self.dap_enabled.unwrap_or(false),
            audit_anchor_heartbeat_seconds: self.audit_anchor_heartbeat_seconds.unwrap_or(900),
            // S443 BACKWARD COMPAT: a pre-S443 row has no key → 24h default.
            qc_calibration_stale_window_seconds: self
                .qc_calibration_stale_window_seconds
                .unwrap_or(86400),
        })
    }
}

/// S434 — parse the fixed `true`/`false` vocabulary written by
/// [`TenantRegistry::to_toml`]. Loud on anything else (rule 12).
fn parse_bool(s: &str) -> Result<bool> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(anyhow!("expected `true` or `false`, got {other:?}")),
    }
}

/// Quote a string for the TOML body: wrap in `"` and escape `\` + `"`.
/// Slug + state + created_at are pre-validated to contain none of these;
/// only `display_name` can, and the escape keeps the round-trip exact.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Inverse of [`quote`]. Errors loud on an unterminated / unquoted value.
fn unquote(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return Err(anyhow!("expected a double-quoted string, got {s:?}"));
    }
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => return Err(anyhow!("unknown escape \\{other} in {s:?}")),
                None => return Err(anyhow!("dangling escape in {s:?}")),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

// ── Path resolution ──────────────────────────────────────────────────

/// Resolve the user home (`$HOME`, then `%USERPROFILE%`). The workspace
/// takes no `dirs` dependency (CLAUDE.md rule 11); this mirrors
/// `serve::dirs_home_or_loud_fail`'s posture so every per-tenant resolver
/// shares one home discipline.
fn home_dir() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    Err(anyhow!(
        "neither HOME nor USERPROFILE is set; cannot resolve the edition data root (~/{})",
        crate::build_profile::edition_data_dirname()
    ))
}

/// `~/.aberp-<edition>` — the edition-locked data root (ADR-0093 §5).
/// `Portable` → `~/.aberp-portable`, `Defense` → `~/.aberp-defense`; NEVER
/// the frozen prod line's `~/.aberp`. The edition segment is a COMPILE-TIME
/// constant ([`crate::build_profile::edition_data_dirname`]), so no env var
/// or launcher string can repoint a build at another edition's — or prod's —
/// root (FOUNDATION §5: path derived, not user-supplied). Errors if neither
/// `$HOME` nor `%USERPROFILE%` is set.
pub fn aberp_root() -> Result<PathBuf> {
    Ok(home_dir()?.join(crate::build_profile::edition_data_dirname()))
}

/// Resolve `path` as far as the filesystem allows, then re-append the
/// components that do not exist yet.
///
/// [`std::fs::canonicalize`] resolves symlinks and `..`, but fails
/// outright when any component is missing — and the DB path legitimately
/// does not exist on a first launch (its parent dir is created later in
/// the serve boot). So: canonicalize the deepest ancestor that DOES
/// exist, then re-join the missing tail. Relative inputs are made
/// absolute against the CWD first, so `./aberp.duckdb` compares as the
/// real path it names rather than as a bare filename.
///
/// `..` and symlinks inside a *missing* tail are not resolved (there is no
/// filesystem to resolve them against), so a `..` can SURVIVE this call.
/// The guard does not reason about what such a leftover might mean — it
/// refuses the path outright ([`ensure_db_path_isolated`]). `ABERP.git`'s
/// copy of this comment argued the leftover was harmless because it could
/// only ever make the comparison read a non-tenant segment; that argument
/// was wrong there (its deny path is an allow-list keyed on the FIRST
/// segment, which `..` leaves untouched) and does not hold here either.
///
/// Kept in step with the `ABERP.git` original
/// (`db_path_guard::canonicalize_deepest`, d9b64a2) — the two repos having
/// the same guard behave differently is what created this defect class.
fn canonicalize_deepest(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            // No CWD: leave it relative. A relative path cannot carry an
            // absolute foreign root, so the caller treats it as an
            // ordinary dev path.
            Err(_) => path.to_path_buf(),
        }
    };
    fn walk(p: &Path) -> PathBuf {
        if let Ok(c) = p.canonicalize() {
            return c;
        }
        match (p.parent(), p.file_name()) {
            (Some(parent), Some(tail)) => walk(parent).join(tail),
            // Filesystem root, or a path with no nameable tail: nothing
            // left to peel.
            _ => p.to_path_buf(),
        }
    }
    walk(&abs)
}

/// ADR-0093 — refuse any DB / data path that resolves into a FOREIGN
/// edition's root: the frozen prod line's `~/.aberp/`, or the sibling
/// edition's `~/.aberp-<other>/`. The own edition's root and ordinary dev
/// paths (`./aberp.duckdb`, a temp dir) are allowed. This is the runtime
/// backstop behind the compile-time root binding: a hand-set `--db` /
/// `ABERP_DB` that NAMES prod's database is refused, however it is spelled.
/// State the rule that narrowly and no wider — ADR-0093 §5 used to promise
/// the binary "literally cannot open `~/.aberp/prod/…`", and that absolute
/// was disproven by execution twice (see below). One residual is NOT
/// covered: a hardlink to a file inside a foreign root, a second name for
/// the same inode rather than a link any path walk can resolve. Every
/// clause here is pinned row by row in the decision table in
/// `apps/aberp/tests/edition_db_isolation.rs`.
///
/// The path is matched BOTH as spelled AND canonicalized, and either hit
/// refuses. Matching the raw components alone was the S2 escape: a symlink
/// into the foreign root (`<dir>/link -> <dir>/.aberp`, passed as
/// `<dir>/link/prod/aberp.duckdb`) carries no `.aberp` component and walked
/// straight through. Same defect class, same fix as `ABERP.git` d9b64a2 —
/// but that repo's rule is an ALLOW-list (only under the own tenant root),
/// where canonicalizing both sides is sufficient on its own. This one is a
/// DENY-list on dirnames, and canonicalizing *instead of* the raw match
/// would open a second hole in the other direction: if `~/.aberp` were
/// itself a symlink, `~/.aberp/prod/aberp.duckdb` would resolve to a path
/// carrying no foreign component and start passing — a refusal that works
/// today, lost. Keeping both walks is strictly additive: every path refused
/// before this change is still refused, and the symlinked ones now are too.
///
/// The dirname compare is ASCII-CASE-INSENSITIVE. macOS APFS (and Windows
/// NTFS) resolve `~/.ABERP` and `~/.aberp` to the SAME directory, so a
/// byte-exact compare let `ABERP_DB=~/.ABERP/prod/aberp.duckdb` open the
/// frozen prod DuckDB read-write — executed, not theorised, on 2026-07-21.
/// Canonicalizing case-corrects an EXISTING root, but the resolved walk
/// alone is not enough: when the foreign root does not exist yet (a
/// machine where the sibling edition was never installed) there is nothing
/// to canonicalize against, the spelling survives verbatim, and the build
/// would create the foreign root under a second spelling of the same
/// directory. The foreign dirnames are pure ASCII, so `eq_ignore_ascii_case`
/// is the whole of it; no Unicode folding is implied or needed.
pub fn ensure_db_path_isolated(path: &Path) -> Result<()> {
    let resolved = canonicalize_deepest(path);
    for candidate in [path, resolved.as_path()] {
        for comp in candidate.components() {
            if let std::path::Component::Normal(name) = comp {
                for foreign in crate::build_profile::foreign_data_dirnames() {
                    if name.to_string_lossy().eq_ignore_ascii_case(foreign) {
                        return Err(anyhow!(
                            "ADR-0093 edition isolation: the {} edition refuses path {} (resolves to {}) — it resolves                          into the foreign data root '{}'. Each edition opens only its own ~/{}                          root, never prod's ~/.aberp/ or the sibling edition's.",
                            crate::build_profile::edition_label(),
                            path.display(),
                            resolved.display(),
                            foreign,
                            crate::build_profile::edition_data_dirname(),
                        ));
                    }
                }
            }
        }
    }

    // A `..` that SURVIVED canonicalization sits behind a component that
    // does not exist, so nothing on disk fixes its meaning: whatever it
    // names is decided later, by whoever creates that component first.
    // The dirname walks above cannot see through it. Refuse — the guard
    // never speculates about a path it could not resolve.
    if resolved
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(anyhow!(
            "ADR-0093 edition isolation: the {} edition refuses path {} — it resolves to {}, which \
             still contains an unresolved '..' behind a component that does not exist. Such a path \
             names no fixed directory, so it cannot be proven to stay out of prod's ~/.aberp/ or \
             the sibling edition's root. Pass a path whose parents exist.",
            crate::build_profile::edition_label(),
            path.display(),
            resolved.display(),
        ));
    }
    Ok(())
}

pub fn registry_path() -> Result<PathBuf> {
    Ok(aberp_root()?.join(REGISTRY_FILENAME))
}

pub fn next_tenant_hint_path() -> Result<PathBuf> {
    Ok(aberp_root()?.join(NEXT_TENANT_FILENAME))
}

/// Canonical per-tenant DuckDB path. Every registry-managed tenant lives
/// here; this is also what `run_prod.sh` sets `ABERP_DB` to, so deriving
/// the path from the slug on switch matches the existing layout exactly.
pub fn tenant_db_path(slug: &str) -> Result<PathBuf> {
    let db = aberp_root()?.join(slug).join(TENANT_DB_FILENAME);
    // Derived from the compile-time edition root, so this can never be a
    // foreign root — assert it anyway so the invariant is enforced at the
    // chokepoint rather than merely trusted (ADR-0093).
    ensure_db_path_isolated(&db)?;
    Ok(db)
}

/// S433 — a fresh install is one with NO `tenants.toml` AND no existing
/// per-tenant DuckDB under `~/.aberp/<slug>/aberp.duckdb`. The second
/// clause is the backward-compat guard: a real install already in flight
/// (prod systems, dev boxes) carries a tenant DB even before this version
/// wrote a registry, so it is NOT fresh — we must not inject demo there.
pub fn is_fresh_install(root: &Path) -> Result<bool> {
    if root.join(REGISTRY_FILENAME).exists() {
        return Ok(false);
    }
    match fs::read_dir(root) {
        Ok(rd) => {
            for entry in rd.flatten() {
                if entry.path().join(TENANT_DB_FILENAME).exists() {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        // `~/.aberp` doesn't exist yet → genuinely fresh.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => {
            Err(e).with_context(|| format!("scan {} for fresh-install check", root.display()))
        }
    }
}

pub fn is_fresh_install_default() -> Result<bool> {
    is_fresh_install(&aberp_root()?)
}

pub fn read_registry() -> Result<TenantRegistry> {
    TenantRegistry::read_from(&registry_path()?)
}

/// S434 — resolve a tenant's NAV-synchron mode for the BOOT decision,
/// read straight from `tenants.toml`. This is the [[trust-code-not-operator]]
/// chokepoint the boot path, the NAV daemons, and `submit_invoice` all
/// consult — not operator discipline.
///
/// Resolution order:
/// 1. A registry row for `slug` → its `nav_enabled` (a pre-S434 row with
///    no key parses as `true`).
/// 2. No row + `slug == "demo"` → `false`. The demo tenant is NAV-off by
///    definition; on a fresh install its registry row is written slightly
///    later in boot (`bootstrap_demo_tenant`), but the boot-state decision
///    at the keychain step must already know demo skips NAV.
/// 3. No row, any other slug → `true`. An existing single-tenant HU
///    install that predates the registry keeps its NAV-on behaviour.
///
/// A read/parse error is logged by the caller; this returns `true`
/// (fail-safe toward the existing NAV behaviour) on `Err`.
pub fn tenant_nav_enabled(slug: &str) -> Result<bool> {
    let reg = read_registry()?;
    Ok(match reg.find(slug) {
        Some(entry) => entry.nav_enabled,
        None => slug != DEMO_SLUG,
    })
}

/// S441 — resolve a tenant's DÁP/QES audit-chain config for the BOOT
/// decision: `(dap_enabled, audit_anchor_heartbeat_seconds)`. A missing row
/// (or a pre-S441 row) → `(false, 900)`: the unsigned chain, exactly as
/// before. The caller logs a read/parse error and treats it as `(false, …)`.
pub fn tenant_dap_config(slug: &str) -> Result<(bool, u64)> {
    let reg = read_registry()?;
    Ok(match reg.find(slug) {
        Some(entry) => (entry.dap_enabled, entry.audit_anchor_heartbeat_seconds),
        None => (false, 900),
    })
}

pub fn write_registry(reg: &TenantRegistry) -> Result<()> {
    reg.write_to(&registry_path()?)
}

// ── Switch hint (honor-once) ─────────────────────────────────────────

pub fn write_next_tenant_hint_at(path: &Path, slug: &str) -> Result<()> {
    write_atomic(path, slug.as_bytes())
}

pub fn write_next_tenant_hint(slug: &str) -> Result<()> {
    write_next_tenant_hint_at(&next_tenant_hint_path()?, slug)
}

/// Read + DELETE the switch hint. Returns `Some(slug)` exactly once per
/// written hint; subsequent calls return `None`. The delete happens
/// before the slug is returned so a crash mid-boot can't replay the
/// switch on the *next* boot — honor-once is the contract.
pub fn consume_next_tenant_hint_at(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(body) => {
            fs::remove_file(path)
                .with_context(|| format!("delete consumed switch hint {}", path.display()))?;
            let slug = body.trim().to_string();
            if slug.is_empty() {
                Ok(None)
            } else {
                Ok(Some(slug))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read switch hint {}", path.display())),
    }
}

pub fn consume_next_tenant_hint() -> Result<Option<String>> {
    consume_next_tenant_hint_at(&next_tenant_hint_path()?)
}

// ── Per-tenant audit emit ────────────────────────────────────────────

/// Routing for a `tenant.*` lifecycle audit append (ADR-0099). Mirrors
/// [`crate::snapshot::SnapshotAudit`]: the corrected fork model bans ANY
/// independent audit opener that appends on the LIVE (booted) tenant DB
/// inside the `aberp serve` process — two such openers off one stale head
/// self-assign the same seq (the 369→416→428→515 fork). The variant makes
/// the routing explicit at each call site.
pub enum TenantAudit<'a> {
    /// In-process (`aberp serve`) append targeting the BOOTED tenant's own
    /// DB — routed through the ONE shared [`aberp_db::Handle`]'s serialized
    /// writer (no independent opener, no stale-head seq collision; the
    /// WriteGuard drop runs the lockstep mirror sync). Used by the tenant
    /// admin HTTP handlers (switch / archive-restore / toggle-nav /
    /// seller-region) which all act on `state.tenant` in `state.db`.
    Handle(&'a HandleArc),
    /// A one-shot opener of a DB that has NO shared Handle in this process,
    /// so it CANNOT fork the serve writer (cf. snapshot `emit_reopen_cli`):
    ///   • a FOREIGN tenant's DB — `TenantCreated` lands in the NEW tenant's
    ///     chain, `TenantDemoSeeded` in the demo tenant's — a DIFFERENT file
    ///     than the booted Handle owns; the shared writer never touches it.
    ///   • a PRE-Handle boot append — `record_tenant_boot` runs at boot
    ///     BEFORE `open_tenant_handle` opens the shared instance, single-
    ///     threaded, with no daemon writer yet.
    /// Routed to the allow-listed [`emit_tenant_reopen`].
    Reopen,
}

/// Append a `tenant.*` lifecycle event, routed per [`TenantAudit`]. The
/// in-serve booted-DB callers append through the shared [`aberp_db::Handle`];
/// the foreign-DB / pre-Handle-boot callers reopen (they have no Handle for
/// that DB and cannot fork the serve writer).
pub fn emit_tenant_event(
    audit: &TenantAudit<'_>,
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    kind: EventKind,
    payload: Vec<u8>,
) -> Result<()> {
    let actor = Actor::from_local_cli(Ulid::new().to_string(), operator_login);
    match audit {
        TenantAudit::Handle(handle) => {
            // Shared writer: the ONE serialized instance. No independent
            // opener, no stale-head seq collision. WriteGuard drop runs the
            // lockstep sync_mirror, so no separate mirror step is needed.
            let mut conn = handle
                .write()
                .map_err(|e| anyhow!("shared writer for tenant lifecycle event: {e}"))?;
            aberp_audit_ledger::ensure_schema(&conn)
                .map_err(|e| anyhow!("ensure audit-ledger schema (tenant event): {e}"))?;
            let tx = conn
                .transaction()
                .map_err(|e| anyhow!("begin DuckDB tx (tenant event): {e}"))?;
            let meta = LedgerMeta::new(tenant, binary_hash);
            aberp_audit_ledger::append_in_tx(&tx, &meta, kind, payload, actor, None).map_err(
                |e| anyhow!("append tenant lifecycle audit entry via shared Handle: {e}"),
            )?;
            tx.commit()
                .map_err(|e| anyhow!("commit DuckDB tx (tenant event): {e}"))?;
            Ok(())
        }
        TenantAudit::Reopen => {
            emit_tenant_reopen(db_path, tenant, binary_hash, actor, kind, payload)
        }
    }
}

/// SANCTIONED RESIDUAL (ADR-0099 gate allow-list: `emit_tenant_reopen`) — a
/// one-shot ledger opener for a DB that has NO [`aberp_db::Handle`] in this
/// process. Opening the ledger by path (not the shared running instance) is
/// exactly what lets the create + demo-seed paths write into a DIFFERENT
/// tenant's chain than the one the binary booted with, and lets the boot
/// path append before the shared Handle exists. Neither can fork the serve
/// writer (that writer never opens these DBs), so this reopen is safe. Kept
/// a distinct, single-purpose fn so the cut-gate can allow-list it by name
/// (mirrors snapshot.rs `emit_reopen_cli`).
fn emit_tenant_reopen(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    actor: Actor,
    kind: EventKind,
    payload: Vec<u8>,
) -> Result<()> {
    let mut ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to record tenant lifecycle event (reopen)")?;
    ledger
        .append(kind, payload, actor, None)
        .context("append tenant lifecycle audit entry (reopen)")?;
    Ok(())
}

// ── Atomic file write (tempfile + fsync + rename, 0600) ──────────────
//
// A focused local copy of the seller.toml write discipline — the registry
// + hint are single-writer files in `~/.aberp/`, so this stays
// self-contained rather than reaching into `setup_seller_info`'s private
// writer.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent dir", path.display()))?;
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("tenants");
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp.{}-{}-{}",
        std::process::id(),
        nanos,
        seq
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("open tempfile {}", tmp_path.display()))?;
        f.write_all(body)
            .with_context(|| format!("write tempfile {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync tempfile {}", tmp_path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&tmp_path, perms)
            .with_context(|| format!("chmod 0600 {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).unwrap()
    }

    /// Unique temp dir under the system temp root. We avoid the
    /// `tempfile` dev-dep to keep the surface tight (rule 13 + matches
    /// `tests/print_invoice_render.rs`); the per-test ULID dir is leaked
    /// at end-of-test, acceptable for the OS-temp-root posture.
    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("aberp-tenant-registry")
            .join(Ulid::new().to_string());
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn sample() -> TenantRegistry {
        let mut r = TenantRegistry::default();
        r.add("prod", "ABEN AG", dt("2026-06-16T04:46:00Z"))
            .unwrap();
        r.add("test", "ABEN Test", dt("2026-06-16T04:47:00Z"))
            .unwrap();
        r
    }

    #[test]
    fn toml_round_trips_byte_exact_after_reparse() {
        let r = sample();
        let body = r.to_toml();
        let back = TenantRegistry::parse_toml(&body).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn display_name_with_quotes_round_trips() {
        let mut r = TenantRegistry::default();
        r.add(
            "acme",
            r#"ACME "Special" Co \ Ltd"#,
            dt("2026-06-16T00:00:00Z"),
        )
        .unwrap();
        let back = TenantRegistry::parse_toml(&r.to_toml()).unwrap();
        assert_eq!(r, back);
        assert_eq!(back.tenants[0].display_name, r#"ACME "Special" Co \ Ltd"#);
    }

    #[test]
    fn missing_file_is_empty_registry() {
        let dir = test_dir();
        let p = dir.as_path().join("tenants.toml");
        assert_eq!(
            TenantRegistry::read_from(&p).unwrap(),
            TenantRegistry::default()
        );
    }

    #[test]
    fn write_then_read_is_atomic_and_exact() {
        let dir = test_dir();
        let p = dir.as_path().join("sub").join("tenants.toml");
        let r = sample();
        r.write_to(&p).unwrap();
        assert_eq!(TenantRegistry::read_from(&p).unwrap(), r);
        // No stray tempfile left behind.
        let leftovers: Vec<_> = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "tempfile not cleaned up: {leftovers:?}"
        );
    }

    #[test]
    fn add_rejects_duplicate_and_bad_slug() {
        let mut r = sample();
        assert_eq!(
            r.add("prod", "dup", dt("2026-06-16T00:00:00Z")),
            Err(TenantRegistryError::SlugTaken("prod".into()))
        );
        assert!(matches!(
            r.add("bad slug", "x", dt("2026-06-16T00:00:00Z")),
            Err(TenantRegistryError::InvalidSlug(_))
        ));
        assert!(matches!(
            r.add("../etc", "x", dt("2026-06-16T00:00:00Z")),
            Err(TenantRegistryError::InvalidSlug(_))
        ));
    }

    #[test]
    fn archive_refuses_running_tenant() {
        let mut r = sample();
        assert_eq!(
            r.archive("prod", "prod"),
            Err(TenantRegistryError::CannotArchiveRunning("prod".into()))
        );
        // prod stays Active.
        assert!(r.is_active("prod"));
    }

    #[test]
    fn archive_refuses_only_active_tenant() {
        let mut r = TenantRegistry::default();
        r.add("solo", "Solo", dt("2026-06-16T00:00:00Z")).unwrap();
        // Not running it (running = something else), so the running guard
        // doesn't fire — the only-active guard must.
        assert_eq!(
            r.archive("solo", "other"),
            Err(TenantRegistryError::CannotArchiveOnlyActive("solo".into()))
        );
        assert!(r.is_active("solo"));
    }

    #[test]
    fn archive_then_restore_lifecycle() {
        let mut r = sample();
        // Running prod, archive test (two Active, not running test → ok).
        r.archive("test", "prod").unwrap();
        assert!(!r.is_active("test"));
        assert_eq!(r.find("test").unwrap().state, TenantState::Archived);
        r.restore("test").unwrap();
        assert!(r.is_active("test"));
    }

    #[test]
    fn archive_and_restore_unknown_slug_errors() {
        let mut r = sample();
        assert_eq!(
            r.archive("ghost", "prod"),
            Err(TenantRegistryError::NotFound("ghost".into()))
        );
        assert_eq!(
            r.restore("ghost"),
            Err(TenantRegistryError::NotFound("ghost".into()))
        );
    }

    #[test]
    fn hint_is_honor_once() {
        let dir = test_dir();
        let p = dir.as_path().join("next_tenant");
        assert_eq!(consume_next_tenant_hint_at(&p).unwrap(), None);
        write_next_tenant_hint_at(&p, "test").unwrap();
        assert_eq!(
            consume_next_tenant_hint_at(&p).unwrap(),
            Some("test".to_string())
        );
        // Second read sees nothing — the hint was consumed + deleted.
        assert_eq!(consume_next_tenant_hint_at(&p).unwrap(), None);
        assert!(!p.exists());
    }

    #[test]
    fn empty_hint_reads_as_none() {
        let dir = test_dir();
        let p = dir.as_path().join("next_tenant");
        write_next_tenant_hint_at(&p, "   ").unwrap();
        assert_eq!(consume_next_tenant_hint_at(&p).unwrap(), None);
    }

    #[test]
    fn nav_enabled_round_trips_and_defaults() {
        // add() → nav-on; add_demo() → nav-off; both survive a TOML
        // round-trip byte-for-byte at the value level.
        let mut r = TenantRegistry::default();
        r.add("hu", "HU Co", dt("2026-06-16T00:00:00Z")).unwrap();
        r.add_demo(dt("2026-06-16T00:00:00Z")).unwrap();
        assert!(r.find("hu").unwrap().nav_enabled);
        assert!(!r.find("demo").unwrap().nav_enabled);
        let back = TenantRegistry::parse_toml(&r.to_toml()).unwrap();
        assert_eq!(r, back);
        assert!(back.find("hu").unwrap().nav_enabled);
        assert!(!back.find("demo").unwrap().nav_enabled);
    }

    #[test]
    fn s441_dap_config_round_trips_and_defaults() {
        // New tenants default to dap-off + 900s heartbeat; an opted-in
        // Defense tenant round-trips through TOML byte-for-byte at the
        // value level.
        let mut r = TenantRegistry::default();
        r.add("prod", "Prod", dt("2026-06-17T00:00:00Z")).unwrap();
        // default: off + 900.
        assert!(!r.find("prod").unwrap().dap_enabled);
        assert_eq!(r.find("prod").unwrap().audit_anchor_heartbeat_seconds, 900);
        // Opt the tenant in with a custom cadence.
        {
            let t = r.tenants.iter_mut().find(|t| t.slug == "prod").unwrap();
            t.dap_enabled = true;
            t.audit_anchor_heartbeat_seconds = 1800;
        }
        let back = TenantRegistry::parse_toml(&r.to_toml()).unwrap();
        assert_eq!(r, back, "DÁP config survives a TOML round-trip");
        assert!(back.find("prod").unwrap().dap_enabled);
        assert_eq!(
            back.find("prod").unwrap().audit_anchor_heartbeat_seconds,
            1800
        );
    }

    #[test]
    fn pre_s441_row_without_dap_keys_defaults_off() {
        // BACKWARD COMPAT: a tenants.toml written before S441 (no dap_enabled
        // / audit_anchor_heartbeat_seconds keys) parses to the unsigned
        // chain (false) + the 15-min default.
        let legacy = "\
[[tenant]]
slug = \"prod\"
display_name = \"Prod\"
state = \"active\"
created_at = \"2026-06-16T00:00:00Z\"
nav_enabled = true
";
        let reg = TenantRegistry::parse_toml(legacy).unwrap();
        let prod = reg.find("prod").unwrap();
        assert!(!prod.dap_enabled, "missing dap_enabled defaults to false");
        assert_eq!(prod.audit_anchor_heartbeat_seconds, 900);
    }

    #[test]
    fn pre_s434_row_without_nav_enabled_defaults_true() {
        // BACKWARD COMPAT: a tenants.toml written by S433 (no nav_enabled
        // key) must parse with nav_enabled = true so existing HU installs
        // keep NAV on.
        let legacy = "\
# old registry
[[tenant]]
slug = \"prod\"
display_name = \"Prod\"
state = \"active\"
created_at = \"2026-06-16T00:00:00Z\"
";
        let reg = TenantRegistry::parse_toml(legacy).unwrap();
        assert!(
            reg.find("prod").unwrap().nav_enabled,
            "missing nav_enabled must default to true (backward compat)"
        );
    }

    #[test]
    fn set_nav_enabled_returns_prior_and_flips() {
        let mut r = sample();
        // prod starts NAV-on; flip off, prior reported true.
        assert!(r.set_nav_enabled("prod", false).unwrap());
        assert!(!r.find("prod").unwrap().nav_enabled);
        // Flip back on, prior reported false.
        assert!(!r.set_nav_enabled("prod", true).unwrap());
        assert!(r.find("prod").unwrap().nav_enabled);
        // Unknown slug errors.
        assert_eq!(
            r.set_nav_enabled("ghost", false),
            Err(TenantRegistryError::NotFound("ghost".into()))
        );
    }

    #[test]
    fn hide_demo_round_trips_and_gates_on_real_tenant() {
        let mut r = TenantRegistry::default();
        r.add_demo(dt("2026-06-16T00:00:00Z")).unwrap();
        // Only demo present → no real tenant.
        assert!(!r.has_real_tenant());
        r.add("acme", "ACME", dt("2026-06-16T01:00:00Z")).unwrap();
        assert!(r.has_real_tenant());
        // hide_demo default false; set + round-trip.
        assert!(!r.hide_demo);
        r.hide_demo = true;
        let back = TenantRegistry::parse_toml(&r.to_toml()).unwrap();
        assert_eq!(r, back);
        assert!(back.hide_demo);
        // Default (false) omits the line entirely → stays S433-shaped.
        let mut r2 = sample();
        r2.hide_demo = false;
        assert!(
            !r2.to_toml().contains("hide_demo"),
            "false hide_demo must not be serialised"
        );
    }

    #[test]
    fn tenant_nav_enabled_resolution_rule() {
        // Mirrors `tenant_nav_enabled`'s no-row arm (which reads from disk):
        // an absent demo slug resolves OFF, any other absent slug resolves
        // ON, and a present row uses its own flag.
        let resolve = |reg: &TenantRegistry, slug: &str| match reg.find(slug) {
            Some(e) => e.nav_enabled,
            None => slug != DEMO_SLUG,
        };
        let mut reg = TenantRegistry::default();
        assert!(!resolve(&reg, DEMO_SLUG), "absent demo → NAV off");
        assert!(resolve(&reg, "prod"), "absent non-demo → NAV on");
        // A present demo row carries its own (off) flag.
        reg.add_demo(dt("2026-06-16T00:00:00Z")).unwrap();
        assert!(!resolve(&reg, DEMO_SLUG));
    }

    #[test]
    fn demo_state_round_trips_and_is_bootable_not_active() {
        let mut r = TenantRegistry::default();
        r.add_demo(dt("2026-06-16T00:00:00Z")).unwrap();
        let back = TenantRegistry::parse_toml(&r.to_toml()).unwrap();
        assert_eq!(r, back);
        assert_eq!(back.find("demo").unwrap().state, TenantState::Demo);
        // Demo is bootable/switchable but does NOT count as Active (so it
        // never satisfies the keep-≥1-Active archive guard).
        assert!(back.is_bootable("demo"));
        assert!(!back.is_active("demo"));
    }

    #[test]
    fn archive_refuses_demo_tenant() {
        let mut r = TenantRegistry::default();
        r.add("prod", "Prod", dt("2026-06-16T00:00:00Z")).unwrap();
        r.add_demo(dt("2026-06-16T00:00:00Z")).unwrap();
        // Running prod, try to archive demo → refused (safety net).
        assert_eq!(
            r.archive("demo", "prod"),
            Err(TenantRegistryError::CannotArchiveDemo("demo".into()))
        );
        assert_eq!(r.find("demo").unwrap().state, TenantState::Demo);
    }

    #[test]
    fn is_fresh_install_detects_empty_vs_inflight() {
        let dir = test_dir();
        let root = dir.as_path();
        // Empty ~/.aberp dir → fresh.
        assert!(is_fresh_install(root).unwrap());
        // A registry file present → not fresh.
        let reg_path = root.join(REGISTRY_FILENAME);
        std::fs::write(&reg_path, b"# registry").unwrap();
        assert!(!is_fresh_install(root).unwrap());
        std::fs::remove_file(&reg_path).unwrap();
        // A per-tenant DB present (install in flight) → not fresh.
        let db = root.join("prod").join(TENANT_DB_FILENAME);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"duck").unwrap();
        assert!(!is_fresh_install(root).unwrap());
    }

    /// CRUD: the full Create → Switch → Archive → Restore lifecycle,
    /// persisted through disk (registry file + hint file) at each step —
    /// the same surfaces the routes drive.
    #[test]
    fn full_lifecycle_create_switch_archive_restore() {
        let dir = test_dir();
        let reg_path = dir.as_path().join("tenants.toml");
        let hint_path = dir.as_path().join("next_tenant");

        // Boot tenant prod (running) + create acme.
        let mut reg = TenantRegistry::default();
        reg.add("prod", "Prod", dt("2026-06-16T00:00:00Z")).unwrap();
        reg.add("acme", "ACME", dt("2026-06-16T01:00:00Z")).unwrap();
        reg.write_to(&reg_path).unwrap();

        // Switch to acme → hint written, honored once.
        write_next_tenant_hint_at(&hint_path, "acme").unwrap();
        assert_eq!(
            consume_next_tenant_hint_at(&hint_path).unwrap(),
            Some("acme".to_string())
        );
        assert_eq!(consume_next_tenant_hint_at(&hint_path).unwrap(), None);

        // Now running=acme: archive prod (not running, two Active → ok).
        let mut reg = TenantRegistry::read_from(&reg_path).unwrap();
        reg.archive("prod", "acme").unwrap();
        reg.write_to(&reg_path).unwrap();
        assert!(!TenantRegistry::read_from(&reg_path)
            .unwrap()
            .is_active("prod"));

        // Restore prod.
        let mut reg = TenantRegistry::read_from(&reg_path).unwrap();
        reg.restore("prod").unwrap();
        reg.write_to(&reg_path).unwrap();
        assert!(TenantRegistry::read_from(&reg_path)
            .unwrap()
            .is_active("prod"));
    }

    /// Audit isolation: TenantCreated lands in the NEW tenant's ledger,
    /// never in the caller's chain (per-tenant chain isolation).
    #[test]
    fn tenant_created_lands_in_new_tenant_ledger_not_caller() {
        let dir = test_dir();
        let bh = BinaryHash::from_bytes([7u8; 32]);
        let db_caller = dir.as_path().join("prod").join("aberp.duckdb");
        let db_new = dir.as_path().join("acme").join("aberp.duckdb");
        std::fs::create_dir_all(db_caller.parent().unwrap()).unwrap();
        std::fs::create_dir_all(db_new.parent().unwrap()).unwrap();

        // Initialise the caller's ledger so it exists but carries no
        // tenant events.
        Ledger::open(&db_caller, TenantId::new("prod").unwrap(), bh).unwrap();

        let payload = crate::audit_payloads::TenantCreatedPayload {
            slug: "acme".to_string(),
            display_name: "ACME".to_string(),
            created_at: "2026-06-16T01:00:00Z".to_string(),
            creator_login: "op".to_string(),
        };
        emit_tenant_event(
            // Foreign-tenant DB (the new tenant's own chain) — no shared
            // Handle owns it, so the sanctioned reopen (as in create).
            &TenantAudit::Reopen,
            &db_new,
            TenantId::new("acme").unwrap(),
            bh,
            "op",
            EventKind::TenantCreated,
            payload.to_bytes(),
        )
        .unwrap();

        let new_kinds: Vec<EventKind> = Ledger::open(&db_new, TenantId::new("acme").unwrap(), bh)
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(
            new_kinds.contains(&EventKind::TenantCreated),
            "new tenant ledger must carry TenantCreated"
        );

        let caller_kinds: Vec<EventKind> =
            Ledger::open(&db_caller, TenantId::new("prod").unwrap(), bh)
                .unwrap()
                .entries()
                .unwrap()
                .into_iter()
                .map(|e| e.kind)
                .collect();
        assert!(
            !caller_kinds.contains(&EventKind::TenantCreated),
            "caller ledger must NOT carry the new tenant's TenantCreated"
        );
    }
}
