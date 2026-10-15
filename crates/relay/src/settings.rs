//! `relay.toml` 的本地加载与启动期校验。
//!
//! `crates/common` 的 [`RelayConfig`] 已正式包含 `snapshot_path`（默认
//! `/var/lib/ssm-relay/ledger.snapshot.db`），因此这里只做两件事：
//!
//! 1. 直接 `RelayConfig::load`（严格模式 `deny_unknown_fields` 由 common 把关）；
//! 2. 叠加 relay 侧独有的校验（`snapshot_path` 的持久性、secrets 未知键等，
//!    见 [`RelaySettings::validate`] 与 [`unknown_secret_keys`]）。

use std::path::Path;

use ssm_common::prelude::*;

use crate::config_check::{self, Issue};

/// relay 的生效配置。
#[derive(Debug, Clone, PartialEq)]
pub struct RelaySettings {
    pub config: RelayConfig,
}

impl RelaySettings {
    /// 从 `relay.toml` 加载。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let path = path.as_ref();
        // common 的 `RelayConfig::load` 已经带上了文件路径，直接透传它的错误。
        let config = RelayConfig::load(path)?;
        Ok(Self { config })
    }

    /// 生效的快照路径。
    #[cfg(test)]
    pub fn snapshot_path(&self) -> &Path {
        &self.config.snapshot_path
    }

    /// 启动期校验：common 的字段 + relay 侧补充项。
    pub fn validate(&self, secrets: &SecretsConfig) -> Vec<Issue> {
        let mut issues = config_check::validate(&self.config, secrets);
        issues.extend(self.storage_issues());
        issues
    }

    /// 存储分层校验（tmpfs 判定）。
    fn storage_issues(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        let db = &self.config.db_path;
        let snapshot = &self.config.snapshot_path;

        if db == snapshot {
            issues.push(Issue::error(
                "snapshot_path",
                format!(
                    "snapshot_path 与 db_path 指向同一个文件（{}）→ 周期快照会覆盖热台账",
                    snapshot.display()
                ),
            ));
            return issues;
        }

        // 关键：持久快照是「relay 重启不误报离线」的唯一依据（docs/03 §5 / docs/07 §3）。
        // 快照落在 tmpfs 上 = 静默放弃持久化能力，属于配置错误，直接 fail fast。
        if let Some(tmpfs) = tmpfs_prefix(snapshot) {
            issues.push(Issue::error(
                "snapshot_path",
                format!(
                    "snapshot_path = {} 落在 tmpfs（{tmpfs}）上，掉电/重启即失，\
                     等于完全没有持久化 → relay 重启会把「重启」误判成「离线」。\
                     请指向持久分区，例如 snapshot_path = \"/var/lib/ssm-relay/ledger.snapshot.db\"",
                    snapshot.display()
                ),
            ));
        }

        if tmpfs_prefix(db).is_none() {
            issues.push(Issue::warning(
                "db_path",
                format!(
                    "db_path = {} 不在 tmpfs 上。热台账建议放 /run/ssm-relay/（tmpfs），\
                     以免频繁小写磨损 SSD；持久性由 snapshot_path 保证（docs/07 §3）",
                    db.display()
                ),
            ));
        }

        issues
    }
}

/// 判断路径是否落在 tmpfs 上，返回命中的前缀。
///
/// **判定依据**：按路径前缀匹配。Ubuntu 上 tmpfs 的标准挂载点就是
/// `/run`、`/tmp`、`/dev/shm` 三处（`man hier`；systemd 的 `RuntimeDirectory=` 也落在 `/run`）。
///
/// **为什么不用 `/proc/mounts` 精确判定**：那需要读挂载表，而在容器/LXC 里
/// `/proc/mounts` 的可见性与宿主不一致（PVE + Debian LXC 是 docs/07 的默认部署形态），
/// 反而更容易误判。前缀匹配的**误判风险只有一种方向**——有人把 `/tmp` 挂成持久盘
/// （极少见，且此时给出的「不该把快照放 /tmp」仍是安全的保守结论）；
/// 反过来**漏判**（把 tmpfs 当持久盘）才是有害的，而前缀匹配不会漏掉这三个标准位置。
fn tmpfs_prefix(path: &Path) -> Option<&'static str> {
    const TMPFS_PREFIXES: [&str; 3] = ["/run", "/tmp", "/dev/shm"];
    let s = path.to_string_lossy();
    // 必须按「路径段」匹配：`/runtime/x` 不属于 `/run`
    TMPFS_PREFIXES
        .into_iter()
        .find(|prefix| s == *prefix || s.starts_with(&format!("{prefix}/")))
}

