//! `aberp setup-nav-credentials` — operator helper to populate the four
//! NAV credential artifacts in the OS keychain for a tenant.
//!
//! # Scope discipline
//!
//! Minimum viable operator tool: prompts to stderr, reads from stdin,
//! one line per artifact. No hidden-input library (e.g. `rpassword`)
//! because adding it is a workspace-level supply-chain decision that
//! belongs in its own ADR — and the alternative (`stty -echo`) is a
//! platform-specific syscall that violates the `#![forbid(unsafe_code)]`
//! policy this binary inherits.
//!
//! Operators populating production credentials should:
//!
//!   1. Run this from a workstation whose shell history is not synced
//!      (`HISTFILE=/dev/null` for the session, or use `zsh` with
//!      `setopt HIST_IGNORE_SPACE` and lead the command with a space).
//!   2. Pipe the four values in from a file with `0600` perms rather
//!      than typing them at the prompt:
//!
//!         aberp setup-nav-credentials --tenant default < creds.txt
//!
//!      where `creds.txt` is `login\npassword\nxml_sign_key\nxml_change_key\n`.
//!
//! Both flows write the same keychain entry — the PR-57 consolidated
//! `aberp.nav.<tenant>` / `nav_credentials_blob` item — that the
//! production `submit-invoice` path reads via
//! `NavCredentials::load_from_keychain`.
//!
//! # PR-46α / session-62 — shared core
//!
//! The CLI `run()` and the new `POST /api/setup-nav-credentials` HTTP
//! route both write the same keychain entry. The actual write is
//! factored into [`setup_credentials_from_inputs`] (mirrors the
//! A159 / A162 / A163 extract-library-helper pattern) so a single
//! validate-then-write implementation backs both flows. The CLI's
//! interactive prompts populate a [`NavCredentialInputs`] struct then
//! call the shared core; the HTTP route deserialises the request body
//! straight into [`NavCredentialInputs`] and calls the same core.
//!
//! # PR-57 / session-77 — consolidated keychain blob
//!
//! Pre-PR-57 the shared core wrote four separate keychain items (one
//! per artifact). This meant a freshly-rebuilt binary paid FOUR ACL
//! prompts on first boot (the macOS keychain re-prompts on a changed
//! binary signature; the prompt count scales with the number of items
//! touched). PR-57 consolidates all four into ONE JSON-encoded entry
//! `nav_credentials_blob`; the shared core now calls
//! [`aberp_nav_transport::credentials::keychain::write_blob`] once and
//! best-effort deletes the four legacy entries (idempotent — see
//! [`delete_legacy_items_best_effort`]). The boot-time read path on
//! the same blob costs ONE ACL prompt per rebuild instead of four.
//!
//! # Why this lives in `apps/aberp/src/` and not in `crates/nav-transport`
//!
//! `aberp-nav-transport` is a library; the keychain WRITE path (as
//! distinct from the read path the library already owns) is an
//! operator-tooling concern that belongs at the binary boundary. The
//! library exposes the `service_name` + item-name constants so this
//! file does not duplicate the convention.

use std::io::{BufRead as _, Write as _};

use aberp_nav_transport::credentials::keychain::{
    delete_legacy_items_best_effort, service_name, write_blob, ITEM_NAV_CREDENTIALS_BLOB,
};
use anyhow::{anyhow, Context as _, Result};

use crate::cli::SetupNavCredentialsArgs;

/// One of the four NAV credential prompts the CLI emits in fixed
/// order. The four values are bundled into a [`NavCredentialInputs`]
/// and written as a single keychain blob per PR-57.
struct ItemPrompt {
    label: &'static str,
    /// Whether the input is a secret — controls whether the prompt
    /// warns the operator about clear-text echo. (We can't disable
    /// echo without an unsafe stty call; the warning is the next best
    /// thing.)
    is_secret: bool,
}

const PROMPTS: [ItemPrompt; 4] = [
    ItemPrompt {
        label: "Technical-user login",
        is_secret: false,
    },
    ItemPrompt {
        label: "Technical-user password",
        is_secret: true,
    },
    ItemPrompt {
        label: "xmlSignKey",
        is_secret: true,
    },
    ItemPrompt {
        label: "xmlChangeKey (16 bytes — AES-128 key)",
        is_secret: true,
    },
];

