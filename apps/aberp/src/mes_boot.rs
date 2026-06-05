//! Stage 3 manufacturing-adapter framework boot wiring
//! (S229 / PR-225 / ADR-0060 Phase β + S250 / PR-242 — sweep of S249
//! finding 1 wires the Zebra / MTConnect / UR-RTDE adapters into the
//! binary alongside the original barcode scanner).
//!
//! Parses per-adapter env-var config, constructs the adapter, calls
//! `start()`, spawns the per-adapter ledger-writer task, and spawns a
//! cancellation-watcher that calls `adapter.stop()` when the shutdown
//! coordinator's root token fires.
//!
//! ## Why env vars and not seller.toml
//!
//! Per ADR-0060 §"Open questions → Operator-configurable adapter
//! registration", a `[mes]` section in `seller.toml` is the documented
//! long-term home. Landing it requires updating the four
//! [[seller-toml-write-invariant]] preservation paths (identity /
//! banks / smtp / numbering) AND the snapshot tool AND the runbook —
//! a substantial PR in its own right, deliberately separated from
//! Phase β per [[pushback-as-method]].
//!
//! Phase β uses env vars to gate adapter presence. The pattern mirrors
//! `ABERP_QUOTE_INTAKE_ENABLED=true` (S210). Default-off; production
//! runs that don't set the env var see no adapter, no port bound, no
//! ledger writer. Per [[trust-code-not-operator]] the DoS bounds
//! (`max_payload_len`, `max_concurrent_connections`, `max_frame_bytes`,
//! `handshake_timeout`, …) are NOT exposed as env vars — only the
//! operator-meaningful identity + endpoint fields.
//!
//! ## Single-instance per adapter type (S250 deliberate)
//!
//! S250 wires one instance of each adapter type, mirroring the
//! barcode-scanner pattern exactly. The S249 brief floated an indexed
//! `ABERP_ZEBRA_HOST_<N>` shape as "probably" the right call; we
//! deliberately chose single-instance to stay symmetric with the
//! pre-existing barcode pattern. Multi-instance per type is future work
//! that should land alongside the `[mes]` seller.toml slot — env-var
//! indexing for ~10 printers per plant is ergonomically worse than a
//! TOML array. Flagged in the S250 report.

use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use aberp_audit_ledger::{BinaryHash, TenantId};
use aberp_mes::{
    spawn_ledger_writer, Adapter, AdapterRegistry, BarcodeScannerAdapter, BarcodeScannerConfig,
    LedgerWriterActor, LedgerWriterDeps, MtconnectAdapter, MtconnectAdapterConfig, UrRtdeAdapter,
    UrRtdeAdapterConfig, ZebraAdapter, ZebraAdapterConfig, DEFAULT_LISTEN_PORT,
    MTCONNECT_DEFAULT_AGENT_PORT, UR_RTDE_DEFAULT_PORT, ZEBRA_DEFAULT_LISTEN_PORT,
};

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

/// Shared dependencies the MES boot path threads into each spawned
/// ledger-writer task. Built from the existing `recovery_state` at
/// the boot call site (`db_path` / `tenant` / `binary_hash`) +
/// operator session info.
#[derive(Debug, Clone)]
pub struct MesBootDeps {
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub session_id: String,
}

