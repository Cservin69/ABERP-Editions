//! PR-71 / session-93 — multi-bank-account schema (PR-A of the
//! PR-A/B/C/D initiative pinned in `project_aberp_tenant_management`).
//!
//! The pre-PR-71 `seller.toml` schema carried at most ONE bank block
//! (flat-key form at root, or `[seller.bank]` single section). The
//! reality at Áben Consulting and any tenant doing cross-border
//! invoicing is N banks per currency — typically one or more HUF
//! accounts AND one or more EUR accounts. PR-71 widens the schema to
//! `[[seller.banks]]` (TOML array-of-tables) and adds the typed
//! read-side + validator + helper accessors PR-B (UI), PR-C (issue
//! path), and PR-D (NAV XML + PDF render) will consume.
//!
//! # Scope discipline (PR-71)
//!
//! - **Schema + load-time validator + helpers ONLY.** PR-B owns the
//!   Tenant Settings + SetupWizard UI; PR-C owns picking the right
//!   bank per invoice; PR-D owns the NAV body + PDF render.
//! - The pre-PR-71 `setup_seller_info::SellerBank` /
//!   `parse_seller_bank` / `read_seller_bank` surfaces are
//!   UNCHANGED. They continue to back the SPA's `GET /api/seller-info`
//!   route (PR-53) and the PDF footer render (PR-44ε.1). PR-B swaps
//!   the SPA wire shape to the new array form; PR-D swaps the PDF
//!   renderer's source.
//!
//! # Schema shape
//!
//! ```toml
//! [[seller.banks]]
//! currency       = "HUF"
//! account_number = "12345678-12345678-12345678"
//! bank_name      = "Erste Bank"
//! swift_bic      = "GIBAHUHB"
//! default        = true
//!
//! [[seller.banks]]
//! currency       = "EUR"
//! account_number = "HU12-3456-7890-1234-5678-9012-3456"
//! bank_name      = "Erste Bank"
//! swift_bic      = "GIBAHUHB"
//! default        = true
//! ```
//!
//! # Backwards-compat migration (load-only)
//!
//! Pre-PR-71 files carry either (a) flat root keys
//! (`bank_account_number = "..."`, `bank_name = "..."`,
//! `swift_bic = "..."`) — the shape `samples/seller.toml.example`
//! ships today — or (b) a `[seller.bank]` single section. Both are
//! folded on load into a single-element `[[seller.banks]]` array
//! with `default = true` and a currency inferred from the SWIFT/BIC
//! country-code positions (5-6): `HU` → HUF; anything else → HUF +
//! a structured `tracing::warn!` instructing the operator to open
//! Tenant Settings → Bank accounts and confirm. The migration runs
//! at load only — the file on disk is NOT rewritten. Persisting the
//! migrated form is an operator action via PR-B's UI write path,
//! keeping PR-71 a non-destructive schema lift.
//!
//! # Validator (loud-fail at load)
//!
//! Per ADR-0040: a `seller.toml` whose `[[seller.banks]]` set has
//! ≥1 entry for some currency MUST carry exactly one
//! `default = true` for that currency. Two defaults for HUF, or
//! zero defaults for HUF when HUF entries exist, fail the load
//! with a typed `SellerBanksError`. The "≥1 entry per used
//! currency" constraint is NOT enforced at load (a file with only
//! HUF banks is valid); per-currency presence is enforced at use
//! time by PR-C's issue-path bank picker.
//!
//! # Stable bank IDs
//!
//! Each loaded entry is assigned `bnk_<26-char>` where the 26
//! characters are the Crockford-base32 rendering of the first 16
//! bytes of `SHA-256(currency_iso || ":" || account_number)`. The
//! ID is deterministic across load cycles — restarting the binary
//! produces the same ID for the same (currency, account_number)
//! pair so PR-C can stamp the chosen `bank_account_id` onto the
//! issued invoice and have it survive a re-load without drift.
//!
//! See ADR-0040 (multi-bank-account schema) for the full design
//! rationale.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use aberp_billing::Currency;
use anyhow::{anyhow, Context as _, Result};
use sha2::{Digest, Sha256};
use ulid::Ulid;

/// One bank-account entry per ADR-0040 §1. Every field is required
/// at the typed level (the parser/validator surfaces missing fields
/// as `SellerBanksError::MissingField` loud-fails); downstream
/// consumers (PR-B's settings dropdown, PR-C's issue-path picker,
/// PR-D's NAV body + PDF render) read these as guaranteed-present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SellerBankEntry {
    /// `bnk_<26-char>` — deterministic Crockford-base32 of the first
    /// 16 bytes of `SHA-256(currency_iso || ":" || account_number)`.
    /// PR-C stamps this id onto the issued invoice so future PRs can
    /// resolve back to the entry (rename of `bank_name`, rotation of
    /// `swift_bic`) without invalidating audit-ledger references.
    pub id: String,
    /// Closed-vocab per ADR-0037 §3 (`Huf` / `Eur`). The parser
    /// refuses any ISO code outside the closed vocab as a
    /// `SellerBanksError::UnsupportedCurrency` per ADR-0040 §3
    /// invariant B1.
    pub currency: Currency,
    /// Account number string — IBAN form for EUR (e.g.,
    /// `HU12-3456-7890-1234-5678-9012-3456`), domestic form for HUF
    /// (e.g., `12345678-12345678-12345678`). Stored verbatim; the
    /// PDF render + NAV body emit it unchanged.
    pub account_number: String,
    /// Operator-typed bank name (e.g., `Erste Bank`). Surfaced on
    /// the printed-invoice footer + the SPA's bank-picker dropdown.
    pub bank_name: String,
    /// SWIFT/BIC (8 or 11 chars). Positions 5-6 are the ISO 3166-1
    /// alpha-2 country code (e.g., `GIBAHUHB` → `HU` Hungary). The
    /// SWIFT-based currency inference at migration time reads
    /// positions 5-6; see [`infer_currency_from_swift`].
    pub swift_bic: String,
    /// Default-for-this-currency flag. Per ADR-0040 §2 invariant
    /// A2, exactly one `default = true` is required per currency
    /// that has entries. Multiple defaults or zero defaults among
    /// entries of the same currency fail load.
    pub default: bool,
}

/// Loaded collection per ADR-0040 §1. Wraps the entry vector so
/// the helper accessors (per ADR-0040 §4) live on the type rather
/// than scattering currency-defaulting logic across PR-B/C/D's
/// call sites.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SellerBanks {
    entries: Vec<SellerBankEntry>,
}

impl SellerBanks {
    /// All entries in declaration order. PR-B's settings page reads
    /// this for the list-view ordering.
    pub fn entries(&self) -> &[SellerBankEntry] {
        &self.entries
    }

    /// The marked-default entry for `currency`, or `None` if no
    /// entries exist for that currency. Per ADR-0040 §2 invariant
    /// A2 the validator guarantees at most ONE default per
    /// currency, so this returns the unique default when present
    /// rather than first-of-many. PR-C's issue-path bank picker is
    /// the primary consumer.
    pub fn default_bank_for(&self, currency: Currency) -> Option<&SellerBankEntry> {
        self.entries
            .iter()
            .find(|e| e.currency == currency && e.default)
    }