/// secrets 文件里的未知键（返回 `(小节, 键名)`）。
///
/// `SecretsConfig` **故意**宽松（agent 与 relay 共用一份文件，serde 忽略未知键），
/// 代价是键名打错会**静默退化**——最典型的：`[bark] endpont = "https://bark.mydomain.com"`
/// 会静默退回官方 `https://api.day.app`，自建 Bark 的计划失效、告警被推到公网第三方。
/// 所以这里把原始 TOML 再读一遍，对未知键各打一条 warn（**只警告，不退出**）。
pub fn unknown_secret_keys(text: &str) -> Vec<(String, String)> {
    const TOP_LEVEL: [&str; 3] = ["shared_secret", "status_token", "bark"];
    const BARK: [&str; 3] = ["endpoint", "key", "group"];

    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut unknown = Vec::new();
    for (key, value) in &table {
        if !TOP_LEVEL.contains(&key.as_str()) {
            unknown.push(("(顶层)".to_string(), key.clone()));
            continue;
        }
        if key == "bark" {
            if let Some(bark) = value.as_table() {
                for sub in bark.keys() {
                    if !BARK.contains(&sub.as_str()) {
                        unknown.push(("bark".to_string(), sub.clone()));
                    }
                }
            }
        }
    }
    unknown
}

/// 把未知键写进日志（含合法键清单，便于直接对照修正）。
pub fn log_unknown_secret_keys(path: &Path, unknown: &[(String, String)]) {
    if unknown.is_empty() {
        return;
    }
    let list = unknown
        .iter()
        .map(|(section, key)| format!("{section}.{key}"))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        secrets_path = %path.display(),
        unknown_keys = %list,
        "secrets 文件里存在未知键：键名拼错会静默取默认值（例如 bark.endpont 会退回官方 api.day.app），请核对"
    );
    tracing::warn!(
        secrets_path = %path.display(),
        "secrets 合法键：顶层 shared_secret / status_token / [bark]，以及 bark.endpoint / bark.key / bark.group"
    );
}

