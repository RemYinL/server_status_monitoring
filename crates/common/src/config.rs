//! 配置契约（SPEC §6）。
//!
//! 两层配置：
//! - `agent.toml` / `relay.toml`：可入库，`.example` 进仓库；
//! - `secrets.toml`：600 权限，**已被 .gitignore 排除**，含 HMAC 共享密钥与 `/status` Token。
//!
//! 字段名严格对齐 SPEC §6 的字面 TOML 示例。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

// ---------------------------------------------------------------------------
// secrets.toml
// ---------------------------------------------------------------------------

/// `secrets.toml`：agent / relay 共用的敏感配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// agent ↔ relay 的 HMAC 共享密钥。
    pub shared_secret: String,
    /// `/status` 的 Bearer Token。
    pub status_token: String,
    #[serde(default)]
    pub bark: BarkConfig,
}

/// Bark 推送配置（`[bark]`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarkConfig {
    #[serde(default = "default_bark_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_bark_group")]
    pub group: String,
}

fn default_bark_endpoint() -> String {
    "https://api.day.app".to_string()
}

fn default_bark_group() -> String {
    "server_status".to_string()
}

impl Default for BarkConfig {
    fn default() -> Self {
        Self {
            endpoint: default_bark_endpoint(),
            key: String::new(),
            group: default_bark_group(),
        }
    }
}

// ---------------------------------------------------------------------------
// agent.toml
// ---------------------------------------------------------------------------

/// `agent.toml`（SPEC §6）。
///
/// **严格模式**：`deny_unknown_fields`。`agent.toml` 与 `relay.toml` 是两个独立文件、
/// 不共享，所以未知键一定是拼写错误。这一点很关键：本结构体大多数字段带
/// `#[serde(default)]`，宽容模式下把 `report_interval_sec` 拼成
/// `report_intervall_sec` 会被静默忽略、悄悄跑 30s 默认值——属于最难查的一类运维 bug。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// 本机标识，必须与 relay 的 `[[hosts]].host_id` 一致。
    pub host_id: String,
    /// relay 上报地址，如 `http://127.0.0.1:9100/report`。
    pub relay_url: String,

    #[serde(default = "default_report_interval_sec")]
    pub report_interval_sec: u64,
    #[serde(default = "default_probe_listen")]
    pub probe_listen: String,
    #[serde(default = "default_seq_state_path")]
    pub seq_state_path: PathBuf,
    #[serde(default = "default_buffer_path")]
    pub buffer_path: PathBuf,
    #[serde(default = "default_buffer_max")]
    pub buffer_max: usize,
    #[serde(default = "default_secrets_path")]
    pub secrets_path: PathBuf,
    /// 需要监控的挂载点，如 `["/"]`。
    #[serde(default = "default_disks")]
    pub disks: Vec<String>,

    #[serde(default)]
    pub probes: Vec<ProbeConfig>,
    #[serde(default)]
    pub log_rules: Vec<LogRule>,
    #[serde(default)]
    pub git_repos: Vec<GitRepoConfig>,
    #[serde(default)]
    pub thresholds: ThresholdConfig,
}

fn default_report_interval_sec() -> u64 {
    30
}

fn default_probe_listen() -> String {
    "0.0.0.0:9101".to_string()
}

fn default_seq_state_path() -> PathBuf {
    PathBuf::from("/var/lib/ssm-agent/seq")
}

fn default_buffer_path() -> PathBuf {
    PathBuf::from("/var/lib/ssm-agent/buffer.jsonl")
}

fn default_buffer_max() -> usize {
    100
}

fn default_secrets_path() -> PathBuf {
    PathBuf::from("/etc/ssm-agent/secrets.toml")
}

fn default_disks() -> Vec<String> {
    vec!["/".to_string()]
}

/// 一条探针配置（`[[probes]]`）。严格模式，见 [`AgentConfig`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    pub name: String,
    /// `http | tcp | process | tailscale`。
    ///
    /// 规范键名是 `kind`（SPEC §6 字面如此，也是 `deploy/agent.toml.example` 用的）。
    /// `type` 作为兼容别名一并接受——注意别写反：字段本身已叫 `kind`，
    /// 若把别名写成 `alias = "kind"` 就是自指，等于没加别名，`type =` 会被
    /// `deny_unknown_fields` 拒绝。
    #[serde(alias = "type")]
    pub kind: ProbeKind,
    pub target: String,
    /// 连续失败 N 次才告警（docs/01 F3）。
    #[serde(default = "default_fail_threshold")]
    pub fail_threshold: u32,
    #[serde(default = "default_probe_timeout_sec")]
    pub timeout_sec: u64,
}

fn default_fail_threshold() -> u32 {
    2
}

fn default_probe_timeout_sec() -> u64 {
    5
}

/// 探针类型；线格式为小写（`kind = "http"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeKind {
    Http,
    Tcp,
    Process,
    Tailscale,
}

/// 一条日志规则（`[[log_rules]]`）。严格模式，见 [`AgentConfig`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRule {
    pub name: String,
    /// `journald:<unit>` 或 `file:<绝对路径>`。
    pub source: LogSource,
    /// 正则；缺省表示不过滤（全部行命中）。
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default = "default_log_level")]
    pub level: LogLevel,
    #[serde(default = "default_throttle_sec")]
    pub throttle_sec: u64,
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    /// 默认 `false`（脱敏，docs/04 §2.3）：summary 只含规则名 + 匹配行数。
    #[serde(default)]
    pub include_raw: bool,
}

fn default_log_level() -> LogLevel {
    LogLevel::Warning
}

fn default_throttle_sec() -> u64 {
    300
}

fn default_max_lines() -> usize {
    3
}

/// 日志来源；线格式为**单个字符串** `journald:<unit>` / `file:<绝对路径>`。
///
/// 注意：这里必须手写 serde（不能用 `#[serde(rename_all)]` 默认派生）——
/// 派生写法会把它序列化成 `{ journald = "nginx.service" }` 这种表结构，
/// 与 SPEC §6 `source = "journald:nginx.service"` 的字面契约不符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSource {
    Journald(String),
    File(PathBuf),
}

impl Serialize for LogSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_config_str())
    }
}

impl<'de> Deserialize<'de> for LogSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid log source {raw:?}: expected \"journald:<unit>\" or \"file:<absolute path>\""
            ))
        })
    }
}

