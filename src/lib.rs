//! cortex-agent Library
//!
//! 提供 cortex-agent 的核心功能。
#![allow(clippy::collapsible_if)]

pub mod bootstrap;
pub mod config;
pub mod domain;
pub mod error;
pub mod prompts;
pub mod skill;

pub mod agent;
pub mod infra;
pub mod llm;
pub mod model_provider;
pub mod monitor;
pub mod server;
pub mod tools;
