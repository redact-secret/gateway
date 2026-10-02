//! RedactSecret Gateway internals.
//!
//! This library target is private to the repository. It exists so the binary and the
//! API-boundary tests share one crate; it is not a published or supported API and no
//! plugin interface exists (ADR 0001, ADR 0011).
//!
//! Authority direction (ADR 0005): `protocol` -> `boundary` / `core_bridge` -> `transport`.
//! Only `boundary` can construct [`boundary::SanitizedRequest`] and only `transport`
//! accepts it.
#![forbid(unsafe_code)]

pub mod admission;
pub mod boundary;
pub mod chat_route;
pub mod cli;
pub mod config;
pub mod core_bridge;
pub mod health;
pub mod protocol;
pub mod server;
pub mod telemetry;
pub mod transport;
mod write_stall;

/// One-line version report printed by `--version`. Contains no configuration or secrets.
#[must_use]
pub fn version_line() -> String {
    format!(
        "redact-secret-gateway {} (core redact-secret {})",
        env!("CARGO_PKG_VERSION"),
        core_bridge::PINNED_CORE_VERSION
    )
}