    /// All entries for `currency` in declaration order. PR-B's
    /// settings dropdown populates from this; PR-C's issue-path
    /// dropdown does too.
    pub fn banks_for_currency(&self, currency: Currency) -> Vec<&SellerBankEntry> {
        self.entries
            .iter()
            .filter(|e| e.currency == currency)
            .collect()
    }

    /// Resolve a stamped `bank_account_id` back to its entry. PR-C
    /// reads the id off the issued-invoice record; PR-D's NAV body
    /// + PDF render walk this to find the entry to emit.
    pub fn bank_by_id(&self, id: &str) -> Option<&SellerBankEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// PR-72 / session-94 — replace this collection's entries with
    /// `entries` and re-run the per-currency-default validator. Mirror
    /// of [`parse_seller_banks`]'s validator step so the in-memory
    /// mutate-then-validate path the write routes use shares one
    /// invariant gate with the read path (no second source of truth
    /// for invariants — CLAUDE.md rule 11).
    pub fn replace_entries(
        &mut self,
        entries: Vec<SellerBankEntry>,
    ) -> std::result::Result<(), SellerBanksError> {
        validate_per_currency_defaults(&entries)?;
        self.entries = entries;
        Ok(())
    }

    /// PR-72 / session-94 — serialise the collection as the
    /// canonical-new-form `[[seller.banks]]` block sequence per
    /// ADR-0040 §1. Used by [`write_seller_banks_section`] to merge
    /// the bank block back into a `seller.toml` body that may also
    /// carry the PR-51 identity sections (`[seller]`,
    /// `[seller.address]`). Entries are emitted in declaration order;
    /// `default = true` is emitted only when `entry.default` is
    /// true (the absent-→-false convention pinned in
    /// [`parse_seller_banks`]).
    pub fn to_toml_section(&self) -> String {
        let mut out = String::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str("[[seller.banks]]\n");
            out.push_str(&format!(
                "currency       = \"{}\"\n",
                entry.currency.iso_code()
            ));
            out.push_str(&format!(
                "account_number = \"{}\"\n",
                escape_toml_string(&entry.account_number)
            ));
            out.push_str(&format!(
                "bank_name      = \"{}\"\n",
                escape_toml_string(&entry.bank_name)
            ));
            out.push_str(&format!(
                "swift_bic      = \"{}\"\n",
                escape_toml_string(&entry.swift_bic)
            ));
            if entry.default {
                out.push_str("default        = true\n");
            }
        }
        out
    }
}

/// PR-72 / session-94 — shared validator extracted out of
/// [`parse_seller_banks`] so the write-side path uses the SAME
/// invariant gate. Two-defaults-per-currency and zero-defaults-among-
/// present-entries are the only invariants enforced (per ADR-0040 §2:
/// the "≥1 entry per used currency" rule lives at the issue surface,
/// not at load/save).
fn validate_per_currency_defaults(
    entries: &[SellerBankEntry],
) -> std::result::Result<(), SellerBanksError> {
    let mut default_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut presence: BTreeMap<&'static str, Currency> = BTreeMap::new();
    for entry in entries {
        let iso = entry.currency.iso_code();
        presence.insert(iso, entry.currency);
        if entry.default {
            *default_counts.entry(iso).or_insert(0) += 1;
        }
    }
    for (iso, currency) in presence {
        let count = default_counts.get(iso).copied().unwrap_or(0);
        if count == 0 {
            return Err(SellerBanksError::NoDefaultAmongEntries { currency });
        }
        if count > 1 {
            return Err(SellerBanksError::MultipleDefaults { currency, count });
        }
    }
    Ok(())
}

/// PR-72 / session-94 — escape a string for TOML double-quoted form.
/// The accepted operator inputs at PR-B time are account numbers,
/// bank names, SWIFT/BICs — none of which carry control characters in
/// practice, but defensively escape `\\` and `"` so a future operator
/// typing a literal quote in the bank name (`Bank "Foo" Kft`) does not
/// produce a malformed file that the parser then rejects.
fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Typed load/parse failures per ADR-0040 §3. Every variant is a
/// loud-fail at the load boundary — no silent fallback, no "best
/// effort" parsing. The CLI / SPA / Tauri boot wrap these into the
/// operator-visible Hungarian + English message pair via [`Self::
/// operator_message`].
#[derive(Debug)]
pub enum SellerBanksError {
    /// Two or more `default = true` entries exist for the same
    /// currency. The operator must pick exactly one default per
    /// currency.
    MultipleDefaults { currency: Currency, count: usize },
    /// One or more entries exist for `currency` but none is marked
    /// `default = true`. Operator must mark exactly one.
    NoDefaultAmongEntries { currency: Currency },
    /// A `[[seller.banks]]` entry omitted a required field. Indexed
    /// by zero-based entry position so the operator can locate the
    /// offender in the file.
    MissingField {
        entry_index: usize,
        field: &'static str,
    },
    /// An entry's `currency` value is not in the ADR-0037 closed
    /// vocab. The operator must use `HUF` or `EUR`.
    UnsupportedCurrency { entry_index: usize, value: String },
    /// Filesystem read error (the path-explicit helper bubbles
    /// `std::io::Error` up via this arm).
    Io(std::io::Error),
}

impl SellerBanksError {
    /// Hungarian + English operator-visible message pair. Mirrors
    /// ADR-0038's preflight-error posture: both languages, names
    /// the file + the field, never English-only.
    pub fn operator_message(&self, path: &Path) -> String {
        let display = path.display();
        match self {
            Self::MultipleDefaults { currency, count } => format!(
                "Több alapértelmezett bankszámla van a {iso} pénznemhez ({count} db). \
                 Pontosan egy `default = true` legyen pénznemenként a `{display}` fájlban.\n\
                 Multiple default bank accounts for {iso} ({count}). \
                 Exactly one `default = true` per currency is required in `{display}`.",
                iso = currency.iso_code(),
            ),
            Self::NoDefaultAmongEntries { currency } => format!(
                "Nincs alapértelmezett bankszámla a {iso} pénznemhez a `{display}` fájlban. \
                 Pontosan egy `default = true` legyen pénznemenként.\n\
                 No default bank account marked for {iso} in `{display}`. \
                 Mark exactly one `[[seller.banks]]` entry as `default = true`.",
                iso = currency.iso_code(),
            ),
            Self::MissingField { entry_index, field } => format!(
                "A {}. `[[seller.banks]]` bejegyzésből hiányzik a kötelező `{field}` mező \
                 a `{display}` fájlban.\n\
                 Bank entry #{} is missing the required `{field}` field in `{display}`.",
                entry_index + 1,
                entry_index + 1,
            ),
            Self::UnsupportedCurrency { entry_index, value } => format!(
                "A {}. `[[seller.banks]]` bejegyzés `currency = \"{value}\"` értéke nem támogatott. \
                 Engedélyezett: HUF, EUR.\n\
                 Bank entry #{} has unsupported `currency = \"{value}\"`. \
                 Allowed values: HUF, EUR.",
                entry_index + 1,
                entry_index + 1,
            ),
            Self::Io(e) => format!(
                "A `{display}` fájl nem olvasható: {e}.\nFailed to read `{display}`: {e}.",
            ),
        }
    }
}

