//! Wiring for the phoebis binary, exposed as a library so integration tests can build the app.

pub mod admin_client;
pub mod app;
pub mod backup;
pub mod config;
pub mod health;
pub mod init;
pub mod listen;
pub mod status;
pub mod tls;
pub mod tls_reload;
