//! 采集器框架 —— `CollectOutcome` / `Collector` trait / `CollectCtx`（SPEC §10）。
//!
//! **本文件是 T2 与 T3 的接口契约。** trait 签名与 `CollectCtx` 字段严格照抄 SPEC §10，
//! 一字不差；T3 只读不改（确需改动时必须在返回结果里说明原因）。
//!
//! # T3 接入步骤（只需两步）
//!
//! 1. 在本文件的「T3 扩展点」处取消对应 `pub mod` 的注释；
//! 2. 在 `crates/agent/src/main.rs` 的 `build_collectors()` 里 `registry.push(...)` 一行。
//!
//! # 指标（`Metrics`）为什么不走 `CollectOutcome`
//!
//! SPEC §4 的 `ReportPacket.metrics` 是**必填单值**，而 `CollectOutcome` 的三个通道
//! （`alerts` / `services` / `git`）对应的是三个可选数组。因此 `Metrics` 由
//! [`sys_metrics`] 写进 [`SharedMetrics`] 共享单元，`main.rs` 与需要读指标的采集器
//! （T3 `thresholds`）从同一个句柄读取——这是 T2 在 SPEC §10 之外增补的辅助机制，
//! **没有增删 `CollectCtx` 的任何字段**。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ssm_common::protocol::{Alert, DiskUsage, GitStatus, Metrics, NetThroughput, ServiceStatus};

pub mod sys_metrics;

// ---------------------------------------------------------------------------
// T3 扩展点（一）：新增采集器文件后，取消下面一行的注释
// ---------------------------------------------------------------------------
pub mod git_tracker;
pub mod log_watcher;
pub mod service_probe;
pub mod thresholds;

/// 一轮采集的产出。三个通道分别对应 `ReportPacket` 的三个可选数组（SPEC §10）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CollectOutcome {
    pub alerts: Vec<Alert>,
    pub services: Vec<ServiceStatus>,
    pub git: Vec<GitStatus>,
}

impl CollectOutcome {
    /// 空产出（三个通道都是空 vec）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 追加语义：三个 `Vec` 依次 `extend`，**不覆盖、不去重、不排序**。
    ///
    /// 注册顺序即 `services[]` / `git[]` / `alerts[]` 在包内的顺序
    /// （`ReportPacket` 里没有时间排序要求，relay 也不依赖顺序）。
    pub fn merge(&mut self, other: CollectOutcome) {
        self.alerts.extend(other.alerts);
        self.services.extend(other.services);
        self.git.extend(other.git);
    }

    /// 三个通道是否都为空。**仅测试代码可用**（生产路径不需要判断空产出）。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.alerts.is_empty() && self.services.is_empty() && self.git.is_empty()
    }

    /// 三个通道的条目总数。**仅测试代码可用**。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.alerts.len() + self.services.len() + self.git.len()
    }
}

/// 跨采集器共享的只读状态（SPEC §10，**字段名以此为准**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectCtx {
    pub host_id: String,
    pub uptime_sec: u64,
    /// 单调递增的轮次号，T3 的节流/持续性判定可基于它。
    pub round: u64,
    /// 本轮时间（便于测试注入）。
    pub now: DateTime<Utc>,
}

impl CollectCtx {
    /// 便捷构造（字段全是 `pub`，直接结构体字面量也可以）。
    pub fn new(
        host_id: impl Into<String>,
        uptime_sec: u64,
        round: u64,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            host_id: host_id.into(),
            uptime_sec,
            round,
            now,
        }
    }
}

/// 采集器（SPEC §10）。
#[async_trait]
pub trait Collector: Send + Sync {
    /// 唯一标识，用于日志与告警 source。
    fn name(&self) -> &'static str;
    /// 每轮上报前调用一次。
    async fn collect(&mut self, ctx: &CollectCtx) -> CollectOutcome;
}

/// 采集器注册表：`main.rs` 每轮按注册顺序跑一遍并 `merge` 所有产出（SPEC §10）。
#[derive(Default)]
pub struct CollectorRegistry {
    collectors: Vec<Box<dyn Collector>>,
}