impl std::fmt::Display for SellerBanksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MultipleDefaults { currency, count } => write!(
                f,
                "multiple default bank accounts for {} ({count} found)",
                currency.iso_code()
            ),
            Self::NoDefaultAmongEntries { currency } => {
                write!(
                    f,
                    "no default bank account marked for {}",
                    currency.iso_code()
                )
            }
            Self::MissingField { entry_index, field } => write!(
                f,
                "[[seller.banks]] entry #{} missing required field `{field}`",
                entry_index + 1
            ),
            Self::UnsupportedCurrency { entry_index, value } => write!(
                f,
                "[[seller.banks]] entry #{} has unsupported currency `{value}`",
                entry_index + 1
            ),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for SellerBanksError {}

/// Read + parse + validate the bank-account section of
/// `~/.aberp/<tenant>/seller.toml`. Returns an empty
/// [`SellerBanks`] when the file is absent (the SPA boot detects
/// the absence and routes the operator to the wizard). Returns
/// `Err` on parse/validate failure — the caller bubbles the
/// typed error to the operator via [`SellerBanksError::
/// operator_message`].
///
/// The legacy fold-on-load behaviour (flat-root keys or
/// `[seller.bank]` single section → single-element
/// `[[seller.banks]]` array with `default = true` and
/// SWIFT-inferred currency) is applied transparently here. The
/// caller never sees the legacy shape after the call returns.
pub fn read_seller_banks(path: &Path) -> Result<SellerBanks, SellerBanksError> {
    if !path.exists() {
        return Ok(SellerBanks::default());
    }
    let body = fs::read_to_string(path).map_err(SellerBanksError::Io)?;
    parse_seller_banks(&body)
}

/// Parse + validate a `seller.toml` body string. Exposed as `pub`
/// for unit tests + the integration test in
/// `tests/seller_banks_round_trip.rs`. Accepts three forms:
///
/// 1. **New canonical form** — one or more `[[seller.banks]]`
///    array-of-tables entries with `currency` / `account_number` /
///    `bank_name` / `swift_bic` / `default` fields.
/// 2. **Legacy single section** — `[seller.bank]` heading followed
///    by `account_number = "..."` / `bank_name = "..."` /
///    `swift_bic = "..."`. Folded to a one-element array with
///    `default = true` and SWIFT-inferred currency.
/// 3. **Legacy flat-root form** — `bank_account_number = "..."` /
///    `bank_name = "..."` / `swift_bic = "..."` at root with no
///    section header. Folded identically to (2).
///
/// Empty bodies (or bodies containing no recognised bank keys)
/// return an empty `SellerBanks` — equivalent to "no bank accounts
/// configured yet". Per ADR-0040 the empty case is valid at load
/// time; PR-C's issue-path picker is the surface that loud-fails
/// when no entry exists for the invoice's currency.
pub fn parse_seller_banks(body: &str) -> Result<SellerBanks, SellerBanksError> {
    let raw_entries = collect_raw_entries(body);

    // Empty body / no recognised bank keys → empty collection.
    if raw_entries.is_empty() {
        return Ok(SellerBanks::default());
    }

    // Build typed entries.
    let mut entries: Vec<SellerBankEntry> = Vec::with_capacity(raw_entries.len());
    for (index, raw) in raw_entries.into_iter().enumerate() {
        entries.push(raw.into_entry(index)?);
    }

    // Per-currency-default invariants. Shared with the write path
    // (PR-72) — both surfaces flow through one validator so a future
    // tightening of the rule lives in one place.
    validate_per_currency_defaults(&entries)?;

    Ok(SellerBanks { entries })
}

/// Internal builder collected during the line walk — every field
/// is `Option<_>` so the legacy-vs-new-vs-missing distinction
/// surfaces at `into_entry` time as a typed `MissingField` error
/// keyed by the entry's zero-based position.
#[derive(Debug, Default)]
struct RawEntry {
    currency: Option<String>,
    account_number: Option<String>,
    bank_name: Option<String>,
    swift_bic: Option<String>,
    default_flag: Option<bool>,
    /// True iff this entry came from a legacy form (flat root
    /// keys OR `[seller.bank]` section). Drives the
    /// SWIFT-inference + always-default-true migration step.
    legacy: bool,
}

impl RawEntry {
    /// True iff at least one bank-related key was set on this
    /// builder. Empty builders (e.g., a `[seller]` identity-only
    /// file in legacy form) drop on the floor rather than become
    /// a phantom entry.
    fn has_any_bank_field(&self) -> bool {
        self.currency.is_some()
            || self.account_number.is_some()
            || self.bank_name.is_some()
            || self.swift_bic.is_some()
    }

    fn into_entry(self, index: usize) -> Result<SellerBankEntry, SellerBanksError> {
        let account_number = self.account_number.ok_or(SellerBanksError::MissingField {
            entry_index: index,
            field: "account_number",
        })?;
        let bank_name = self.bank_name.ok_or(SellerBanksError::MissingField {
            entry_index: index,
            field: "bank_name",
        })?;
        let swift_bic = self.swift_bic.ok_or(SellerBanksError::MissingField {
            entry_index: index,
            field: "swift_bic",
        })?;

        // Currency: explicit value when present; SWIFT inference
        // + a structured warn for legacy entries when absent. New-
        // form entries (`[[seller.banks]]`) without a currency are
        // a MissingField loud-fail — the new schema makes it
        // required.
        let currency = match self.currency {
            Some(raw) => parse_currency(&raw).ok_or(SellerBanksError::UnsupportedCurrency {
                entry_index: index,
                value: raw,
            })?,
            None if self.legacy => {
                let inferred = infer_currency_from_swift_with_flag(&swift_bic);
                if inferred.fell_back {
                    tracing::warn!(
                        swift_bic = %swift_bic,
                        "seller.toml legacy bank entry has no `currency` and the SWIFT/BIC \
                         country code is not `HU`; defaulted to HUF. Open Tenant Settings → \
                         Bank accounts and confirm the currency."
                    );
                } else {
                    tracing::warn!(
                        swift_bic = %swift_bic,
                        "seller.toml legacy bank entry has no `currency`; inferred `HUF` \
                         from SWIFT/BIC country code `HU`. Persist the new \
                         `[[seller.banks]]` form via Tenant Settings to silence this warning."
                    );
                }
                inferred.currency
            }
            None => {
                return Err(SellerBanksError::MissingField {
                    entry_index: index,
                    field: "currency",
                })
            }
        };

        // Legacy entries always default to `default = true` because
        // there is exactly one of them. New-form entries inherit
        // the explicit flag (absent → false).
        let default = if self.legacy {
            true
        } else {
            self.default_flag.unwrap_or(false)
        };

        let id = deterministic_id(currency, &account_number);
        Ok(SellerBankEntry {
            id,
            currency,
            account_number,
            bank_name,
            swift_bic,
            default,
        })
    }
}