impl LogSource {
    /// 从 `"journald:nginx.service"` / `"file:/var/log/app.log"` 字面量解析。
    pub fn parse(s: &str) -> Option<Self> {
        let (scheme, rest) = s.split_once(':')?;
        match scheme {
            "journald" => Some(LogSource::Journald(rest.to_string())),
            "file" => Some(LogSource::File(PathBuf::from(rest))),
            _ => None,
        }
    }

    /// 还原为配置里的字面量形式。
    pub fn as_config_str(&self) -> String {
        match self {
            LogSource::Journald(unit) => format!("journald:{unit}"),
            LogSource::File(path) => format!("file:{}", path.display()),
        }
    }

    /// journald unit 名（仅 `Journald` 变体）。
    pub fn unit(&self) -> Option<&str> {
        match self {
            LogSource::Journald(unit) => Some(unit),
            LogSource::File(_) => None,
        }
    }

    /// 文件路径（仅 `File` 变体）。
    pub fn path(&self) -> Option<&Path> {
        match self {
            LogSource::File(path) => Some(path.as_path()),
            LogSource::Journald(_) => None,
        }
    }
}

/// 告警级别；线格式为小写（`level = "warning"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warning,
    Critical,
}

/// 一个被跟踪的 git 仓库（`[[git_repos]]`）。严格模式，见 [`AgentConfig`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRepoConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_git_timeout_sec")]
    pub timeout_sec: u64,
}

fn default_git_timeout_sec() -> u64 {
    5
}

/// 阈值规则（`[thresholds]`）。严格模式，见 [`AgentConfig`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdConfig {
    #[serde(default = "default_disk_percent")]
    pub disk_percent: f64,
    #[serde(default = "default_disk_throttle_sec")]
    pub disk_throttle_sec: u64,
    #[serde(default = "default_cpu_percent")]
    pub cpu_percent: f64,
    #[serde(default = "default_mem_percent")]
    pub mem_percent: f64,
    /// CPU / 内存需持续超阈该秒数才告警。
    #[serde(default = "default_sustained_sec")]
    pub sustained_sec: u64,
}

fn default_disk_percent() -> f64 {
    85.0
}

fn default_disk_throttle_sec() -> u64 {
    21_600
}

fn default_cpu_percent() -> f64 {
    95.0
}

fn default_mem_percent() -> f64 {
    90.0
}

fn default_sustained_sec() -> u64 {
    300
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        Self {
            disk_percent: default_disk_percent(),
            disk_throttle_sec: default_disk_throttle_sec(),
            cpu_percent: default_cpu_percent(),
            mem_percent: default_mem_percent(),
            sustained_sec: default_sustained_sec(),
        }
    }
}

// ---------------------------------------------------------------------------
// relay.toml
// ---------------------------------------------------------------------------

/// `relay.toml`（SPEC §6）。
///
/// **严格模式**：`deny_unknown_fields`，理由同 [`AgentConfig`]——独立文件，
/// 未知键必是拼写错误，而带默认值的键拼错会静默退化。
///
/// 存储分层（docs/07 §3）：`db_path` 是 **tmpfs 上的热台账**（掉电即失、零磁盘磨损），
/// `snapshot_path` 是 **SSD 上的周期快照**；relay 重启时若 `db_path` 不存在而
/// `snapshot_path` 存在，则先从快照恢复，避免把「重启」误报成「离线」（docs/03 §5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// 热台账路径，默认在 tmpfs（`/run/ssm-relay/ledger.db`）。
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// 周期快照路径，默认在持久分区（`/var/lib/ssm-relay/ledger.snapshot.db`）。
    /// **必须与 `db_path` 不同**：指向同一文件会让快照覆盖热台账。
    #[serde(default = "default_snapshot_path")]
    pub snapshot_path: PathBuf,
    #[serde(default = "default_relay_secrets_path")]
    pub secrets_path: PathBuf,

    /// 心跳超时阈值（= 3 个上报周期）。
    #[serde(default = "default_heartbeat_timeout_sec")]
    pub heartbeat_timeout_sec: u64,
    #[serde(default = "default_handshake_retries")]
    pub handshake_retries: u32,
    #[serde(default = "default_handshake_interval_sec")]
    pub handshake_interval_sec: u64,
    #[serde(default = "default_handshake_timeout_sec")]
    pub handshake_timeout_sec: u64,
    /// 持续离线期间的重复提醒间隔；`0` = 不重复。
    #[serde(default = "default_repeat_alert_min")]
    pub repeat_alert_min: u64,
    /// 启动宽限期（上报周期数），避免 relay 重启误报。
    #[serde(default = "default_startup_grace_periods")]
    pub startup_grace_periods: u32,
    /// Bark 全局限流（每分钟）。
    #[serde(default = "default_rate_limit_per_min")]
    pub rate_limit_per_min: u32,
    /// SQLite 快照落盘间隔（tmpfs 场景）。
    #[serde(default = "default_snapshot_interval_sec")]
    pub snapshot_interval_sec: u64,
    /// 补报过期阈值：`sent_at` 超过该秒数的告警只入库不推送。
    #[serde(default = "default_lag_threshold_sec")]
    pub lag_threshold_sec: u64,

    /// 被监控主机清单，顺序即 `/status` 返回顺序。
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
}

fn default_listen() -> String {
    "0.0.0.0:9100".to_string()
}

/// 热台账默认放 **tmpfs**（SPEC §6 / docs/07 §3）：掉电即失但零磁盘磨损，
/// 持久性由 `snapshot_path` 的周期快照保证。
fn default_db_path() -> PathBuf {
    PathBuf::from("/run/ssm-relay/ledger.db")
}

/// 周期快照默认放 **持久分区**（SPEC §6）。
fn default_snapshot_path() -> PathBuf {
    PathBuf::from("/var/lib/ssm-relay/ledger.snapshot.db")
}

fn default_relay_secrets_path() -> PathBuf {
    PathBuf::from("/etc/ssm-relay/secrets.toml")
}

fn default_heartbeat_timeout_sec() -> u64 {
    90
}

fn default_handshake_retries() -> u32 {
    3
}

fn default_handshake_interval_sec() -> u64 {
    10
}

fn default_handshake_timeout_sec() -> u64 {
    5
}

fn default_repeat_alert_min() -> u64 {
    30
}

fn default_startup_grace_periods() -> u32 {
    2
}

fn default_rate_limit_per_min() -> u32 {
    5
}

fn default_snapshot_interval_sec() -> u64 {
    300
}

