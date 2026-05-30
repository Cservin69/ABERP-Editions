//! PR-89 / ADR-0045 — operator-configurable invoice-number template.
//!
//! Pre-PR-89 the invoice-number assembly was hardcoded at eight emit
//! sites as `format!("{}/{:05}", series_code, sequence)`. PR-89 lifts
//! that into a closed-vocab segment template the operator assembles
//! from Tenant Settings → Invoice numbering. The template + render +
//! validate live in this module; the eight emit sites read it once at
//! issuance time via [`format_invoice_number`].
//!
//! # Closed-vocab segment model
//!
//! A template is an ordered list of [`Segment`]s. Three kinds today:
//!
//! - [`Segment::Literal`] — arbitrary operator-typed text or separator.
//!   Constrained to the NAV `invoiceNumber` XSD charset
//!   (`[0-9A-Za-z\-/]`) so a Literal can never produce a NAV-illegal
//!   string at submit time; the rejection happens at config time, not
//!   submit time. Backslash is rejected (NAV does not accept it).
//! - [`Segment::Year`] — invoice-issue-date year, rendered as 2 or 4
//!   digits.
//! - [`Segment::Counter`] — the atomically-reserved sequence number,
//!   zero-padded to a MINIMUM width. Pad is a floor, not a cap —
//!   overflow grows (`01` → `99` → `100`). Exactly one Counter is
//!   required (zero → no monotonic progression; two → ambiguous
//!   render); both are loud-fail at [`validate_template`].
//!
//! # Reset policy
//!
//! [`ResetPolicy::OnYearChange`] resets the counter to `start_value`
//! when the calendar year of the issue date changes (Hungarian
//! convention: `ABERP-2026/000123` → `ABERP-2027/000001` on Jan 1).
//! [`ResetPolicy::Never`] runs continuous forever (the pre-PR-89
//! `INV-default` behaviour). The reset must stay gap-free within each
//! year — the existing `invoice_sequence_state` atomic allocator (keyed
//! by `(series_id, fiscal_year)`) is the gap-free guarantor; PR-89
//! lifts the `AnnualResetUnimplemented` gate by driving `fiscal_year`
//! from the issue-date year when `OnYearChange` is in effect.
//!
//! # Gap-free + uniqueness invariants
//!
//! - Within a `(series_code, fiscal_year)` bucket the counter
//!   increments by exactly 1, no gaps. Setting `start_value` > 1 is a
//!   SETUP/MIGRATION action only (continuing an external sequence such
//!   as Billingo); after the first invoice burns at `start_value`,
//!   subsequent invoices burn `start_value + 1, +2, ...` monotonically.
//!   See the SPA save endpoint for the gate that locks `start_value`
//!   once an allocation exists.
//! - Uniqueness across history: changing the template can re-render
//!   historical invoices' display strings (the renderer is recomputed
//!   from the current template at read time today — PR-90 may stamp
//!   the rendered number onto the invoice row for forward-proof
//!   immutability). For PR-89 v1 the operator is expected to configure
//!   the template BEFORE issuing real invoices; the handoff documents
//!   this caveat loudly.
//!
//! # seller.toml integration
//!
//! Persisted as a `[seller.numbering]` section in
//! `~/.aberp/<tenant>/seller.toml`. The write path
//! ([`write_numbering_section`]) is a non-destructive merge that
//! preserves the identity sections (`[seller]`, `[seller.address]`),
//! the bank-account block (`[[seller.banks]]`), AND any comment prefix
//! — same posture as PR-72's [`crate::seller_banks::merge_bank_section`].
//! Absent file or absent section returns [`default_template`]
//! (`INV-default/` + Counter{pad:5}, `ResetPolicy::Never`,
//! `start_value: 1`) — exactly reproducing the pre-PR-89 format so
//! existing tenants on the default scheme keep working with no
//! intervention.
//!
//! See ADR-0045 for the full design + reset-policy fork resolution.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{anyhow, Context as _, Result};

/// One segment in an invoice-number template per ADR-0045 §1. The
/// closed vocab is the operator-visible builder palette; adding a
/// fourth segment kind is a deliberate one-line widening here + a
/// render arm + a validate arm + a parse/emit arm + a UI chip kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Arbitrary operator-typed text — separator or brand. Constrained
    /// to [`is_nav_invoice_number_char`] at [`validate_template`]; the
    /// builder UI rejects illegal characters inline so the operator
    /// never assembles a config that would ABORT at NAV submit.
    Literal(String),
    /// Invoice-issue-date year. Two-digit form renders `26` for 2026;
    /// four-digit form renders `2026`. Other widths are not in scope
    /// (NAV examples + Hungarian convention use 2 or 4 only).
    Year { digits: YearDigits },
    /// Atomically-reserved sequence number. `pad_width` is the MINIMUM
    /// zero-padded width — overflow grows naturally
    /// (`pad_width=2` renders `01`..`99`..`100`..). `pad_width=0` is
    /// treated as 1 (render the integer with no leading zeros).
    Counter { pad_width: u8 },
}

/// Closed-vocab year width per ADR-0045 §1. `Two` renders the last
/// two digits of the year (modulo 100); `Four` renders the full year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YearDigits {
    Two,
    Four,
}

