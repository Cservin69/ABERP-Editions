//! [`MockExportControlProvider`] — a deterministic, **non-production** backend.
//!
//! ⚠️ It answers [`ExportClassification::NotClassified`] for every item and
//! [`ScreeningResult::Clear`] for every party. It performs NO real
//! classification and NO denied-party screening — relying on it for a real
//! shipment would be an export-control violation. It logs a WARN on every
//! construction so it can never silently back a production decision (same
//! guard as the S344 `MockProvider`).

use crate::export_control::{
    Classifiable, ExportClassification, ExportControlError, ExportControlProvider, PartyRef,
    ScreeningResult,
};

/// A deterministic, non-production [`ExportControlProvider`].
#[derive(Debug, Clone, Default)]
pub struct MockExportControlProvider;

impl MockExportControlProvider {
    /// Construct the mock. Emits a WARN — by design — so a production boot
    /// that falls through to the mock is loud, not silent.
    pub fn new() -> Self {
        tracing::warn!(
            "ExportControlProvider: MOCK — performs NO classification or screening, NOT FOR PRODUCTION USE"
        );
        Self
    }
}

impl ExportControlProvider for MockExportControlProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn classify(
        &self,
        _item: &dyn Classifiable,
    ) -> Result<ExportClassification, ExportControlError> {
        Ok(ExportClassification::NotClassified)
    }

    /// S441 (review finding #1) — answers [`ScreeningResult::NotScreened`], NOT
    /// `Clear`. `Clear` is a positive finding ("we consulted the denied-party
    /// lists and this party is not on them"); this backend consults nothing, so
    /// claiming it would put a fabricated screening result into an append-only,
    /// hash-chained ITAR row. Symmetric with [`Self::classify`] answering
    /// `NotClassified` rather than `EAR99`.
    fn screen_party(&self, _party: &PartyRef) -> Result<ScreeningResult, ExportControlError> {
        Ok(ScreeningResult::NotScreened)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial [`Classifiable`] for the mock tests.
    struct Item(&'static str);
    impl Classifiable for Item {
        fn classification_descriptor(&self) -> String {
            self.0.to_string()
        }
    }

    #[test]
    fn s345_mock_export_classify_returns_not_classified() {
        let p = MockExportControlProvider::new();
        let got = p.classify(&Item("AL-6061-BRACKET")).expect("classify");
        assert_eq!(got, ExportClassification::NotClassified);
    }

    /// S441 (review finding #1) — this test previously asserted
    /// `ScreeningResult::Clear`, which is what let the mock manufacture a
    /// `decision="granted"` compliance row. The mock screens nothing, so the
    /// only honest answer is `NotScreened`. `Clear` is now reserved for a
    /// backend that actually consulted the lists.
    #[test]
    fn s441_mock_export_screen_party_returns_not_screened_never_clear() {
        let p = MockExportControlProvider::new();
        let party = PartyRef {
            name: "ACME Aerospace GmbH".to_string(),
            country: Some("DE".to_string()),
        };
        let got = p.screen_party(&party).expect("screen");
        assert_eq!(got, ScreeningResult::NotScreened);
        assert_ne!(
            got,
            ScreeningResult::Clear,
            "a backend that consults no lists must never report a CLEAR screen"
        );
    }

    #[test]
    fn s345_mock_export_provider_name_is_mock() {
        assert_eq!(MockExportControlProvider::new().name(), "mock");
    }

    #[test]
    fn s345_mock_export_provider_logs_warning_on_construction() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct BufMaker(Arc<Mutex<Vec<u8>>>);
        struct BufGuard(Arc<Mutex<Vec<u8>>>);
        impl Write for BufGuard {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufMaker {
            type Writer = BufGuard;
            fn make_writer(&'a self) -> Self::Writer {
                BufGuard(self.0.clone())
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufMaker(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let _p = MockExportControlProvider::new();
        });

        let logged = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8 log");
        assert!(
            logged.contains("NOT FOR PRODUCTION USE"),
            "construction must emit the production-guard WARN; got: {logged:?}"
        );
        assert!(
            logged.contains("WARN"),
            "the guard line must be at WARN level"
        );
    }

    #[test]
    fn s345_export_classification_roundtrip() {
        for c in [
            ExportClassification::ECCN("7A994".to_string()),
            ExportClassification::USMLCategory("VIII(h)".to_string()),
            ExportClassification::EAR99,
            ExportClassification::NotClassified,
            ExportClassification::Pending,
        ] {
            let json = serde_json::to_string(&c).expect("serialize");
            let back: ExportClassification = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(c, back);
        }
    }

    #[test]
    fn s345_export_classification_eccn_carries_code() {
        match ExportClassification::ECCN("3A001".to_string()) {
            ExportClassification::ECCN(code) => assert_eq!(code, "3A001"),
            other => panic!("expected ECCN, got {other:?}"),
        }
    }
}
