//! Forward-tolerant deserialization of ABERP-site quote payloads.
//!
//! Authoritative schema: `ABERP-site/src/lib/server/quote-store.ts`
//! `interface QuoteMetadata`. Unknown fields flow into a
//! `serde_json::Value` tail so future ABERP-site additions don't
//! crash the daemon.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuoteListResponse {
    pub quotes: Vec<Quote>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Quote {
    pub id: String,
    pub received_at: String,
    pub contact: QuoteContact,
    pub request: QuoteRequest,
    #[serde(default)]
    pub files: Vec<QuoteFile>,
    pub status: String,
    pub consent_at: String,
    #[serde(default)]
    pub status_history: Option<Vec<QuoteStatusHistoryEntry>>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuoteContact {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub company: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuoteRequest {
    pub material_preference: String,
    #[serde(default)]
    pub quantity: Option<i64>,
    #[serde(default)]
    pub deadline: Option<String>,
    pub notes: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuoteFile {
    pub filename: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub stored_at: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuoteStatusHistoryEntry {
    pub at: String,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub notes: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusWritebackBody {
    pub status: String,
    pub notes: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_envelope_round_trip() {
        let raw = r#"{"quotes":[]}"#;
        let parsed: QuoteListResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.quotes.is_empty());
    }

    #[test]
    fn forward_tolerant_on_unknown_fields() {
        let raw = r#"{
            "quotes": [{
                "id": "11111111-2222-3333-4444-555555555555",
                "received_at": "2026-05-31T12:00:00Z",
                "currency": "EUR",
                "future_top_field": {"nested": 42},
                "contact": {
                    "name": "Test Customer",
                    "email": "test@example.com",
                    "company": "",
                    "priority": "high"
                },
                "request": {
                    "material_preference": "aluminum",
                    "quantity": 5,
                    "deadline": "2026-06-30",
                    "notes": "rush",
                    "tolerance_mm": 0.05
                },
                "files": [{
                    "filename": "part.step",
                    "size_bytes": 1024,
                    "stored_at": "2026-05-31T12:00:01Z",
                    "checksum": "sha256:abc"
                }],
                "status": "approved",
                "consent_at": "2026-05-31T11:59:00Z"
            }]
        }"#;
        let parsed: QuoteListResponse = serde_json::from_str(raw).unwrap();
        let q = &parsed.quotes[0];
        assert_eq!(q.id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(q.contact.name, "Test Customer");
        assert_eq!(q.request.quantity, Some(5));
        assert_eq!(q.files[0].filename, "part.step");
    }

    #[test]
    fn rejects_missing_required_field() {
        let raw = r#"{
            "quotes": [{
                "received_at": "2026-05-31T12:00:00Z",
                "contact": {"name": "n", "email": "e", "company": ""},
                "request": {"material_preference": "x", "notes": ""},
                "status": "approved",
                "consent_at": "x"
            }]
        }"#;
        let err = serde_json::from_str::<QuoteListResponse>(raw).unwrap_err();
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[test]
    fn handles_minimum_viable_quote() {
        let raw = r#"{
            "quotes": [{
                "id": "abc",
                "received_at": "t",
                "contact": {"name": "n", "email": "e", "company": ""},
                "request": {"material_preference": "x", "quantity": null, "notes": ""},
                "files": [],
                "status": "approved",
                "consent_at": "t"
            }]
        }"#;
        let parsed: QuoteListResponse = serde_json::from_str(raw).unwrap();
        let q = &parsed.quotes[0];
        assert_eq!(q.request.quantity, None);
        assert_eq!(q.request.deadline, None);
        assert!(q.contact.company.is_empty());
        assert!(q.files.is_empty());
    }
}
