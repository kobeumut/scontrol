#![forbid(unsafe_code)]

//! panel-core: shared types, errors, and adapter traits for the server control panel.

pub mod adapters;
pub mod agent_client;
pub mod agent_protocol;
pub mod database_services;
pub mod error;
pub mod metrics;
pub mod observability;
pub mod previews;
pub mod schedule;
pub mod security;
pub mod types;