/// Counter-reset policy per ADR-0045 §2. Two arms:
///
/// - [`ResetPolicy::Never`] — the counter increments forever, no
///   per-year reset. Pre-PR-89 default. Matches `INV-default`'s
///   pre-existing behaviour.
/// - [`ResetPolicy::OnYearChange`] — the counter resets to
///   [`NumberingTemplate::start_value`] when the calendar year of the
///   issue date changes. Hungarian-business convention. Requires the
///   template to carry a [`Segment::Year`] (otherwise the reset would
///   silently produce duplicate numbers — the validator rejects this
///   combo at config time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetPolicy {
    Never,
    OnYearChange,
}

impl ResetPolicy {
    /// PR-90 / ADR-0045 §2 — project the operator's template-side
    /// reset policy onto the billing module's allocator-side
    /// `ResetPolicy`. The two enums are deliberately distinct
    /// (template-vocab vs. allocator-vocab) but the two arms map
    /// 1:1: `Never → Never`, `OnYearChange → AnnualOnFiscalYear`.
    /// The binary's `ensure_series` uses this to thread the operator's
    /// Tenant-Settings choice into the series row's policy.
    pub fn to_billing(self) -> aberp_billing::ResetPolicy {
        match self {
            Self::Never => aberp_billing::ResetPolicy::Never,
            Self::OnYearChange => aberp_billing::ResetPolicy::AnnualOnFiscalYear,
        }
    }
}

/// A complete numbering template: an ordered list of [`Segment`]s, a
/// [`ResetPolicy`], and a `start_value` for the counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberingTemplate {
    pub segments: Vec<Segment>,
    pub reset_policy: ResetPolicy,
    /// First value the counter takes when the bucket
    /// `(series, fiscal_year)` is first allocated. Set > 1 only as a
    /// migration step to continue an external sequence (e.g.,
    /// "start at 1247 to continue my Billingo numbering"). Once any
    /// invoice has been issued in the current bucket the SPA save
    /// route locks this field — Hungarian §169 forbids gaps post-issue.
    pub start_value: u64,
}

impl NumberingTemplate {
    /// Render the template at `(year, sequence)`. Pure function; no
    /// IO, no clock read. Used by the eight emit sites + the SPA
    /// "live preview" widget.
    pub fn render(&self, year: i32, sequence: u64) -> String {
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Segment::Literal(s) => out.push_str(s),
                Segment::Year { digits } => match digits {
                    YearDigits::Two => {
                        let modded = year.rem_euclid(100);
                        out.push_str(&format!("{modded:02}"));
                    }
                    YearDigits::Four => out.push_str(&format!("{year:04}")),
                },
                Segment::Counter { pad_width } => {
                    let width = (*pad_width).max(1) as usize;
                    out.push_str(&format!("{sequence:0width$}", width = width));
                }
            }
        }
        out
    }

    /// S165 — render with the build-profile prefix applied. Dev/test
    /// builds prepend [`crate::build_profile::INVOICE_NUMBER_TEST_PREFIX`]
    /// (`TEST-`) so test-endpoint submissions carry a visually distinct,
    /// NAV-charset-legal number; production builds render unprefixed.
    /// Purely render-side — the DB sequence counter is untouched, so a
    /// build switch never resets or skips a number. This is the method
    /// the live emit sites (`issue_invoice`, `issue_storno`,
    /// `issue_modification`) call; the bare [`render`](Self::render)
    /// stays pure for the validator + the unit-test pins.
    pub fn render_for_build(&self, year: i32, sequence: u64) -> String {
        format!(
            "{}{}",
            crate::build_profile::INVOICE_NUMBER_TEST_PREFIX,
            self.render(year, sequence)
        )
    }
}

/// Typed validate-time failures per ADR-0045 §3. Every variant is a
/// loud-fail at the config boundary — the SPA save endpoint surfaces
/// the typed error to the operator as a bilingual field-level message
/// (no fallback to "rendered something invalid at submit time").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumberingError {
    /// The template carries zero [`Segment::Counter`] segments. Without
    /// a counter, every render produces the same string → duplicate
    /// invoice numbers. Loud-fail.
    NoCounter,
    /// The template carries more than one [`Segment::Counter`]. The
    /// render would be ambiguous (which counter advances? both?).
    /// Loud-fail.
    MultipleCounters { count: usize },
    /// A [`Segment::Literal`]'s string is empty. The builder UI should
    /// already prevent this; the validator double-gates so a hand-
    /// edited seller.toml cannot smuggle one in.
    EmptyLiteral { segment_index: usize },
    /// A [`Segment::Literal`] contains a character outside the NAV
    /// `invoiceNumber` XSD charset (`[0-9A-Za-z\-/]`). The offending
    /// character is named so the operator can spot it in the input.
    InvalidLiteralCharacter {
        segment_index: usize,
        character: char,
    },
    /// The rendered shape (worst-case: counter at u64::MAX) would
    /// exceed the NAV `invoiceNumber` 50-character limit. Defensive
    /// gate; not expected in practice (the operator would have to
    /// type a very long literal). The check uses the minimum-width
    /// render — overflow growth widens this further but no operator
    /// would notice until they hit ~10^49 invoices.
    TooLong { rendered_min_len: usize },
    /// [`ResetPolicy::OnYearChange`] selected but the template has no
    /// [`Segment::Year`]. A year-change reset without a year segment
    /// silently produces duplicate numbers — loud-fail rather than
    /// allow the inconsistency.
    OnYearChangeWithoutYearSegment,
    /// `start_value` is zero. Counter starts at 1 by convention
    /// (Hungarian sorszám numbering); 0 is rejected so a typo in the
    /// SPA's start-value field cannot silently produce
    /// `ABERP-2026/000000` as the first invoice.
    InvalidStartValue,
    /// Empty segment list — no segments to render. Loud-fail.
    EmptyTemplate,
}

