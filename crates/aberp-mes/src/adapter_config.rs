//! S257 / PR-246 — typed, persistable MES-adapter configuration.
//!
//! S250's `boot_mes_adapters` wired the four adapter types from env
//! vars; this module replaces that env-only shape with a durable,
//! operator-managed config record. One [`AdapterConfigEntry`] describes
//! one registered adapter; [`build_adapter`] turns it into a live
//! `Arc<dyn Adapter>` ready to `start()`.
//!
//! ## Why the config lives here, the persistence in `apps/aberp`
//!
//! [`build_adapter`] must reference every concrete adapter + config
//! type, so it belongs in this crate. The `[[mes.adapters]]`
//! seller.toml read/write (the 7th preservation slot per
//! [[seller-toml-write-invariant]]) lives in the `aberp` binary's
//! `mes_adapters_config` module — it knows the file path + the other
//! sections it must preserve, which this crate must not.
//!
//! ## Closed-vocab adapter kinds
//!
//! [`AdapterKind`] is a closed vocab whose wire strings MATCH each
//! adapter's `Adapter::kind()` so the live-registry health snapshot and
//! the persisted config join on a single value. A SPA build older than
//! the backend that ships a new kind sees the unknown wire string and
//! is expected to skip the row gracefully (per the S257 brief's
//! forward-compat note) — the backend never emits a kind the closed
//! vocab here doesn't carry.

use std::net::IpAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::adapter::Adapter;
use crate::adapters::barcode_scanner::{BarcodeScannerAdapter, BarcodeScannerConfig};
use crate::adapters::mtconnect::{MtconnectAdapter, MtconnectAdapterConfig};
use crate::adapters::trumpf::{TrumpfAdapter, TrumpfAdapterConfig};
use crate::adapters::ur_rtde::{UrRtdeAdapter, UrRtdeAdapterConfig};
use crate::adapters::zebra::{ZebraAdapter, ZebraAdapterConfig};

/// Default MTConnect `device_name` when the config omits it. Mirrors
/// the env-boot default (`ABERP_MTCONNECT_DEVICE_NAME` → `"default"`).
pub const DEFAULT_MTCONNECT_DEVICE_NAME: &str = "default";
/// Default UR model label when the config omits it. Mirrors the env-
/// boot default (`ABERP_UR_RTDE_MODEL` → `"UR"`).
pub const DEFAULT_UR_RTDE_MODEL: &str = "UR";
/// Default laser device path when the config omits it. The v1 Trumpf
/// backend is MTConnect, so this mirrors the MTConnect default.
pub const DEFAULT_LASER_DEVICE_NAME: &str = "default";

/// Closed-vocab adapter kind. Wire strings are byte-identical to the
/// matching `Adapter::kind()` so config and live-registry rows join on
/// one value. Adding a kind is a deliberate widening here + a
/// [`build_adapter`] arm — there is no `Other`/`Unknown` bucket
/// (deny-default per CLAUDE.md rule 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterKind {
    /// Barcode scanner — a TCP *listener* on `host:port` emitting
    /// `ScanReceived`. `host` MUST parse as an `IpAddr` (it is a local
    /// bind address, not a remote hostname).
    #[serde(rename = "barcode-scanner")]
    BarcodeScanner,
    /// Zebra ZPL label printer — TCP client to `host:port`.
    #[serde(rename = "label-printer")]
    LabelPrinter,
    /// MTConnect CNC agent — HTTP poller of `host:port`. Uses the
    /// kind-specific `device_name`.
    #[serde(rename = "cnc-machine")]
    Cnc,
    /// Universal Robots RTDE — binary TCP client to `host:port`. Uses
    /// the kind-specific `model` label.
    #[serde(rename = "robot")]
    Robot,
    /// Trumpf laser cutter — polled through the [`TrumpfSource`] seam.
    /// The v1 backend is MTConnect-over-HTTP (so `host:port` is the
    /// gateway fronting the laser) and it reuses the kind-specific
    /// `device_name`; a future OPC-UA / Oseon backend swaps in behind
    /// the same seam without touching this vocab.
    ///
    /// [`TrumpfSource`]: crate::TrumpfSource
    #[serde(rename = "laser-cutter")]
    Laser,
}

