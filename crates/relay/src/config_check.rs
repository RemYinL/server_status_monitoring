//! 启动期配置校验（**fail fast**）。
//!
//! 背景：`ssm-common` 的配置加载**故意不用 `deny_unknown_fields`**（serde 默认忽略未知键，
//! 这样一份合集 `secrets.toml` 能同时喂给 agent 与 relay）。代价是**键名打错会静默取默认值**：
//! 写 `shared_secrett = "..."` 不报错，`shared_secret` 直接是空串，症状变成
//! 「所有 `/report` 恒 401」——极难排查。
//!
//! 因此 relay 在**加载配置之后、开始服务之前**跑一次本校验：
//! - 命中「致命」项 → 打 `tracing::error!` 并让进程以非 0 退出码退出（不带病启动）；
//! - 命中「可疑」项 → 只打 `tracing::warn!`，继续启动。
//!
//! 校验逻辑与被校验数据都在这里，便于单测直接喂 `RelayConfig` / `SecretsConfig`。

use std::path::Path;

use ssm_common::prelude::*;

/// 问题严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 致命：必须退出。
    Error,
    /// 可疑：只警告。
    Warning,
}

/// 一条校验结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    /// 出问题的字段/位置，便于日志里一眼定位。
    pub field: String,
    /// 可操作的中文提示。
    pub message: String,
}

impl Issue {
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// 构造一条致命结论。
    pub fn error(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            field: field.into(),
            message: message.into(),
        }
    }

    /// 构造一条可疑结论。
    pub fn warning(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            field: field.into(),
            message: message.into(),
        }
    }
}

/// 心跳超时的合理下限：低于它几乎必然与 agent 的 `report_interval_sec` 不匹配。
const MIN_SENSIBLE_HEARTBEAT_TIMEOUT_SEC: u64 = 2;

