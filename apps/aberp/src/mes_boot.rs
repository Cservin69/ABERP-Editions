//! Stage 3 manufacturing-adapter boot config.
//!
//! S229/S250 wired adapters straight from env vars at boot. S257 /
//! PR-246 moves the source of truth to the `[[mes.adapters]]`
//! seller.toml slot ([[trust-code-not-operator]] — operators manage
//! adapters from Settings → Adapters, never by editing env + restart).
//!
//! This module now does ONE thing at boot: a **one-shot, idempotent
//! migration** of any env-defined adapter into the TOML. After
//! migration the env vars are ignored — the persisted config is
//! authoritative and [`crate::mes_manager::AdapterManager::boot_from_toml`]
//! starts every persisted adapter. An operator who had
//! `ABERP_ZEBRA_ENABLED=true` set sees that printer appear in the
//! Adapters page on first boot post-S257 with no action required, and a
//! second boot does not duplicate it (the migration upserts by
//! `adapter_id`).
//!
//! [`MesBootDeps`] still bundles the ledger-writer + audit dependencies
//! the manager threads into each spawned adapter task.
//!
//! ## DoS bounds stay compiled-in
//!
//! Per [[trust-code-not-operator]] the DoS bounds (`max_payload_len`,
//! `max_concurrent_connections`, `max_frame_bytes`, timeouts, …) are
//! NOT operator-exposed — neither here nor in the TOML. Only the
//! operator-meaningful identity + endpoint fields persist.

use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_mes::{
    AdapterConfigEntry, AdapterKind, DEFAULT_LISTEN_PORT, MTCONNECT_DEFAULT_AGENT_PORT,
    UR_RTDE_DEFAULT_PORT, ZEBRA_DEFAULT_LISTEN_PORT,
};

use crate::mes_adapters_config;

const ENV_BARCODE_ENABLED: &str = "ABERP_BARCODE_SCANNER_ENABLED";
const ENV_BARCODE_ID: &str = "ABERP_BARCODE_SCANNER_ID";
const ENV_BARCODE_HOST: &str = "ABERP_BARCODE_SCANNER_HOST";
const ENV_BARCODE_PORT: &str = "ABERP_BARCODE_SCANNER_PORT";

const ENV_ZEBRA_ENABLED: &str = "ABERP_ZEBRA_ENABLED";
const ENV_ZEBRA_PRINTER_ID: &str = "ABERP_ZEBRA_PRINTER_ID";
const ENV_ZEBRA_FRIENDLY_NAME: &str = "ABERP_ZEBRA_FRIENDLY_NAME";
const ENV_ZEBRA_HOST: &str = "ABERP_ZEBRA_HOST";
const ENV_ZEBRA_PORT: &str = "ABERP_ZEBRA_PORT";

const ENV_MTCONNECT_ENABLED: &str = "ABERP_MTCONNECT_ENABLED";
const ENV_MTCONNECT_MACHINE_ID: &str = "ABERP_MTCONNECT_MACHINE_ID";
const ENV_MTCONNECT_FRIENDLY_NAME: &str = "ABERP_MTCONNECT_FRIENDLY_NAME";
const ENV_MTCONNECT_HOST: &str = "ABERP_MTCONNECT_HOST";
const ENV_MTCONNECT_PORT: &str = "ABERP_MTCONNECT_PORT";
const ENV_MTCONNECT_DEVICE_NAME: &str = "ABERP_MTCONNECT_DEVICE_NAME";

const ENV_UR_RTDE_ENABLED: &str = "ABERP_UR_RTDE_ENABLED";
const ENV_UR_RTDE_ROBOT_ID: &str = "ABERP_UR_RTDE_ROBOT_ID";
const ENV_UR_RTDE_FRIENDLY_NAME: &str = "ABERP_UR_RTDE_FRIENDLY_NAME";
const ENV_UR_RTDE_HOST: &str = "ABERP_UR_RTDE_HOST";
const ENV_UR_RTDE_PORT: &str = "ABERP_UR_RTDE_PORT";
const ENV_UR_RTDE_MODEL: &str = "ABERP_UR_RTDE_MODEL";

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_SCANNER_ID: &str = "barcode-scanner-default";
const DEFAULT_ZEBRA_PRINTER_ID: &str = "label-printer-default";
const DEFAULT_ZEBRA_FRIENDLY_NAME: &str = "Label Printer";
const DEFAULT_MTCONNECT_MACHINE_ID: &str = "cnc-default";
const DEFAULT_MTCONNECT_FRIENDLY_NAME: &str = "CNC Machine";
const DEFAULT_MTCONNECT_DEVICE_NAME: &str = "default";
const DEFAULT_UR_RTDE_ROBOT_ID: &str = "robot-default";
const DEFAULT_UR_RTDE_FRIENDLY_NAME: &str = "UR Robot";
const DEFAULT_UR_RTDE_MODEL: &str = "UR";

