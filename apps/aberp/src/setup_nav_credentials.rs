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
//! Both flows write the same keychain entries (`aberp.nav.<tenant>` /
//! `technical_user.login` etc.) the production `submit-invoice` path
//! reads via `NavCredentials::load_from_keychain`.
//!
//! # PR-46α / session-62 — shared core
//!
//! The CLI `run()` and the new `POST /api/setup-nav-credentials` HTTP
//! route both write the same four keychain entries. The actual write
//! is factored into [`setup_credentials_from_inputs`] (mirrors the
//! A159 / A162 / A163 extract-library-helper pattern) so a single
//! validate-then-write implementation backs both flows. The CLI's
//! interactive prompts populate a [`NavCredentialInputs`] struct then
//! call the shared core; the HTTP route deserialises the request body
//! straight into [`NavCredentialInputs`] and calls the same core.
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
    service_name, ITEM_CHANGE_KEY, ITEM_LOGIN, ITEM_PASSWORD, ITEM_SIGN_KEY,
};
use anyhow::{anyhow, Context as _, Result};

use crate::cli::SetupNavCredentialsArgs;

/// One of the four NAV keychain items. Hand-listed (not derived from
/// the strings in `nav_transport::credentials::keychain`) so the prompt
/// labels are operator-readable rather than the on-disk form.
struct ItemPrompt {
    label: &'static str,
    storage_name: &'static str,
    /// Whether the input is a secret — controls whether the prompt
    /// warns the operator about clear-text echo. (We can't disable
    /// echo without an unsafe stty call; the warning is the next best
    /// thing.)
    is_secret: bool,
}

const PROMPTS: [ItemPrompt; 4] = [
    ItemPrompt {
        label: "Technical-user login",
        storage_name: ITEM_LOGIN,
        is_secret: false,
    },
    ItemPrompt {
        label: "Technical-user password",
        storage_name: ITEM_PASSWORD,
        is_secret: true,
    },
    ItemPrompt {
        label: "xmlSignKey",
        storage_name: ITEM_SIGN_KEY,
        is_secret: true,
    },
    ItemPrompt {
        label: "xmlChangeKey (16 bytes — AES-128 key)",
        storage_name: ITEM_CHANGE_KEY,
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

    let service = service_name(tenant);
    write_entry(&service, ITEM_LOGIN, &inputs.technical_user_login)?;
    write_entry(&service, ITEM_PASSWORD, &inputs.technical_user_password)?;
    write_entry(&service, ITEM_SIGN_KEY, &inputs.xml_sign_key)?;
    write_entry(&service, ITEM_CHANGE_KEY, &inputs.xml_change_key)?;
    Ok(())
}

fn validate_input(value: &str, label: &'static str) -> std::result::Result<(), SetupCredentialsError> {
    if value.trim().is_empty() {
        return Err(SetupCredentialsError::Validation(format!(
            "{label} is required"
        )));
    }
    Ok(())
}

fn write_entry(
    service: &str,
    item: &'static str,
    value: &str,
) -> std::result::Result<(), SetupCredentialsError> {
    let entry = keyring::Entry::new(service, item).map_err(|e| {
        SetupCredentialsError::Backend(anyhow!(
            "open keychain entry for service `{service}` item `{item}`: {e}"
        ))
    })?;
    entry.set_password(value).map_err(|e| {
        SetupCredentialsError::Backend(anyhow!(
            "write keychain entry `{item}` for service `{service}`: {e}"
        ))
    })
}

pub fn run(args: &SetupNavCredentialsArgs) -> Result<()> {
    let service = service_name(&args.tenant);
    eprintln!(
        "Populating NAV credentials for tenant `{}` (service `{}`).",
        args.tenant, service,
    );
    if args.refuse_overwrite {
        eprintln!("(--refuse-overwrite set: existing keychain entries will NOT be replaced.)");
    } else {
        eprintln!(
            "(Existing keychain entries WILL be replaced; pass --refuse-overwrite to opt out.)"
        );
    }

    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut values: [String; 4] = Default::default();
    let mut skipped = 0usize;
    let mut populated = 0usize;

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
        let value = buf.trim_end_matches(['\n', '\r']).to_string();

        if args.refuse_overwrite && keychain_entry_exists(&service, prompt.storage_name)? {
            eprintln!(
                "    → {} already populated; skipping (--refuse-overwrite)",
                prompt.storage_name,
            );
            skipped += 1;
            values[idx] = String::new();
            continue;
        }
        values[idx] = value;
        populated += 1;
    }

    // PR-46α / session-62 — route through the shared core when we have
    // all four values to write. For the `--refuse-overwrite` mix-and-
    // match case (some entries skipped) we cannot use the shared core
    // because it refuses empty inputs; fall back to per-entry writes
    // for those slots that survived the skip check.
    if skipped == 0 {
        let inputs = NavCredentialInputs {
            technical_user_login: values[0].clone(),
            technical_user_password: values[1].clone(),
            xml_sign_key: values[2].clone(),
            xml_change_key: values[3].clone(),
        };
        setup_credentials_from_inputs(&args.tenant, &inputs)
            .map_err(|e| anyhow!("setup_credentials_from_inputs: {e}"))?;
    } else {
        // --refuse-overwrite hit at least one entry; write per-slot
        // for the slots that survived. Validation still applies to
        // each non-skipped value (empty → loud-fail).
        for (idx, prompt) in PROMPTS.iter().enumerate() {
            if values[idx].is_empty() {
                continue;
            }
            validate_input(&values[idx], prompt.label)
                .map_err(|e| anyhow!("setup_credentials_from_inputs: {e}"))?;
            write_entry(&service, prompt.storage_name, &values[idx])
                .map_err(|e| anyhow!("setup_credentials_from_inputs: {e}"))?;
        }
    }

    eprintln!(
        "Done: {populated} written, {skipped} skipped \
         (service `{service}`, 4 expected total)",
    );

    // Loud failure if the on-disk state is not "all four populated".
    // The submit path's `NavCredentials::load_from_keychain` would
    // surface this anyway as `KeychainItemMissing`, but failing here
    // means the operator does not get a confused "I just ran setup
    // and submit still says missing" loop.
    let all_present = PROMPTS
        .iter()
        .all(|p| keychain_entry_exists(&service, p.storage_name).unwrap_or(false));
    if !all_present {
        return Err(anyhow!(
            "after setup, one or more of the four NAV keychain items \
             is still not present for tenant `{}`; rerun without \
             --refuse-overwrite to fill the gaps",
            args.tenant
        ));
    }

    Ok(())
}

/// Probe the keychain for an entry's existence without printing the
/// value. Returns `false` for both "no such entry" and "backend
/// returned NoEntry"; returns the underlying error for any other
/// failure (locked keychain, permission denied) — loud per
/// CLAUDE.md rule 12.
fn keychain_entry_exists(service: &str, item: &'static str) -> Result<bool> {
    let entry = keyring::Entry::new(service, item)
        .with_context(|| format!("probe keychain entry for service `{service}` item `{item}`"))?;
    match entry.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(other) => Err(anyhow!(
            "keychain probe failed for service `{service}` item `{item}`: {other}"
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