/// 校验配置，返回全部结论（可能同时含 error 与 warning）。
///
/// `secrets_path` 只用于让提示里带上**实际读取的路径**。
pub fn validate(cfg: &RelayConfig, secrets: &SecretsConfig) -> Vec<Issue> {
    let mut issues = Vec::new();

    // ---- secrets ----
    if secrets.shared_secret.trim().is_empty() {
        issues.push(Issue::error(
            "secrets.shared_secret",
            format!(
                "shared_secret 为空 → 所有 POST /report 都会恒返回 401。\
                 请检查 {} 里的键名是否拼错（serde 忽略未知键，写错的键会静默变成默认空串），\
                 例如必须是 shared_secret = \"...\"",
                path_hint(secrets_path_of(cfg))
            ),
        ));
    }

    if secrets.status_token.trim().is_empty() {
        issues.push(Issue::error(
            "secrets.status_token",
            format!(
                "status_token 为空 → /status 的 Bearer 鉴权形同虚设（空 Token 即可通过）或全部被拒。\
                 请在 {} 里配置 status_token = \"...\"",
                path_hint(secrets_path_of(cfg))
            ),
        ));
    }

    if secrets.bark.key.trim().is_empty() {
        issues.push(Issue::warning(
            "secrets.bark.key",
            format!(
                "bark.key 为空 → 不会推送任何 Bark 告警（其它功能正常，/status 仍可用）。\
                 想收告警请在 {} 的 [bark] 段配置 key = \"...\"",
                path_hint(secrets_path_of(cfg))
            ),
        ));
    }

    if secrets.bark.endpoint.trim().is_empty() {
        issues.push(Issue::warning(
            "secrets.bark.endpoint",
            "bark.endpoint 为空 → 推送 URL 不完整，Bark 一定失败。默认应为 https://api.day.app",
        ));
    }

    // ---- hosts / 探测路径 ----
    if cfg.hosts.is_empty() {
        issues.push(Issue::error(
            "hosts",
            "relay.toml 里没有任何 [[hosts]] → 没有可监控的主机，/status 会返回 404。\
             请至少配置一台主机及其 [[hosts.paths]] 探测地址（http://<agent-ip>:9101/probe）",
        ));
    }

    for host in &cfg.hosts {
        if host.host_id.trim().is_empty() {
            issues.push(Issue::error(
                "hosts.host_id",
                "存在 host_id 为空的主机条目 → 无法与 agent 的 host_id 匹配",
            ));
            continue;
        }
        if host.paths.is_empty() {
            issues.push(Issue::error(
                format!("hosts[{}].paths", host.host_id),
                format!(
                    "主机 {} 没有任何 [[hosts.paths]] → 握手探测无从发起，\
                     该主机永远判不出离线（只会一直保持最后状态）。\
                     请补一条 kind = \"lan\" 或 \"tailscale\" 的探测地址",
                    host.host_id
                ),
            ));
        }
        for path in &host.paths {
            if path.url.trim().is_empty() {
                issues.push(Issue::error(
                    format!("hosts[{}].paths.url", host.host_id),
                    format!("主机 {} 的探测 URL 为空", host.host_id),
                ));
            } else if !path.url.starts_with("http://") && !path.url.starts_with("https://") {
                issues.push(Issue::warning(
                    format!("hosts[{}].paths.url", host.host_id),
                    format!(
                        "主机 {} 的探测 URL {:?} 不是 http(s):// 开头，探测必然失败",
                        host.host_id, path.url
                    ),
                ));
            }
        }
        if host.paths.len() == 1 {
            issues.push(Issue::warning(
                format!("hosts[{}].paths", host.host_id),
                format!(
                    "主机 {} 只配了 1 条探测路径 → 判定矩阵退化为「在线/离线」，\
                     无法区分「隧道断开」与「整机离线」（docs/03 §2）",
                    host.host_id
                ),
            ));
        }
    }

    // ---- 判定参数 ----
    if cfg.heartbeat_timeout_sec < MIN_SENSIBLE_HEARTBEAT_TIMEOUT_SEC {
        issues.push(Issue::warning(
            "heartbeat_timeout_sec",
            format!(
                "heartbeat_timeout_sec = {} 过小（建议 ≥ 3 × agent 的 report_interval_sec，默认 90）\
                 → 心跳阈值低于一个上报周期，会持续误报离线",
                cfg.heartbeat_timeout_sec
            ),
        ));
    }

    if cfg.handshake_retries == 0 {
        issues.push(Issue::warning(
            "handshake_retries",
            "handshake_retries = 0 → 心跳一超时就直接判离线，没有握手复核（docs/03 §3 要求 3 次）",
        ));
    }

    if cfg.repeat_alert_min == 0 {
        issues.push(Issue::warning(
            "repeat_alert_min",
            "repeat_alert_min = 0 → 持续离线期间不重复提醒（这是合法配置，仅在提醒确认符合预期时保留）",
        ));
    }

    if cfg.rate_limit_per_min == 0 {
        issues.push(Issue::warning(
            "rate_limit_per_min",
            "rate_limit_per_min = 0 → 所有 Bark 推送都会被限流合并，等于收不到实时告警",
        ));
    }

    // 注意：`snapshot_path` 不在冻结的 `RelayConfig` 里，相关校验在
    // `settings::RelaySettings::validate`（见 settings.rs 顶部说明）。
    issues
}

fn secrets_path_of(cfg: &RelayConfig) -> &Path {
    &cfg.secrets_path
}

fn path_hint(path: &Path) -> String {
    path.display().to_string()
}

/// 是否存在致命问题。
pub fn has_errors(issues: &[Issue]) -> bool {
    issues.iter().any(Issue::is_error)
}

