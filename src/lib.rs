//! Servant — per-user static file server with dual-plane (TCP serving +
//! UDS control) architecture.

pub mod cli;
pub mod client;
pub mod config;
pub mod control;
pub mod daemon;
pub mod db;
pub mod guards;
pub mod host;
pub mod install;
pub mod lifecycle;
pub mod listing;
pub mod output;
pub mod paths;
pub mod reaper;
pub mod serve;
pub mod ttl;
pub mod url_alloc;