impl NumberingError {
    /// Hungarian + English operator-visible message pair. Mirrors
    /// PR-72's `SellerBanksError::operator_message` posture: both
    /// languages, names the offending segment index when applicable.
    pub fn operator_message(&self) -> String {
        match self {
            Self::NoCounter => "A sablonnak pontosan egy számlálót (Counter) kell tartalmaznia. \
                A sorszám nélkül a kiadott számlák száma ütközne.\n\
                The template must contain exactly one Counter segment. \
                Without a counter, issued invoice numbers would collide."
                .to_string(),
            Self::MultipleCounters { count } => format!(
                "A sablon {count} db számlálót tartalmaz; pontosan egy szükséges.\n\
                The template contains {count} Counter segments; exactly one is required."
            ),
            Self::EmptyLiteral { segment_index } => format!(
                "A(z) {idx}. szöveg-szegmens üres.\n\
                Literal segment #{idx} is empty.",
                idx = segment_index + 1,
            ),
            Self::InvalidLiteralCharacter {
                segment_index,
                character,
            } => format!(
                "A(z) {idx}. szöveg-szegmens érvénytelen karaktert tartalmaz: '{character}'. \
                Engedélyezett: A-Z, a-z, 0-9, kötőjel (-), perjel (/).\n\
                Literal segment #{idx} contains an invalid character: '{character}'. \
                Allowed: A-Z, a-z, 0-9, dash (-), slash (/).",
                idx = segment_index + 1,
            ),
            Self::TooLong { rendered_min_len } => format!(
                "A sablon kimenete legalább {rendered_min_len} karakter, ami meghaladja a NAV \
                `invoiceNumber` mező 50-karakteres korlátját.\n\
                The template renders at least {rendered_min_len} characters, which exceeds the NAV \
                `invoiceNumber` field's 50-character limit."
            ),
            Self::OnYearChangeWithoutYearSegment => {
                "Az 'évváltáskor nullázódik' beállítás csak akkor használható, ha a sablon \
                tartalmaz év (Year) szegmenst.\n\
                The 'reset on year change' policy requires the template to contain a Year segment."
                    .to_string()
            }
            Self::InvalidStartValue => {
                "A kezdő érték nem lehet nulla; a számláló 1-től vagy nagyobb értéktől indul.\n\
                Start value must be >= 1; the counter cannot begin at zero."
                    .to_string()
            }
            Self::EmptyTemplate => "A sablon legalább egy szegmenst kell tartalmazzon.\n\
                The template must contain at least one segment."
                .to_string(),
        }
    }
}

impl std::fmt::Display for NumberingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.operator_message())
    }
}

impl std::error::Error for NumberingError {}

/// NAV `invoiceNumber` XSD charset per the v3.0 schema:
/// `pattern value="[0-9A-Za-z\-/]{1,50}"`. Length 1-50, ASCII letters
/// + digits + dash + slash only. Backslash, dot, underscore, space
/// — all rejected.
pub fn is_nav_invoice_number_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '/'
}

/// NAV `invoiceNumber` maximum length per the XSD `maxLength=50`.
pub const NAV_INVOICE_NUMBER_MAX_LEN: usize = 50;

/// Validate a template against ADR-0045 §3 invariants. Loud-fail on
/// the first violation found; the SPA save route surfaces the typed
/// error inline. Pure; no IO. Pinned by the unit tests below + the
/// integration test in `tests/serve_numbering_route.rs` (PR-89).
pub fn validate_template(t: &NumberingTemplate) -> std::result::Result<(), NumberingError> {
    if t.segments.is_empty() {
        return Err(NumberingError::EmptyTemplate);
    }
    if t.start_value == 0 {
        return Err(NumberingError::InvalidStartValue);
    }

    let mut counter_count = 0;
    let mut has_year = false;
    for (idx, seg) in t.segments.iter().enumerate() {
        match seg {
            Segment::Counter { .. } => counter_count += 1,
            Segment::Year { .. } => has_year = true,
            Segment::Literal(s) => {
                if s.is_empty() {
                    return Err(NumberingError::EmptyLiteral { segment_index: idx });
                }
                for c in s.chars() {
                    if !is_nav_invoice_number_char(c) {
                        return Err(NumberingError::InvalidLiteralCharacter {
                            segment_index: idx,
                            character: c,
                        });
                    }
                }
            }
        }
    }
    if counter_count == 0 {
        return Err(NumberingError::NoCounter);
    }
    if counter_count > 1 {
        return Err(NumberingError::MultipleCounters {
            count: counter_count,
        });
    }
    if matches!(t.reset_policy, ResetPolicy::OnYearChange) && !has_year {
        return Err(NumberingError::OnYearChangeWithoutYearSegment);
    }

    // Minimum-width render sanity check. Render at year = 9999 (worst
    // case for the four-digit Year) and the start_value to estimate
    // the minimum length. Overflow growth past the pad_width can push
    // this further; the deferred check is fine because no operator
    // crosses 10^49 invoices.
    let min_render_len = t.render(9999, t.start_value).len();
    if min_render_len > NAV_INVOICE_NUMBER_MAX_LEN {
        return Err(NumberingError::TooLong {
            rendered_min_len: min_render_len,
        });
    }

    Ok(())
}