/// 把结论写进日志（error 用 `tracing::error!`，warning 用 `tracing::warn!`）。
pub fn log_issues(issues: &[Issue], secrets_path: &Path) {
    for issue in issues {
        match issue.severity {
            Severity::Error => tracing::error!(
                field = %issue.field,
                secrets_path = %secrets_path.display(),
                "配置错误：{}",
                issue.message
            ),
            Severity::Warning => tracing::warn!(
                field = %issue.field,
                secrets_path = %secrets_path.display(),
                "配置可疑：{}",
                issue.message
            ),
        }
    }
    if has_errors(issues) {
        tracing::error!(
            errors = issues.iter().filter(|i| i.is_error()).count(),
            warnings = issues.iter().filter(|i| !i.is_error()).count(),
            "配置校验未通过，进程将以非 0 退出码退出（不带病启动）"
        );
    } else if !issues.is_empty() {
        tracing::warn!(
            warnings = issues.len(),
            "配置校验通过，但存在可疑项（见上）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(hosts: &str) -> RelayConfig {
        toml::from_str(&format!(
            r#"
db_path       = "/run/ssm-relay/ledger.db"
secrets_path  = "/etc/ssm-relay/secrets.toml"
heartbeat_timeout_sec = 90
handshake_retries = 3
repeat_alert_min = 30
rate_limit_per_min = 5
{hosts}
"#
        ))
        .unwrap()
    }

    fn good_hosts() -> &'static str {
        r#"
[[hosts]]
host_id = "kebei-prod-01"
[[hosts.paths]]
kind = "lan"
url = "http://192.168.1.10:9101/probe"
[[hosts.paths]]
kind = "tailscale"
url = "http://100.64.0.10:9101/probe"
"#
    }

    fn secrets(shared: &str, token: &str, bark_key: &str) -> SecretsConfig {
        SecretsConfig {
            shared_secret: shared.to_string(),
            status_token: token.to_string(),
            bark: BarkConfig {
                endpoint: "https://api.day.app".to_string(),
                key: bark_key.to_string(),
                group: "server_status".to_string(),
            },
        }
    }

    fn fields(issues: &[Issue]) -> Vec<&str> {
        issues.iter().map(|i| i.field.as_str()).collect()
    }

    #[test]
    fn healthy_config_has_no_issues() {
        let issues = validate(&cfg(good_hosts()), &secrets("s3cret", "tok", "barkkey"));
        assert!(issues.is_empty(), "健康配置不应有任何结论: {issues:?}");
        assert!(!has_errors(&issues));
    }

    #[test]
    fn empty_shared_secret_is_an_error() {
        let issues = validate(&cfg(good_hosts()), &secrets("", "tok", "barkkey"));
        assert!(has_errors(&issues));
        let e = issues
            .iter()
            .find(|i| i.field == "secrets.shared_secret")
            .expect("必须报 shared_secret");
        assert!(e.is_error());
        assert!(
            e.message.contains("401"),
            "提示必须点出症状是 401: {}",
            e.message
        );
        assert!(
            e.message.contains("secrets.toml"),
            "提示必须带上实际读取的路径: {}",
            e.message
        );
        // 只有空白字符也算空
        let issues = validate(&cfg(good_hosts()), &secrets("   ", "tok", "barkkey"));
        assert!(has_errors(&issues));
    }

    #[test]
    fn empty_status_token_is_an_error() {
        let issues = validate(&cfg(good_hosts()), &secrets("s3cret", "  ", "barkkey"));
        assert!(has_errors(&issues));
        assert!(fields(&issues).contains(&"secrets.status_token"));
    }

    #[test]
    fn empty_bark_key_only_warns_and_does_not_fail() {
        let issues = validate(&cfg(good_hosts()), &secrets("s3cret", "tok", ""));
        assert!(
            !has_errors(&issues),
            "bark.key 为空不得阻断启动: {issues:?}"
        );
        let w = issues
            .iter()
            .find(|i| i.field == "secrets.bark.key")
            .expect("必须给出 bark.key 警告");
        assert_eq!(w.severity, Severity::Warning);
        assert!(w.message.contains("不会推送"), "{}", w.message);
        assert_eq!(issues.len(), 1, "只应有一条警告: {issues:?}");
    }

    #[test]
    fn no_hosts_is_an_error() {
        let issues = validate(&cfg(""), &secrets("s3cret", "tok", "barkkey"));
        assert!(has_errors(&issues));
        let e = issues.iter().find(|i| i.field == "hosts").unwrap();
        assert!(e.is_error());
        assert!(e.message.contains("[[hosts]]"), "{}", e.message);
    }

    #[test]
    fn host_without_paths_is_an_error() {
        let c = cfg(r#"
[[hosts]]
host_id = "h1"
"#);
        let issues = validate(&c, &secrets("s3cret", "tok", "barkkey"));
        assert!(has_errors(&issues));
        let e = issues
            .iter()
            .find(|i| i.field == "hosts[h1].paths")
            .expect("必须报该 host 缺探测路径");
        assert!(e.message.contains("判不出离线"), "{}", e.message);
    }

    #[test]
    fn single_path_host_warns_about_degraded_matrix() {
        let c = cfg(r#"
[[hosts]]
host_id = "h1"
[[hosts.paths]]
kind = "tailscale"
url = "http://100.64.0.10:9101/probe"
"#);
        let issues = validate(&c, &secrets("s3cret", "tok", "barkkey"));
        assert!(!has_errors(&issues), "单路是可接受的部署形态: {issues:?}");
        assert!(issues
            .iter()
            .any(|i| i.field == "hosts[h1].paths" && i.message.contains("退化")));
    }

    #[test]
    fn tiny_heartbeat_timeout_warns() {
        let mut c = cfg(good_hosts());
        c.heartbeat_timeout_sec = 1;
        let issues = validate(&c, &secrets("s3cret", "tok", "barkkey"));
        assert!(!has_errors(&issues), "阈值过小只是可疑，不是致命");
        let w = issues
            .iter()
            .find(|i| i.field == "heartbeat_timeout_sec")
            .expect("必须警告");
        assert!(w.message.contains("report_interval_sec"), "{}", w.message);
    }

    #[test]
    fn zero_handshake_retries_warns() {
        let mut c = cfg(good_hosts());
        c.handshake_retries = 0;
        let issues = validate(&c, &secrets("s3cret", "tok", "barkkey"));
        assert!(issues.iter().any(|i| i.field == "handshake_retries"));
    }

    #[test]
    fn db_path_equal_to_snapshot_path_is_an_error() {
        // 该组合由 settings::RelaySettings::validate 负责（snapshot_path 不在 common 的
        // RelayConfig 里），这里只验证「构造出来的 Issue 能被判定为致命」。
        let issue = Issue::error("snapshot_path", "db_path 与 snapshot_path 指向同一个文件");
        assert!(issue.is_error());
        assert!(has_errors(&[issue]));
        let ok = Issue::warning("snapshot_path", "未显式配置 snapshot_path");
        assert!(!has_errors(&[ok]));
    }

    #[test]
    fn bad_probe_url_warns() {
        let c = cfg(r#"
[[hosts]]
host_id = "h1"
[[hosts.paths]]
kind = "lan"
url = "192.168.1.10:9101/probe"
[[hosts.paths]]
kind = "tailscale"
url = ""
"#);
        let issues = validate(&c, &secrets("s3cret", "tok", "barkkey"));
        // 空 URL 是致命，缺 scheme 是可疑
        assert!(has_errors(&issues));
        assert!(issues.iter().any(|i| i.message.contains("不是 http(s)://")));
        assert!(issues.iter().any(|i| i.message.contains("探测 URL 为空")));
    }

    #[test]
    fn issues_are_all_actionable_and_chinese() {
        // 每条提示都必须点出「哪个字段 + 怎么修」
        let mut c = cfg("");
        c.heartbeat_timeout_sec = 1;
        c.rate_limit_per_min = 0;
        c.handshake_retries = 0;
        let issues = validate(&c, &secrets("", "", ""));
        assert!(issues.len() >= 5, "{issues:?}");
        for i in &issues {
            assert!(!i.field.is_empty());
            assert!(i.message.chars().count() > 10, "提示太短: {}", i.message);
            assert!(
                i.message
                    .chars()
                    .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
                "提示必须是中文: {}",
                i.message
            );
        }
    }
}