/// Outcome of the SWIFT-based currency inference. Carries the
/// inferred `Currency` AND a `fell_back` flag so the caller can
/// emit a strong-vs-mild warn message.
struct InferredCurrency {
    currency: Currency,
    /// True when the SWIFT/BIC country code positions (5-6) were
    /// NOT `HU` — the inference is therefore a fallback to HUF
    /// rather than a confident match.
    fell_back: bool,
}

/// Currency inference per ADR-0040 §2 migration rule: a SWIFT/BIC
/// with country-code positions (5-6) equal to `HU` is a Hungarian
/// bank → infer HUF; anything else → default to HUF and signal the
/// fallback so the caller can emit a louder warn directing the
/// operator to confirm in the UI.
///
/// Public for the integration test in
/// `tests/seller_banks_round_trip.rs`.
pub fn infer_currency_from_swift(swift_bic: &str) -> Currency {
    infer_currency_from_swift_with_flag(swift_bic).currency
}

fn infer_currency_from_swift_with_flag(swift_bic: &str) -> InferredCurrency {
    let trimmed = swift_bic.trim();
    // SWIFT/BIC layout: bank(4) + country(2) + location(2) + optional branch(3).
    // Country code lives at byte positions 4..6 (zero-indexed); guard against
    // shorter strings so we never panic on bad input.
    if trimmed.len() >= 6 && trimmed.is_ascii() {
        let country = &trimmed[4..6];
        if country.eq_ignore_ascii_case("HU") {
            return InferredCurrency {
                currency: Currency::Huf,
                fell_back: false,
            };
        }
    }
    InferredCurrency {
        currency: Currency::Huf,
        fell_back: true,
    }
}

/// Parse a wire-form currency string (`"HUF"` / `"EUR"`,
/// case-insensitive) into the typed `Currency`. Returns `None`
/// for anything outside the ADR-0037 closed vocab so the caller
/// can surface a typed `UnsupportedCurrency` error.
fn parse_currency(raw: &str) -> Option<Currency> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "HUF" => Some(Currency::Huf),
        "EUR" => Some(Currency::Eur),
        _ => None,
    }
}

/// Deterministic `bnk_<26-char>` id per ADR-0040 §1: the
/// Crockford-base32 rendering of the first 16 bytes of
/// `SHA-256(currency_iso || ":" || account_number)`.
///
/// Public for the integration test in
/// `tests/seller_banks_round_trip.rs` that pins
/// load-cycle determinism.
pub fn deterministic_id(currency: Currency, account_number: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(currency.iso_code().as_bytes());
    hasher.update(b":");
    hasher.update(account_number.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let ulid = Ulid::from_bytes(bytes);
    format!("bnk_{ulid}")
}

/// Line-oriented section walker that collects raw entries from
/// any of the three accepted shapes (new `[[seller.banks]]`,
/// legacy `[seller.bank]`, legacy flat-root). The validator + the
/// typed `into_entry` step run afterwards on the resulting
/// `Vec<RawEntry>`.
///
/// The walker is deliberately custom-built (mirroring the
/// existing `setup_seller_info::parse_seller_bank` /
/// `parse_seller_identity` posture) rather than reaching for the
/// `toml` crate. The accepted grammar is constrained enough that
/// a 70-line walker is more readable than a `toml::Value` lookup
/// path and avoids dragging in a new direct dependency for what
/// is read-only at PR-71 (the write path lands with PR-B).
fn collect_raw_entries(body: &str) -> Vec<RawEntry> {
    let mut entries: Vec<RawEntry> = Vec::new();
    let mut current: Option<RawEntry> = None;
    let mut root_legacy = RawEntry {
        legacy: true,
        ..RawEntry::default()
    };
    // Tracks "we're inside a `[<something else>]` section like
    // `[seller]` or `[seller.address]`" so identity-block lines
    // never accidentally populate a phantom bank entry.
    let mut in_other_section = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Array-of-tables header: `[[seller.banks]]` opens a new
        // bank entry; any other `[[...]]` array-of-tables header
        // closes any pending entry and is treated as foreign.
        if line.starts_with("[[") && line.ends_with("]]") {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            let inner = line[2..line.len() - 2].trim();
            if inner == "seller.banks" {
                current = Some(RawEntry::default());
                in_other_section = false;
            } else {
                in_other_section = true;
            }
            continue;
        }

        // Single-section header.
        if line.starts_with('[') && line.ends_with(']') {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            let inner = line[1..line.len() - 1].trim();
            if inner == "seller.bank" {
                // Legacy single-block form — one entry, marked legacy.
                current = Some(RawEntry {
                    legacy: true,
                    ..RawEntry::default()
                });
                in_other_section = false;
            } else {
                in_other_section = true;
            }
            continue;
        }

        // `key = value` line.
        if in_other_section {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        let key = k.trim();
        let raw_value = v.trim();
        let target = match current.as_mut() {
            Some(e) => e,
            None => &mut root_legacy,
        };
        apply_kv(target, key, raw_value);
    }

    if let Some(e) = current.take() {
        entries.push(e);
    }
    // Legacy flat-root fold: if we collected no section-bounded
    // entries but the root accumulator picked up bank keys, promote
    // it to a single legacy entry.
    if entries.is_empty() && root_legacy.has_any_bank_field() {
        entries.push(root_legacy);
    }
    // Drop any phantom empty `[[seller.banks]]` entries (e.g., the
    // operator left an empty stub at the bottom of the file). PR-C's
    // picker would otherwise see them as missing-field loud-fails;
    // dropping silently is safer.
    entries.retain(RawEntry::has_any_bank_field);
    entries
}

fn apply_kv(target: &mut RawEntry, key: &str, raw_value: &str) {
    let unquoted = strip_quotes(raw_value);
    match key {
        "currency" => target.currency = Some(unquoted.to_string()),
        // `account_number` is the new canonical key;
        // `bank_account_number` is the legacy flat-root spelling
        // that the existing `samples/seller.toml.example` + PDF
        // renderer consume.
        "account_number" | "bank_account_number" => {
            target.account_number = Some(unquoted.to_string())
        }
        "bank_name" => target.bank_name = Some(unquoted.to_string()),
        "swift_bic" => target.swift_bic = Some(unquoted.to_string()),
        "default" => {
            // TOML boolean literal: bare `true` / `false` (no quotes).
            // Tolerate quoted forms too because the operator may have
            // hand-edited the file.
            let v = unquoted.to_ascii_lowercase();
            target.default_flag = Some(matches!(v.as_str(), "true"));
        }
        _ => {}
    }
}

fn strip_quotes(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// PR-72 / session-94 — mint a `SellerBankEntry` from the canonical
/// field set, deriving the deterministic `bnk_<26-char>` id over
/// `(currency, account_number)`. Used by the write-path routes when
/// the operator submits a new bank entry (the id is not operator-
/// typed — it's derived so re-typing the same `(currency,
/// account_number)` re-uses the existing id, which lets PR-C's stamped
/// references survive a delete + re-add round-trip).
pub fn mint_entry(
    currency: Currency,
    account_number: String,
    bank_name: String,
    swift_bic: String,
    default: bool,
) -> SellerBankEntry {
    let id = deterministic_id(currency, &account_number);
    SellerBankEntry {
        id,
        currency,
        account_number,
        bank_name,
        swift_bic,
        default,
    }
}

/// PR-72 / session-94 — atomically replace `path`'s
/// `[[seller.banks]]` block (and only that block) with the
/// canonical-new-form serialisation of `banks`. The PR-51 identity
/// block (`[seller]`, `[seller.address]` headings + the comment
/// preamble) is preserved verbatim so the write path is non-
/// destructive across the file's other sections.
///
/// Behaviour:
///   - If the file does NOT exist, the new file is written with the
///     bank block only (the operator can fold an identity block in
///     later via [`crate::setup_seller_info::setup_seller_info_to_path`]).
///   - If the file exists and carries identity sections OR a legacy
///     bank block, the identity sections + any non-bank prefix lines
///     are preserved, and the bank block (legacy flat-root keys,
///     `[seller.bank]` section, OR existing `[[seller.banks]]`
///     array-of-tables entries) is REPLACED by `banks.to_toml_section()`.
///   - The on-disk replace is POSIX-atomic (tempfile in the same dir,
///     `fsync` + `rename`) with `0600` permissions — mirrors
///     [`crate::setup_seller_info::setup_seller_info_to_path`]'s
///     posture so the operator sees consistent disk hygiene across
///     the two surfaces.
///
/// `banks` is validated pre-write (per-currency-default invariants);
/// the route layer should validate FIRST so the typed
/// `SellerBanksError` reaches the operator before this is called, but
/// the redundant gate here keeps a non-route caller (a future CLI
/// command, for example) safe.
pub fn write_seller_banks_section(path: &Path, banks: &SellerBanks) -> Result<()> {
    validate_per_currency_defaults(banks.entries())
        .map_err(|e| anyhow!("bank invariants violated pre-write: {e}"))?;

    let new_section = banks.to_toml_section();

    // Build the post-write body: identity sections + comments + any
    // non-bank prefix preserved verbatim, bank block replaced.
    let body = if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("read existing seller.toml at {}", path.display()))?;
        merge_bank_section(&existing, &new_section)
    } else {
        new_section
    };

    write_atomic(path, body.as_bytes())
}

