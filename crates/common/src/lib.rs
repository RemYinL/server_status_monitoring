//! `ssm-common`：协议、配置、签名的唯一来源。
//!
//! - [`protocol`]：线上 JSON 契约（SPEC §4 / docs/03）
//! - [`config`]：`agent.toml` / `relay.toml` / `secrets.toml`（SPEC §6）
//! - [`sign`]：HMAC-SHA256 上报签名（SPEC §5）
//! - [`error`]：共享错误类型

pub mod config;
pub mod error;
pub mod protocol;
pub mod sign;

pub use config::{
    AgentConfig, BarkConfig, GitRepoConfig, HostConfig, LogLevel, LogRule, LogSource, PathKind,
    ProbeConfig, ProbeKind, ProbePath, RelayConfig, SecretsConfig, ThresholdConfig,
};
pub use error::{ConfigError, SignError};
pub use protocol::PROTOCOL_VERSION;
pub use sign::{sign, verify, MAX_SKEW_SEC, SIGNATURE_HEADER, TIMESTAMP_HEADER};

/// 协议与配置模块的全部类型，`use ssm_common::prelude::*;` 一把抓。
pub mod prelude {
    pub use crate::config::*;
    pub use crate::error::{ConfigError, SignError};
    pub use crate::protocol::*;
    pub use crate::sign::{sign, verify, MAX_SKEW_SEC, SIGNATURE_HEADER, TIMESTAMP_HEADER};
}