/// Default template per ADR-0045 §4: `Literal("INV-default/")` +
/// `Counter{pad:5}`, `ResetPolicy::Never`, `start_value: 1`. This
/// EXACTLY reproduces the pre-PR-89 `format!("{}/{:05}", "INV-default",
/// seq)` shape so a tenant without a `[seller.numbering]` section in
/// their seller.toml keeps emitting `INV-default/00001` with zero
/// migration churn.
pub fn default_template() -> NumberingTemplate {
    NumberingTemplate {
        segments: vec![
            Segment::Literal("INV-default/".to_string()),
            Segment::Counter { pad_width: 5 },
        ],
        reset_policy: ResetPolicy::Never,
        start_value: 1,
    }
}

/// Read + parse + validate the `[seller.numbering]` section of
/// `~/.aberp/<tenant>/seller.toml`. Returns [`default_template`] when
/// the file is absent OR the section is absent — the pre-PR-89
/// behaviour is the unconfigured default. Returns `Err` on
/// parse/validate failure.
pub fn read_numbering_template(path: &Path) -> Result<NumberingTemplate> {
    if !path.exists() {
        return Ok(default_template());
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller.toml at {}", path.display()))?;
    parse_numbering_section(&body)
}

/// Parse a `seller.toml` body string for the `[seller.numbering]`
/// section. Section absent → [`default_template`]; section present →
/// build + validate. Public for the integration test pin.
pub fn parse_numbering_section(body: &str) -> Result<NumberingTemplate> {
    let raw = collect_raw_section(body);
    let Some(raw) = raw else {
        return Ok(default_template());
    };
    let template = raw.into_template()?;
    validate_template(&template).map_err(|e| anyhow!("{e}"))?;
    Ok(template)
}

#[derive(Debug, Default)]
struct RawNumberingSection {
    segments_line: Option<String>,
    reset_policy: Option<String>,
    start_value: Option<String>,
}

impl RawNumberingSection {
    fn into_template(self) -> Result<NumberingTemplate> {
        let segments_line = self
            .segments_line
            .ok_or_else(|| anyhow!("[seller.numbering] missing required `segments` key"))?;
        let segments = parse_segments_array(&segments_line)?;
        let reset_policy = match self.reset_policy.as_deref() {
            None | Some("never") | Some("Never") => ResetPolicy::Never,
            Some("on_year_change") | Some("OnYearChange") => ResetPolicy::OnYearChange,
            Some(other) => {
                return Err(anyhow!(
                    "[seller.numbering].reset_policy `{other}` is not in the closed vocab \
                     (`never` | `on_year_change`)"
                ))
            }
        };
        let start_value = match self.start_value.as_deref() {
            None => 1u64,
            Some(s) => s
                .parse::<u64>()
                .map_err(|_| anyhow!("[seller.numbering].start_value `{s}` is not a u64"))?,
        };
        Ok(NumberingTemplate {
            segments,
            reset_policy,
            start_value,
        })
    }
}

fn collect_raw_section(body: &str) -> Option<RawNumberingSection> {
    let mut in_section = false;
    let mut raw = RawNumberingSection::default();
    let mut found = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("[[") {
            in_section = false;
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            in_section = inner == "seller.numbering";
            if in_section {
                found = true;
            }
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let value = v.trim();
        match key {
            "segments" => raw.segments_line = Some(value.to_string()),
            "reset_policy" => raw.reset_policy = Some(strip_quotes(value).to_string()),
            "start_value" => raw.start_value = Some(value.to_string()),
            _ => {}
        }
    }
    if found {
        Some(raw)
    } else {
        None
    }
}

/// Parse a `segments = [...]` value into the typed [`Vec<Segment>`].
/// The on-disk form is a TOML inline array of inline tables, e.g.:
///
/// ```toml
/// segments = [
///   { kind = "Literal", text = "ABERP-" },
///   { kind = "Year", digits = 4 },
///   { kind = "Literal", text = "/" },
///   { kind = "Counter", pad_width = 6 },
/// ]
/// ```
///
/// The walker is custom-built (mirroring PR-71's
/// `collect_raw_entries` posture) rather than reaching for the `toml`
/// crate — the accepted grammar is tight enough to keep the dependency
/// surface small. Multi-line array forms ARE supported because
/// [`collect_raw_section`] hands us the rest-of-line which is the
/// inline-array body verbatim; for the multi-line write path
/// [`write_numbering_section`] emits a single-line inline-array shape
/// that round-trips cleanly through here.
fn parse_segments_array(line: &str) -> Result<Vec<Segment>> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| anyhow!("segments value `{trimmed}` is not a TOML inline array"))?;
    let mut segments = Vec::new();
    for raw in split_top_level_commas(inner) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let table = raw
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .ok_or_else(|| anyhow!("segment `{raw}` is not an inline table"))?;
        let mut kind: Option<String> = None;
        let mut text: Option<String> = None;
        let mut digits: Option<u8> = None;
        let mut pad_width: Option<u8> = None;
        for kv in table.split(',') {
            let kv = kv.trim();
            if kv.is_empty() {
                continue;
            }
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow!("segment kv `{kv}` missing `=`"))?;
            let key = k.trim();
            let value = v.trim();
            match key {
                "kind" => kind = Some(strip_quotes(value).to_string()),
                "text" => text = Some(unescape_toml_string(strip_quotes(value))),
                "digits" => digits = Some(value.parse::<u8>().map_err(|_| anyhow!("bad digits"))?),
                "pad_width" => {
                    pad_width = Some(value.parse::<u8>().map_err(|_| anyhow!("bad pad_width"))?)
                }
                _ => {}
            }
        }
        let kind = kind.ok_or_else(|| anyhow!("segment missing `kind`"))?;
        let seg = match kind.as_str() {
            "Literal" => Segment::Literal(text.ok_or_else(|| anyhow!("Literal missing text"))?),
            "Year" => {
                let d = digits.ok_or_else(|| anyhow!("Year missing digits"))?;
                let yd = match d {
                    2 => YearDigits::Two,
                    4 => YearDigits::Four,
                    other => return Err(anyhow!("Year.digits {other} not in (2, 4)")),
                };
                Segment::Year { digits: yd }
            }
            "Counter" => Segment::Counter {
                pad_width: pad_width.ok_or_else(|| anyhow!("Counter missing pad_width"))?,
            },
            other => return Err(anyhow!("segment kind `{other}` not in closed vocab")),
        };
        segments.push(seg);
    }
    Ok(segments)
}