impl AdapterKind {
    /// The closed-vocab wire string (== `Adapter::kind()`).
    pub fn wire_str(self) -> &'static str {
        match self {
            AdapterKind::BarcodeScanner => "barcode-scanner",
            AdapterKind::LabelPrinter => "label-printer",
            AdapterKind::Cnc => "cnc-machine",
            AdapterKind::Robot => "robot",
            AdapterKind::Laser => "laser-cutter",
        }
    }

    /// Parse a wire string back into a kind. `None` for anything
    /// outside the closed vocab — the caller decides whether an unknown
    /// kind is a skip (SPA) or a loud-fail (backend parse).
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "barcode-scanner" => Some(AdapterKind::BarcodeScanner),
            "label-printer" => Some(AdapterKind::LabelPrinter),
            "cnc-machine" => Some(AdapterKind::Cnc),
            "robot" => Some(AdapterKind::Robot),
            "laser-cutter" => Some(AdapterKind::Laser),
            _ => None,
        }
    }

    /// Every kind, in display order. Powers the Add-wizard kind picker
    /// + the round-trip test.
    pub fn all() -> [AdapterKind; 5] {
        [
            AdapterKind::BarcodeScanner,
            AdapterKind::LabelPrinter,
            AdapterKind::Cnc,
            AdapterKind::Robot,
            AdapterKind::Laser,
        ]
    }
}

/// One durable adapter configuration. The registry key is
/// `adapter_id` (== the started adapter's `name()`); `friendly_name`
/// is display-only metadata the live registry does not carry, so the
/// Adapters page sources the list from these entries and joins live
/// health by `adapter_id`.
///
/// `device_name` / `model` are kind-specific; `None` for kinds that
/// don't use them. [`build_adapter`] applies the documented defaults
/// when a kind needs one but the entry omits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterConfigEntry {
    pub kind: AdapterKind,
    pub adapter_id: String,
    pub friendly_name: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One field-level validation problem. Mirrors
/// `QuoteIntakeConfigValidationError`'s shape so the SPA can surface
/// every problem at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdapterConfigFieldError {
    pub field: &'static str,
    pub message: String,
}

