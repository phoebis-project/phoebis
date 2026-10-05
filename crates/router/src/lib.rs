//! Wiring for the phoebis binary, exposed as a library so integration tests can build the app.

pub mod app;
pub mod config;
pub mod health;
pub mod listen;
pub mod tls;