/// Split `s` on commas that sit OUTSIDE any `{...}` nesting. The
/// segment-array's outer separator is comma; inline tables also use
/// comma, so a naive split would mis-split the table interior.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_string = false;
    let mut prev_backslash = false;
    for c in s.chars() {
        if in_string {
            current.push(c);
            if prev_backslash {
                prev_backslash = false;
            } else if c == '\\' {
                prev_backslash = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                current.push(c);
            }
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                out.push(current.clone());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn strip_quotes(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn unescape_toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a [`NumberingTemplate`] as the canonical
/// `[seller.numbering]` section. Pinned by the round-trip unit test
/// below: serialize → parse → equal-template.
pub fn to_toml_section(t: &NumberingTemplate) -> String {
    let mut out = String::new();
    out.push_str("[seller.numbering]\n");
    out.push_str("segments = [");
    let mut first = true;
    for seg in &t.segments {
        if !first {
            out.push_str(", ");
        }
        first = false;
        match seg {
            Segment::Literal(s) => {
                out.push_str(&format!(
                    "{{ kind = \"Literal\", text = \"{}\" }}",
                    escape_toml_string(s)
                ));
            }
            Segment::Year { digits } => {
                let d = match digits {
                    YearDigits::Two => 2u8,
                    YearDigits::Four => 4u8,
                };
                out.push_str(&format!("{{ kind = \"Year\", digits = {d} }}"));
            }
            Segment::Counter { pad_width } => {
                out.push_str(&format!(
                    "{{ kind = \"Counter\", pad_width = {pad_width} }}"
                ));
            }
        }
    }
    out.push_str("]\n");
    let policy_token = match t.reset_policy {
        ResetPolicy::Never => "never",
        ResetPolicy::OnYearChange => "on_year_change",
    };
    out.push_str(&format!("reset_policy = \"{policy_token}\"\n"));
    out.push_str(&format!("start_value = {}\n", t.start_value));
    out
}

/// PR-89 — atomically replace `path`'s `[seller.numbering]` section
/// (and only that section) with the canonical serialisation of
/// `template`. Preserves the identity sections (`[seller]`,
/// `[seller.address]`), the bank-account block (`[[seller.banks]]`),
/// AND any comment prefix — mirrors PR-72's
/// [`crate::seller_banks::write_seller_banks_section`] posture so the
/// three SPA write surfaces (identity, banks, numbering) compose
/// without stomping each other.
pub fn write_numbering_section(path: &Path, template: &NumberingTemplate) -> Result<()> {
    validate_template(template)
        .map_err(|e| anyhow!("numbering template invariants violated pre-write: {e}"))?;
    let new_section = to_toml_section(template);
    let body = if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("read existing seller.toml at {}", path.display()))?;
        merge_numbering_section(&existing, &new_section)
    } else {
        new_section
    };
    write_atomic(path, body.as_bytes())
}

/// PR-89 — replace the `[seller.numbering]` section of an existing
/// `seller.toml` body. Walks the lines and partitions them into:
///   - **prefix**: everything that isn't inside a
///     `[seller.numbering]` block.
///   - **numbering lines** (DROPPED): the existing
///     `[seller.numbering]` section header + its key=value body until
///     the next section header.
///
/// Same posture as PR-72's `merge_bank_section`: the replacement
/// section is appended to the preserved prefix with exactly one
/// blank-line separator when both are non-empty.
fn merge_numbering_section(existing: &str, new_section: &str) -> String {
    let mut prefix = String::new();
    let mut in_numbering = false;
    for raw_line in existing.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            in_numbering = false;
            prefix.push_str(raw_line);
            prefix.push('\n');
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            in_numbering = inner == "seller.numbering";
            if in_numbering {
                continue;
            }
            prefix.push_str(raw_line);
            prefix.push('\n');
            continue;
        }
        if in_numbering {
            continue;
        }
        prefix.push_str(raw_line);
        prefix.push('\n');
    }
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