impl AdapterConfigEntry {
    /// Field-level invariants shared by every kind. Endpoint-shape
    /// problems specific to a kind (barcode `host` must be an `IpAddr`)
    /// surface at [`build_adapter`] time, not here — this gate is what
    /// the route layer runs before persistence so the operator sees
    /// every text-field problem in one pass.
    pub fn validate(&self) -> Result<(), Vec<AdapterConfigFieldError>> {
        let mut errors = Vec::new();
        if self.adapter_id.trim().is_empty() {
            errors.push(AdapterConfigFieldError {
                field: "adapter_id",
                message: "adapter_id must not be empty".to_string(),
            });
        }
        if let Err(msg) = reject_toml_metachars("adapter_id", &self.adapter_id) {
            errors.push(AdapterConfigFieldError {
                field: "adapter_id",
                message: msg,
            });
        }
        if self.friendly_name.trim().is_empty() {
            errors.push(AdapterConfigFieldError {
                field: "friendly_name",
                message: "friendly_name must not be empty".to_string(),
            });
        }
        if let Err(msg) = reject_toml_metachars("friendly_name", &self.friendly_name) {
            errors.push(AdapterConfigFieldError {
                field: "friendly_name",
                message: msg,
            });
        }
        if self.host.trim().is_empty() {
            errors.push(AdapterConfigFieldError {
                field: "host",
                message: "host must not be empty".to_string(),
            });
        }
        if let Err(msg) = reject_toml_metachars("host", &self.host) {
            errors.push(AdapterConfigFieldError {
                field: "host",
                message: msg,
            });
        }
        if self.port == 0 {
            errors.push(AdapterConfigFieldError {
                field: "port",
                message: "port must be between 1 and 65535".to_string(),
            });
        }
        if let Some(device) = &self.device_name {
            if let Err(msg) = reject_toml_metachars("device_name", device) {
                errors.push(AdapterConfigFieldError {
                    field: "device_name",
                    message: msg,
                });
            }
        }
        if let Some(model) = &self.model {
            if let Err(msg) = reject_toml_metachars("model", model) {
                errors.push(AdapterConfigFieldError {
                    field: "model",
                    message: msg,
                });
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `host:port` endpoint key. Used by the route layer's
    /// "two adapters on the same endpoint" refusal (conservative per
    /// the S257 adversarial note).
    pub fn endpoint_key(&self) -> String {
        format!("{}:{}", self.host.trim(), self.port)
    }
}

/// Reject characters that would break the hand-rolled TOML line-walker
/// on the next read. Same posture as `quote_intake_config`'s metachar
/// guard.
fn reject_toml_metachars(field: &'static str, value: &str) -> Result<(), String> {
    for c in value.chars() {
        if c == '"' || c == '\n' || c == '\r' || c == '[' || c == ']' {
            return Err(format!(
                "{field} contains a character that would break TOML serialisation: {c:?}"
            ));
        }
    }
    Ok(())
}

/// Failure building a live adapter from a config entry. The only
/// build-time problem the validator can't catch ahead of time is a
/// barcode `host` that doesn't parse as an `IpAddr`.
#[derive(Debug, thiserror::Error)]
pub enum AdapterConfigError {
    #[error("barcode-scanner host `{host}` is not a valid IP address: {source}")]
    BadListenAddr {
        host: String,
        source: std::net::AddrParseError,
    },
}

/// Build a live (not-yet-started) adapter from its config entry. The
/// returned adapter's `name()` is `entry.adapter_id`, so it registers
/// under that key. The caller is responsible for `start()` +
/// registry insertion + task spawning (see the `aberp` binary's
/// `mes_manager`).
pub fn build_adapter(entry: &AdapterConfigEntry) -> Result<Arc<dyn Adapter>, AdapterConfigError> {
    match entry.kind {
        AdapterKind::BarcodeScanner => {
            let listen_addr: IpAddr =
                entry
                    .host
                    .trim()
                    .parse()
                    .map_err(|source| AdapterConfigError::BadListenAddr {
                        host: entry.host.clone(),
                        source,
                    })?;
            let mut cfg = BarcodeScannerConfig::new(entry.adapter_id.clone());
            cfg.listen_addr = listen_addr;
            cfg.listen_port = entry.port;
            Ok(Arc::new(BarcodeScannerAdapter::new(cfg)))
        }
        AdapterKind::LabelPrinter => {
            let cfg = ZebraAdapterConfig::new(
                entry.adapter_id.clone(),
                entry.friendly_name.clone(),
                entry.host.clone(),
                entry.port,
            );
            Ok(Arc::new(ZebraAdapter::new(cfg)))
        }
        AdapterKind::Cnc => {
            let device_name = entry
                .device_name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_MTCONNECT_DEVICE_NAME.to_string());
            let cfg = MtconnectAdapterConfig::new(
                entry.adapter_id.clone(),
                entry.friendly_name.clone(),
                entry.host.clone(),
                entry.port,
                device_name,
            );
            Ok(Arc::new(MtconnectAdapter::new(cfg)))
        }
        AdapterKind::Robot => {
            let model = entry
                .model
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_UR_RTDE_MODEL.to_string());
            let cfg = UrRtdeAdapterConfig::new(
                entry.adapter_id.clone(),
                entry.friendly_name.clone(),
                entry.host.clone(),
                entry.port,
                model,
            );
            Ok(Arc::new(UrRtdeAdapter::new(cfg)))
        }
        AdapterKind::Laser => {
            let device_name = entry
                .device_name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_LASER_DEVICE_NAME.to_string());
            let cfg = TrumpfAdapterConfig::new(
                entry.adapter_id.clone(),
                entry.friendly_name.clone(),
                entry.host.clone(),
                entry.port,
                device_name,
            );
            // `TrumpfAdapter::new` selects the v1 MTConnect backend. The
            // backend is deliberately NOT an operator field — swapping
            // it is a code decision (`TrumpfAdapter::with_source`), per
            // [[trust-code-not-operator]].
            Ok(Arc::new(TrumpfAdapter::new(cfg)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(kind: AdapterKind, host: &str, port: u16) -> AdapterConfigEntry {
        AdapterConfigEntry {
            kind,
            adapter_id: "adapter-a".to_string(),
            friendly_name: "Adapter A".to_string(),
            host: host.to_string(),
            port,
            device_name: None,
            model: None,
        }
    }

    /// Every kind's wire string is byte-identical to its built
    /// adapter's `Adapter::kind()` — the join key the Adapters page
    /// relies on. A drift here would orphan live-health from config.
    #[test]
    fn wire_str_matches_built_adapter_kind() {
        // barcode binds a local addr; the others accept a hostname.
        let cases = [
            (AdapterKind::BarcodeScanner, "127.0.0.1"),
            (AdapterKind::LabelPrinter, "printer.local"),
            (AdapterKind::Cnc, "cnc.local"),
            (AdapterKind::Robot, "robot.local"),
            (AdapterKind::Laser, "laser-gateway.local"),
        ];
        assert_eq!(
            cases.len(),
            AdapterKind::all().len(),
            "every AdapterKind must be exercised here — a new kind added \
             without a case would slip past this join-key pin"
        );
        for (kind, host) in cases {
            let entry = base(kind, host, 9000);
            let adapter = build_adapter(&entry).expect("build");
            assert_eq!(
                kind.wire_str(),
                adapter.kind(),
                "wire string must equal Adapter::kind() for {kind:?}"
            );
            // The registry key is the adapter_id.
            assert_eq!(adapter.name(), "adapter-a");
        }
    }

    /// `AdapterKind` round-trips through its wire string for every
    /// variant (closed-vocab integrity).
    #[test]
    fn kind_wire_round_trips() {
        for kind in AdapterKind::all() {
            assert_eq!(AdapterKind::from_wire_str(kind.wire_str()), Some(kind));
        }
        assert_eq!(AdapterKind::from_wire_str("totally-unknown"), None);
    }

    /// A barcode entry whose host is not an IP address loud-fails at
    /// build (it is a bind address, not a hostname). The other kinds
    /// accept the same string as a remote hostname.
    #[test]
    fn barcode_host_must_be_ip() {
        let entry = base(AdapterKind::BarcodeScanner, "not-an-ip.local", 5800);
        let err = build_adapter(&entry).expect_err("non-IP barcode host must fail");
        assert!(matches!(err, AdapterConfigError::BadListenAddr { .. }));
        // Same string is fine for a label printer (remote hostname).
        let ok = base(AdapterKind::LabelPrinter, "not-an-ip.local", 9100);
        assert!(build_adapter(&ok).is_ok());
    }

    /// `validate` surfaces every text-field problem at once.
    #[test]
    fn validate_collects_all_field_errors() {
        let entry = AdapterConfigEntry {
            kind: AdapterKind::LabelPrinter,
            adapter_id: "  ".to_string(),
            friendly_name: String::new(),
            host: String::new(),
            port: 0,
            device_name: None,
            model: None,
        };
        let errs = entry.validate().expect_err("must fail");
        let fields: Vec<&str> = errs.iter().map(|e| e.field).collect();
        assert!(fields.contains(&"adapter_id"));
        assert!(fields.contains(&"friendly_name"));
        assert!(fields.contains(&"host"));
        assert!(fields.contains(&"port"));
    }

    /// A TOML metachar in a text field is rejected so a later read
    /// can't be broken by an operator-typed `"` or `]`.
    #[test]
    fn validate_rejects_toml_metachars() {
        let mut entry = base(AdapterKind::LabelPrinter, "printer.local", 9100);
        entry.friendly_name = "Bench \"A\"".to_string();
        let errs = entry.validate().expect_err("quote must fail");
        assert!(errs.iter().any(|e| e.field == "friendly_name"));
    }

    /// MTConnect device_name / UR model default when omitted — mirrors
    /// the env-boot defaults so a migrated entry behaves identically.
    #[test]
    fn kind_specific_defaults_applied_on_build() {
        let cnc = base(AdapterKind::Cnc, "cnc.local", 5000);
        assert!(build_adapter(&cnc).is_ok());
        let robot = base(AdapterKind::Robot, "robot.local", 30004);
        assert!(build_adapter(&robot).is_ok());
        let laser = base(AdapterKind::Laser, "laser-gateway.local", 5000);
        assert!(build_adapter(&laser).is_ok());
    }

    /// A laser entry builds an adapter whose endpoint metadata round-
    /// trips from the config entry — the Adapters page shows the
    /// gateway the operator typed, not a default.
    #[test]
    fn laser_entry_carries_endpoint_through_to_the_adapter() {
        let mut entry = base(AdapterKind::Laser, "10.0.2.40", 5000);
        entry.device_name = Some("TruLaser5030".to_string());
        let adapter = build_adapter(&entry).expect("build laser");
        assert_eq!(adapter.kind(), "laser-cutter");
        assert_eq!(adapter.endpoint_host().as_deref(), Some("10.0.2.40"));
        assert_eq!(adapter.endpoint_port(), Some(5000));
    }

    #[test]
    fn endpoint_key_is_host_colon_port() {
        let entry = base(AdapterKind::LabelPrinter, " printer.local ", 9100);
        assert_eq!(entry.endpoint_key(), "printer.local:9100");
    }
}