/// PR-72 / session-94 — replace the bank section of an existing
/// `seller.toml` body. Walks the lines and partitions them into:
///   - **identity_prefix**: comments + non-bank `[seller]` /
///     `[seller.address]` / other foreign sections.
///   - **bank lines** (DROPPED): the existing `[[seller.banks]]`
///     entries, the legacy `[seller.bank]` section, AND the legacy
///     flat-root bank keys at file root (`bank_account_number`,
///     `iban`, `bank_name`, `swift_bic`).
///
/// The replacement section is appended to the identity prefix with
/// exactly one blank-line separator when the prefix is non-empty and
/// the new section is non-empty.
fn merge_bank_section(existing: &str, new_section: &str) -> String {
    let mut prefix = String::new();
    let mut in_bank_section = false;
    let mut at_root = true;
    for raw_line in existing.lines() {
        let trimmed = raw_line.trim();

        // Section header line.
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            let inner = trimmed[2..trimmed.len() - 2].trim();
            in_bank_section = inner == "seller.banks";
            at_root = false;
            if in_bank_section {
                // Drop this header and the entry that follows.
                continue;
            }
            push_with_newline(&mut prefix, raw_line);
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            in_bank_section = inner == "seller.bank";
            at_root = false;
            if in_bank_section {
                continue;
            }
            push_with_newline(&mut prefix, raw_line);
            continue;
        }

        // Inside a dropped bank section → discard.
        if in_bank_section {
            continue;
        }

        // Legacy flat-root bank keys at file root → discard.
        if at_root && is_legacy_root_bank_line(trimmed) {
            continue;
        }

        push_with_newline(&mut prefix, raw_line);
    }

    // Trim trailing blank lines from the preserved prefix so the
    // separator below is deterministic.
    while prefix.ends_with("\n\n") {
        prefix.pop();
    }

    if prefix.is_empty() {
        return new_section.to_string();
    }
    if new_section.is_empty() {
        return prefix;
    }
    if !prefix.ends_with('\n') {
        prefix.push('\n');
    }
    prefix.push('\n');
    prefix.push_str(new_section);
    prefix
}

fn push_with_newline(target: &mut String, raw_line: &str) {
    target.push_str(raw_line);
    target.push('\n');
}

fn is_legacy_root_bank_line(trimmed: &str) -> bool {
    let key = match trimmed.split_once('=') {
        Some((k, _)) => k.trim(),
        None => return false,
    };
    matches!(
        key,
        "bank_account_number" | "iban" | "bank_name" | "swift_bic"
    )
}