/// POSIX-atomic write helper. Mirror of
/// `setup_seller_info::write_atomic` + `seller_banks::write_atomic`
/// (same dir tempfile, fsync, rename, 0600 perms, 0700 parent dir).
/// Kept as a local copy rather than re-exported to avoid widening the
/// `setup_seller_info` surface — the comment in `seller_banks.rs`
/// explains the same trade.
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
        ".seller.toml.numbering.tmp.{}-{}-{}",
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

/// PR-89 — convenience for the eight pre-PR-89 emit sites: open the
/// `seller.toml` at the active-tenant path, read the template (or
/// fall back to [`default_template`]), render at `(year, sequence)`.
/// The render is pure once the template is in hand; this helper bundles
/// the IO step so the emit sites stay one-liner-shaped.
///
/// On read failure the helper falls back to [`default_template`]'s
/// render — the pre-PR-89 behaviour — and logs a structured warn. A
/// malformed seller.toml at submit time is operator-visible elsewhere
/// (the SPA boot's NeedsSellerConfig branch); the emit path must
/// remain non-blocking so a half-edited file does not stop an
/// in-flight submission.
pub fn format_invoice_number(seller_toml_path: &Path, year: i32, sequence: u64) -> String {
    match read_numbering_template(seller_toml_path) {
        Ok(t) => t.render_for_build(year, sequence),
        Err(e) => {
            tracing::warn!(
                err = %e,
                path = %seller_toml_path.display(),
                "could not read invoice-number template; falling back to default INV-default/NNNNN"
            );
            default_template().render_for_build(year, sequence)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-PR-89 baseline: the default template renders the
    /// pre-existing `INV-default/00001` shape at sequence 1, year
    /// irrelevant. The literal-then-counter ordering MUST stay stable
    /// across releases — every legacy invoice was issued under this
    /// shape, and PR-89's migration relies on it.
    #[test]
    fn default_template_renders_pre_pr89_shape() {
        let t = default_template();
        assert_eq!(t.render(2026, 1), "INV-default/00001");
        assert_eq!(t.render(2026, 42), "INV-default/00042");
        assert_eq!(t.render(2026, 99999), "INV-default/99999");
    }

    /// Pad-as-FLOOR pinning: width-2 counter renders `01`, `99`,
    /// `100`, `101` — never truncates past the pad width. Ervin
    /// named this case in the spec.
    #[test]
    fn counter_pad_is_floor_not_cap_overflow_grows() {
        let t = NumberingTemplate {
            segments: vec![Segment::Counter { pad_width: 2 }],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(t.render(2026, 1), "01");
        assert_eq!(t.render(2026, 9), "09");
        assert_eq!(t.render(2026, 99), "99");
        assert_eq!(t.render(2026, 100), "100");
        assert_eq!(t.render(2026, 101), "101");
        assert_eq!(t.render(2026, 99999), "99999");
    }

    /// Year segment renders 2 or 4 digits per the closed vocab.
    #[test]
    fn year_segment_renders_two_or_four_digits() {
        let t2 = NumberingTemplate {
            segments: vec![
                Segment::Year {
                    digits: YearDigits::Two,
                },
                Segment::Counter { pad_width: 4 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(t2.render(2026, 7), "260007");
        let t4 = NumberingTemplate {
            segments: vec![
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Counter { pad_width: 4 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(t4.render(2026, 7), "20260007");
    }

    /// Reorder pin: `0001/2026-ABEDIFFERENT` is one of Ervin's
    /// example shapes from the spec. The render order MUST follow the
    /// segment order.
    #[test]
    fn segment_order_drives_render_order() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Counter { pad_width: 4 },
                Segment::Literal("/".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("-ABEDIFFERENT".to_string()),
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(t.render(2026, 1), "0001/2026-ABEDIFFERENT");
    }

    /// Reject templates with zero Counter segments — duplicate
    /// numbers would silently follow.
    #[test]
    fn validate_rejects_zero_counters() {
        let t = NumberingTemplate {
            segments: vec![Segment::Literal("ABERP-".to_string())],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(validate_template(&t), Err(NumberingError::NoCounter));
    }

    /// Reject templates with multiple Counter segments — ambiguous
    /// render semantics.
    #[test]
    fn validate_rejects_multiple_counters() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Counter { pad_width: 2 },
                Segment::Literal("-".to_string()),
                Segment::Counter { pad_width: 2 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(
            validate_template(&t),
            Err(NumberingError::MultipleCounters { count: 2 })
        );
    }

    /// Reject Literal containing backslash — NAV `invoiceNumber`
    /// XSD pattern is `[0-9A-Za-z\-/]`. Backslash MUST loud-fail at
    /// config time so it never reaches the wire.
    #[test]
    fn validate_rejects_literal_with_backslash() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP\\".to_string()),
                Segment::Counter { pad_width: 4 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        match validate_template(&t).unwrap_err() {
            NumberingError::InvalidLiteralCharacter {
                segment_index,
                character,
            } => {
                assert_eq!(segment_index, 0);
                assert_eq!(character, '\\');
            }
            other => panic!("expected InvalidLiteralCharacter, got {other:?}"),
        }
    }

    /// Reject Literal containing space, dot, underscore — all
    /// outside the NAV XSD charset.
    #[test]
    fn validate_rejects_other_nav_illegal_characters() {
        for bad in [" ", ".", "_", "#", "@"] {
            let t = NumberingTemplate {
                segments: vec![
                    Segment::Literal(format!("ABE{bad}RP")),
                    Segment::Counter { pad_width: 4 },
                ],
                reset_policy: ResetPolicy::Never,
                start_value: 1,
            };
            let err = validate_template(&t).unwrap_err();
            assert!(
                matches!(err, NumberingError::InvalidLiteralCharacter { .. }),
                "bad char `{bad}` must loud-fail, got {err:?}"
            );
        }
    }

    /// Accept Literal with the four NAV-legal special characters:
    /// dash + slash + digit + letter mix. Pinning the affirmative
    /// arm alongside the rejection arms.
    #[test]
    fn validate_accepts_nav_legal_literal_charset() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("AB-ERP/2026-".to_string()),
                Segment::Counter { pad_width: 6 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert!(validate_template(&t).is_ok());
    }

    /// Reject OnYearChange without a Year segment — silent duplicate
    /// numbers.
    #[test]
    fn validate_rejects_on_year_change_without_year_segment() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP/".to_string()),
                Segment::Counter { pad_width: 6 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        assert_eq!(
            validate_template(&t),
            Err(NumberingError::OnYearChangeWithoutYearSegment)
        );
    }

    /// Reject empty template + empty literal segment.
    #[test]
    fn validate_rejects_empty_template_and_empty_literal() {
        let t_empty = NumberingTemplate {
            segments: vec![],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(
            validate_template(&t_empty),
            Err(NumberingError::EmptyTemplate)
        );

        let t_empty_lit = NumberingTemplate {
            segments: vec![
                Segment::Literal(String::new()),
                Segment::Counter { pad_width: 4 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 1,
        };
        assert_eq!(
            validate_template(&t_empty_lit),
            Err(NumberingError::EmptyLiteral { segment_index: 0 })
        );
    }

    /// Reject start_value = 0 — silent leading zeros from the very
    /// first invoice.
    #[test]
    fn validate_rejects_zero_start_value() {
        let t = NumberingTemplate {
            segments: vec![Segment::Counter { pad_width: 4 }],
            reset_policy: ResetPolicy::Never,
            start_value: 0,
        };
        assert_eq!(
            validate_template(&t),
            Err(NumberingError::InvalidStartValue)
        );
    }

    /// Ervin's primary shape: `ABERP-2026/000001` with annual reset.
    /// This is what Tenant Settings will assemble on go-live; pinning
    /// the render + the validate-ok arm.
    #[test]
    fn ervin_primary_template_renders_and_validates() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP-".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 6 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        assert!(validate_template(&t).is_ok());
        assert_eq!(t.render(2026, 1), "ABERP-2026/000001");
        assert_eq!(t.render(2026, 1247), "ABERP-2026/001247");
        assert_eq!(t.render(2027, 1), "ABERP-2027/000001"); // annual-reset visualised
    }

    /// Round-trip: serialise to TOML, parse back, equal template.
    /// Pin the contract between [`to_toml_section`] and
    /// [`parse_numbering_section`] so a write-then-read does not
    /// quietly mutate the operator's configured template.
    #[test]
    fn toml_section_round_trips() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP-".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 6 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1247,
        };
        let body = to_toml_section(&t);
        let back = parse_numbering_section(&body).expect("re-parses");
        assert_eq!(t, back);
    }

    /// Absent section → default template. Backwards-compat with all
    /// pre-PR-89 seller.toml files.
    #[test]
    fn missing_section_returns_default_template() {
        let body = "[seller]\nlegal_name = \"X\"\n";
        let t = parse_numbering_section(body).expect("parses");
        assert_eq!(t, default_template());
    }

    /// Merge preserves identity + bank sections + comments. PR-89
    /// non-destructive write invariant.
    #[test]
    fn merge_preserves_identity_and_bank_sections() {
        let existing = "\
# ABERP seller config\n\
[seller]\n\
legal_name = \"Áben Consulting KFT.\"\n\
\n\
[seller.address]\n\
country_code = \"HU\"\n\
\n\
[[seller.banks]]\n\
currency = \"HUF\"\n\
account_number = \"X\"\n\
bank_name = \"Bank\"\n\
swift_bic = \"GIBAHUHB\"\n\
default = true\n\
\n\
[seller.numbering]\n\
segments = [{ kind = \"Literal\", text = \"OLD-\" }, { kind = \"Counter\", pad_width = 4 }]\n\
reset_policy = \"never\"\n\
start_value = 1\n";
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("NEW-".to_string()),
                Segment::Counter { pad_width: 5 },
            ],
            reset_policy: ResetPolicy::Never,
            start_value: 100,
        };
        let new_section = to_toml_section(&t);
        let merged = merge_numbering_section(existing, &new_section);
        assert!(merged.contains("[seller]"), "identity preserved: {merged}");
        assert!(
            merged.contains("Áben Consulting KFT."),
            "identity name preserved"
        );
        assert!(merged.contains("[[seller.banks]]"), "bank block preserved");
        assert!(
            merged.contains("# ABERP seller config"),
            "comment preserved"
        );
        assert!(!merged.contains("OLD-"), "old numbering dropped: {merged}");
        assert!(merged.contains("NEW-"), "new numbering present: {merged}");
        assert!(
            merged.contains("start_value = 100"),
            "new start_value present"
        );
    }

    /// Reset policy parser accepts the closed-vocab tokens; rejects
    /// anything outside.
    #[test]
    fn reset_policy_parser_closed_vocab() {
        let body = "\
[seller.numbering]\n\
segments = [{ kind = \"Counter\", pad_width = 4 }]\n\
reset_policy = \"never\"\n";
        let t = parse_numbering_section(body).expect("parses never");
        assert!(matches!(t.reset_policy, ResetPolicy::Never));

        // OnYearChange parser requires Year segment to validate.
        let body2 = "\
[seller.numbering]\n\
segments = [{ kind = \"Year\", digits = 4 }, { kind = \"Counter\", pad_width = 4 }]\n\
reset_policy = \"on_year_change\"\n";
        let t2 = parse_numbering_section(body2).expect("parses on_year_change");
        assert!(matches!(t2.reset_policy, ResetPolicy::OnYearChange));

        let bad = "\
[seller.numbering]\n\
segments = [{ kind = \"Counter\", pad_width = 4 }]\n\
reset_policy = \"weekly\"\n";
        assert!(parse_numbering_section(bad).is_err());
    }

    /// Hand-rolled splitter pin: comma INSIDE `{...}` does not split
    /// the outer array. Two segments separated by top-level comma; each
    /// segment is a `{ key = "v", key2 = N }` inline table.
    #[test]
    fn split_top_level_commas_respects_nesting() {
        let parts = split_top_level_commas(
            "{ kind = \"Literal\", text = \"AB,ERP\" }, { kind = \"Counter\", pad_width = 4 }",
        );
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("AB,ERP"), "inner comma preserved");
    }

    /// `format_invoice_number` with an absent file returns the default
    /// shape — the pre-PR-89 emit behaviour, byte-for-byte. The eight
    /// migrated emit sites rely on this invariant so legacy tenants
    /// see no change post-PR-89.
    #[test]
    fn format_invoice_number_falls_back_to_default_on_missing_file() {
        let tmp =
            std::env::temp_dir().join(format!("aberp-pr89-missing-{}.toml", std::process::id()));
        // ensure absent
        let _ = std::fs::remove_file(&tmp);
        let rendered = format_invoice_number(&tmp, 2026, 42);
        // S165 — the emit path now carries the build-profile prefix
        // (`TEST-` on dev/test builds, empty on production). Compose the
        // expectation from the const so this pins identically under both
        // build flavours.
        assert_eq!(
            rendered,
            format!(
                "{}INV-default/00042",
                crate::build_profile::INVOICE_NUMBER_TEST_PREFIX
            )
        );
    }

    /// S165 — `render_for_build` carries the build prefix while the bare
    /// `render` stays clean. On dev/test builds (feature OFF) the emit
    /// shape is `TEST-ABERP/2026/0042`; on production builds (feature ON)
    /// it is the unprefixed `ABERP/2026/0042`.
    #[cfg(not(feature = "production"))]
    #[test]
    fn render_for_build_prepends_test_prefix_in_dev_build() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP/".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 4 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        // The pure render is unprefixed; the build render adds TEST-.
        assert_eq!(t.render(2026, 42), "ABERP/2026/0042");
        assert_eq!(t.render_for_build(2026, 42), "TEST-ABERP/2026/0042");
    }

    #[cfg(feature = "production")]
    #[test]
    fn render_for_build_omits_prefix_in_production_build() {
        let t = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP/".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 4 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        assert_eq!(t.render_for_build(2026, 42), "ABERP/2026/0042");
    }

    /// S165 — the `TEST-` prefix MUST stay inside the NAV `invoiceNumber`
    /// XSD charset (`[0-9A-Za-z\-/]`): `TEST-ABERP/2026/0042` is all
    /// letters + digits + hyphen + slash, so every character passes
    /// [`is_nav_invoice_number_char`] and the length is well under 50.
    /// (Hyphen is legal; underscore — empirically rejected by the
    /// validator — is NOT used.)
    #[test]
    fn test_prefix_passes_nav_invoice_number_charset() {
        let rendered = "TEST-ABERP/2026/0042";
        assert!(rendered.len() <= NAV_INVOICE_NUMBER_MAX_LEN);
        for c in rendered.chars() {
            assert!(
                is_nav_invoice_number_char(c),
                "char {c:?} in `{rendered}` is not NAV-invoiceNumber-charset-legal"
            );
        }
    }
}
