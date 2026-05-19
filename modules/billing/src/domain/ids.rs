//! ULID newtypes owned by the billing module.
//!
//! Per ADR-0005 every entity gets a newtype; type confusion is a compile
//! error, not a runtime hunt. The storage key is the bare ULID; the
//! prefixed form (`inv_<ULID>`, `cus_<ULID>`, ...) is what crosses
//! serialization boundaries.
//!
//! No serde derives here for the same reason as
//! `crate::audit-ledger::entry::ids`: PR-4 does not run these types
//! through serde. Add the derive + enable `ulid`'s `serde` feature when
//! a serialization path actually needs them.

use ulid::Ulid;

macro_rules! ulid_newtype {
    ($name:ident, $prefix:literal) => {
        /// ULID newtype. See [`ADR-0005`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub Ulid);

        impl $name {
            /// Generate a new ID with the monotonic ULID counter.
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            /// Render in ADR-0005 prefixed form, e.g. `inv_01J9...`.
            pub fn to_prefixed_string(&self) -> String {
                format!("{}_{}", $prefix, self.0)
            }

            /// Bare ULID for storage and indexing.
            pub fn as_ulid(&self) -> Ulid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

ulid_newtype!(InvoiceId, "inv");
ulid_newtype!(CustomerId, "cus");
ulid_newtype!(SeriesId, "srs");
ulid_newtype!(ReservationId, "rsv");
