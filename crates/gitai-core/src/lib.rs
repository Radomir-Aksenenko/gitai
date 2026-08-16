//! Domain core for gitai.
//!
//! This crate holds the types every other crate agrees on, plus the four
//! boundaries the system is built around:
//!
//! - [`forge::Forge`] - where issues and pull requests live
//! - [`llm::LlmProvider`] - where the thinking happens
//! - [`sandbox::Sandbox`] - where model-written code is allowed to run
//! - [`store::Store`] - where state and the job queue live
//!
//! Nothing here performs I/O. Adapters implement the traits, and
//! `gitai-pipeline` composes them.

pub mod config;
pub mod error;
pub mod event;
pub mod forge;
pub mod llm;
pub mod model;
pub mod sandbox;
pub mod store;

pub use error::{Error, Result};

/// Marks branches, commits and pull requests created by gitai.
pub const BRANCH_PREFIX: &str = "gitai/";

/// User agent sent to forges and model providers.
pub const USER_AGENT: &str = concat!("gitai/", env!("CARGO_PKG_VERSION"));