/// Shared dependencies the MES boot + CRUD paths thread into each
/// spawned ledger-writer task and every audit append. Built from the
/// boot call site (`db_path` / `tenant` / `binary_hash`) + operator
/// session info.
#[derive(Clone)]
pub struct MesBootDeps {
    /// The ONE shared DuckDB handle (ADR-0098 Gap 1a). Replaces the old
    /// `db_path`: the ledger-writer now appends on this shared instance
    /// rather than opening its own connection per event, so two adapters
    /// can no longer read the same chain head and fork it (ADR-0104).
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub session_id: String,
}

/// One-shot, idempotent migration of env-defined adapters into the
/// `[[mes.adapters]]` TOML slot. Returns the number of NEW entries
/// added (0 on a re-boot where everything is already migrated).
///
/// Idempotency is by `adapter_id` absence: an env adapter whose id
/// already has a TOML row is left untouched, so re-running on every
/// boot never duplicates. (Chosen over an audit sentinel — the
/// id-absence check is simpler, needs no ledger read, and is robust to
/// an operator who hand-deletes a row and re-enables the env var,
/// which correctly re-adds it.)
pub fn migrate_env_adapters_to_toml(seller_toml_path: &Path) -> Result<usize> {
    let mut entries = mes_adapters_config::read_mes_adapters(seller_toml_path)
        .context("read existing [[mes.adapters]] for env migration")?;
    let mut added = 0usize;
    for env_entry in env_adapter_entries()? {
        if entries.iter().any(|e| e.adapter_id == env_entry.adapter_id) {
            continue; // already migrated — idempotent
        }
        tracing::info!(
            adapter_id = %env_entry.adapter_id,
            kind = %env_entry.kind.wire_str(),
            "migrating env-defined MES adapter into [[mes.adapters]] (S257)"
        );
        entries.push(env_entry);
        added += 1;
    }
    if added > 0 {
        mes_adapters_config::write_mes_adapters_section(seller_toml_path, &entries)
            .context("persist migrated env adapters into [[mes.adapters]]")?;
    }
    Ok(added)
}

/// Collect a config entry for each enabled env adapter. A misconfigured
/// adapter (blank id, malformed port) fails the whole migration loud
/// per CLAUDE.md rule 12 — better to refuse boot than silently drop an
/// adapter the operator's env asked for.
fn env_adapter_entries() -> Result<Vec<AdapterConfigEntry>> {
    let mut out = Vec::new();
    if env_bool_true(ENV_BARCODE_ENABLED) {
        out.push(read_barcode_entry_from_env().context("read barcode-scanner env config")?);
    }
    if env_bool_true(ENV_ZEBRA_ENABLED) {
        out.push(read_zebra_entry_from_env().context("read Zebra env config")?);
    }
    if env_bool_true(ENV_MTCONNECT_ENABLED) {
        out.push(read_mtconnect_entry_from_env().context("read MTConnect env config")?);
    }
    if env_bool_true(ENV_UR_RTDE_ENABLED) {
        out.push(read_ur_rtde_entry_from_env().context("read UR RTDE env config")?);
    }
    Ok(out)
}

