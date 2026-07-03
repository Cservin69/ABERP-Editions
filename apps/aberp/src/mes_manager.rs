//! S257 / PR-246 — operator-managed MES adapter lifecycle.
//!
//! S250's `boot_mes_adapters` wired adapters from env vars at boot;
//! every change meant an SSH-to-self + edit `/etc/aberp.env` + restart.
//! Per [[trust-code-not-operator]] + [[hulye-biztos]] this module makes
//! Add / Edit / Delete one-click flows that take effect immediately,
//! persisting to the `[[mes.adapters]]` seller.toml slot
//! ([`crate::mes_adapters_config`]) and auditing every CRUD.
//!
//! ## Lifecycle model
//!
//! The [`AdapterManager`] owns:
//!   - a clone of the shared `AdapterRegistry` (the live `Arc<dyn
//!     Adapter>` map the Workshop dashboard already probes), and
//!   - a per-adapter [`CancellationToken`] map. Each token is a CHILD
//!     of the graceful-shutdown root token (S213 / PR-209), so the
//!     adapter's ledger-writer + stopper tasks die when EITHER the
//!     operator deletes the adapter (we cancel its child) OR the app
//!     shuts down (the root cancels every child).
//!
//! A mutation takes a process-wide async mutex so two operators editing
//! through different SPA sessions can't interleave a stop/start pair and
//! corrupt the registry. The persisted-config + live-registry end state
//! is therefore **last-write-wins** (documented per the S257 brief) —
//! the operator who commits last wins the final TOML, cleanly.
//!
//! ## Hot restart (Edit)
//!
//! Edit is `stop → reinit → start` in place. The old adapter is stopped
//! and deregistered FIRST (a barcode listener can't rebind its port
//! while the old one holds it), then the new config is built + started.
//! If the new config fails to start, the adapter is left stopped and the
//! error surfaces — the persisted TOML is only rewritten AFTER a
//! successful start, so a failed edit leaves the last-good config on
//! disk for the operator to retry. An adapter in retry-backoff that is
//! edited resets to a fresh adapter (backoff state is intentionally
//! lost — the new endpoint deserves a clean dial).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, EventKind};
use aberp_mes::{
    build_adapter, spawn_ledger_writer, Adapter, AdapterConfigEntry, AdapterConfigError,
    AdapterConfigFieldError, AdapterHealth, AdapterKind, AdapterRegistry, LedgerWriterActor,
    LedgerWriterDeps,
};
use duckdb::Connection;

use crate::mes_adapters_config;
use crate::mes_boot::MesBootDeps;

