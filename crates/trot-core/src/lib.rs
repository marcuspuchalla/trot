//! TROT core — the tracking engine as a library.
//!
//! Presentation-agnostic: device ingestion, session lifecycle, storage, and a
//! stable local HTTP/WS API. Anything on top (the `trot` CLI, or the Nowhere UI)
//! consumes this over that API — it never links UI concerns in here.

pub mod api;
pub mod app;
pub mod ble;
pub mod config;
pub mod db;
pub mod diagnostics;
pub mod drivers;
pub mod engine;
pub mod telemetry;

#[cfg(test)]
mod pipeline_tests;

pub use app::AppState;
pub use engine::{start_engine, Engine};