/// PR-46α / session-62 — bundled inputs for the shared
/// [`setup_credentials_from_inputs`] core. Mirrors the four NAV
/// credential artifacts; the CLI populates it from stdin, the HTTP
/// route deserialises the request body straight into it.
///
/// The struct does NOT carry the tenant — the tenant is the surface-
/// level parameter, threaded as `&str` to the core function so the CLI
/// args struct and the HTTP route's `state.tenant` can both supply
/// theirs without lifting `TenantId` to the shared seam.
#[derive(Debug, Clone)]
pub struct NavCredentialInputs {
    pub technical_user_login: String,
    pub technical_user_password: String,
    pub xml_sign_key: String,
    pub xml_change_key: String,
}

/// PR-46α / session-62 — typed error from
/// [`setup_credentials_from_inputs`]. Two arms so the HTTP route can
/// map `Validation` → 400 and `Backend` → 500; the CLI wraps both
/// into `anyhow::Error` for its existing surface.
#[derive(Debug)]
pub enum SetupCredentialsError {
    /// One of the four inputs was empty (after trim). The associated
    /// string names the offending field in operator-readable form.
    Validation(String),
    /// The OS keychain backend itself errored (locked keychain,
    /// permission denied, unsupported platform).
    Backend(anyhow::Error),
}

