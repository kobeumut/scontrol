//! Thin re-export of the shared Argon2id implementation in `panel-core`, so
//! `panel-api` and `panel-cli` never drift on hashing parameters.

pub use panel_core::security::{hash_password, verify_password};
