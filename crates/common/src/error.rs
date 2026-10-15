//! 共享错误类型。

use thiserror::Error;

/// HMAC 签名校验错误（SPEC §5）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignError {
    /// `X-SSM-Signature` 不是合法的十六进制串。
    #[error("signature is not valid hex: {0}")]
    InvalidHex(String),

    /// 签名与本地计算结果不匹配（内容被篡改或密钥不对）。
    #[error("signature mismatch")]
    Mismatch,

    /// 时间戳超出 ±300s 容忍窗（防重放）。
    #[error("timestamp {timestamp} outside allowed skew: |{timestamp} - {now}| = {delta}s > {max_skew}s")]
    TimestampOutOfWindow {
        timestamp: i64,
        now: i64,
        delta: i64,
        max_skew: i64,
    },
}

/// 配置文件加载错误。
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse TOML in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to parse {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}
