//! Backwards-compatibility shim (T05): the only public fetch API is
//! [`crate::fetch::SafeFetcher`].
//!
//! The old `build_client` baked a synchronous redirect policy into the
//! `Client`, which falsely promised safety by calling convention — and the
//! avatar path bypassed even the first-hop check. New code must fetch
//! untrusted URLs through `SafeFetcher` (manual per-hop re-checks, hop cap,
//! streaming byte cap), never a bare client.

pub use crate::fetch::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_REDIRECTS, FetchError, FetchedDoc, SafeFetcher,
};