/// Outcome of booting the MES adapter set: the spawned task handles
/// the caller must register with the shutdown coordinator. Labels are
/// `&'static str` to match the coordinator's `register` signature; the
/// per-adapter identity is logged separately at spawn time and is not
/// needed inside the shutdown summary line.
#[derive(Debug)]
pub struct SpawnedMesTasks {
    pub handles: Vec<(&'static str, JoinHandle<()>)>,
}

/// Boot the MES adapter set as configured by env vars. Returns
/// `Ok(None)` when no adapter is enabled — boot proceeds silently and
/// `registry` is left empty.
///
/// Each adapter type (barcode / Zebra / MTConnect / UR RTDE) is gated
/// by its own `ABERP_<TYPE>_ENABLED` flag and configured independently.
/// A misconfigured adapter (parse error, blank id) fails the whole
/// boot — per CLAUDE.md rule 12 we fail loud rather than silently
/// dropping an adapter the operator asked for.
///
/// On success the registered tasks fan out from `cancel`: every
/// spawned task respects `cancel.cancelled()` so a Tauri-window close
/// or a Ctrl-C exits within ms. The same started adapter is also
/// registered into `registry` so the Workshop dashboard route can
/// probe live health (S240 / PR-234).
pub async fn boot_mes_adapters(
    deps: MesBootDeps,
    cancel: CancellationToken,
    registry: Arc<RwLock<AdapterRegistry>>,
) -> Result<Option<SpawnedMesTasks>> {
    let mut handles: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

    if barcode_scanner_enabled() {
        handles.extend(boot_barcode_scanner(&deps, &cancel, &registry).await?);
    } else {
        tracing::info!(
            "MES barcode-scanner adapter disabled ({ENV_BARCODE_ENABLED} != true); skipping"
        );
    }

    if zebra_enabled() {
        handles.extend(boot_zebra(&deps, &cancel, &registry).await?);
    } else {
        tracing::info!(
            "MES Zebra label-printer adapter disabled ({ENV_ZEBRA_ENABLED} != true); skipping"
        );
    }

    if mtconnect_enabled() {
        handles.extend(boot_mtconnect(&deps, &cancel, &registry).await?);
    } else {
        tracing::info!(
            "MES MTConnect adapter disabled ({ENV_MTCONNECT_ENABLED} != true); skipping"
        );
    }

    if ur_rtde_enabled() {
        handles.extend(boot_ur_rtde(&deps, &cancel, &registry).await?);
    } else {
        tracing::info!(
            "MES Universal Robots RTDE adapter disabled ({ENV_UR_RTDE_ENABLED} != true); skipping"
        );
    }

    if handles.is_empty() {
        Ok(None)
    } else {
        Ok(Some(SpawnedMesTasks { handles }))
    }
}

// ===== Barcode scanner (S229 / PR-225) =====

async fn boot_barcode_scanner(
    deps: &MesBootDeps,
    cancel: &CancellationToken,
    registry: &Arc<RwLock<AdapterRegistry>>,
) -> Result<Vec<(&'static str, JoinHandle<()>)>> {
    let cfg =
        read_barcode_scanner_config_from_env().context("read barcode-scanner config from env")?;
    let scanner_id = cfg.scanner_id.clone();
    let listen_addr = cfg.listen_addr;
    let listen_port = cfg.listen_port;
    tracing::info!(
        scanner_id = %scanner_id,
        listen_addr = %listen_addr,
        listen_port,
        "spawning MES barcode-scanner adapter (S229 / PR-225)"
    );

    let adapter: Arc<BarcodeScannerAdapter> = Arc::new(BarcodeScannerAdapter::new(cfg));
    adapter
        .start()
        .await
        .with_context(|| format!("barcode scanner adapter '{scanner_id}' start failed"))?;

    register_started_adapter(registry, adapter.clone() as Arc<dyn Adapter>, &scanner_id)?;

    let writer_handle = spawn_writer(adapter.clone() as Arc<dyn Adapter>, deps, cancel);
    let stopper_handle = spawn_stopper(
        adapter.clone() as Arc<dyn Adapter>,
        cancel,
        "barcode-scanner",
    );

    Ok(vec![
        ("mes-barcode-scanner-writer", writer_handle),
        ("mes-barcode-scanner-stopper", stopper_handle),
    ])
}

// ===== Zebra label printer (S245 / PR-238) =====

async fn boot_zebra(
    deps: &MesBootDeps,
    cancel: &CancellationToken,
    registry: &Arc<RwLock<AdapterRegistry>>,
) -> Result<Vec<(&'static str, JoinHandle<()>)>> {
    let cfg = read_zebra_config_from_env().context("read Zebra label-printer config from env")?;
    let printer_id = cfg.printer_id.clone();
    let host = cfg.host.clone();
    let port = cfg.port;
    tracing::info!(
        printer_id = %printer_id,
        host = %host,
        port,
        "spawning MES Zebra label-printer adapter (S245 / PR-238)"
    );

    let adapter: Arc<ZebraAdapter> = Arc::new(ZebraAdapter::new(cfg));
    adapter
        .start()
        .await
        .with_context(|| format!("Zebra label-printer adapter '{printer_id}' start failed"))?;

    register_started_adapter(registry, adapter.clone() as Arc<dyn Adapter>, &printer_id)?;

    let writer_handle = spawn_writer(adapter.clone() as Arc<dyn Adapter>, deps, cancel);
    let stopper_handle = spawn_stopper(adapter.clone() as Arc<dyn Adapter>, cancel, "zebra");

    Ok(vec![
        ("mes-zebra-writer", writer_handle),
        ("mes-zebra-stopper", stopper_handle),
    ])
}

// ===== MTConnect CNC (S247 / PR-240) =====

async fn boot_mtconnect(
    deps: &MesBootDeps,
    cancel: &CancellationToken,
    registry: &Arc<RwLock<AdapterRegistry>>,
) -> Result<Vec<(&'static str, JoinHandle<()>)>> {
    let cfg = read_mtconnect_config_from_env().context("read MTConnect config from env")?;
    let machine_id = cfg.machine_id.clone();
    let host = cfg.host.clone();
    let port = cfg.port;
    let device_name = cfg.device_name.clone();
    tracing::info!(
        machine_id = %machine_id,
        host = %host,
        port,
        device_name = %device_name,
        "spawning MES MTConnect adapter (S247 / PR-240)"
    );

    let adapter: Arc<MtconnectAdapter> = Arc::new(MtconnectAdapter::new(cfg));
    adapter
        .start()
        .await
        .with_context(|| format!("MTConnect adapter '{machine_id}' start failed"))?;

    register_started_adapter(registry, adapter.clone() as Arc<dyn Adapter>, &machine_id)?;

    let writer_handle = spawn_writer(adapter.clone() as Arc<dyn Adapter>, deps, cancel);
    let stopper_handle = spawn_stopper(adapter.clone() as Arc<dyn Adapter>, cancel, "mtconnect");

    Ok(vec![
        ("mes-mtconnect-writer", writer_handle),
        ("mes-mtconnect-stopper", stopper_handle),
    ])
}

// ===== Universal Robots RTDE (S248 / PR-241) =====

async fn boot_ur_rtde(
    deps: &MesBootDeps,
    cancel: &CancellationToken,
    registry: &Arc<RwLock<AdapterRegistry>>,
) -> Result<Vec<(&'static str, JoinHandle<()>)>> {
    let cfg =
        read_ur_rtde_config_from_env().context("read Universal Robots RTDE config from env")?;
    let robot_id = cfg.robot_id.clone();
    let host = cfg.host.clone();
    let port = cfg.port;
    let model = cfg.model.clone();
    tracing::info!(
        robot_id = %robot_id,
        host = %host,
        port,
        model = %model,
        "spawning MES Universal Robots RTDE adapter (S248 / PR-241)"
    );

    let adapter: Arc<UrRtdeAdapter> = Arc::new(UrRtdeAdapter::new(cfg));
    adapter
        .start()
        .await
        .with_context(|| format!("UR RTDE adapter '{robot_id}' start failed"))?;

    register_started_adapter(registry, adapter.clone() as Arc<dyn Adapter>, &robot_id)?;

    let writer_handle = spawn_writer(adapter.clone() as Arc<dyn Adapter>, deps, cancel);
    let stopper_handle = spawn_stopper(adapter.clone() as Arc<dyn Adapter>, cancel, "ur-rtde");

    Ok(vec![
        ("mes-ur-rtde-writer", writer_handle),
        ("mes-ur-rtde-stopper", stopper_handle),
    ])
}

// ===== Shared helpers =====

fn register_started_adapter(
    registry: &Arc<RwLock<AdapterRegistry>>,
    adapter: Arc<dyn Adapter>,
    label: &str,
) -> Result<()> {
    // Done AFTER `start()` succeeds — a failed-to-start adapter must
    // not appear in the dashboard list, per [[trust-code-not-operator]].
    let mut guard = registry
        .write()
        .map_err(|_| anyhow!("adapter registry rwlock poisoned during boot"))?;
    guard
        .register(adapter)
        .with_context(|| format!("register adapter '{label}' into runtime registry"))?;
    Ok(())
}

fn spawn_writer(
    adapter: Arc<dyn Adapter>,
    deps: &MesBootDeps,
    cancel: &CancellationToken,
) -> JoinHandle<()> {
    let writer_deps = LedgerWriterDeps {
        db_path: deps.db_path.clone(),
        tenant: deps.tenant.clone(),
        binary_hash: deps.binary_hash,
        actor: LedgerWriterActor {
            session_id: deps.session_id.clone(),
            operator_login: deps.operator_login.clone(),
        },
    };
    spawn_ledger_writer(adapter, writer_deps, cancel.clone())
}

fn spawn_stopper(
    adapter: Arc<dyn Adapter>,
    cancel: &CancellationToken,
    kind: &'static str,
) -> JoinHandle<()> {
    let stopper_cancel = cancel.clone();
    tokio::spawn(async move {
        stopper_cancel.cancelled().await;
        if let Err(e) = adapter.stop().await {
            tracing::warn!(
                adapter_name = %adapter.name(),
                kind,
                error = %e,
                "MES adapter stop failed during shutdown"
            );
        }
    })
}

fn env_bool_true(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn barcode_scanner_enabled() -> bool {
    env_bool_true(ENV_BARCODE_ENABLED)
}

fn zebra_enabled() -> bool {
    env_bool_true(ENV_ZEBRA_ENABLED)
}

fn mtconnect_enabled() -> bool {
    env_bool_true(ENV_MTCONNECT_ENABLED)
}

fn ur_rtde_enabled() -> bool {
    env_bool_true(ENV_UR_RTDE_ENABLED)
}

fn read_barcode_scanner_config_from_env() -> Result<BarcodeScannerConfig> {
    let scanner_id =
        std::env::var(ENV_BARCODE_ID).unwrap_or_else(|_| DEFAULT_SCANNER_ID.to_string());
    if scanner_id.trim().is_empty() {
        return Err(anyhow!(
            "{ENV_BARCODE_ID} is empty; refusing to start scanner with anonymous name"
        ));
    }

    let host_str = std::env::var(ENV_BARCODE_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    let listen_addr = IpAddr::from_str(&host_str)
        .with_context(|| format!("parse {ENV_BARCODE_HOST}={host_str}"))?;

    let listen_port = match std::env::var(ENV_BARCODE_PORT) {
        Ok(s) => s
            .parse::<u16>()
            .with_context(|| format!("parse {ENV_BARCODE_PORT}={s}"))?,
        Err(_) => DEFAULT_LISTEN_PORT,
    };

    let mut cfg = BarcodeScannerConfig::new(scanner_id);
    cfg.listen_addr = listen_addr;
    cfg.listen_port = listen_port;
    Ok(cfg)
}

fn read_zebra_config_from_env() -> Result<ZebraAdapterConfig> {
    let printer_id = std::env::var(ENV_ZEBRA_PRINTER_ID)
        .unwrap_or_else(|_| DEFAULT_ZEBRA_PRINTER_ID.to_string());
    if printer_id.trim().is_empty() {
        return Err(anyhow!(
            "{ENV_ZEBRA_PRINTER_ID} is empty; refusing to start printer with anonymous name"
        ));
    }
    let friendly_name = std::env::var(ENV_ZEBRA_FRIENDLY_NAME)
        .unwrap_or_else(|_| DEFAULT_ZEBRA_FRIENDLY_NAME.to_string());
    let host = std::env::var(ENV_ZEBRA_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    if host.trim().is_empty() {
        return Err(anyhow!("{ENV_ZEBRA_HOST} is empty"));
    }
    let port = match std::env::var(ENV_ZEBRA_PORT) {
        Ok(s) => s
            .parse::<u16>()
            .with_context(|| format!("parse {ENV_ZEBRA_PORT}={s}"))?,
        Err(_) => ZEBRA_DEFAULT_LISTEN_PORT,
    };
    Ok(ZebraAdapterConfig::new(
        printer_id,
        friendly_name,
        host,
        port,
    ))
}

fn read_mtconnect_config_from_env() -> Result<MtconnectAdapterConfig> {
    let machine_id = std::env::var(ENV_MTCONNECT_MACHINE_ID)
        .unwrap_or_else(|_| DEFAULT_MTCONNECT_MACHINE_ID.to_string());
    if machine_id.trim().is_empty() {
        return Err(anyhow!(
            "{ENV_MTCONNECT_MACHINE_ID} is empty; refusing to start CNC adapter with anonymous name"
        ));
    }
    let friendly_name = std::env::var(ENV_MTCONNECT_FRIENDLY_NAME)
        .unwrap_or_else(|_| DEFAULT_MTCONNECT_FRIENDLY_NAME.to_string());
    let host = std::env::var(ENV_MTCONNECT_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    if host.trim().is_empty() {
        return Err(anyhow!("{ENV_MTCONNECT_HOST} is empty"));
    }
    let port = match std::env::var(ENV_MTCONNECT_PORT) {
        Ok(s) => s
            .parse::<u16>()
            .with_context(|| format!("parse {ENV_MTCONNECT_PORT}={s}"))?,
        Err(_) => MTCONNECT_DEFAULT_AGENT_PORT,
    };
    let device_name = std::env::var(ENV_MTCONNECT_DEVICE_NAME)
        .unwrap_or_else(|_| DEFAULT_MTCONNECT_DEVICE_NAME.to_string());
    if device_name.trim().is_empty() {
        return Err(anyhow!("{ENV_MTCONNECT_DEVICE_NAME} is empty"));
    }
    Ok(MtconnectAdapterConfig::new(
        machine_id,
        friendly_name,
        host,
        port,
        device_name,
    ))
}

fn read_ur_rtde_config_from_env() -> Result<UrRtdeAdapterConfig> {
    let robot_id = std::env::var(ENV_UR_RTDE_ROBOT_ID)
        .unwrap_or_else(|_| DEFAULT_UR_RTDE_ROBOT_ID.to_string());
    if robot_id.trim().is_empty() {
        return Err(anyhow!(
            "{ENV_UR_RTDE_ROBOT_ID} is empty; refusing to start robot adapter with anonymous name"
        ));
    }
    let friendly_name = std::env::var(ENV_UR_RTDE_FRIENDLY_NAME)
        .unwrap_or_else(|_| DEFAULT_UR_RTDE_FRIENDLY_NAME.to_string());
    let host = std::env::var(ENV_UR_RTDE_HOST).unwrap_or_else(|_| DEFAULT_HOST.to_string());
    if host.trim().is_empty() {
        return Err(anyhow!("{ENV_UR_RTDE_HOST} is empty"));
    }
    let port = match std::env::var(ENV_UR_RTDE_PORT) {
        Ok(s) => s
            .parse::<u16>()
            .with_context(|| format!("parse {ENV_UR_RTDE_PORT}={s}"))?,
        Err(_) => UR_RTDE_DEFAULT_PORT,
    };
    let model =
        std::env::var(ENV_UR_RTDE_MODEL).unwrap_or_else(|_| DEFAULT_UR_RTDE_MODEL.to_string());
    Ok(UrRtdeAdapterConfig::new(
        robot_id,
        friendly_name,
        host,
        port,
        model,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: env vars are process-global; these tests serialize on a
    // shared mutex to avoid cross-test cross-talk. The set of tests is
    // small enough that the serialisation overhead is negligible.
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
    fn enabled_defaults_to_false() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(!barcode_scanner_enabled());
        assert!(!zebra_enabled());
        assert!(!mtconnect_enabled());
        assert!(!ur_rtde_enabled());
    }

    #[test]
    fn enabled_is_case_insensitive_true() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_BARCODE_ENABLED, "TRUE");
        assert!(barcode_scanner_enabled());
        std::env::set_var(ENV_BARCODE_ENABLED, "True");
        assert!(barcode_scanner_enabled());
        std::env::set_var(ENV_BARCODE_ENABLED, "true");
        assert!(barcode_scanner_enabled());
        std::env::set_var(ENV_BARCODE_ENABLED, "false");
        assert!(!barcode_scanner_enabled());
        clear_env();
    }

    #[test]
    fn config_from_env_uses_documented_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = read_barcode_scanner_config_from_env().unwrap();
        assert_eq!(cfg.scanner_id, DEFAULT_SCANNER_ID);
        assert_eq!(cfg.listen_port, DEFAULT_LISTEN_PORT);
        assert_eq!(cfg.listen_addr, IpAddr::from_str(DEFAULT_HOST).unwrap());
    }

    #[test]
    fn config_from_env_picks_up_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_BARCODE_ID, "barcode-scanner-receiving-dock");
        std::env::set_var(ENV_BARCODE_HOST, "0.0.0.0");
        std::env::set_var(ENV_BARCODE_PORT, "9100");
        let cfg = read_barcode_scanner_config_from_env().unwrap();
        assert_eq!(cfg.scanner_id, "barcode-scanner-receiving-dock");
        assert_eq!(cfg.listen_port, 9100);
        assert_eq!(cfg.listen_addr, IpAddr::from_str("0.0.0.0").unwrap());
        clear_env();
    }

    #[test]
    fn config_from_env_rejects_blank_scanner_id() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_BARCODE_ID, "   ");
        let err = read_barcode_scanner_config_from_env().unwrap_err();
        assert!(err.to_string().contains(ENV_BARCODE_ID));
        clear_env();
    }

    #[test]
    fn config_from_env_rejects_malformed_port() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_BARCODE_PORT, "not-a-number");
        let err = read_barcode_scanner_config_from_env().unwrap_err();
        assert!(err.to_string().contains(ENV_BARCODE_PORT));
        clear_env();
    }

    /// S240 / PR-234 — when the adapter is disabled the boot path
    /// leaves the shared registry empty. The dashboard handler then
    /// renders the "no adapters configured" empty state — honest
    /// replacement for the prior env-snapshot's "disabled" pill.
    ///
    /// Synchronous test driving the future with `block_on` so the
    /// `ENV_LOCK` std-Mutex guard doesn't cross an `.await` point
    /// (clippy `await_holding_lock`).
    #[test]
    fn boot_with_adapter_disabled_leaves_registry_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let registry = Arc::new(RwLock::new(AdapterRegistry::new()));
        let deps = MesBootDeps {
            db_path: PathBuf::from("/tmp/unused-by-disabled-path.duckdb"),
            tenant: TenantId::new("tenant-test").unwrap(),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            operator_login: "op".to_string(),
            session_id: "sess".to_string(),
        };
        let cancel = CancellationToken::new();
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(boot_mes_adapters(deps, cancel, registry.clone()))
            .unwrap();
        assert!(outcome.is_none());
        assert!(registry.read().unwrap().is_empty());
    }

    #[test]
    fn config_from_env_rejects_malformed_host() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_BARCODE_HOST, "not::an::ip::!!");
        let err = read_barcode_scanner_config_from_env().unwrap_err();
        assert!(err.to_string().contains(ENV_BARCODE_HOST));
        clear_env();
    }

    // ===== S250 / PR-242 — Zebra / MTConnect / UR RTDE wiring pins =====

    #[test]
    fn zebra_config_from_env_uses_documented_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = read_zebra_config_from_env().unwrap();
        assert_eq!(cfg.printer_id, DEFAULT_ZEBRA_PRINTER_ID);
        assert_eq!(cfg.friendly_name, DEFAULT_ZEBRA_FRIENDLY_NAME);
        assert_eq!(cfg.host, DEFAULT_HOST);
        assert_eq!(cfg.port, ZEBRA_DEFAULT_LISTEN_PORT);
    }

    #[test]
    fn zebra_config_from_env_picks_up_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_ZEBRA_PRINTER_ID, "label-printer-dispatch-a");
        std::env::set_var(ENV_ZEBRA_FRIENDLY_NAME, "Dispatch — left bench");
        std::env::set_var(ENV_ZEBRA_HOST, "192.168.1.50");
        std::env::set_var(ENV_ZEBRA_PORT, "9100");
        let cfg = read_zebra_config_from_env().unwrap();
        assert_eq!(cfg.printer_id, "label-printer-dispatch-a");
        assert_eq!(cfg.friendly_name, "Dispatch — left bench");
        assert_eq!(cfg.host, "192.168.1.50");
        assert_eq!(cfg.port, 9100);
        clear_env();
    }

    #[test]
    fn zebra_config_from_env_rejects_blank_printer_id() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_ZEBRA_PRINTER_ID, "   ");
        let err = read_zebra_config_from_env().unwrap_err();
        assert!(err.to_string().contains(ENV_ZEBRA_PRINTER_ID));
        clear_env();
    }

    #[test]
    fn mtconnect_config_from_env_uses_documented_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = read_mtconnect_config_from_env().unwrap();
        assert_eq!(cfg.machine_id, DEFAULT_MTCONNECT_MACHINE_ID);
        assert_eq!(cfg.friendly_name, DEFAULT_MTCONNECT_FRIENDLY_NAME);
        assert_eq!(cfg.host, DEFAULT_HOST);
        assert_eq!(cfg.port, MTCONNECT_DEFAULT_AGENT_PORT);
        assert_eq!(cfg.device_name, DEFAULT_MTCONNECT_DEVICE_NAME);
    }

    #[test]
    fn mtconnect_config_from_env_picks_up_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_MTCONNECT_MACHINE_ID, "cnc-line-a-1");
        std::env::set_var(ENV_MTCONNECT_FRIENDLY_NAME, "Line A CNC");
        std::env::set_var(ENV_MTCONNECT_HOST, "10.0.0.20");
        std::env::set_var(ENV_MTCONNECT_PORT, "5000");
        std::env::set_var(ENV_MTCONNECT_DEVICE_NAME, "M1");
        let cfg = read_mtconnect_config_from_env().unwrap();
        assert_eq!(cfg.machine_id, "cnc-line-a-1");
        assert_eq!(cfg.friendly_name, "Line A CNC");
        assert_eq!(cfg.host, "10.0.0.20");
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.device_name, "M1");
        clear_env();
    }

    #[test]
    fn mtconnect_config_from_env_rejects_blank_device_name() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_MTCONNECT_DEVICE_NAME, "   ");
        let err = read_mtconnect_config_from_env().unwrap_err();
        assert!(err.to_string().contains(ENV_MTCONNECT_DEVICE_NAME));
        clear_env();
    }

    #[test]
    fn ur_rtde_config_from_env_uses_documented_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let cfg = read_ur_rtde_config_from_env().unwrap();
        assert_eq!(cfg.robot_id, DEFAULT_UR_RTDE_ROBOT_ID);
        assert_eq!(cfg.friendly_name, DEFAULT_UR_RTDE_FRIENDLY_NAME);
        assert_eq!(cfg.host, DEFAULT_HOST);
        assert_eq!(cfg.port, UR_RTDE_DEFAULT_PORT);
        assert_eq!(cfg.model, DEFAULT_UR_RTDE_MODEL);
    }

    #[test]
    fn ur_rtde_config_from_env_picks_up_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_UR_RTDE_ROBOT_ID, "cnc-cell-a-robot");
        std::env::set_var(ENV_UR_RTDE_FRIENDLY_NAME, "Cell A — UR10e");
        std::env::set_var(ENV_UR_RTDE_HOST, "10.0.0.30");
        std::env::set_var(ENV_UR_RTDE_PORT, "30004");
        std::env::set_var(ENV_UR_RTDE_MODEL, "UR10e");
        let cfg = read_ur_rtde_config_from_env().unwrap();
        assert_eq!(cfg.robot_id, "cnc-cell-a-robot");
        assert_eq!(cfg.friendly_name, "Cell A — UR10e");
        assert_eq!(cfg.host, "10.0.0.30");
        assert_eq!(cfg.port, 30004);
        assert_eq!(cfg.model, "UR10e");
        clear_env();
    }

    #[test]
    fn ur_rtde_config_from_env_rejects_blank_robot_id() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var(ENV_UR_RTDE_ROBOT_ID, "   ");
        let err = read_ur_rtde_config_from_env().unwrap_err();
        assert!(err.to_string().contains(ENV_UR_RTDE_ROBOT_ID));
        clear_env();
    }
}