/// 配置加载错误。
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("读取 secrets 文件 {path} 失败: {source}")]
    Secrets {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl SettingsError {
    /// 出错的路径（便于测试与日志定位）。
    #[cfg(test)]
    pub fn path(&self) -> &str {
        match self {
            SettingsError::Config(err) => match err {
                ConfigError::Read { path, .. }
                | ConfigError::Parse { path, .. }
                | ConfigError::Json { path, .. } => path,
            },
            SettingsError::Secrets { path, .. } => path,
        }
    }
}

/// 读取 secrets 原文（用于未知键检查）。
pub fn read_secrets_text(path: &Path) -> Result<String, SettingsError> {
    std::fs::read_to_string(path).map_err(|source| SettingsError::Secrets {
        path: path.display().to_string(),
        source,
    })
}

/// 便捷：`RelayConfig` 里 `[[hosts]]` 的 host_id 列表。
pub fn host_ids(cfg: &RelayConfig) -> Vec<String> {
    cfg.hosts.iter().map(|h| h.host_id.clone()).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    const RELAY_TOML: &str = r#"
listen                 = "127.0.0.1:9100"
db_path                = "/run/ssm-relay/ledger.db"
snapshot_path          = "/var/lib/ssm-relay/ledger.snapshot.db"
secrets_path           = "/etc/ssm-relay/secrets.toml"
heartbeat_timeout_sec  = 90
handshake_retries      = 3
handshake_interval_sec = 10
handshake_timeout_sec  = 5
repeat_alert_min       = 30
startup_grace_periods  = 2
rate_limit_per_min     = 5
snapshot_interval_sec  = 300
lag_threshold_sec      = 300

[[hosts]]
host_id = "kebei-prod-01"
[[hosts.paths]]
kind = "lan"
url  = "http://192.168.1.10:9101/probe"
[[hosts.paths]]
kind = "tailscale"
url  = "http://100.64.0.10:9101/probe"
"#;

    fn secrets() -> SecretsConfig {
        SecretsConfig {
            shared_secret: "s".into(),
            status_token: "t".into(),
            bark: BarkConfig {
                endpoint: "https://api.day.app".into(),
                key: "k".into(),
                group: "server_status".into(),
            },
        }
    }

    fn load_text(text: &str) -> RelaySettings {
        let dir = TempDir::new("settings-load");
        let path = dir.path().join("relay.toml");
        std::fs::write(&path, text).unwrap();
        RelaySettings::load(&path).unwrap()
    }

    fn issues_for(text: &str) -> Vec<Issue> {
        load_text(text).validate(&secrets())
    }

    fn errors_for(text: &str) -> Vec<String> {
        issues_for(text)
            .into_iter()
            .filter(Issue::is_error)
            .map(|i| format!("{}: {}", i.field, i.message))
            .collect()
    }

    #[test]
    fn snapshot_path_comes_from_common_config_now() {
        let s = load_text(RELAY_TOML);
        assert_eq!(
            s.snapshot_path(),
            Path::new("/var/lib/ssm-relay/ledger.snapshot.db"),
            "直接来自 RelayConfig::snapshot_path（本地适配层已删除）"
        );
        assert_eq!(
            s.config.db_path,
            Path::new("/run/ssm-relay/ledger.db"),
            "热台账在 tmpfs"
        );
        assert!(
            s.validate(&secrets()).is_empty(),
            "{:?}",
            s.validate(&secrets())
        );
    }

    #[test]
    fn default_paths_follow_common_defaults() {
        let s = load_text(
            "[[hosts]]\nhost_id = \"h\"\n[[hosts.paths]]\nkind = \"lan\"\nurl = \"http://x/probe\"\n",
        );
        assert_eq!(s.config.db_path, Path::new("/run/ssm-relay/ledger.db"));
        assert_eq!(
            s.snapshot_path(),
            Path::new("/var/lib/ssm-relay/ledger.snapshot.db")
        );
        assert!(
            !config_check::has_errors(&s.validate(&secrets())),
            "{:?}",
            s.validate(&secrets())
        );
    }

    // ---------------- tmpfs 校验 ----------------

    #[test]
    fn snapshot_on_run_tmpfs_is_a_fatal_error() {
        let errs = errors_for(&RELAY_TOML.replace(
            "/var/lib/ssm-relay/ledger.snapshot.db",
            "/run/ssm-relay/ledger.snapshot.db",
        ));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("snapshot_path"), "{}", errs[0]);
        assert!(errs[0].contains("tmpfs"), "{}", errs[0]);
        assert!(errs[0].contains("/run"), "{}", errs[0]);
    }

    #[test]
    fn snapshot_on_tmp_is_a_fatal_error() {
        let errs = errors_for(&RELAY_TOML.replace(
            "/var/lib/ssm-relay/ledger.snapshot.db",
            "/tmp/ledger.snapshot.db",
        ));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("tmpfs"), "{}", errs[0]);
        assert!(errs[0].contains("/tmp"), "{}", errs[0]);
    }

    #[test]
    fn snapshot_on_dev_shm_is_a_fatal_error() {
        let errs = errors_for(&RELAY_TOML.replace(
            "/var/lib/ssm-relay/ledger.snapshot.db",
            "/dev/shm/ledger.snapshot.db",
        ));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("/dev/shm"), "{}", errs[0]);
    }

    #[test]
    fn snapshot_on_persistent_partition_passes() {
        for good in [
            "/var/lib/ssm-relay/ledger.snapshot.db",
            "/srv/ssm/ledger.snapshot.db",
            "/opt/ssm/ledger.snapshot.db",
            "/home/ssm/ledger.snapshot.db",
        ] {
            let text = RELAY_TOML.replace("/var/lib/ssm-relay/ledger.snapshot.db", good);
            let errs = errors_for(&text);
            assert!(errs.is_empty(), "{good} 应当通过，实际: {errs:?}");
        }
    }

    #[test]
    fn tmpfs_prefix_requires_a_path_segment_boundary() {
        // 不能把 `/runtime` / `/tmpfoo` 误判成 tmpfs
        assert_eq!(tmpfs_prefix(Path::new("/runtime/x.db")), None);
        assert_eq!(tmpfs_prefix(Path::new("/tmpfoo/x.db")), None);
        assert_eq!(tmpfs_prefix(Path::new("/dev/shmx/x.db")), None);
        // 边界正确
        assert_eq!(tmpfs_prefix(Path::new("/run")), Some("/run"));
        assert_eq!(tmpfs_prefix(Path::new("/run/x.db")), Some("/run"));
        assert_eq!(tmpfs_prefix(Path::new("/tmp/x.db")), Some("/tmp"));
        assert_eq!(tmpfs_prefix(Path::new("/dev/shm/x.db")), Some("/dev/shm"));
        assert_eq!(tmpfs_prefix(Path::new("/var/lib/x.db")), None);
    }

    #[test]
    fn snapshot_equal_to_db_is_a_fatal_error() {
        let errs = errors_for(&RELAY_TOML.replace(
            "/var/lib/ssm-relay/ledger.snapshot.db",
            "/run/ssm-relay/ledger.db",
        ));
        assert!(!errs.is_empty());
        assert!(
            errs.iter().any(|e| e.contains("同一个文件")),
            "同文件检查必须先命中: {errs:?}"
        );
    }

    #[test]
    fn db_path_outside_tmpfs_only_warns() {
        let issues = issues_for(&RELAY_TOML.replace(
            "db_path                = \"/run/ssm-relay/ledger.db\"",
            "db_path                = \"/var/lib/ssm-relay/ledger.db\"",
        ));
        assert!(!config_check::has_errors(&issues), "{issues:?}");
        assert!(issues.iter().any(|i| i.field == "db_path"));
    }

    // ---------------- secrets 未知键 ----------------

    #[test]
    fn typo_in_bark_endpoint_is_reported_as_unknown_key() {
        // 这就是「自建 Bark 静默失效、告警推去公网」的真实故障模式
        let text = r#"
shared_secret = "s"
status_token  = "t"
[bark]
endpont = "https://bark.mydomain.com"
key     = "k"
group   = "server_status"
"#;
        let unknown = unknown_secret_keys(text);
        assert_eq!(unknown, vec![("bark".to_string(), "endpont".to_string())]);

        // 只警告，不退出：宽松契约下加载必须成功
        let cfg: SecretsConfig = toml::from_str(text).expect("未知键不得导致 secrets 加载失败");
        assert_eq!(
            cfg.bark.endpoint, "https://api.day.app",
            "正是这种静默退化让 endpont 拼错变得危险"
        );
        assert_eq!(cfg.bark.key, "k");
    }

    #[test]
    fn unknown_top_level_key_is_reported() {
        let text = "shared_secret = \"s\"\nstatus_token = \"t\"\nshared_secrett = \"typo\"\n";
        let unknown = unknown_secret_keys(text);
        assert_eq!(
            unknown,
            vec![("(顶层)".to_string(), "shared_secrett".to_string())]
        );
    }

    #[test]
    fn clean_secrets_have_no_unknown_keys() {
        let text = r#"
shared_secret = "s"
status_token  = "t"
[bark]
endpoint = "https://bark.mydomain.com"
key      = "k"
group    = "server_status"
"#;
        assert!(unknown_secret_keys(text).is_empty());
        assert!(unknown_secret_keys("shared_secret = \"s\"\nstatus_token = \"t\"\n").is_empty());
    }

    #[test]
    fn log_unknown_secret_keys_is_a_noop_when_clean() {
        // 不 panic 即可（无未知键时直接返回，不打日志）
        log_unknown_secret_keys(Path::new("/etc/ssm-relay/secrets.toml"), &[]);
        log_unknown_secret_keys(
            Path::new("/etc/ssm-relay/secrets.toml"),
            &[("bark".to_string(), "endpont".to_string())],
        );
    }

    #[test]
    fn unparsable_secrets_text_yields_no_unknown_claims() {
        assert!(unknown_secret_keys("this is not toml ===").is_empty());
    }

    // ---------------- 加载错误 ----------------

    #[test]
    fn genuinely_unknown_relay_key_still_fails() {
        let dir = TempDir::new("settings-bad");
        let path = dir.path().join("relay.toml");
        std::fs::write(
            &path,
            "not_a_real_key = 1\n[[hosts]]\nhost_id = \"h\"\n[[hosts.paths]]\nkind = \"lan\"\nurl = \"http://x/probe\"\n",
        )
        .unwrap();
        let err = RelaySettings::load(&path).unwrap_err();
        assert!(err.to_string().contains("not_a_real_key"), "{err}");
    }

    #[test]
    fn missing_required_field_still_reports_clear_error() {
        let dir = TempDir::new("settings-missing");
        let path = dir.path().join("relay.toml");
        std::fs::write(
            &path,
            "[[hosts]]\n[[hosts.paths]]\nkind = \"lan\"\nurl = \"http://x/probe\"\n",
        )
        .unwrap();
        let err = RelaySettings::load(&path).unwrap_err();
        assert!(err.to_string().contains("host_id"), "{err}");
    }

    #[test]
    fn missing_file_reports_read_error() {
        let dir = TempDir::new("settings-nofile");
        let path = dir.path().join("nope.toml");
        let err = RelaySettings::load(&path).unwrap_err();
        assert!(err.to_string().contains("nope.toml"), "{err}");
        assert_eq!(err.path(), path.display().to_string());
    }

    #[test]
    fn read_secrets_text_reports_missing_file() {
        let dir = TempDir::new("settings-secrets");
        let err = read_secrets_text(&dir.path().join("nope.toml")).unwrap_err();
        assert!(err.to_string().contains("nope.toml"), "{err}");
    }

    /// **关键回归**：e2e `ssm-e2e.sh` 里的 relay.toml 必须能加载。
    #[test]
    fn loads_the_literal_e2e_relay_toml() {
        let e2e = r#"
listen                 = "127.0.0.1:9100"
db_path                = "/home/x/ssm-e2e-work/ledger.db"
snapshot_path          = "/home/x/ssm-e2e-work/ledger.snapshot.db"
secrets_path           = "/home/x/ssm-e2e-work/secrets.toml"
heartbeat_timeout_sec  = 6
handshake_retries      = 3
handshake_interval_sec = 1
handshake_timeout_sec  = 2
repeat_alert_min       = 0
startup_grace_periods  = 2
rate_limit_per_min     = 50
snapshot_interval_sec  = 3
lag_threshold_sec      = 300

[[hosts]]
host_id = "e2e-host"
[[hosts.paths]]
kind = "lan"
url  = "http://127.0.0.1:9101/probe"
"#;
        let s = load_text(e2e);
        assert_eq!(s.config.listen, "127.0.0.1:9100");
        assert_eq!(s.config.heartbeat_timeout_sec, 6);
        assert_eq!(s.config.handshake_interval_sec, 1);
        assert_eq!(s.config.repeat_alert_min, 0);
        assert_eq!(
            s.snapshot_path(),
            Path::new("/home/x/ssm-e2e-work/ledger.snapshot.db")
        );
        assert_eq!(host_ids(&s.config), vec!["e2e-host".to_string()]);

        let issues = s.validate(&secrets());
        assert!(!config_check::has_errors(&issues), "{issues:?}");
    }
}