fn default_lag_threshold_sec() -> u64 {
    300
}

/// 一台被监控主机（`[[hosts]]`）。严格模式，见 [`RelayConfig`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub host_id: String,
    /// 握手探测路径，可声明 `lan` 与 `tailscale` 两条（docs/03 §2 判定矩阵）。
    #[serde(default)]
    pub paths: Vec<ProbePath>,
}

/// 一条握手探测路径（`[[hosts.paths]]`）。严格模式，见 [`RelayConfig`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbePath {
    pub kind: PathKind,
    /// 完整 URL，如 `http://192.168.1.10:9101/probe`。
    pub url: String,
}

/// 探测路径类型；线格式为小写（`kind = "lan"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    Lan,
    Tailscale,
}

impl HostConfig {
    /// 取指定类型的路径。
    pub fn path(&self, kind: PathKind) -> Option<&ProbePath> {
        self.paths.iter().find(|p| p.kind == kind)
    }
}

// ---------------------------------------------------------------------------
// 加载
// ---------------------------------------------------------------------------

impl AgentConfig {
    /// 从 TOML 文件加载。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        load_toml(path.as_ref())
    }
}

impl RelayConfig {
    /// 从 TOML 文件加载。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        load_toml(path.as_ref())
    }
}

impl SecretsConfig {
    /// 从 TOML 文件加载，并检查文件权限（非 600 时告警，SPEC §6）。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let cfg: Self = load_toml(path)?;
        warn_if_world_readable(path);
        Ok(cfg)
    }
}

fn load_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:o}"),
                "secrets 文件权限过宽，建议 chmod 600（SPEC §6）"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §6 的 `agent.toml` 字面示例（**不要改写**）。
    const DOC_AGENT_TOML: &str = r#"
host_id             = "kebei-prod-01"
relay_url           = "http://127.0.0.1:9100/report"
report_interval_sec = 30
probe_listen        = "0.0.0.0:9101"
seq_state_path      = "/var/lib/ssm-agent/seq"
buffer_path         = "/var/lib/ssm-agent/buffer.jsonl"
buffer_max          = 100
secrets_path        = "/etc/ssm-agent/secrets.toml"
disks               = ["/"]

[[probes]]
name = "web-frontend"
kind = "http"              # http | tcp | process | tailscale
target = "https://example.com"
fail_threshold = 2         # 连续失败 N 次才告警（docs/01 F3）
timeout_sec = 5

[[log_rules]]
name = "nginx-error"
source = "journald:nginx.service"   # journald:<unit> 或 file:<绝对路径>
pattern = "ERROR|upstream timed out"
level = "warning"
throttle_sec = 300
max_lines = 3
include_raw = false        # 默认 false（脱敏，docs/04 §2.3）

[[git_repos]]
name = "kebei-web"
path = "/srv/kebei-web"

[thresholds]
disk_percent = 85.0
disk_throttle_sec = 21600
cpu_percent = 95.0
mem_percent = 90.0
sustained_sec = 300
"#;

    /// SPEC §6 的 `relay.toml` 字面示例（**不要改写**）。
    const DOC_RELAY_TOML: &str = r#"
listen               = "0.0.0.0:9100"
db_path              = "/run/ssm-relay/ledger.db"
snapshot_path        = "/var/lib/ssm-relay/ledger.snapshot.db"
secrets_path         = "/etc/ssm-relay/secrets.toml"
heartbeat_timeout_sec = 90
handshake_retries     = 3
handshake_interval_sec = 10
handshake_timeout_sec  = 5
repeat_alert_min       = 30
startup_grace_periods  = 2
rate_limit_per_min     = 5
snapshot_interval_sec  = 300     # SQLite 快照落盘间隔（tmpfs 场景）
lag_threshold_sec      = 300     # 补报过期告警只入库不推送

[[hosts]]
host_id = "kebei-prod-01"
[[hosts.paths]]
kind = "lan"                     # lan | tailscale
url  = "http://192.168.1.10:9101/probe"
[[hosts.paths]]
kind = "tailscale"
url  = "http://100.x.x.x:9101/probe"
"#;

    /// SPEC §6 的 `secrets.toml` 字面示例（**不要改写**）。
    const DOC_SECRETS_TOML: &str = r#"