/// PR-72 / session-94 — POSIX-atomic write helper, mirror of the
/// `setup_seller_info::write_atomic` pattern (same dir tempfile, fsync,
/// rename, 0600 perms, 0700 parent dir). Kept as a local copy rather
/// than re-exported to avoid widening the
/// `setup_seller_info` surface for a single new caller. The CLAUDE.md
/// rule 2 (minimum code, no speculative abstractions) trade vs. rule
/// 8 (read before write) lands here on the side of a 40-line local
/// helper — extracting a shared `atomic_write_toml` would force a new
/// public surface on both modules and the test boundary for one
/// additional call site.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("seller.toml path `{}` has no parent dir", path.display()))?;
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(parent)
                .with_context(|| format!("stat {}", parent.display()))?
                .permissions();
            perms.set_mode(0o700);
            fs::set_permissions(parent, perms)
                .with_context(|| format!("chmod 0700 {}", parent.display()))?;
        }
    }

    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".seller.toml.banks.tmp.{}-{}-{}",
        std::process::id(),
        nanos,
        seq,
    );
    let tmp_path = parent.join(tmp_name);
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
        let mut perms = fs::metadata(&tmp_path)
            .with_context(|| format!("stat {}", tmp_path.display()))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp_path, perms)
            .with_context(|| format!("chmod 0600 {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "rename tempfile {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_huf_one_eur() -> &'static str {
        "\
[[seller.banks]]
currency       = \"HUF\"
account_number = \"12345678-12345678-12345678\"
bank_name      = \"Erste Bank\"
swift_bic      = \"GIBAHUHB\"
default        = true

[[seller.banks]]
currency       = \"EUR\"
account_number = \"HU12-3456-7890-1234-5678-9012-3456\"
bank_name      = \"Erste Bank\"
swift_bic      = \"GIBAHUHB\"
default        = true
"
    }

    /// Happy path: a clean new-form file with one HUF + one EUR
    /// entry parses into a two-element collection with the
    /// per-currency-default invariant satisfied.
    #[test]
    fn parses_canonical_new_form_with_one_per_currency() {
        let banks = parse_seller_banks(one_huf_one_eur()).expect("parses");
        assert_eq!(banks.entries().len(), 2);
        let huf = banks.default_bank_for(Currency::Huf).expect("HUF default");
        assert_eq!(huf.account_number, "12345678-12345678-12345678");
        assert!(huf.default);
        let eur = banks.default_bank_for(Currency::Eur).expect("EUR default");
        assert_eq!(eur.account_number, "HU12-3456-7890-1234-5678-9012-3456");
        assert!(eur.default);
    }

    /// Per-currency-default invariant: two HUF defaults must
    /// loud-fail (this is the regression the validator exists to
    /// prevent — silent "first wins" would surface the wrong
    /// account on the printed invoice).
    #[test]
    fn rejects_multiple_defaults_for_same_currency() {
        let body = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"A\"
bank_name = \"Bank One\"
swift_bic = \"GIBAHUHB\"
default = true

[[seller.banks]]
currency = \"HUF\"
account_number = \"B\"
bank_name = \"Bank Two\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let err = parse_seller_banks(body).expect_err("two defaults must fail");
        match err {
            SellerBanksError::MultipleDefaults { currency, count } => {
                assert_eq!(currency, Currency::Huf);
                assert_eq!(count, 2);
            }
            other => panic!("expected MultipleDefaults, got {other:?}"),
        }
    }

    /// Per-currency-default invariant flipside: zero defaults
    /// among entries of the same currency must loud-fail. The
    /// operator must mark one — the loader will not silently
    /// pick.
    #[test]
    fn rejects_zero_defaults_among_entries_of_same_currency() {
        let body = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"A\"
bank_name = \"Bank One\"
swift_bic = \"GIBAHUHB\"
default = false

[[seller.banks]]
currency = \"HUF\"
account_number = \"B\"
bank_name = \"Bank Two\"
swift_bic = \"GIBAHUHB\"
default = false
";
        let err = parse_seller_banks(body).expect_err("zero defaults must fail");
        match err {
            SellerBanksError::NoDefaultAmongEntries { currency } => {
                assert_eq!(currency, Currency::Huf);
            }
            other => panic!("expected NoDefaultAmongEntries, got {other:?}"),
        }
    }

    /// Legacy single-section `[seller.bank]` form migrates to a
    /// one-element collection marked `default = true` with a
    /// SWIFT-inferred currency.
    #[test]
    fn migrates_legacy_seller_bank_single_section() {
        let body = "\
[seller.bank]
account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
";
        let banks = parse_seller_banks(body).expect("legacy parses");
        assert_eq!(banks.entries().len(), 1);
        let entry = &banks.entries()[0];
        assert_eq!(entry.currency, Currency::Huf);
        assert!(entry.default);
        assert_eq!(entry.bank_name, "Erste Bank");
    }

    /// Legacy flat-root form (the shape `samples/seller.toml.example`
    /// ships today + `setup_seller_info::parse_seller_bank` reads):
    /// `bank_account_number = "..."`, `bank_name = "..."`,
    /// `swift_bic = "..."` at root with no section header.
    /// Migrates identically to the `[seller.bank]` case.
    #[test]
    fn migrates_legacy_flat_root_form() {
        let body = "\
bank_account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
";
        let banks = parse_seller_banks(body).expect("legacy flat-root parses");
        assert_eq!(banks.entries().len(), 1);
        let entry = &banks.entries()[0];
        assert_eq!(entry.currency, Currency::Huf);
        assert!(entry.default);
        assert_eq!(entry.account_number, "12345678-12345678-12345678");
    }

    /// SWIFT inference: `GIBAHUHB` country code positions (5-6) =
    /// `HU` → HUF. Pinned because the migration's correctness
    /// hinges on positions 5-6 being read, not positions 0-1 or
    /// 1-2.
    #[test]
    fn swift_inference_country_hu_returns_huf() {
        assert_eq!(infer_currency_from_swift("GIBAHUHB"), Currency::Huf);
        assert_eq!(infer_currency_from_swift("OTPVHUHB"), Currency::Huf);
        // Trailing branch (11-char form) still reads positions 4-5.
        assert_eq!(infer_currency_from_swift("GIBAHUHBXXX"), Currency::Huf);
    }

    /// SWIFT inference fallback: a non-HU country code resolves
    /// to HUF (the conservative pick) and triggers the louder
    /// warn message the operator sees in the launcher logs.
    #[test]
    fn swift_inference_non_hu_falls_back_to_huf() {
        // `DEUTDEFF` is Deutsche Bank Frankfurt — country code `DE`.
        // The brief's fallback rule defaults to HUF + warn so the
        // legacy migration never silently produces an `Err`.
        assert_eq!(infer_currency_from_swift("DEUTDEFF"), Currency::Huf);
        let flagged = infer_currency_from_swift_with_flag("DEUTDEFF");
        assert!(
            flagged.fell_back,
            "non-HU SWIFT must signal the fell_back flag so the warn message escalates",
        );
    }

    /// `banks_for_currency` returns entries in declaration order
    /// (PR-B's settings dropdown needs predictable ordering;
    /// shuffling would surface as a UI churn regression).
    #[test]
    fn banks_for_currency_preserves_declaration_order() {
        let body = "\
[[seller.banks]]
currency = \"EUR\"
account_number = \"EUR-1\"
bank_name = \"Bank EUR 1\"
swift_bic = \"GIBAHUHB\"
default = true

[[seller.banks]]
currency = \"HUF\"
account_number = \"HUF-1\"
bank_name = \"Bank HUF 1\"
swift_bic = \"GIBAHUHB\"
default = false

[[seller.banks]]
currency = \"HUF\"
account_number = \"HUF-2\"
bank_name = \"Bank HUF 2\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let banks = parse_seller_banks(body).expect("parses");
        let hufs = banks.banks_for_currency(Currency::Huf);
        assert_eq!(hufs.len(), 2);
        assert_eq!(hufs[0].account_number, "HUF-1");
        assert_eq!(hufs[1].account_number, "HUF-2");
        let eurs = banks.banks_for_currency(Currency::Eur);
        assert_eq!(eurs.len(), 1);
        assert_eq!(eurs[0].account_number, "EUR-1");
        // `default_bank_for(EUR)` returns the marked-default entry.
        let eur_default = banks.default_bank_for(Currency::Eur).expect("EUR default");
        assert_eq!(eur_default.account_number, "EUR-1");
        // `default_bank_for(HUF)` returns HUF-2 (the marked default)
        // — NOT HUF-1, even though HUF-1 is first in declaration order.
        let huf_default = banks.default_bank_for(Currency::Huf).expect("HUF default");
        assert_eq!(huf_default.account_number, "HUF-2");
    }

    /// `default_bank_for(EUR)` returns None when no EUR entries
    /// exist. PR-C's issue-path picker turns this into the
    /// loud-fail "no bank account for invoice currency".
    #[test]
    fn default_bank_for_returns_none_when_no_entries_for_currency() {
        let body = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"HUF-1\"
bank_name = \"Bank HUF 1\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let banks = parse_seller_banks(body).expect("parses");
        assert!(banks.default_bank_for(Currency::Eur).is_none());
    }

    /// Determinism pin: the bank id is stable across load cycles
    /// for the same (currency, account_number) pair. This is the
    /// load-bearing invariant PR-C relies on when stamping the id
    /// onto the issued invoice — a non-deterministic id would
    /// invalidate every stamped reference on a restart.
    #[test]
    fn bank_id_is_deterministic_across_load_cycles() {
        let id1 = deterministic_id(Currency::Huf, "12345678-12345678-12345678");
        let id2 = deterministic_id(Currency::Huf, "12345678-12345678-12345678");
        assert_eq!(id1, id2, "ids must be stable across load cycles");
        assert!(id1.starts_with("bnk_"), "ids must use the `bnk_` prefix");
        // The 26-char ULID body keeps the total length at 30
        // (`bnk_` + 26).
        assert_eq!(id1.len(), 30);
        // A different account number produces a different id.
        let id3 = deterministic_id(Currency::Huf, "99999999-99999999-99999999");
        assert_ne!(id1, id3);
        // A different currency for the same account produces a
        // different id — this matters because EUR + HUF accounts
        // with the same domestic-form number must not collide.
        let id4 = deterministic_id(Currency::Eur, "12345678-12345678-12345678");
        assert_ne!(id1, id4);
    }

    /// Unsupported currency value (anything outside the
    /// ADR-0037 closed vocab) must loud-fail at load. Closes the
    /// boundary so a future operator typo (`currency = "USD"`)
    /// surfaces immediately instead of falling through to a
    /// later runtime panic.
    #[test]
    fn rejects_currency_outside_closed_vocab() {
        let body = "\
[[seller.banks]]
currency = \"USD\"
account_number = \"USD-1\"
bank_name = \"Some Bank\"
swift_bic = \"DEUTUS33\"
default = true
";
        let err = parse_seller_banks(body).expect_err("USD must fail closed-vocab");
        match err {
            SellerBanksError::UnsupportedCurrency { entry_index, value } => {
                assert_eq!(entry_index, 0);
                assert_eq!(value, "USD");
            }
            other => panic!("expected UnsupportedCurrency, got {other:?}"),
        }
    }

    /// A `[[seller.banks]]` entry that omits a required field
    /// (here: `bank_name`) is a loud-fail with the entry index
    /// in the message. PR-B's settings page surfaces this as a
    /// per-row inline error after the wizard re-validates on
    /// save.
    #[test]
    fn rejects_missing_required_field_in_new_form() {
        let body = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"HUF-1\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let err = parse_seller_banks(body).expect_err("missing bank_name must fail");
        match err {
            SellerBanksError::MissingField { entry_index, field } => {
                assert_eq!(entry_index, 0);
                assert_eq!(field, "bank_name");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    /// Empty body returns an empty collection (no entries; no
    /// load-time error). The "missing bank account" case is
    /// PR-C's responsibility — at PR-71 the loader simply says
    /// "no banks configured yet".
    #[test]
    fn empty_body_returns_empty_collection() {
        let banks = parse_seller_banks("").expect("parses");
        assert!(banks.entries().is_empty());
        assert!(banks.default_bank_for(Currency::Huf).is_none());
    }

    /// Backwards-compat round-trip: a legacy flat-root file
    /// loads into the same `SellerBanks` shape as the equivalent
    /// new-form file. Serialising the legacy load back through
    /// the new form (via the canonical TOML body we'd write at
    /// PR-B time) re-parses into a state that's identical
    /// modulo the migration-time default flag.
    #[test]
    fn legacy_load_matches_new_form_load() {
        let legacy = "\
bank_account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
";
        let new_form = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let from_legacy = parse_seller_banks(legacy).expect("legacy parses");
        let from_new = parse_seller_banks(new_form).expect("new-form parses");
        assert_eq!(
            from_legacy, from_new,
            "legacy migration must produce the same SellerBanks as the equivalent new-form file"
        );
    }

    /// `bank_by_id` resolves a stamped id back to its entry. PR-C
    /// stamps the id onto the issued-invoice record; this is the
    /// reverse lookup PR-D's NAV body + PDF render walk.
    #[test]
    fn bank_by_id_resolves_loaded_entry() {
        let banks = parse_seller_banks(one_huf_one_eur()).expect("parses");
        let huf = banks.default_bank_for(Currency::Huf).expect("HUF default");
        let resolved = banks.bank_by_id(&huf.id).expect("id resolves");
        assert_eq!(resolved.account_number, huf.account_number);
        assert!(banks.bank_by_id("bnk_does-not-exist").is_none());
    }

    /// Empty body returns an empty collection even with whitespace,
    /// comments, and unrelated identity sections (`[seller]` /
    /// `[seller.address]`). The bank-section walker MUST NOT
    /// accidentally pull identity fields into a phantom entry.
    #[test]
    fn identity_sections_do_not_create_phantom_bank_entries() {
        let body = "\
# top of file
[seller]
legal_name = \"Áben Consulting KFT.\"
tax_number = \"24904362-2-41\"

[seller.address]
country_code = \"HU\"
postal_code = \"1037\"
city = \"Budapest\"
street = \"Visszatérő köz 6\"
";
        let banks = parse_seller_banks(body).expect("parses");
        assert!(
            banks.entries().is_empty(),
            "identity-only file must produce zero bank entries, got {:?}",
            banks.entries(),
        );
    }

    /// PR-72 / session-94 — `to_toml_section` round-trips through
    /// [`parse_seller_banks`] back to the same `SellerBanks` value.
    /// Anchors the contract that the serialiser and the parser agree
    /// on the canonical-new-form shape — a divergence here would let
    /// a write-then-read cycle quietly mutate the operator's bank
    /// data.
    #[test]
    fn to_toml_section_round_trips_through_parser() {
        let banks = parse_seller_banks(one_huf_one_eur()).expect("parses");
        let body = banks.to_toml_section();
        let back = parse_seller_banks(&body).expect("re-parses");
        assert_eq!(banks, back, "round-trip must preserve every entry");
    }

    /// PR-72 / session-94 — the serialiser elides `default = true`
    /// when the entry is not marked default, and emits it when it is.
    /// Together with the validator's "exactly-one-per-currency"
    /// invariant this means a round-trip of a multi-bank-per-currency
    /// collection re-establishes the single default on re-parse.
    #[test]
    fn to_toml_section_emits_default_only_when_true() {
        let body = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"HUF-1\"
bank_name = \"Bank One\"
swift_bic = \"GIBAHUHB\"
default = false

[[seller.banks]]
currency = \"HUF\"
account_number = \"HUF-2\"
bank_name = \"Bank Two\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let banks = parse_seller_banks(body).expect("parses");
        let out = banks.to_toml_section();
        // HUF-1's section must NOT carry a `default` line; HUF-2's
        // section must carry `default        = true`.
        let huf1_idx = out.find("HUF-1").expect("HUF-1 emitted");
        let huf2_idx = out.find("HUF-2").expect("HUF-2 emitted");
        let huf1_block = &out[huf1_idx..huf2_idx];
        assert!(
            !huf1_block.contains("default"),
            "HUF-1 (non-default) must NOT emit a `default` line, got: {huf1_block}"
        );
        let huf2_block = &out[huf2_idx..];
        assert!(
            huf2_block.contains("default        = true"),
            "HUF-2 (marked default) must emit `default = true`, got: {huf2_block}"
        );
    }

    /// PR-72 / session-94 — the file-merge helper preserves the
    /// PR-51 identity block (`[seller]`, `[seller.address]` + the
    /// comment preamble) and replaces ONLY the bank block. Critical
    /// non-destructive-write pin: the operator's identity must survive
    /// every Tenant Settings → Bank accounts mutation.
    #[test]
    fn merge_bank_section_preserves_identity_block() {
        let existing = "\
# ABERP seller config\n\
[seller]\n\
legal_name = \"Áben Consulting KFT.\"\n\
tax_number = \"24904362-2-41\"\n\
\n\
[seller.address]\n\
country_code = \"HU\"\n\
postal_code = \"1037\"\n\
city = \"Budapest\"\n\
street = \"Visszatérő köz 6\"\n\
\n\
[[seller.banks]]\n\
currency = \"HUF\"\n\
account_number = \"OLD\"\n\
bank_name = \"Old Bank\"\n\
swift_bic = \"GIBAHUHB\"\n\
default = true\n";

        let new_section = "\
[[seller.banks]]\n\
currency       = \"HUF\"\n\
account_number = \"NEW\"\n\
bank_name      = \"New Bank\"\n\
swift_bic      = \"GIBAHUHB\"\n\
default        = true\n";

        let merged = merge_bank_section(existing, new_section);
        assert!(
            merged.contains("legal_name = \"Áben Consulting KFT.\""),
            "identity preserved: {merged}"
        );
        assert!(
            merged.contains("[seller.address]"),
            "address heading preserved: {merged}"
        );
        assert!(
            merged.contains("city = \"Budapest\""),
            "address fields preserved: {merged}"
        );
        assert!(
            !merged.contains("\"OLD\""),
            "old bank entry must be dropped: {merged}"
        );
        assert!(
            merged.contains("\"NEW\""),
            "new bank entry must be present: {merged}"
        );
        assert!(
            merged.contains("# ABERP seller config"),
            "comment preamble preserved: {merged}"
        );
    }

    /// PR-72 / session-94 — the file-merge helper also drops the
    /// legacy flat-root bank keys (`bank_account_number`, `iban`,
    /// `bank_name`, `swift_bic` at file root with no section header)
    /// so a write-back on a legacy file produces the canonical new-
    /// form bank block, not a duplicate of the legacy keys + the new
    /// block.
    #[test]
    fn merge_bank_section_drops_legacy_flat_root_keys() {
        let existing = "\
# legacy seller.toml\n\
bank_account_number = \"OLD-FLAT\"\n\
iban = \"HU00000000\"\n\
bank_name = \"Old Flat Bank\"\n\
swift_bic = \"GIBAHUHB\"\n";

        let new_section = "\
[[seller.banks]]\n\
currency       = \"HUF\"\n\
account_number = \"NEW\"\n\
bank_name      = \"New Bank\"\n\
swift_bic      = \"GIBAHUHB\"\n\
default        = true\n";

        let merged = merge_bank_section(existing, new_section);
        assert!(
            !merged.contains("OLD-FLAT"),
            "legacy flat-root account_number must be dropped: {merged}"
        );
        assert!(
            !merged.contains("Old Flat Bank"),
            "legacy flat-root bank_name must be dropped: {merged}"
        );
        assert!(
            !merged.contains("iban ="),
            "legacy flat-root iban key must be dropped: {merged}"
        );
        assert!(
            merged.contains("# legacy seller.toml"),
            "non-bank comment preserved: {merged}"
        );
        assert!(merged.contains("NEW"), "new bank entry present: {merged}");
    }

    /// PR-72 / session-94 — replace_entries shares ONE validator with
    /// the parse path. Two HUF defaults must surface the same
    /// `MultipleDefaults` error on the write side as on the read side.
    #[test]
    fn replace_entries_loud_fails_on_multiple_defaults() {
        let mut banks = SellerBanks::default();
        let entries = vec![
            mint_entry(
                Currency::Huf,
                "A".to_string(),
                "Bank One".to_string(),
                "GIBAHUHB".to_string(),
                true,
            ),
            mint_entry(
                Currency::Huf,
                "B".to_string(),
                "Bank Two".to_string(),
                "GIBAHUHB".to_string(),
                true,
            ),
        ];
        let err = banks
            .replace_entries(entries)
            .expect_err("two HUF defaults must fail at write time");
        match err {
            SellerBanksError::MultipleDefaults { currency, count } => {
                assert_eq!(currency, Currency::Huf);
                assert_eq!(count, 2);
            }
            other => panic!("expected MultipleDefaults, got {other:?}"),
        }
    }

    /// PR-72 / session-94 — `mint_entry` is the write-side
    /// counterpart of the parser's id-derivation step. The id must be
    /// identical to the parser's id for the same `(currency,
    /// account_number)` so a write-then-read round-trip does not
    /// invent a new id and invalidate every PR-C-stamped reference.
    #[test]
    fn mint_entry_id_matches_parser_id() {
        let minted = mint_entry(
            Currency::Huf,
            "12345678-12345678-12345678".to_string(),
            "Erste Bank".to_string(),
            "GIBAHUHB".to_string(),
            true,
        );
        let body = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
default = true
";
        let parsed = parse_seller_banks(body).expect("parses");
        assert_eq!(parsed.entries()[0].id, minted.id);
    }

    /// `operator_message` carries Hungarian + English strings and
    /// names the file path. Pinned because the message is the
    /// operator-facing artifact — silent English-only drift would
    /// break the ADR-0038 posture inherited here.
    #[test]
    fn operator_message_is_bilingual_and_names_path() {
        let err = SellerBanksError::MultipleDefaults {
            currency: Currency::Huf,
            count: 2,
        };
        let msg = err.operator_message(Path::new("/tmp/seller.toml"));
        assert!(
            msg.contains("Több alapértelmezett"),
            "must include Hungarian: {msg}"
        );
        assert!(
            msg.contains("Multiple default"),
            "must include English: {msg}"
        );
        assert!(
            msg.contains("/tmp/seller.toml"),
            "must name the file path: {msg}"
        );
        assert!(msg.contains("HUF"), "must name the currency: {msg}");
    }
}