fn env_bool_true(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_port(key: &str, default: u16) -> Result<u16> {
    match std::env::var(key) {
        Ok(s) => s.parse::<u16>().with_context(|| format!("parse {key}={s}")),
        Err(_) => Ok(default),
    }
}

fn read_barcode_entry_from_env() -> Result<AdapterConfigEntry> {
    let adapter_id =
        std::env::var(ENV_BARCODE_ID).unwrap_or_else(|_| DEFAULT_SCANNER_ID.to_string());
    if adapter_id.trim().is_empty() {
        return Err(anyhow!("{ENV_BARCODE_ID} is empty"));
    }
    let host = std::env::var(ENV_BARCODE_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    // Validate the listen address parses now so a bad env value fails
    // the migration loud rather than at first boot_from_toml.
    std::net::IpAddr::from_str(host.trim())
        .with_context(|| format!("parse {ENV_BARCODE_HOST}={host}"))?;
    let port = env_port(ENV_BARCODE_PORT, DEFAULT_LISTEN_PORT)?;
    Ok(AdapterConfigEntry {
        kind: AdapterKind::BarcodeScanner,
        friendly_name: adapter_id.clone(),
        adapter_id,
        host,
        port,
        device_name: None,
        model: None,
    })
}

fn read_zebra_entry_from_env() -> Result<AdapterConfigEntry> {
    let adapter_id = std::env::var(ENV_ZEBRA_PRINTER_ID)
        .unwrap_or_else(|_| DEFAULT_ZEBRA_PRINTER_ID.to_string());
    if adapter_id.trim().is_empty() {
        return Err(anyhow!("{ENV_ZEBRA_PRINTER_ID} is empty"));
    }
    let friendly_name = std::env::var(ENV_ZEBRA_FRIENDLY_NAME)
        .unwrap_or_else(|_| DEFAULT_ZEBRA_FRIENDLY_NAME.to_string());
    let host = std::env::var(ENV_ZEBRA_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    if host.trim().is_empty() {
        return Err(anyhow!("{ENV_ZEBRA_HOST} is empty"));
    }
    let port = env_port(ENV_ZEBRA_PORT, ZEBRA_DEFAULT_LISTEN_PORT)?;
    Ok(AdapterConfigEntry {
        kind: AdapterKind::LabelPrinter,
        adapter_id,
        friendly_name,
        host,
        port,
        device_name: None,
        model: None,
    })
}

fn read_mtconnect_entry_from_env() -> Result<AdapterConfigEntry> {
    let adapter_id = std::env::var(ENV_MTCONNECT_MACHINE_ID)
        .unwrap_or_else(|_| DEFAULT_MTCONNECT_MACHINE_ID.to_string());
    if adapter_id.trim().is_empty() {
        return Err(anyhow!("{ENV_MTCONNECT_MACHINE_ID} is empty"));
    }
    let friendly_name = std::env::var(ENV_MTCONNECT_FRIENDLY_NAME)
        .unwrap_or_else(|_| DEFAULT_MTCONNECT_FRIENDLY_NAME.to_string());
    let host = std::env::var(ENV_MTCONNECT_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    if host.trim().is_empty() {
        return Err(anyhow!("{ENV_MTCONNECT_HOST} is empty"));
    }
    let port = env_port(ENV_MTCONNECT_PORT, MTCONNECT_DEFAULT_AGENT_PORT)?;
    let device_name = std::env::var(ENV_MTCONNECT_DEVICE_NAME)
        .unwrap_or_else(|_| DEFAULT_MTCONNECT_DEVICE_NAME.to_string());
    if device_name.trim().is_empty() {
        return Err(anyhow!("{ENV_MTCONNECT_DEVICE_NAME} is empty"));
    }
    Ok(AdapterConfigEntry {
        kind: AdapterKind::Cnc,
        adapter_id,
        friendly_name,
        host,
        port,
        device_name: Some(device_name),
        model: None,
    })
}

fn read_ur_rtde_entry_from_env() -> Result<AdapterConfigEntry> {
    let adapter_id = std::env::var(ENV_UR_RTDE_ROBOT_ID)
        .unwrap_or_else(|_| DEFAULT_UR_RTDE_ROBOT_ID.to_string());
    if adapter_id.trim().is_empty() {
        return Err(anyhow!("{ENV_UR_RTDE_ROBOT_ID} is empty"));
    }
    let friendly_name = std::env::var(ENV_UR_RTDE_FRIENDLY_NAME)
        .unwrap_or_else(|_| DEFAULT_UR_RTDE_FRIENDLY_NAME.to_string());
    let host = std::env::var(ENV_UR_RTDE_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    if host.trim().is_empty() {
        return Err(anyhow!("{ENV_UR_RTDE_HOST} is empty"));
    }
    let port = env_port(ENV_UR_RTDE_PORT, UR_RTDE_DEFAULT_PORT)?;
    let model =
        std::env::var(ENV_UR_RTDE_MODEL).unwrap_or_else(|_| DEFAULT_UR_RTDE_MODEL.to_string());
    Ok(AdapterConfigEntry {
        kind: AdapterKind::Robot,
        adapter_id,
        friendly_name,
        host,
        port,
        device_name: None,
        model: Some(model),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // env vars are process-global; serialise the env-touching tests.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ALL_ENV_KEYS: &[&str] = &[
        ENV_BARCODE_ENABLED,
        ENV_BARCODE_ID,
        ENV_BARCODE_HOST,
        ENV_BARCODE_PORT,
        ENV_ZEBRA_ENABLED,
        ENV_ZEBRA_PRINTER_ID,
        ENV_ZEBRA_FRIENDLY_NAME,
        ENV_ZEBRA_HOST,
        ENV_ZEBRA_PORT,
        ENV_MTCONNECT_ENABLED,
        ENV_MTCONNECT_MACHINE_ID,
        ENV_MTCONNECT_FRIENDLY_NAME,
        ENV_MTCONNECT_HOST,
        ENV_MTCONNECT_PORT,
        ENV_MTCONNECT_DEVICE_NAME,
        ENV_UR_RTDE_ENABLED,
        ENV_UR_RTDE_ROBOT_ID,
        ENV_UR_RTDE_FRIENDLY_NAME,
        ENV_UR_RTDE_HOST,
        ENV_UR_RTDE_PORT,
        ENV_UR_RTDE_MODEL,
    ];

    fn clear_env() {
        for k in ALL_ENV_KEYS {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn no_env_means_no_entries() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(env_adapter_entries().unwrap().is_empty());
    }

    #[test]
    fn enabled_env_maps_to_typed_entries_with_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_ZEBRA_ENABLED, "true");
        std::env::set_var(ENV_UR_RTDE_ENABLED, "TRUE");
        let entries = env_adapter_entries().unwrap();
        clear_env();
        assert_eq!(entries.len(), 2);
        let zebra = entries
            .iter()
            .find(|e| e.kind == AdapterKind::LabelPrinter)
            .unwrap();
        assert_eq!(zebra.adapter_id, DEFAULT_ZEBRA_PRINTER_ID);
        assert_eq!(zebra.friendly_name, DEFAULT_ZEBRA_FRIENDLY_NAME);
        assert_eq!(zebra.host, DEFAULT_HOST);
        assert_eq!(zebra.port, ZEBRA_DEFAULT_LISTEN_PORT);
        let robot = entries
            .iter()
            .find(|e| e.kind == AdapterKind::Robot)
            .unwrap();
        assert_eq!(robot.adapter_id, DEFAULT_UR_RTDE_ROBOT_ID);
        assert_eq!(robot.model.as_deref(), Some(DEFAULT_UR_RTDE_MODEL));
    }

    #[test]
    fn barcode_env_rejects_non_ip_host() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_BARCODE_ENABLED, "true");
        std::env::set_var(ENV_BARCODE_HOST, "not-an-ip");
        let err = env_adapter_entries().unwrap_err();
        clear_env();
        assert!(
            err.to_string().contains(ENV_BARCODE_HOST) || format!("{err:#}").contains("barcode")
        );
    }

    #[test]
    fn migration_is_idempotent_by_adapter_id() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_MTCONNECT_ENABLED, "true");
        std::env::set_var(ENV_MTCONNECT_MACHINE_ID, "cnc-line-a");
        std::env::set_var(ENV_MTCONNECT_HOST, "10.0.0.20");

        let dir = std::env::temp_dir().join(format!("aberp-migrate-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seller.toml");

        let first = migrate_env_adapters_to_toml(&path).unwrap();
        assert_eq!(first, 1, "first migration adds the env adapter");
        let second = migrate_env_adapters_to_toml(&path).unwrap();
        assert_eq!(second, 0, "second migration is a no-op (idempotent)");

        let entries = mes_adapters_config::read_mes_adapters(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].adapter_id, "cnc-line-a");
        assert_eq!(entries[0].host, "10.0.0.20");
        assert_eq!(entries[0].kind, AdapterKind::Cnc);

        clear_env();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Migration preserves an operator's hand-added adapter alongside the
    /// migrated env one (no clobber of the other [[mes.adapters]] rows).
    #[test]
    fn migration_preserves_existing_adapters() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("aberp-migrate2-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seller.toml");

        // Pre-seed a hand-added adapter.
        let preexisting = vec![AdapterConfigEntry {
            kind: AdapterKind::Robot,
            adapter_id: "robot-hand-added".to_string(),
            friendly_name: "Cell A".to_string(),
            host: "10.0.0.30".to_string(),
            port: 30004,
            device_name: None,
            model: Some("UR10e".to_string()),
        }];
        mes_adapters_config::write_mes_adapters_section(&path, &preexisting).unwrap();

        std::env::set_var(ENV_ZEBRA_ENABLED, "true");
        std::env::set_var(ENV_ZEBRA_PRINTER_ID, "lp-dispatch");
        let added = migrate_env_adapters_to_toml(&path).unwrap();
        assert_eq!(added, 1);

        let entries = mes_adapters_config::read_mes_adapters(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.adapter_id == "robot-hand-added"));
        assert!(entries.iter().any(|e| e.adapter_id == "lp-dispatch"));

        clear_env();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
