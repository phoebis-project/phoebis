//! Wiring for the phoebis binary, exposed as a library so integration tests can build the app.

pub mod admin_client;
pub mod app;
pub mod backup;
pub mod config;
pub mod health;
pub mod init;
pub mod listen;
pub mod selfsigned;
pub mod status;
pub mod subcommands;
pub mod tls;