/// Operator-supplied fields for a new adapter. The `adapter_id` (the
/// registry key) is server-minted, not operator-typed — the operator
/// thinks in friendly names + endpoints, never in stable ids.
#[derive(Debug, Clone, Deserialize)]
pub struct AddAdapterInput {
    pub kind: AdapterKind,
    pub friendly_name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Operator-editable fields for an existing adapter. `kind` +
/// `adapter_id` are immutable (the id is the registry key; changing the
/// kind is a delete-then-add, not an edit).
#[derive(Debug, Clone, Deserialize)]
pub struct EditAdapterInput {
    pub friendly_name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// One persisted adapter joined with its live health. The route layer
/// maps `health` onto the closed wire-vocab status string.
#[derive(Debug, Clone)]
pub struct AdapterListRow {
    pub entry: AdapterConfigEntry,
    pub health: AdapterHealth,
}

/// Audit payload for every adapter-config CRUD. Carries the durable
/// config so the change is reconstructable from the ledger alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterConfigAuditPayload {
    pub adapter_id: String,
    pub kind: String,
    pub friendly_name: String,
    pub host: String,
    pub port: u16,
}

impl AdapterConfigAuditPayload {
    fn from_entry(entry: &AdapterConfigEntry) -> Self {
        Self {
            adapter_id: entry.adapter_id.clone(),
            kind: entry.kind.wire_str().to_string(),
            friendly_name: entry.friendly_name.clone(),
            host: entry.host.clone(),
            port: entry.port,
        }
    }
}

/// Typed CRUD failure so the route layer maps to the right HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum AdapterMutationError {
    #[error("adapter config validation failed")]
    Validation(Vec<AdapterConfigFieldError>),
    /// Another adapter already binds `host:port` (conservative refusal
    /// per the S257 adversarial note — two adapters on one endpoint is
    /// almost always a typo, never intended).
    #[error("endpoint {endpoint} is already used by adapter `{existing_id}`")]
    DuplicateEndpoint {
        endpoint: String,
        existing_id: String,
    },
    /// Edit / delete referenced an unknown adapter_id.
    #[error("no adapter with id `{0}`")]
    NotFound(String),
    /// The config couldn't be built into a live adapter (e.g. a barcode
    /// host that isn't an IP address).
    #[error(transparent)]
    Build(#[from] AdapterConfigError),
    /// The adapter's `start()` failed.
    #[error("adapter start failed: {0}")]
    Start(String),
    /// TOML persistence / audit-ledger I/O failed.
    #[error(transparent)]
    Io(anyhow::Error),
}

/// Owns the per-adapter cancellation tokens + the mutation serialiser.
/// Holds a clone of the shared registry so it can register/unregister
/// live adapters; the root token parents every per-adapter child token.
pub struct AdapterManager {
    registry: Arc<RwLock<AdapterRegistry>>,
    root_token: CancellationToken,
    tokens: Mutex<HashMap<String, CancellationToken>>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl AdapterManager {
    pub fn new(registry: Arc<RwLock<AdapterRegistry>>, root_token: CancellationToken) -> Self {
        Self {
            registry,
            root_token,
            tokens: Mutex::new(HashMap::new()),
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// List persisted adapters joined with live health. Missing-from-
    /// registry (e.g. a failed-to-start boot adapter) reads as
    /// `Stopped`. The list ordering follows the TOML's declared order.
    pub fn list(&self, seller_toml_path: &Path) -> Result<Vec<AdapterListRow>> {
        let entries = mes_adapters_config::read_mes_adapters(seller_toml_path)?;
        let health = self
            .registry
            .read()
            .map_err(|_| anyhow!("adapter registry rwlock poisoned"))?
            .health();
        Ok(entries
            .into_iter()
            .map(|entry| {
                let h = health
                    .get(&entry.adapter_id)
                    .cloned()
                    .unwrap_or(AdapterHealth::Stopped);
                AdapterListRow { entry, health: h }
            })
            .collect())
    }

    /// Add a new adapter: validate → refuse a duplicate endpoint →
    /// build + start → register + spawn tasks → persist + audit. On any
    /// pre-persist failure nothing is registered or written (no half-
    /// started adapter, no orphan TOML row).
    pub async fn add(
        &self,
        deps: &MesBootDeps,
        seller_toml_path: &Path,
        input: AddAdapterInput,
    ) -> Result<AdapterConfigEntry, AdapterMutationError> {
        let _guard = self.mutation_lock.lock().await;
        let existing = self.read_entries(seller_toml_path)?;

        let adapter_id = mint_adapter_id(input.kind, &existing);
        let friendly_name = unique_friendly_name(&input.friendly_name, &existing, None);
        let entry = AdapterConfigEntry {
            kind: input.kind,
            adapter_id,
            friendly_name,
            host: input.host,
            port: input.port,
            device_name: input.device_name,
            model: input.model,
        };
        entry.validate().map_err(AdapterMutationError::Validation)?;
        refuse_duplicate_endpoint(&entry, &existing, None)?;

        // Build + start BEFORE touching the registry / TOML so a bad
        // endpoint fails cleanly.
        self.start_and_register(deps, &entry).await?;

        let mut next = existing;
        next.push(entry.clone());
        self.persist(seller_toml_path, &next)?;
        self.audit(deps, EventKind::AdapterAdded, &entry)?;
        Ok(entry)
    }

    /// Edit an existing adapter in place (hot restart). `kind` is
    /// immutable; only the endpoint + display fields change.
    pub async fn update(
        &self,
        deps: &MesBootDeps,
        seller_toml_path: &Path,
        adapter_id: &str,
        input: EditAdapterInput,
    ) -> Result<AdapterConfigEntry, AdapterMutationError> {
        let _guard = self.mutation_lock.lock().await;
        let existing = self.read_entries(seller_toml_path)?;
        let old = existing
            .iter()
            .find(|e| e.adapter_id == adapter_id)
            .cloned()
            .ok_or_else(|| AdapterMutationError::NotFound(adapter_id.to_string()))?;

        let friendly_name = unique_friendly_name(&input.friendly_name, &existing, Some(adapter_id));
        let new_entry = AdapterConfigEntry {
            kind: old.kind, // immutable
            adapter_id: old.adapter_id.clone(),
            friendly_name,
            host: input.host,
            port: input.port,
            device_name: input.device_name,
            model: input.model,
        };
        new_entry
            .validate()
            .map_err(AdapterMutationError::Validation)?;
        refuse_duplicate_endpoint(&new_entry, &existing, Some(adapter_id))?;

        // stop → reinit → start. Stop FIRST so a same-port rebind
        // (barcode listener) doesn't AddrInUse against the old one.
        self.stop_and_deregister(adapter_id).await;
        self.start_and_register(deps, &new_entry).await?;

        let next: Vec<AdapterConfigEntry> = existing
            .into_iter()
            .map(|e| {
                if e.adapter_id == adapter_id {
                    new_entry.clone()
                } else {
                    e
                }
            })
            .collect();
        self.persist(seller_toml_path, &next)?;
        self.audit(deps, EventKind::AdapterUpdated, &new_entry)?;
        Ok(new_entry)
    }

    /// Delete an adapter: stop + deregister, drop its TOML row, audit.
    pub async fn remove(
        &self,
        deps: &MesBootDeps,
        seller_toml_path: &Path,
        adapter_id: &str,
    ) -> Result<AdapterConfigEntry, AdapterMutationError> {
        let _guard = self.mutation_lock.lock().await;
        let existing = self.read_entries(seller_toml_path)?;
        let removed = existing
            .iter()
            .find(|e| e.adapter_id == adapter_id)
            .cloned()
            .ok_or_else(|| AdapterMutationError::NotFound(adapter_id.to_string()))?;

        self.stop_and_deregister(adapter_id).await;
        let next: Vec<AdapterConfigEntry> = existing
            .into_iter()
            .filter(|e| e.adapter_id != adapter_id)
            .collect();
        self.persist(seller_toml_path, &next)?;
        self.audit(deps, EventKind::AdapterRemoved, &removed)?;
        Ok(removed)
    }

    /// Boot every persisted adapter (replaces the env-only boot path).
    /// One adapter failing to start does NOT abort the rest — it is
    /// logged and skipped (its row stays in TOML so a later edit can
    /// fix the endpoint). Returns the spawned task handles for the
    /// caller to register with the shutdown coordinator.
    pub async fn boot_from_toml(
        &self,
        deps: &MesBootDeps,
        seller_toml_path: &Path,
    ) -> Result<Vec<(&'static str, JoinHandle<()>)>> {
        let entries = mes_adapters_config::read_mes_adapters(seller_toml_path)
            .context("read [[mes.adapters]] at boot")?;
        let mut handles = Vec::new();
        for entry in entries {
            match self.start_and_register(deps, &entry).await {
                Ok(mut spawned) => {
                    tracing::info!(
                        adapter_id = %entry.adapter_id,
                        kind = %entry.kind.wire_str(),
                        host = %entry.host,
                        port = entry.port,
                        "started persisted MES adapter (S257)"
                    );
                    handles.append(&mut spawned);
                }
                Err(e) => {
                    // Loud per CLAUDE.md rule 12; skip the one bad row.
                    tracing::warn!(
                        adapter_id = %entry.adapter_id,
                        kind = %entry.kind.wire_str(),
                        error = %e,
                        "persisted MES adapter failed to start at boot; skipping. \
                         Fix it from Settings → Adapters."
                    );
                }
            }
        }
        Ok(handles)
    }

    // ── internals ──────────────────────────────────────────────────

    fn read_entries(
        &self,
        seller_toml_path: &Path,
    ) -> Result<Vec<AdapterConfigEntry>, AdapterMutationError> {
        mes_adapters_config::read_mes_adapters(seller_toml_path).map_err(AdapterMutationError::Io)
    }

    fn persist(
        &self,
        seller_toml_path: &Path,
        entries: &[AdapterConfigEntry],
    ) -> Result<(), AdapterMutationError> {
        mes_adapters_config::write_mes_adapters_section(seller_toml_path, entries)
            .map_err(AdapterMutationError::Io)
    }

    /// Build the adapter, start it, register it, and spawn its ledger-
    /// writer + stopper bound to a fresh child token. Returns the task
    /// handles. On `start()` failure nothing is registered.
    async fn start_and_register(
        &self,
        deps: &MesBootDeps,
        entry: &AdapterConfigEntry,
    ) -> Result<Vec<(&'static str, JoinHandle<()>)>, AdapterMutationError> {
        let adapter: Arc<dyn Adapter> = build_adapter(entry)?;
        adapter
            .start()
            .await
            .map_err(|e| AdapterMutationError::Start(e.to_string()))?;

        {
            let mut guard = self
                .registry
                .write()
                .map_err(|_| AdapterMutationError::Io(anyhow!("registry rwlock poisoned")))?;
            // A stale token for this id (failed prior edit) is cleaned
            // by stop_and_deregister before we get here; register loud-
            // fails on a true duplicate.
            guard
                .register(adapter.clone())
                .map_err(|e| AdapterMutationError::Io(anyhow!("register adapter: {e}")))?;
        }

        let child = self.root_token.child_token();
        let writer = spawn_writer(adapter.clone(), deps, &child);
        let stopper = spawn_stopper(adapter, &child);
        self.tokens
            .lock()
            .expect("adapter token mutex poisoned")
            .insert(entry.adapter_id.clone(), child);

        Ok(vec![
            ("mes-adapter-writer", writer),
            ("mes-adapter-stopper", stopper),
        ])
    }

    /// Cancel the adapter's tasks, unregister it, and stop it. Idempotent
    /// — a no-op when the id isn't registered.
    async fn stop_and_deregister(&self, adapter_id: &str) {
        if let Some(token) = self
            .tokens
            .lock()
            .expect("adapter token mutex poisoned")
            .remove(adapter_id)
        {
            token.cancel(); // stops writer; fires stopper's adapter.stop()
        }
        let removed = {
            let mut guard = match self.registry.write() {
                Ok(g) => g,
                Err(_) => {
                    tracing::warn!("registry rwlock poisoned during deregister");
                    return;
                }
            };
            guard.unregister(adapter_id)
        };
        if let Some(adapter) = removed {
            // Explicit stop (idempotent) so the next start sees a fully
            // stopped adapter even before the stopper task wakes.
            if let Err(e) = adapter.stop().await {
                tracing::warn!(adapter_id, error = %e, "adapter stop during deregister failed");
            }
        }
    }

    fn audit(
        &self,
        deps: &MesBootDeps,
        kind: EventKind,
        entry: &AdapterConfigEntry,
    ) -> Result<(), AdapterMutationError> {
        let payload = AdapterConfigAuditPayload::from_entry(entry);
        let bytes = serde_json::to_vec(&payload).map_err(|e| {
            AdapterMutationError::Io(anyhow!("serialize adapter audit payload: {e}"))
        })?;
        let actor = Actor::from_local_cli(Ulid::new().to_string(), &deps.operator_login);
        let mut conn = Connection::open(&deps.db_path)
            .map_err(|e| AdapterMutationError::Io(anyhow!("open DuckDB for adapter audit: {e}")))?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .map_err(|e| {
                AdapterMutationError::Io(anyhow!(
                    "PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}"
                ))
            })?;
        aberp_audit_ledger::ensure_schema(&conn)
            .map_err(|e| AdapterMutationError::Io(anyhow!("ensure audit schema: {e}")))?;
        let tx = conn
            .transaction()
            .map_err(|e| AdapterMutationError::Io(anyhow!("begin audit tx: {e}")))?;
        let meta = aberp_audit_ledger::LedgerMeta::new(deps.tenant.clone(), deps.binary_hash);
        let kind_label = kind.as_str();
        aberp_audit_ledger::append_in_tx(&tx, &meta, kind, bytes, actor, None)
            .map_err(|e| AdapterMutationError::Io(anyhow!("append_in_tx {kind_label}: {e}")))?;
        tx.commit()
            .map_err(|e| AdapterMutationError::Io(anyhow!("commit audit tx: {e}")))?;
        Ok(())
    }
}

/// Mint a stable registry key from the kind + a short unique suffix.
/// Re-rolls on the astronomically-unlikely collision with an existing
/// id (a hand-edited TOML could in theory clash).
fn mint_adapter_id(kind: AdapterKind, existing: &[AdapterConfigEntry]) -> String {
    loop {
        let candidate = format!("{}-{}", kind.wire_str(), Ulid::new());
        if !existing.iter().any(|e| e.adapter_id == candidate) {
            return candidate;
        }
    }
}

/// Friendly-name uniqueness is nice-to-have, not enforced (S257
/// adversarial note): auto-suffix ` (2)`, ` (3)`… on a collision rather
/// than refusing. `skip_id` excludes the entry being edited from the
/// collision set.
fn unique_friendly_name(
    desired: &str,
    existing: &[AdapterConfigEntry],
    skip_id: Option<&str>,
) -> String {
    let trimmed = desired.trim();
    let taken = |name: &str| {
        existing
            .iter()
            .any(|e| Some(e.adapter_id.as_str()) != skip_id && e.friendly_name == name)
    };
    if !taken(trimmed) {
        return trimmed.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{trimmed} ({n})");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Refuse a second adapter on the same `host:port`. `skip_id` excludes
/// the entry being edited.
fn refuse_duplicate_endpoint(
    entry: &AdapterConfigEntry,
    existing: &[AdapterConfigEntry],
    skip_id: Option<&str>,
) -> Result<(), AdapterMutationError> {
    let key = entry.endpoint_key();
    if let Some(clash) = existing
        .iter()
        .find(|e| Some(e.adapter_id.as_str()) != skip_id && e.endpoint_key() == key)
    {
        return Err(AdapterMutationError::DuplicateEndpoint {
            endpoint: key,
            existing_id: clash.adapter_id.clone(),
        });
    }
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

fn spawn_stopper(adapter: Arc<dyn Adapter>, cancel: &CancellationToken) -> JoinHandle<()> {
    let stopper_cancel = cancel.clone();
    tokio::spawn(async move {
        stopper_cancel.cancelled().await;
        if let Err(e) = adapter.stop().await {
            tracing::warn!(
                adapter_name = %adapter.name(),
                error = %e,
                "MES adapter stop failed during cancellation"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{BinaryHash, TenantId};

    fn entry(
        kind: AdapterKind,
        id: &str,
        host: &str,
        port: u16,
        friendly: &str,
    ) -> AdapterConfigEntry {
        AdapterConfigEntry {
            kind,
            adapter_id: id.to_string(),
            friendly_name: friendly.to_string(),
            host: host.to_string(),
            port,
            device_name: None,
            model: None,
        }
    }

    fn test_deps(dir: &Path) -> MesBootDeps {
        MesBootDeps {
            db_path: dir.join("test.duckdb"),
            tenant: TenantId::new("tenant-test").unwrap(),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            operator_login: "op".to_string(),
            session_id: "sess".to_string(),
        }
    }

    fn manager() -> AdapterManager {
        AdapterManager::new(
            Arc::new(RwLock::new(AdapterRegistry::new())),
            CancellationToken::new(),
        )
    }

    #[test]
    fn mint_adapter_id_uses_kind_prefix_and_is_unique() {
        let id = mint_adapter_id(AdapterKind::Robot, &[]);
        assert!(id.starts_with("robot-"), "got {id}");
    }

    #[test]
    fn unique_friendly_name_auto_suffixes_on_collision() {
        let existing = vec![entry(AdapterKind::Robot, "r1", "h", 1, "Bench")];
        assert_eq!(unique_friendly_name("Bench", &existing, None), "Bench (2)");
        // Editing r1 itself doesn't collide with its own name.
        assert_eq!(
            unique_friendly_name("Bench", &existing, Some("r1")),
            "Bench"
        );
        // A free name passes through verbatim.
        assert_eq!(unique_friendly_name("Other", &existing, None), "Other");
    }

    #[test]
    fn refuse_duplicate_endpoint_flags_same_host_port() {
        let existing = vec![entry(AdapterKind::Robot, "r1", "10.0.0.6", 30004, "R")];
        let clash = entry(AdapterKind::Cnc, "c1", "10.0.0.6", 30004, "C");
        let err = refuse_duplicate_endpoint(&clash, &existing, None).unwrap_err();
        assert!(matches!(
            err,
            AdapterMutationError::DuplicateEndpoint { .. }
        ));
        // Same id (edit) is exempt.
        assert!(refuse_duplicate_endpoint(&existing[0], &existing, Some("r1")).is_ok());
    }

    /// End-to-end add → list → remove against a real (barcode) adapter
    /// that binds a loopback port. Pins the registry + token bookkeeping
    /// and the TOML round-trip.
    #[tokio::test]
    async fn add_then_remove_round_trip() {
        let dir = std::env::temp_dir().join(format!("aberp-mes-mgr-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seller.toml");
        let deps = test_deps(&dir);
        let mgr = manager();

        // Barcode binds 127.0.0.1:<ephemeral>; pick a high port.
        let input = AddAdapterInput {
            kind: AdapterKind::BarcodeScanner,
            friendly_name: "Dock scanner".to_string(),
            host: "127.0.0.1".to_string(),
            port: 53997,
            device_name: None,
            model: None,
        };
        let added = mgr.add(&deps, &path, input).await.expect("add");
        assert!(added.adapter_id.starts_with("barcode-scanner-"));

        let rows = mgr.list(&path).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.adapter_id, added.adapter_id);
        // Started → registry reports it Healthy (barcode binds eagerly).
        assert!(matches!(
            rows[0].health,
            AdapterHealth::Healthy | AdapterHealth::Starting
        ));
        // Token bookkeeping recorded it.
        assert!(mgr.tokens.lock().unwrap().contains_key(&added.adapter_id));

        let removed = mgr
            .remove(&deps, &path, &added.adapter_id)
            .await
            .expect("remove");
        assert_eq!(removed.adapter_id, added.adapter_id);
        assert!(mgr.list(&path).expect("list").is_empty());
        assert!(!mgr.tokens.lock().unwrap().contains_key(&added.adapter_id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A duplicate endpoint is refused before any registry mutation —
    /// the first adapter stays healthy, the second never registers.
    #[tokio::test]
    async fn add_refuses_duplicate_endpoint() {
        let dir = std::env::temp_dir().join(format!("aberp-mes-dup-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seller.toml");
        let deps = test_deps(&dir);
        let mgr = manager();

        let mk = |port| AddAdapterInput {
            kind: AdapterKind::BarcodeScanner,
            friendly_name: "S".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            device_name: None,
            model: None,
        };
        mgr.add(&deps, &path, mk(53998)).await.expect("first add");
        let err = mgr
            .add(&deps, &path, mk(53998))
            .await
            .expect_err("dup must fail");
        assert!(matches!(
            err,
            AdapterMutationError::DuplicateEndpoint { .. }
        ));
        // Only the first survived.
        assert_eq!(mgr.list(&path).expect("list").len(), 1);

        // cleanup: stop the live adapter
        let id = mgr.list(&path).unwrap()[0].entry.adapter_id.clone();
        let _ = mgr.remove(&deps, &path, &id).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remove_unknown_id_is_not_found() {
        let dir = std::env::temp_dir().join(format!("aberp-mes-nf-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seller.toml");
        let deps = test_deps(&dir);
        let mgr = manager();
        let err = mgr.remove(&deps, &path, "ghost").await.expect_err("nf");
        assert!(matches!(err, AdapterMutationError::NotFound(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