shared_secret = "..."
status_token  = "..."
[bark]
endpoint = "https://api.day.app"
key      = "..."
group    = "server_status"
"#;

    // ---------------- agent.toml ----------------

    #[test]
    fn parse_doc_agent_toml_field_by_field() {
        let cfg: AgentConfig =
            toml::from_str(DOC_AGENT_TOML).expect("SPEC §6 agent.toml 必须可解析");

        assert_eq!(cfg.host_id, "kebei-prod-01");
        assert_eq!(cfg.relay_url, "http://127.0.0.1:9100/report");
        assert_eq!(cfg.report_interval_sec, 30);
        assert_eq!(cfg.probe_listen, "0.0.0.0:9101");
        assert_eq!(cfg.seq_state_path, PathBuf::from("/var/lib/ssm-agent/seq"));
        assert_eq!(
            cfg.buffer_path,
            PathBuf::from("/var/lib/ssm-agent/buffer.jsonl")
        );
        assert_eq!(cfg.buffer_max, 100);
        assert_eq!(
            cfg.secrets_path,
            PathBuf::from("/etc/ssm-agent/secrets.toml")
        );
        assert_eq!(cfg.disks, vec!["/".to_string()]);

        assert_eq!(cfg.probes.len(), 1);
        let probe = &cfg.probes[0];
        assert_eq!(probe.name, "web-frontend");
        assert_eq!(probe.kind, ProbeKind::Http);
        assert_eq!(probe.target, "https://example.com");
        assert_eq!(probe.fail_threshold, 2);
        assert_eq!(probe.timeout_sec, 5);

        assert_eq!(cfg.log_rules.len(), 1);
        let rule = &cfg.log_rules[0];
        assert_eq!(rule.name, "nginx-error");
        assert_eq!(
            rule.source,
            LogSource::Journald("nginx.service".to_string())
        );
        assert_eq!(rule.source.unit(), Some("nginx.service"));
        assert_eq!(rule.source.path(), None);
        assert_eq!(rule.pattern.as_deref(), Some("ERROR|upstream timed out"));
        assert_eq!(rule.level, LogLevel::Warning);
        assert_eq!(rule.throttle_sec, 300);
        assert_eq!(rule.max_lines, 3);
        assert!(!rule.include_raw, "include_raw 默认必须为 false（脱敏）");

        assert_eq!(cfg.git_repos.len(), 1);
        assert_eq!(cfg.git_repos[0].name, "kebei-web");
        assert_eq!(cfg.git_repos[0].path, PathBuf::from("/srv/kebei-web"));

        assert_eq!(cfg.thresholds.disk_percent, 85.0);
        assert_eq!(cfg.thresholds.disk_throttle_sec, 21_600);
        assert_eq!(cfg.thresholds.cpu_percent, 95.0);
        assert_eq!(cfg.thresholds.mem_percent, 90.0);
        assert_eq!(cfg.thresholds.sustained_sec, 300);
    }

    #[test]
    fn agent_toml_roundtrip_is_lossless() {
        let cfg: AgentConfig = toml::from_str(DOC_AGENT_TOML).unwrap();
        let dumped = toml::to_string(&cfg).expect("AgentConfig 必须可序列化");
        let back: AgentConfig = toml::from_str(&dumped).expect("序列化结果必须可回读");
        assert_eq!(back, cfg, "agent.toml 往返必须无损");
    }

    #[test]
    fn agent_toml_minimal_uses_documented_defaults() {
        let cfg: AgentConfig = toml::from_str(
            r#"
host_id = "kebei-prod-01"
relay_url = "http://127.0.0.1:9100/report"
"#,
        )
        .expect("只有必填字段时必须可解析");

        assert_eq!(cfg.report_interval_sec, 30);
        assert_eq!(cfg.probe_listen, "0.0.0.0:9101");
        assert_eq!(cfg.buffer_max, 100);
        assert_eq!(cfg.disks, vec!["/".to_string()]);
        assert!(cfg.probes.is_empty());
        assert!(cfg.log_rules.is_empty());
        assert!(cfg.git_repos.is_empty());
        assert_eq!(cfg.thresholds, ThresholdConfig::default());
        assert_eq!(cfg.thresholds.disk_percent, 85.0);
        assert_eq!(cfg.thresholds.sustained_sec, 300);
    }

    #[test]
    fn agent_toml_missing_host_id_reports_clear_error() {
        let err = toml::from_str::<AgentConfig>(r#"relay_url = "http://127.0.0.1:9100/report""#)
            .expect_err("缺 host_id 必须报错");
        let msg = err.to_string();
        assert!(
            msg.contains("host_id"),
            "错误信息必须点出缺失字段，实际: {msg}"
        );
        assert!(msg.contains("missing field"), "实际: {msg}");
    }

    #[test]
    fn agent_toml_missing_relay_url_reports_clear_error() {
        let err = toml::from_str::<AgentConfig>(r#"host_id = "kebei-prod-01""#)
            .expect_err("缺 relay_url 必须报错");
        let msg = err.to_string();
        assert!(msg.contains("relay_url"), "实际: {msg}");
    }

    #[test]
    fn agent_toml_unknown_probe_kind_reports_clear_error() {
        let err = toml::from_str::<AgentConfig>(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"
[[probes]]
name = "p"
kind = "icmp"
target = "1.1.1.1"
"#,
        )
        .expect_err("未知 kind 必须报错");
        let msg = err.to_string();
        assert!(msg.contains("kind") || msg.contains("icmp"), "实际: {msg}");
    }

    #[test]
    fn probe_and_log_types_accept_all_documented_values() {
        for (text, expected) in [
            ("http", ProbeKind::Http),
            ("tcp", ProbeKind::Tcp),
            ("process", ProbeKind::Process),
            ("tailscale", ProbeKind::Tailscale),
        ] {
            let cfg: AgentConfig = toml::from_str(&format!(
                "host_id = \"h\"\nrelay_url = \"u\"\n[[probes]]\nname = \"p\"\nkind = \"{text}\"\ntarget = \"t\"\n"
            ))
            .unwrap();
            assert_eq!(cfg.probes[0].kind, expected, "kind = {text} 解析错误");
        }

        for (text, expected) in [
            ("info", LogLevel::Info),
            ("warning", LogLevel::Warning),
            ("critical", LogLevel::Critical),
        ] {
            let cfg: AgentConfig = toml::from_str(&format!(
                "host_id = \"h\"\nrelay_url = \"u\"\n[[log_rules]]\nname = \"r\"\nsource = \"file:/var/log/a.log\"\nlevel = \"{text}\"\n"
            ))
            .unwrap();
            assert_eq!(cfg.log_rules[0].level, expected, "level = {text} 解析错误");
        }
    }

    #[test]
    fn log_source_file_variant_and_helpers() {
        let cfg: AgentConfig = toml::from_str(
            r#"
host_id = "h"
relay_url = "u"
[[log_rules]]
name = "app"
source = "file:/var/log/app.log"
"#,
        )
        .unwrap();
        let src = &cfg.log_rules[0].source;
        assert_eq!(src, &LogSource::File(PathBuf::from("/var/log/app.log")));
        assert_eq!(src.path(), Some(Path::new("/var/log/app.log")));
        assert_eq!(src.unit(), None);
        assert_eq!(src.as_config_str(), "file:/var/log/app.log");

        // 日志规则的默认值
        let rule = &cfg.log_rules[0];
        assert_eq!(rule.pattern, None);
        assert_eq!(rule.level, LogLevel::Warning);
        assert_eq!(rule.throttle_sec, 300);
        assert_eq!(rule.max_lines, 3);
        assert!(!rule.include_raw);

        assert_eq!(
            LogSource::parse("journald:nginx.service"),
            Some(LogSource::Journald("nginx.service".into()))
        );
        assert_eq!(
            LogSource::parse("file:/tmp/x.log"),
            Some(LogSource::File(PathBuf::from("/tmp/x.log")))
        );
        assert_eq!(LogSource::parse("syslog:/dev/null"), None);
        assert_eq!(LogSource::parse("nocolon"), None);
    }

    #[test]
    fn log_source_wire_format_is_a_plain_string() {
        // 回归测试：LogSource 必须是 "journald:<unit>" / "file:<path>" 字符串，
        // 不能退化成 serde 默认的 { journald = "..." } 表结构（SPEC §6）。
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            source: LogSource,
        }

        for (src, expected) in [
            (
                LogSource::Journald("nginx.service".into()),
                r#""journald:nginx.service""#,
            ),
            (
                LogSource::File(PathBuf::from("/var/log/app.log")),
                r#""file:/var/log/app.log""#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&src).unwrap(), expected);
            assert_eq!(serde_json::from_str::<LogSource>(expected).unwrap(), src);
            // TOML 只能序列化 table，包一层再断言
            assert_eq!(
                toml::to_string(&Wrap {
                    source: src.clone()
                })
                .unwrap()
                .trim(),
                format!("source = {expected}")
            );
            let back: Wrap = toml::from_str(&format!("source = {expected}")).unwrap();
            assert_eq!(back.source, src);
        }

        assert!(serde_json::from_str::<LogSource>(r#""syslog:/dev/null""#).is_err());
        assert!(serde_json::from_str::<LogSource>(r#""nocolon""#).is_err());
    }

    // ---------------- relay.toml ----------------

    #[test]
    fn parse_doc_relay_toml_field_by_field() {
        let cfg: RelayConfig =
            toml::from_str(DOC_RELAY_TOML).expect("SPEC §6 relay.toml 必须可解析");

        assert_eq!(cfg.listen, "0.0.0.0:9100");
        assert_eq!(cfg.db_path, PathBuf::from("/run/ssm-relay/ledger.db"));
        assert_eq!(
            cfg.snapshot_path,
            PathBuf::from("/var/lib/ssm-relay/ledger.snapshot.db")
        );
        assert_eq!(
            cfg.secrets_path,
            PathBuf::from("/etc/ssm-relay/secrets.toml")
        );
        assert_eq!(cfg.heartbeat_timeout_sec, 90);
        assert_eq!(cfg.handshake_retries, 3);
        assert_eq!(cfg.handshake_interval_sec, 10);
        assert_eq!(cfg.handshake_timeout_sec, 5);
        assert_eq!(cfg.repeat_alert_min, 30);
        assert_eq!(cfg.startup_grace_periods, 2);
        assert_eq!(cfg.rate_limit_per_min, 5);
        assert_eq!(cfg.snapshot_interval_sec, 300);
        assert_eq!(cfg.lag_threshold_sec, 300);

        assert_eq!(cfg.hosts.len(), 1);
        let host = &cfg.hosts[0];
        assert_eq!(host.host_id, "kebei-prod-01");
        assert_eq!(host.paths.len(), 2);
        assert_eq!(host.paths[0].kind, PathKind::Lan);
        assert_eq!(host.paths[0].url, "http://192.168.1.10:9101/probe");
        assert_eq!(host.paths[1].kind, PathKind::Tailscale);
        assert_eq!(host.paths[1].url, "http://100.x.x.x:9101/probe");
        assert_eq!(
            host.path(PathKind::Lan).map(|p| p.url.as_str()),
            Some("http://192.168.1.10:9101/probe")
        );
        assert_eq!(
            host.path(PathKind::Tailscale).map(|p| p.url.as_str()),
            Some("http://100.x.x.x:9101/probe")
        );
    }

    #[test]
    fn relay_toml_roundtrip_is_lossless() {
        let cfg: RelayConfig = toml::from_str(DOC_RELAY_TOML).unwrap();
        let dumped = toml::to_string(&cfg).unwrap();
        let back: RelayConfig = toml::from_str(&dumped).unwrap();
        assert_eq!(back, cfg, "relay.toml 往返必须无损");
    }

    #[test]
    fn relay_toml_empty_uses_documented_defaults() {
        let cfg: RelayConfig = toml::from_str("").expect("全默认的 relay.toml 必须可解析");
        assert_eq!(cfg.listen, "0.0.0.0:9100");
        assert_eq!(cfg.db_path, PathBuf::from("/run/ssm-relay/ledger.db"));
        assert_eq!(
            cfg.snapshot_path,
            PathBuf::from("/var/lib/ssm-relay/ledger.snapshot.db")
        );
        assert_eq!(
            cfg.secrets_path,
            PathBuf::from("/etc/ssm-relay/secrets.toml")
        );
        assert_eq!(cfg.heartbeat_timeout_sec, 90);
        assert_eq!(cfg.handshake_retries, 3);
        assert_eq!(cfg.handshake_interval_sec, 10);
        assert_eq!(cfg.handshake_timeout_sec, 5);
        assert_eq!(cfg.repeat_alert_min, 30);
        assert_eq!(cfg.startup_grace_periods, 2);
        assert_eq!(cfg.rate_limit_per_min, 5);
        assert_eq!(cfg.snapshot_interval_sec, 300);
        assert_eq!(cfg.lag_threshold_sec, 300);
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn relay_toml_host_without_paths_is_allowed_but_no_path_found() {
        let cfg: RelayConfig = toml::from_str("[[hosts]]\nhost_id = \"h\"\n").unwrap();
        assert_eq!(cfg.hosts[0].paths.len(), 0);
        assert!(cfg.hosts[0].path(PathKind::Lan).is_none());
    }

    #[test]
    fn relay_toml_missing_host_id_reports_clear_error() {
        let err = toml::from_str::<RelayConfig>(
            "[[hosts]]\n[[hosts.paths]]\nkind = \"lan\"\nurl = \"http://x/probe\"\n",
        )
        .expect_err("hosts 缺 host_id 必须报错");
        let msg = err.to_string();
        assert!(msg.contains("host_id"), "实际: {msg}");
    }

    #[test]
    fn relay_toml_bad_path_kind_reports_clear_error() {
        let err = toml::from_str::<RelayConfig>(
            "[[hosts]]\nhost_id = \"h\"\n[[hosts.paths]]\nkind = \"wan\"\nurl = \"http://x/probe\"\n",
        )
        .expect_err("未知 path kind 必须报错");
        let msg = err.to_string();
        assert!(msg.contains("kind") || msg.contains("wan"), "实际: {msg}");
    }

    // ---------------- secrets.toml ----------------

    #[test]
    fn parse_doc_secrets_toml_field_by_field() {
        let cfg: SecretsConfig =
            toml::from_str(DOC_SECRETS_TOML).expect("SPEC §6 secrets.toml 必须可解析");
        assert_eq!(cfg.shared_secret, "...");
        assert_eq!(cfg.status_token, "...");
        assert_eq!(cfg.bark.endpoint, "https://api.day.app");
        assert_eq!(cfg.bark.key, "...");
        assert_eq!(cfg.bark.group, "server_status");
    }

    #[test]
    fn secrets_toml_bark_defaults() {
        let cfg: SecretsConfig =
            toml::from_str("shared_secret = \"s\"\nstatus_token = \"t\"\n").unwrap();
        assert_eq!(cfg.bark, BarkConfig::default());
        assert_eq!(cfg.bark.endpoint, "https://api.day.app");
        assert_eq!(cfg.bark.group, "server_status");
        assert_eq!(cfg.bark.key, "");
    }

    #[test]
    fn secrets_toml_missing_shared_secret_reports_clear_error() {
        let err = toml::from_str::<SecretsConfig>("status_token = \"t\"\n")
            .expect_err("缺 shared_secret 必须报错");
        let msg = err.to_string();
        assert!(msg.contains("shared_secret"), "实际: {msg}");
    }

    #[test]
    fn secrets_toml_missing_status_token_reports_clear_error() {
        let err = toml::from_str::<SecretsConfig>("shared_secret = \"s\"\n")
            .expect_err("缺 status_token 必须报错");
        assert!(err.to_string().contains("status_token"), "实际: {err}");
    }

    #[test]
    fn secrets_toml_roundtrip_is_lossless() {
        let cfg: SecretsConfig = toml::from_str(DOC_SECRETS_TOML).unwrap();
        let dumped = toml::to_string(&cfg).unwrap();
        assert_eq!(toml::from_str::<SecretsConfig>(&dumped).unwrap(), cfg);
    }

    // ---------------- 未知键策略：分而治之 ----------------
    //
    // 策略（SPEC 派生决策）：
    // - `agent.toml` / `relay.toml` 是**两个独立文件、不共享** → 严格
    //   （`deny_unknown_fields`）。它们大量字段带 `#[serde(default)]`，
    //   宽容模式下拼错键会被静默忽略并悄悄采用默认值，是最难排查的一类运维 bug。
    // - `secrets.toml` 是 **agent 与 relay 共用的一份合集文件**（SPEC §6 只给一份）
    //   → 必须保持宽松，否则含 `[bark]` 的文件在 agent 侧会直接拒绝启动。
    //   注意它本来就不静默：`shared_secret` / `status_token` 没有 default，
    //   拼错会报 `missing field`。

    /// 回归护栏 A：`SecretsConfig` 必须继续容忍对方的键与未知键（合集模板不破）。
    #[test]
    fn secrets_config_stays_lenient_for_shared_file() {
        // 真实合集模板：agent 只需要 shared_secret，relay 需要 status_token + [bark]
        let cfg: SecretsConfig =
            toml::from_str(COMBINED_SECRETS_TOML).expect("合集 secrets.toml（含未知键）必须可解析");
        assert_eq!(cfg.shared_secret, "CHANGE_ME_shared_secret_64_hex_chars");
        assert_eq!(cfg.status_token, "CHANGE_ME_status_token_64_hex_chars");
        assert_eq!(cfg.bark.key, "CHANGE_ME_bark_key");

        // 未知顶层键 + 未知 [bark] 子键都必须被忽略而不是报错
        let cfg: SecretsConfig = toml::from_str(
            r#"
shared_secret = "s"
status_token = "t"
unknown_future_key = "forward compat"
[bark]
endpoint = "https://api.day.app"
another_unknown_key = 1
"#,
        )
        .expect("SecretsConfig 的未知键必须被忽略（它 agent/relay 共用）");
        assert_eq!(cfg.shared_secret, "s");
        assert_eq!(cfg.bark.endpoint, "https://api.day.app");
    }

    /// 回归护栏 B：`AgentConfig` 必须拒绝未知键，且错误里看得到那个键名。
    ///
    /// 核心场景：`report_interval_sec` 拼错 → 不报错就会静默跑 30s 默认值。
    #[test]
    fn agent_config_rejects_typo_in_defaulted_key() {
        let err = toml::from_str::<AgentConfig>(
            r#"
host_id = "kebei-prod-01"
relay_url = "http://127.0.0.1:9100/report"
report_intervall_sec = 2
"#,
        )
        .expect_err("拼错的键必须报错（否则会静默退化成 30s）");
        let msg = err.to_string();
        assert!(
            msg.contains("report_intervall_sec"),
            "错误必须点出未知键名，实际: {msg}"
        );
        assert!(msg.contains("unknown field"), "实际: {msg}");
        assert!(
            msg.contains("report_interval_sec"),
            "错误应同时列出合法键，实际: {msg}"
        );
    }

    #[test]
    fn agent_config_rejects_unknown_top_and_nested_keys() {
        // 未知顶层键
        let err = toml::from_str::<AgentConfig>(
            "host_id = \"h\"\nrelay_url = \"u\"\nsome_future_key = 42\n",
        )
        .expect_err("未知顶层键必须报错");
        assert!(err.to_string().contains("some_future_key"), "实际: {err}");

        // 未知嵌套键：[thresholds] 里
        let err = toml::from_str::<AgentConfig>(
            "host_id = \"h\"\nrelay_url = \"u\"\n[thresholds]\ndisk_percent = 80.0\nnot_a_real_key = \"x\"\n",
        )
        .expect_err("未知嵌套键必须报错");
        assert!(err.to_string().contains("not_a_real_key"), "实际: {err}");

        // 未知嵌套键：[[probes]] / [[log_rules]] / [[git_repos]] 逐个覆盖
        let cases = [
            ("host_id = \"h\"\nrelay_url = \"u\"\n[[probes]]\nname = \"p\"\nkind = \"http\"\ntarget = \"t\"\nbogus = 1\n", "bogus"),
            (
                "host_id = \"h\"\nrelay_url = \"u\"\n[[log_rules]]\nname = \"r\"\nsource = \"file:/a.log\"\nbogus = 1\n",
                "bogus",
            ),
            ("host_id = \"h\"\nrelay_url = \"u\"\n[[git_repos]]\nname = \"g\"\npath = \"/srv/g\"\nbogus = 1\n", "bogus"),
        ];
        for (text, key) in cases {
            let err = match toml::from_str::<AgentConfig>(text) {
                Ok(_) => panic!("[[...]] 里的未知键 {key} 必须报错"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains(key),
                "错误必须点出 {key}，实际: {err}"
            );
        }
    }

    /// 回归护栏 C：`RelayConfig`（含 `[[hosts]]` / `[[hosts.paths]]`）必须拒绝未知键。
    #[test]
    fn relay_config_rejects_unknown_keys() {
        let err = toml::from_str::<RelayConfig>("heartbeat_timeout_secs = 120\n")
            .expect_err("未知顶层键必须报错");
        assert!(
            err.to_string().contains("heartbeat_timeout_secs"),
            "实际: {err}"
        );
        assert!(
            err.to_string().contains("heartbeat_timeout_sec"),
            "应列出合法键，实际: {err}"
        );

        let err = toml::from_str::<RelayConfig>("[[hosts]]\nhost_id = \"h\"\nbogus_host_key = 1\n")
            .expect_err("[[hosts]] 未知键必须报错");
        assert!(err.to_string().contains("bogus_host_key"), "实际: {err}");

        let err = toml::from_str::<RelayConfig>(
            "[[hosts]]\nhost_id = \"h\"\n[[hosts.paths]]\nkind = \"lan\"\nurl = \"http://x/probe\"\nbogus_path_key = 1\n",
        )
        .expect_err("[[hosts.paths]] 未知键必须报错");
        assert!(err.to_string().contains("bogus_path_key"), "实际: {err}");
    }

    /// 严格模式不能误伤：两个配置**各自的合法键**必须照常接受，
    /// 且未被禁止的 `ProbeConfig` 别名（`type =`）仍然可用。
    #[test]
    fn strict_mode_still_accepts_all_legitimate_keys() {
        let agent: AgentConfig = toml::from_str(
            r#"
host_id = "h"
relay_url = "u"
report_interval_sec = 2
probe_listen = "0.0.0.0:9101"
seq_state_path = "/tmp/seq"
buffer_path = "/tmp/buf.jsonl"
buffer_max = 7
secrets_path = "/tmp/s.toml"
disks = ["/", "/data"]
[[probes]]
name = "p"
kind = "tcp"
target = "1.1.1.1:53"
fail_threshold = 3
timeout_sec = 2
[[log_rules]]
name = "r"
source = "journald:nginx.service"
pattern = "ERR"
level = "critical"
throttle_sec = 60
max_lines = 9
include_raw = true
[[git_repos]]
name = "g"
path = "/srv/g"
timeout_sec = 9
[thresholds]
disk_percent = 1.0
disk_throttle_sec = 2
cpu_percent = 3.0
mem_percent = 4.0
sustained_sec = 5
"#,
        )
        .expect("全部合法键必须被接受");
        assert_eq!(agent.report_interval_sec, 2);
        assert_eq!(agent.buffer_max, 7);
        assert_eq!(agent.probes[0].kind, ProbeKind::Tcp);
        assert_eq!(agent.log_rules[0].level, LogLevel::Critical);
        assert_eq!(agent.git_repos[0].timeout_sec, 9);

        // `type = ` 是 `kind = ` 的别名，严格模式下也必须继续可用
        let aliased: AgentConfig = toml::from_str(
            "host_id = \"h\"\nrelay_url = \"u\"\n[[probes]]\nname = \"p\"\ntype = \"http\"\ntarget = \"t\"\n",
        )
        .expect("别名 type= 必须仍然可用");
        assert_eq!(aliased.probes[0].kind, ProbeKind::Http);

        let relay: RelayConfig = toml::from_str(
            r#"
listen = "0.0.0.0:9100"
db_path = "/tmp/l.db"
secrets_path = "/tmp/s.toml"
heartbeat_timeout_sec = 90
handshake_retries = 3
handshake_interval_sec = 10
handshake_timeout_sec = 5
repeat_alert_min = 30
startup_grace_periods = 2
rate_limit_per_min = 5
snapshot_interval_sec = 300
lag_threshold_sec = 300
[[hosts]]
host_id = "h"
[[hosts.paths]]
kind = "tailscale"
url = "http://100.0.0.1:9101/probe"
"#,
        )
        .expect("relay 全部合法键必须被接受");
        assert_eq!(relay.hosts[0].paths[0].kind, PathKind::Tailscale);
    }

    /// 回归护栏 D：严格模式下错误**必须能透出到调用方**（经 `ConfigError` 包装后
    /// 仍然看得到未知键名与合法键列表，不能被 `#[error(...)]` 吞掉细节）。
    #[test]
    fn config_error_preserves_unknown_field_detail_from_file() {
        let dir = std::env::temp_dir().join(format!("ssm-cfg-strict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.toml");
        std::fs::write(
            &path,
            "host_id = \"h\"\nrelay_url = \"u\"\nreport_intervall_sec = 2\n",
        )
        .unwrap();

        let err = AgentConfig::load(&path).expect_err("拼错键的文件必须加载失败");
        let msg = err.to_string();
        assert!(
            msg.contains("report_intervall_sec"),
            "ConfigError 必须保留未知键名，实际: {msg}"
        );
        assert!(
            msg.contains("agent.toml"),
            "ConfigError 必须保留文件路径，实际: {msg}"
        );

        // 并且能按类型取出原始 toml 错误（细节没被吞）
        match err {
            ConfigError::Parse { source, .. } => {
                let src = source.to_string();
                assert!(src.contains("report_intervall_sec"), "实际: {src}");
                assert!(src.contains("unknown field"), "实际: {src}");
            }
            other => panic!("期望 ConfigError::Parse，得到 {other}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- 合集 secrets.toml 夹具 ----------------

    /// 一份**合集** secrets.toml：同时含 agent 需要的 `shared_secret` 与
    /// relay 需要的 `status_token` + `[bark]`，另加一个未知键。
    const COMBINED_SECRETS_TOML: &str = r#"
shared_secret = "CHANGE_ME_shared_secret_64_hex_chars"
status_token  = "CHANGE_ME_status_token_64_hex_chars"
unknown_future_key = "forward compat"

[bark]
endpoint = "https://api.day.app"
key      = "CHANGE_ME_bark_key"
group    = "server_status"
"#;

    /// agent 侧解析路径：合集 secrets.toml 必须能读，且 `shared_secret` 精确。
    #[test]
    fn agent_path_reads_combined_secrets_template() {
        let cfg: SecretsConfig =
            toml::from_str(COMBINED_SECRETS_TOML).expect("合集 secrets.toml 在 agent 侧必须可解析");
        assert_eq!(cfg.shared_secret, "CHANGE_ME_shared_secret_64_hex_chars");
        // status_token / bark 对 agent 无用但必须被接受（serde 默认忽略未知字段）
        assert_eq!(cfg.status_token, "CHANGE_ME_status_token_64_hex_chars");
        assert_eq!(cfg.bark.key, "CHANGE_ME_bark_key");
    }

    /// relay 侧解析路径：同一份合集 secrets.toml 也必须能读。
    #[test]
    fn relay_path_reads_combined_secrets_template() {
        let cfg: SecretsConfig =
            toml::from_str(COMBINED_SECRETS_TOML).expect("合集 secrets.toml 在 relay 侧必须可解析");
        assert_eq!(cfg.status_token, "CHANGE_ME_status_token_64_hex_chars");
        assert_eq!(cfg.bark.endpoint, "https://api.day.app");
        assert_eq!(cfg.bark.group, "server_status");
        assert_eq!(cfg.shared_secret, "CHANGE_ME_shared_secret_64_hex_chars");
    }

    /// 衍生护栏：严格模式下 **`agent.toml` 与 `relay.toml` 不能互相混用**。
    ///
    /// 这是严格策略的必然推论，写在这里当文档用：一份「超集」文档不可能同时
    /// 喂给两个结构体。唯一允许共用的是 `secrets.toml`（见护栏 A）。
    #[test]
    fn agent_and_relay_configs_cannot_be_mixed() {
        let superset = r#"
host_id = "kebei-prod-01"
relay_url = "http://127.0.0.1:9100/report"
listen = "0.0.0.0:9100"
db_path = "/var/lib/ssm-relay/ledger.db"
shared_secret = "s"
status_token = "t"
heartbeat_timeout_sec = 90
[bark]
endpoint = "https://api.day.app"
key = "k"
group = "server_status"
[[hosts]]
host_id = "kebei-prod-01"
"#;
        assert!(
            toml::from_str::<AgentConfig>(superset).is_err(),
            "严格模式下 agent.toml 混入 relay 键必须报错（防静默退化）"
        );
        assert!(
            toml::from_str::<RelayConfig>(superset).is_err(),
            "严格模式下 relay.toml 混入 agent 键必须报错（防静默退化）"
        );
        // 但 secrets 必须能吃下这份合集（普通键不同名，互不冲突）
        let secrets: SecretsConfig = toml::from_str(superset).expect("SecretsConfig 必须保持宽松");
        assert_eq!(secrets.status_token, "t");
    }

    // ---------------- 官方 example 模板（真实文件，防止模板与代码漂移）----------------

    /// 读取仓库内文件；不存在时返回 `None`（该文件由 T4 维护，缺失时跳过而不是失败）。
    fn repo_file(rel: &str) -> Option<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel);
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(_) => {
                eprintln!("跳过：{} 不存在（该文件由 T4 维护）", path.display());
                None
            }
        }
    }

    /// T4 的 `deploy/agent.toml.example` 必须能被 `AgentConfig` 直接解析。
    /// 若 T4 写错键名，这个测试会失败 —— 这正是它存在的意义。
    #[test]
    fn shipped_agent_example_parses() {
        let Some(text) = repo_file("deploy/agent.toml.example") else {
            return;
        };
        let cfg: AgentConfig =
            toml::from_str(&text).expect("deploy/agent.toml.example 必须能被 AgentConfig 解析");
        assert_eq!(cfg.host_id, "kebei-prod-01");
        assert!(
            cfg.relay_url.ends_with("/report"),
            "relay_url 必须指向 /report"
        );
        assert!(!cfg.probes.is_empty());
        assert!(!cfg.log_rules.is_empty());
    }

    /// T4 的 `deploy/secrets.toml.example` 是**合集模板**，必须走真实加载路径
    /// （`SecretsConfig::load`，含权限检查）解析成功。
    #[test]
    fn shipped_secrets_example_parses_via_load() {
        let Some(_) = repo_file("deploy/secrets.toml.example") else {
            return;
        };
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/secrets.toml.example");
        let cfg = SecretsConfig::load(&path)
            .expect("deploy/secrets.toml.example 必须能被 SecretsConfig::load 解析");
        assert_eq!(cfg.shared_secret, "CHANGE_ME_shared_secret_64_hex_chars");
        assert_eq!(cfg.status_token, "CHANGE_ME_status_token_64_hex_chars");
        assert_eq!(cfg.bark.endpoint, "https://api.day.app");
        assert_eq!(cfg.bark.group, "server_status");
    }

    /// T4 的 `deploy/relay.toml.example` 必须能被 `RelayConfig` 解析。
    ///
    /// **这是严格模式（`deny_unknown_fields`）下的关键回归**：example 里只要有一个
    /// 多余/拼错的键，生产部署就会直接起不来 —— 这个测试就是用来提前抓它的。
    #[test]
    fn shipped_relay_example_parses() {
        let Some(text) = repo_file("deploy/relay.toml.example") else {
            return;
        };
        let cfg: RelayConfig = toml::from_str(&text)
            .expect("deploy/relay.toml.example 必须能在严格模式下被 RelayConfig 解析");
        assert!(!cfg.listen.is_empty());
        assert!(
            !cfg.hosts.is_empty(),
            "relay.toml.example 应至少声明一台 [[hosts]]"
        );
    }

    // ---------------- 文件加载 ----------------

    #[test]
    fn load_from_file_and_missing_file_error() {
        let dir = std::env::temp_dir().join(format!("ssm-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let agent_path = dir.join("agent.toml");
        std::fs::write(&agent_path, DOC_AGENT_TOML).unwrap();
        let cfg = AgentConfig::load(&agent_path).expect("文件加载必须成功");
        assert_eq!(cfg.host_id, "kebei-prod-01");

        let secrets_path = dir.join("secrets.toml");
        std::fs::write(&secrets_path, DOC_SECRETS_TOML).unwrap();
        let secrets = SecretsConfig::load(&secrets_path).expect("secrets 加载必须成功");
        assert_eq!(secrets.bark.group, "server_status");

        let err = AgentConfig::load(dir.join("nope.toml")).expect_err("不存在的文件必须报错");
        match err {
            ConfigError::Read { ref path, .. } => {
                assert!(path.ends_with("nope.toml"), "实际: {path}")
            }
            other => panic!("期望 ConfigError::Read，得到 {other}"),
        }
        assert!(err.to_string().contains("nope.toml"));

        let bad_path = dir.join("bad.toml");
        std::fs::write(&bad_path, "host_id = \"h\"\n").unwrap();
        let err = AgentConfig::load(&bad_path).expect_err("缺字段的文件必须报错");
        match err {
            ConfigError::Parse {
                ref path,
                ref source,
            } => {
                assert!(path.ends_with("bad.toml"));
                assert!(source.to_string().contains("relay_url"), "实际: {source}");
            }
            other => panic!("期望 ConfigError::Parse，得到 {other}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
