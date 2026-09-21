//! Re-encodes files on a Filen cloud drive into more efficient formats without
//! losing data, driving rclone's Filen backend.
//!
//! The crate is split into a library and a thin binary so integration tests can
//! exercise the same code paths the CLI uses.

pub mod classify;
pub mod cli;
pub mod config;
pub mod convert;
pub mod governor;
pub mod hash;
pub mod ledger;
pub mod pipeline;
pub mod policy;
pub mod preflight;
pub mod remote;
pub mod report;
pub mod scope;
pub mod staging;
pub mod trash;