impl std::fmt::Display for SetupCredentialsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupCredentialsError::Validation(msg) => write!(f, "{msg}"),
            SetupCredentialsError::Backend(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SetupCredentialsError {}

/// PR-46α / session-62 — shared core that writes the four NAV
/// credential entries to the OS keychain for a tenant. Backs both the
/// CLI `aberp setup-nav-credentials` subcommand AND the
/// `POST /api/setup-nav-credentials` HTTP route the SPA's first-run
/// setup wizard hits.
///
/// Validates that all four inputs are non-empty (after trim) per
/// CLAUDE.md rule 12 — a half-populated keychain produces a hard
/// error downstream at `NavCredentials::load_from_keychain`, so we
/// refuse to write partial state at this seam. Writes overwrite any
/// existing entries (the CLI's `--refuse-overwrite` flag is CLI-only
/// scope; the HTTP route is for first-run setup OR re-credentialing,
/// both of which want overwrite semantics).
pub fn setup_credentials_from_inputs(
    tenant: &str,
    inputs: &NavCredentialInputs,
) -> std::result::Result<(), SetupCredentialsError> {
    validate_input(&inputs.technical_user_login, "Technical-user login")?;
    validate_input(&inputs.technical_user_password, "Technical-user password")?;
    validate_input(&inputs.xml_sign_key, "xmlSignKey")?;
    validate_input(&inputs.xml_change_key, "xmlChangeKey")?;

    // PR-57 / session-77 — write the consolidated blob (ONE keychain
    // item) instead of four per-artifact entries. Boot-time read cost
    // drops from 4 ACL prompts to 1 on a freshly-rebuilt binary.
    write_blob(
        tenant,
        &inputs.technical_user_login,
        &inputs.technical_user_password,
        &inputs.xml_sign_key,
        &inputs.xml_change_key,
    )
    .map_err(|e| {
        SetupCredentialsError::Backend(anyhow!(
            "write NAV credentials blob to OS keychain for tenant `{tenant}`: {e}"
        ))
    })?;

    // Best-effort cleanup of any pre-PR-57 legacy entries on this
    // tenant. Idempotent — absent entries are a no-op. Backend errors
    // on delete are logged via `tracing` (not propagated) so a partial
    // failure does NOT undo the successful blob write.
    delete_legacy_items_best_effort(tenant);

    Ok(())
}

fn validate_input(
    value: &str,
    label: &'static str,
) -> std::result::Result<(), SetupCredentialsError> {
    if value.trim().is_empty() {
        return Err(SetupCredentialsError::Validation(format!(
            "{label} is required"
        )));
    }
    Ok(())
}

pub fn run(args: &SetupNavCredentialsArgs) -> Result<()> {
    let service = service_name(&args.tenant);
    eprintln!(
        "Populating NAV credentials for tenant `{}` (service `{}`, item `{}`).",
        args.tenant, service, ITEM_NAV_CREDENTIALS_BLOB,
    );

    // PR-57 / session-77 — under the consolidated-blob model, the
    // `--refuse-overwrite` flag is a single-decision check at the start
    // (does the blob already exist?), not a per-field skip across the
    // prompt loop. Pre-PR-57 the operator could skip individual
    // fields; that surface was an artifact of having four entries and
    // is gone with the blob.
    if args.refuse_overwrite && blob_already_populated(&args.tenant)? {
        eprintln!(
            "(--refuse-overwrite set: keychain item `{ITEM_NAV_CREDENTIALS_BLOB}` \
             already populated for service `{service}`; refusing to replace. \
             Re-run without --refuse-overwrite to rotate the blob, or use the \
             SPA's Settings → NAV Credentials → Rotate flow.)"
        );
        return Ok(());
    }
    eprintln!("(Existing keychain blob WILL be replaced; pass --refuse-overwrite to opt out.)");

    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut values: [String; 4] = Default::default();

    for (idx, prompt) in PROMPTS.iter().enumerate() {
        // Prompt to stderr (not stdout) so a stdin-redirected pipe
        // doesn't see the prompt text in its own output stream.
        if prompt.is_secret {
            eprintln!(
                "  [{}] (SECRET — input echoes in clear text; pipe from a file for production):",
                prompt.label,
            );
        } else {
            eprintln!("  [{}]:", prompt.label);
        }
        std::io::stderr().flush().ok();

        let mut buf = String::new();
        let n = stdin_lock
            .read_line(&mut buf)
            .with_context(|| format!("read {} from stdin", prompt.label))?;
        if n == 0 {
            return Err(anyhow!(
                "stdin closed before all four credentials were read \
                 (expected 4, got {idx})"
            ));
        }
        // Trim only trailing newline / CR; do not touch leading
        // whitespace in case it is part of the value (NAV-generated
        // keys don't carry whitespace, but stay defensive).
        values[idx] = buf.trim_end_matches(['\n', '\r']).to_string();
    }

    let inputs = NavCredentialInputs {
        technical_user_login: values[0].clone(),
        technical_user_password: values[1].clone(),
        xml_sign_key: values[2].clone(),
        xml_change_key: values[3].clone(),
    };
    setup_credentials_from_inputs(&args.tenant, &inputs)
        .map_err(|e| anyhow!("setup_credentials_from_inputs: {e}"))?;

    eprintln!(
        "Done: NAV credentials blob written for service `{service}`, \
         item `{ITEM_NAV_CREDENTIALS_BLOB}` (legacy per-artifact entries \
         deleted if present)."
    );

    Ok(())
}

/// PR-57 / session-77 — probe ONLY the consolidated blob entry for
/// existence. Used by the CLI's `--refuse-overwrite` gate. Backend
/// failures are loud per CLAUDE.md rule 12 — silent fall-through to
/// "blob absent → overwrite anyway" would mask a locked-keychain
/// situation as a successful overwrite of a credential the operator
/// thought was protected.
fn blob_already_populated(tenant: &str) -> Result<bool> {
    let service = service_name(tenant);
    let entry = keyring::Entry::new(&service, ITEM_NAV_CREDENTIALS_BLOB).with_context(|| {
        format!("probe keychain entry for service `{service}` item `{ITEM_NAV_CREDENTIALS_BLOB}`")
    })?;
    match entry.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(other) => Err(anyhow!(
            "keychain probe failed for service `{service}` item `{ITEM_NAV_CREDENTIALS_BLOB}`: {other}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PR-46α / session-62 — validation rejects an empty
    /// `technical_user_login`. The other three fields share the same
    /// validator; one pin per field would be tautological. Covers
    /// the CLI + HTTP-route 400 branch.
    #[test]
    fn setup_credentials_rejects_empty_login() {
        let inputs = NavCredentialInputs {
            technical_user_login: "  ".to_string(),
            technical_user_password: "pw".to_string(),
            xml_sign_key: "sk".to_string(),
            xml_change_key: "ck".to_string(),
        };
        let err = setup_credentials_from_inputs("t-validation", &inputs)
            .expect_err("blank login must fail validation");
        match err {
            SetupCredentialsError::Validation(msg) => {
                assert!(msg.contains("Technical-user login"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// PR-46α / session-62 — validation rejects an empty
    /// `xml_change_key` (the LAST field). Guards against a regression
    /// that drops a validate call from the chain.
    #[test]
    fn setup_credentials_rejects_empty_change_key() {
        let inputs = NavCredentialInputs {
            technical_user_login: "lg".to_string(),
            technical_user_password: "pw".to_string(),
            xml_sign_key: "sk".to_string(),
            xml_change_key: "".to_string(),
        };
        let err = setup_credentials_from_inputs("t-validation", &inputs)
            .expect_err("blank change_key must fail validation");
        match err {
            SetupCredentialsError::Validation(msg) => {
                assert!(msg.contains("xmlChangeKey"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
