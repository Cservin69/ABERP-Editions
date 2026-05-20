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
    let mut populated = 0usize;
    let mut skipped = 0usize;

    for prompt in &PROMPTS {
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
                 (expected 4, got {populated})"
            ));
        }
        // Trim only trailing newline / CR; do not touch leading
        // whitespace in case it is part of the value (NAV-generated
        // keys don't carry whitespace, but stay defensive).
        let value = buf.trim_end_matches(['\n', '\r']);

        if args.refuse_overwrite && keychain_entry_exists(&service, prompt.storage_name)? {
            eprintln!(
                "    → {} already populated; skipping (--refuse-overwrite)",
                prompt.storage_name,
            );
            skipped += 1;
            continue;
        }

        let entry = keyring::Entry::new(&service, prompt.storage_name).with_context(|| {
            format!(
                "open keychain entry for service `{service}` item `{}`",
                prompt.storage_name
            )
        })?;
        entry
            .set_password(value)
            .with_context(|| format!("write keychain entry `{}`", prompt.storage_name))?;
        populated += 1;
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