impl CollectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个采集器。**顺序有意义**：`sys_metrics` 必须第一个，
    /// 它先写入 [`SharedMetrics`]，后面的 `thresholds` 才能读到本轮指标。
    pub fn push(&mut self, collector: Box<dyn Collector>) {
        self.collectors.push(collector);
    }

    /// 按注册顺序跑一遍，逐个 `merge`。
    ///
    /// `Collector::collect` 没有错误通道（SPEC §10 的签名如此），
    /// 单个采集器内部失败时应自行 `tracing::warn!` 并返回空/部分产出。
    pub async fn collect_all(&mut self, ctx: &CollectCtx) -> CollectOutcome {
        let mut merged = CollectOutcome::empty();
        for collector in &mut self.collectors {
            let name = collector.name();
            let outcome = collector.collect(ctx).await;
            tracing::debug!(
                collector = name,
                round = ctx.round,
                alerts = outcome.alerts.len(),
                services = outcome.services.len(),
                git = outcome.git.len(),
                "采集器完成一轮"
            );
            merged.merge(outcome);
        }
        merged
    }

    /// 已注册的采集器名字，顺序同注册顺序。
    pub fn names(&self) -> Vec<&'static str> {
        self.collectors.iter().map(|c| c.name()).collect()
    }

    pub fn len(&self) -> usize {
        self.collectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.collectors.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 共享指标单元
// ---------------------------------------------------------------------------

/// 最近一轮 [`Metrics`] 的共享句柄。
///
/// `sys_metrics` 每轮写入；`main.rs` 组包时读取；T3 `thresholds` 等需要
/// CPU/内存/磁盘当前值的采集器读取同一个句柄。
pub type SharedMetrics = std::sync::Arc<std::sync::Mutex<Metrics>>;

/// 全零指标（尚未采集过时的初值）。
pub fn zero_metrics() -> Metrics {
    Metrics {
        cpu_percent: 0.0,
        mem_used_mb: 0,
        mem_total_mb: 0,
        disks: Vec::<DiskUsage>::new(),
        load_avg: [0.0, 0.0, 0.0],
        uptime_sec: 0,
        net: Option::<NetThroughput>::None,
    }
}

/// 新建共享指标单元（初值为 [`zero_metrics`]）。
pub fn new_shared_metrics() -> SharedMetrics {
    std::sync::Arc::new(std::sync::Mutex::new(zero_metrics()))
}

/// 读取当前指标快照（互斥锁中毒也能读出最后写入的值，不 panic）。
pub fn read_metrics(shared: &SharedMetrics) -> Metrics {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 写入当前指标快照（互斥锁中毒时恢复写入，不 panic）。
pub fn write_metrics(shared: &SharedMetrics, metrics: Metrics) {
    let mut guard = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = metrics;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssm_common::protocol::{AlertLevel, ServiceKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn alert(id: &str) -> Alert {
        Alert {
            id: id.to_string(),
            level: AlertLevel::Warning,
            source: "test".to_string(),
            title: format!("title-{id}"),
            summary: format!("summary-{id}"),
            first_seen: Utc::now(),
        }
    }

    fn service(name: &str) -> ServiceStatus {
        ServiceStatus {
            name: name.to_string(),
            kind: ServiceKind::Http,
            target: "http://127.0.0.1/".to_string(),
            ok: true,
            latency_ms: Some(1),
            detail: None,
        }
    }

    fn git(repo: &str) -> GitStatus {
        GitStatus {
            repo: repo.to_string(),
            branch: "main".to_string(),
            head: "deadbee".to_string(),
            dirty: false,
            changed: false,
        }
    }

    /// `merge` 的三通道追加语义：逐个追加、顺序保持、互不干扰。
    #[test]
    fn merge_appends_all_three_channels_in_order() {
        let mut acc = CollectOutcome::empty();
        assert!(acc.is_empty());
        assert_eq!(acc.len(), 0);

        acc.merge(CollectOutcome {
            alerts: vec![alert("a1"), alert("a2")],
            services: vec![service("s1")],
            git: vec![],
        });
        acc.merge(CollectOutcome {
            alerts: vec![],
            services: vec![service("s2"), service("s3")],
            git: vec![git("g1")],
        });
        acc.merge(CollectOutcome {
            alerts: vec![alert("a3")],
            services: vec![],
            git: vec![git("g2")],
        });

        let alert_ids: Vec<&str> = acc.alerts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(alert_ids, ["a1", "a2", "a3"], "alerts 必须按追加顺序拼接");
        let svc_names: Vec<&str> = acc.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(svc_names, ["s1", "s2", "s3"], "services 必须按追加顺序拼接");
        let repos: Vec<&str> = acc.git.iter().map(|g| g.repo.as_str()).collect();
        assert_eq!(repos, ["g1", "g2"], "git 必须按追加顺序拼接");
        assert_eq!(acc.len(), 8);
        assert!(!acc.is_empty());

        // 合并空产出不改变任何东西
        let before = acc.clone();
        acc.merge(CollectOutcome::empty());
        assert_eq!(acc, before);
    }

    /// 追加是 `extend` 而不是替换：目标里原有的条目不能被丢掉。
    #[test]
    fn merge_does_not_replace_existing_entries() {
        let mut acc = CollectOutcome {
            alerts: vec![alert("keep")],
            services: vec![service("keep")],
            git: vec![git("keep")],
        };
        acc.merge(CollectOutcome {
            alerts: vec![alert("new")],
            services: vec![],
            git: vec![],
        });
        assert_eq!(acc.alerts[0].id, "keep");
        assert_eq!(acc.alerts[1].id, "new");
        assert_eq!(acc.services[0].name, "keep");
        assert_eq!(acc.git[0].repo, "keep");
    }

    /// T3 会照着这个形状写：注册顺序 = 合并顺序 = `services[]` 顺序。
    #[tokio::test]
    async fn registry_collects_in_registration_order() {
        struct Fixed(&'static str, Vec<&'static str>);

        #[async_trait]
        impl Collector for Fixed {
            fn name(&self) -> &'static str {
                self.0
            }
            async fn collect(&mut self, _ctx: &CollectCtx) -> CollectOutcome {
                CollectOutcome {
                    alerts: vec![],
                    services: self.1.iter().map(|n| service(n)).collect(),
                    git: vec![],
                }
            }
        }

        let mut registry = CollectorRegistry::new();
        assert!(registry.is_empty());
        registry.push(Box::new(Fixed("first", vec!["a", "b"])));
        registry.push(Box::new(Fixed("second", vec!["c"])));

        assert_eq!(registry.names(), vec!["first", "second"]);
        assert_eq!(registry.len(), 2);

        let ctx = CollectCtx::new("h", 1, 7, Utc::now());
        let outcome = registry.collect_all(&ctx).await;
        let names: Vec<&str> = outcome.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(outcome.alerts.is_empty());
        assert!(outcome.git.is_empty());
    }

    #[test]
    fn collect_ctx_constructor_matches_field_names() {
        let now = Utc::now();
        let ctx = CollectCtx::new("kebei-prod-01", 123_456, 9, now);
        assert_eq!(ctx.host_id, "kebei-prod-01");
        assert_eq!(ctx.uptime_sec, 123_456);
        assert_eq!(ctx.round, 9);
        assert_eq!(ctx.now, now);
        // 结构体字面量也必须可用（T3 会这样构造）
        let literal = CollectCtx {
            host_id: "x".to_string(),
            uptime_sec: 1,
            round: 2,
            now,
        };
        assert_eq!(literal.host_id, "x");
    }

    #[test]
    fn shared_metrics_roundtrip_and_zero_default() {
        let shared = new_shared_metrics();
        let initial = read_metrics(&shared);
        assert_eq!(initial, zero_metrics());
        assert_eq!(initial.mem_total_mb, 0);
        assert!(initial.net.is_none());

        let mut updated = zero_metrics();
        updated.cpu_percent = 12.5;
        updated.mem_total_mb = 8192;
        write_metrics(&shared, updated.clone());
        assert_eq!(read_metrics(&shared), updated);
    }

    /// 互斥锁中毒（某个采集器 panic 过）不应让后续读取 panic。
    #[test]
    fn shared_metrics_survives_poisoned_lock() {
        let shared = new_shared_metrics();
        let clone = std::sync::Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock().unwrap();
            panic!("故意让锁中毒");
        })
        .join();

        write_metrics(&shared, zero_metrics());
        assert_eq!(read_metrics(&shared), zero_metrics());
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 保证本模块的测试不写仓库（虽然这里没有落盘操作，留着给 T3 复制）。
    #[test]
    fn test_counter_is_available_for_temp_dirs() {
        let a = COUNTER.fetch_add(1, Ordering::Relaxed);
        let b = COUNTER.fetch_add(1, Ordering::Relaxed);
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // 回归护栏：agent 从不产出「已恢复」类告警（SPEC §13.2）
    // -----------------------------------------------------------------------

    /// **SPEC §13.2 硬约束**：恢复通知由 relay 依据「条件型告警从 `alerts[]` 中消失」推导，
    /// **agent 侧绝不产出**任何 `*:recovered` 告警。
    ///
    /// 这条测试跑的是**注册表 + 四个真实采集器**（不是 mock），故意制造「每一类告警都真的
    /// 产出过」的场景，然后断言所有产出里没有任何 `recovered`（id）或「恢复」（title）：
    /// 将来谁把 `service:<name>:recovered` / `threshold:*:recovered` 加回来，它必须红。
    ///
    /// 断言里带 `assert!(seen_... )` 的「非恒真」前置检查：如果一条告警都没跑出来，
    /// 这条护栏就是恒真的假阳性测试，同样判定失败。
    #[tokio::test]
    async fn agent_never_emits_recovered_alerts() {
        use crate::collectors::{
            git_tracker::GitTrackerCollector, log_watcher::LogWatcherCollector,
            service_probe::ServiceProbeCollector, thresholds::ThresholdsCollector,
        };
        use chrono::TimeZone;
        use ssm_common::config::{
            GitRepoConfig, LogLevel, LogRule, LogSource, ProbeConfig, ProbeKind, ThresholdConfig,
        };

        let base = std::env::temp_dir().join(format!(
            "ssm-recovered-guard-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        // ---- service_probe：取一个**实测连不上**的目标（占端口再放掉并确认连接失败）----
        let dead_target = {
            let mut found = None;
            for _ in 0..10 {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                drop(listener);
                let candidate = format!("127.0.0.1:{port}");
                let refused = std::net::TcpStream::connect_timeout(
                    &candidate.parse().unwrap(),
                    std::time::Duration::from_millis(200),
                )
                .is_err();
                if refused {
                    found = Some(candidate);
                    break;
                }
            }
            found.expect("找不到连不上的测试目标（本机端口分配异常）")
        };

        // ---- log_watcher：写一行必然命中规则的文件 ----
        let log_path = base.join("app.log");
        std::fs::write(&log_path, "ERROR guard-triggered\n").unwrap();

        // ---- git_tracker：真建仓库 + 真提交（git 不可用就跳过该部分）----
        let repo_path = base.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let git_repo_ok = {
            let git = |args: &[&str]| {
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo_path)
                    .output()
                    .map(|out| out.status.success())
                    .unwrap_or(false)
            };
            git(&["init", "-q", "."])
                && git(&["config", "user.email", "guard@test.local"])
                && git(&["config", "user.name", "guard"])
                && std::fs::write(repo_path.join("a.txt"), "v1\n").is_ok()
                && git(&["add", "-A"])
                && git(&["commit", "-qm", "init"])
        };

        // ---- thresholds：磁盘 95% + CPU 99%，sustained_sec = 0（立刻达标）----
        let threshold_cfg = ThresholdConfig {
            disk_percent: 85.0,
            disk_throttle_sec: 21_600,
            cpu_percent: 95.0,
            mem_percent: 90.0,
            sustained_sec: 0,
        };

        let metrics_handle = new_shared_metrics();
        write_metrics(
            &metrics_handle,
            Metrics {
                cpu_percent: 99.0,
                mem_used_mb: 9_500,
                mem_total_mb: 10_000,
                disks: vec![DiskUsage {
                    mount: "/guard".to_string(),
                    used_gb: 95.0,
                    total_gb: 100.0,
                }],
                load_avg: [1.0, 1.0, 1.0],
                uptime_sec: 42,
                net: None,
            },
        );

        let mut registry = CollectorRegistry::new();
        registry.push(Box::new(ServiceProbeCollector::new(&[ProbeConfig {
            name: "web-frontend".to_string(),
            kind: ProbeKind::Tcp,
            target: dead_target,
            fail_threshold: 2,
            timeout_sec: 1,
        }])));
        registry.push(Box::new(LogWatcherCollector::new(&[LogRule {
            name: "guard-error".to_string(),
            source: LogSource::File(log_path.clone()),
            pattern: Some("ERROR".to_string()),
            level: LogLevel::Warning,
            throttle_sec: 0,
            max_lines: 3,
            include_raw: false,
        }])));
        if git_repo_ok {
            let mut git_collector = GitTrackerCollector::new(&[GitRepoConfig {
                name: "guard-repo".to_string(),
                path: repo_path.clone(),
                timeout_sec: 5,
            }]);
            // 生产阈值是 24h（测试里等不起），注入 0 让 `git:guard-repo:dirty` 立刻进入
            // 「条件成立」状态 —— 这样下面的护栏断言才真的覆盖到 `git:` 前缀的告警。
            git_collector.set_dirty_alert_after_sec(0);
            registry.push(Box::new(git_collector));
        }
        registry.push(Box::new(ThresholdsCollector::new(
            &threshold_cfg,
            metrics_handle.clone(),
        )));

        let t0 = Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap();
        let mut alerts = Vec::new();
        let mut git_changed_seen = false;
        let mut git_dirty_rounds = 0_usize;

        for round in 1..=6_u64 {
            // 第 4 轮造一次 HEAD 变化，覆盖 git 的**事件型**告警
            if round == 4 && git_repo_ok {
                std::fs::write(repo_path.join("a.txt"), "v2\n").unwrap();
                let committed = std::process::Command::new("git")
                    .args(["commit", "-qam", "guard change"])
                    .current_dir(&repo_path)
                    .output()
                    .map(|out| out.status.success())
                    .unwrap_or(false);
                assert!(committed, "guard 仓库的第二笔提交必须成功");
                // 提交后再留一个**未跟踪**文件 → 第 4 轮起工作区持续脏，
                // 用来覆盖 `git:<repo>:dirty`（条件存续期间每轮都要产出）
                std::fs::write(repo_path.join("untracked.txt"), "left behind\n").unwrap();
            }

            let ctx = CollectCtx::new(
                "guard-host",
                100,
                round,
                t0 + chrono::Duration::seconds((round as i64 - 1) * 30),
            );
            let outcome = registry.collect_all(&ctx).await;
            git_changed_seen |= outcome
                .alerts
                .iter()
                .any(|a| a.id == "git:guard-repo:changed");
            if outcome
                .alerts
                .iter()
                .any(|a| a.id == "git:guard-repo:dirty")
            {
                git_dirty_rounds += 1;
            }
            alerts.extend(outcome.alerts);
        }

        // ---- 非恒真前置：这几类告警必须真的产出过，否则护栏没有意义 ----
        let ids: Vec<&str> = alerts.iter().map(|a| a.id.as_str()).collect();
        assert!(
            ids.iter()
                .filter(|id| **id == "service:web-frontend")
                .count()
                >= 5,
            "服务探针告警必须真的产出过（且不止一次）: {ids:?}"
        );
        assert!(
            ids.contains(&"log:guard-error"),
            "日志规则告警必须真的产出过: {ids:?}"
        );
        assert!(
            ids.contains(&"threshold:cpu") && ids.contains(&"threshold:mem"),
            "CPU/内存阈值告警必须真的产出过: {ids:?}"
        );
        assert!(
            ids.contains(&"threshold:disk:/guard"),
            "磁盘阈值告警必须真的产出过: {ids:?}"
        );
        if git_repo_ok {
            assert!(
                git_changed_seen,
                "git 的 HEAD 变化告警（事件型）必须真的产出过: {ids:?}"
            );
            assert_eq!(
                git_dirty_rounds, 3,
                "`git:guard-repo:dirty` 必须真的产出过、且工作区脏的每一轮都在（第 4/5/6 轮）: {ids:?}"
            );
        } else {
            eprintln!("跳过 git 部分的覆盖：本机 git 不可用");
        }

        // ---- 被护栏保护的断言（覆盖全部四个采集器，含 `git:` / `log:` / `threshold:` / `service:`）----
        for alert in &alerts {
            assert!(
                !alert.id.contains("recovered"),
                "agent 侧不得产出恢复告警（SPEC §13.2），实际 id = {:?}（全部: {ids:?}）",
                alert.id
            );
            assert!(
                !alert.title.contains("恢复"),
                "agent 侧不得产出恢复通知（SPEC §13.2），实际 title = {:?}",
                alert.title
            );
        }
        for prefix in ["service:", "threshold:", "log:", "git:"] {
            assert!(
                ids.iter().any(|id| id.starts_with(prefix)),
                "护栏必须覆盖 {prefix}* 前缀的告警，否则那一段是空的: {ids:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
    }
}
