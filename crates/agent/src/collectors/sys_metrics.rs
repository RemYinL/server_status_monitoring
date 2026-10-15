//! 系统指标采集：CPU / 内存 / 磁盘 / 负载 / uptime / 网络吞吐（SPEC §7）。
//!
//! 用 `sysinfo` 0.36（API 与旧版差异很大，见 SPEC §11）。几个必须注意的点：
//!
//! - **CPU 使用率需要两次采样 + 间隔**：`refresh_cpu_usage()` 两次调用之间的间隔
//!   小于 [`MINIMUM_CPU_UPDATE_INTERVAL`] 时数值不可信。首轮只做「打点」，报 `0.0`。
//! - **网络吞吐是增量速率**：`NetworkData::received()` 是「距上次 refresh 的字节数」，
//!   除以真实间隔秒数得到 KiB/s。首轮没有基线，`net = None`（协议里 `net` 可空）。
//! - **磁盘按 `disks` 挂载点白名单过滤**：只上报配置里点名的挂载点，
//!   `DiskUsage.mount` 用配置里的字面值（规范化掉结尾 `/`）。
//!
//! 采集结果写进 [`SharedMetrics`]；`CollectOutcome` 返回空（本采集器不产出告警/服务/git）。

use std::collections::HashSet;
use std::time::Instant;

use async_trait::async_trait;
use ssm_common::protocol::{DiskUsage, Metrics, NetThroughput};
use sysinfo::{Disks, MemoryRefreshKind, Networks, System, MINIMUM_CPU_UPDATE_INTERVAL};

use super::{write_metrics, CollectCtx, CollectOutcome, Collector, SharedMetrics};

/// KiB（1024 字节）。
const KIB: f64 = 1024.0;
/// MiB。
const MIB: f64 = 1024.0 * 1024.0;
/// GiB。
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// 系统指标采集器。
pub struct SysMetricsCollector {
    system: System,
    disks: Disks,
    /// `agent.toml` 的 `disks`，挂载点白名单。
    disks_filter: Vec<String>,
    networks: Networks,
    /// 上次网络采样时刻；`None` 表示还没采过（首轮 `net = None`）。
    last_net_sample: Option<Instant>,
    /// 上次 CPU 采样时刻；`None` 表示还没打点（首轮 `cpu_percent = 0.0`）。
    last_cpu_sample: Option<Instant>,
    /// 上一次成功的 CPU 读数（间隔不足时沿用它，避免报 0 抖动）。
    cpu_percent: f32,
    /// 已经就「挂载点找不到」告警过的挂载点，防止每轮刷日志。
    missing_mounts_warned: HashSet<String>,
    shared: SharedMetrics,
}

impl SysMetricsCollector {
    /// 用默认的私有共享单元构造。
    ///
    /// **仅测试代码可用**（`cargo test` 与 T3 的单测）：`main.rs` 一律用
    /// [`SysMetricsCollector::with_shared`]，这样句柄能分享给 `thresholds`。
    #[cfg(test)]
    pub fn new(disks_filter: Vec<String>) -> Self {
        Self::with_shared(disks_filter, super::new_shared_metrics())
    }

    /// 所有 sysinfo 句柄都在构造时建立初值：`System`/`Disks`/`Networks` 的
    /// 「首次 refresh 只打点、不给差分」语义由本结构体的 `last_*_sample` 显式管理。
    pub fn with_shared(disks_filter: Vec<String>, shared: SharedMetrics) -> Self {
        let system = System::new();
        let disks = Disks::new_with_refreshed_list();
        let networks = Networks::new_with_refreshed_list();
        Self {
            system,
            disks,
            disks_filter: disks_filter
                .into_iter()
                .map(|m| normalize_mount(&m))
                .collect(),
            networks,
            last_net_sample: None,
            last_cpu_sample: None,
            cpu_percent: 0.0,
            missing_mounts_warned: HashSet::new(),
            shared,
        }
    }

    /// 共享指标句柄。
    ///
    /// **仅测试代码可用**：生产路径上句柄是由 `main.rs` 创建后再传进来的
    /// （`with_shared`），T3 需要的也是那一个。
    #[cfg(test)]
    pub fn shared(&self) -> &SharedMetrics {
        &self.shared
    }

    /// 采集一次并返回 [`Metrics`]（不写共享单元，便于测试）。
    pub fn sample(&mut self) -> Metrics {
        self.refresh_memory();
        self.refresh_cpu();

        Metrics {
            cpu_percent: self.cpu_percent,
            mem_used_mb: self.mem_used_mb(),
            mem_total_mb: bytes_to_mib(self.system.total_memory()),
            disks: self.sample_disks(),
            load_avg: self.load_avg(),
            uptime_sec: System::uptime(),
            net: self.sample_net(),
        }
    }

    // ---------------------------------------------------------------- 内存

    fn refresh_memory(&mut self) {
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    }

    fn mem_used_mb(&self) -> u64 {
        let total = self.system.total_memory();
        let available = self.system.available_memory();
        bytes_to_mib(total.saturating_sub(available))
    }

    // ---------------------------------------------------------------- CPU

    /// 首轮只打点（`cpu_percent` 保持 `0.0`）；之后每次间隔 >=
    /// [`MINIMUM_CPU_UPDATE_INTERVAL`] 才重采，间隔不足则沿用上次读数。
    fn refresh_cpu(&mut self) {
        let now = Instant::now();
        match self.last_cpu_sample {
            None => {
                self.system.refresh_cpu_usage();
                self.last_cpu_sample = Some(now);
                self.cpu_percent = 0.0;
                tracing::debug!("CPU 首轮打点完成，本轮 cpu_percent = 0.0，下一轮起有效");
            }
            Some(previous) => {
                if now.duration_since(previous) >= MINIMUM_CPU_UPDATE_INTERVAL {
                    self.system.refresh_cpu_usage();
                    self.cpu_percent = self.system.global_cpu_usage();
                    self.last_cpu_sample = Some(now);
                }
            }
        }
    }

    // ---------------------------------------------------------------- 磁盘

    /// 按 `disks` 白名单逐个挂载点取值；找不到的挂载点跳过（每个只告警一次）。
    fn sample_disks(&mut self) -> Vec<DiskUsage> {
        self.disks.refresh(true);
        let mut out = Vec::with_capacity(self.disks_filter.len());

        for wanted in &self.disks_filter {
            match self
                .disks
                .list()
                .iter()
                .find(|disk| normalize_mount(&disk.mount_point().to_string_lossy()) == *wanted)
            {
                Some(disk) => {
                    let total = disk.total_space();
                    let used = total.saturating_sub(disk.available_space());
                    out.push(DiskUsage {
                        mount: wanted.clone(),
                        used_gb: bytes_to_gib(used),
                        total_gb: bytes_to_gib(total),
                    });
                }
                None => {
                    if self.missing_mounts_warned.insert(wanted.clone()) {
                        tracing::warn!(
                            mount = %wanted,
                            "agent.toml 的 disks 里配置了本机不存在的挂载点，已跳过（此告警只出现一次）"
                        );
                    }
                }
            }
        }
        out
    }

    // ---------------------------------------------------------------- 负载

    fn load_avg(&self) -> [f64; 3] {
        let load = System::load_average();
        [load.one, load.five, load.fifteen]
    }

    // ---------------------------------------------------------------- 网络

    /// 汇总所有网卡的收发增量，除以真实间隔得到 KiB/s；首轮无基线返回 `None`。
    fn sample_net(&mut self) -> Option<NetThroughput> {
        let now = Instant::now();
        self.networks.refresh(true);

        let mut rx_bytes = 0_u64;
        let mut tx_bytes = 0_u64;
        for data in self.networks.list().values() {
            rx_bytes = rx_bytes.saturating_add(data.received());
            tx_bytes = tx_bytes.saturating_add(data.transmitted());
        }

        let previous = self.last_net_sample.replace(now)?;
        let elapsed_secs = now.duration_since(previous).as_secs_f64();
        if elapsed_secs <= 0.0 {
            return None;
        }

        Some(NetThroughput {
            rx_kbps: round2(rx_bytes as f64 / elapsed_secs / KIB),
            tx_kbps: round2(tx_bytes as f64 / elapsed_secs / KIB),
        })
    }
}

#[async_trait]
impl Collector for SysMetricsCollector {
    fn name(&self) -> &'static str {
        "sys_metrics"
    }

    /// 产出写进 [`SharedMetrics`]，三通道全空（阈值告警由 T3 `thresholds` 负责）。
    async fn collect(&mut self, _ctx: &CollectCtx) -> CollectOutcome {
        let metrics = self.sample();
        tracing::debug!(
            cpu_percent = metrics.cpu_percent,
            mem_used_mb = metrics.mem_used_mb,
            mem_total_mb = metrics.mem_total_mb,
            disks = metrics.disks.len(),
            net = metrics.net.is_some(),
            "系统指标采集完成"
        );
        write_metrics(&self.shared, metrics);
        CollectOutcome::empty()
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// `/data/` → `/data`；`/` 保持不变；顺带 trim 空白。
fn normalize_mount(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() > 1 {
        trimmed.trim_end_matches('/').to_string()
    } else {
        trimmed.to_string()
    }
}

fn bytes_to_mib(bytes: u64) -> u64 {
    (bytes as f64 / MIB).round() as u64
}

fn bytes_to_gib(bytes: u64) -> f64 {
    round2(bytes as f64 / GIB)
}

/// 保留两位小数（协议里的 `used_gb` / `total_gb` / `rx_kbps` 都是这种量级）。
fn round2(value: f64) -> f64 {
    if value.is_finite() {
        (value * 100.0).round() / 100.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ctx() -> CollectCtx {
        CollectCtx::new("test-host", System::uptime(), 1, Utc::now())
    }

    fn temp_mount_name() -> String {
        format!("/definitely-not-a-real-mount-{}", std::process::id())
    }

    #[test]
    fn normalize_mount_strips_trailing_slash_only() {
        assert_eq!(normalize_mount("/"), "/");
        assert_eq!(normalize_mount("/data"), "/data");
        assert_eq!(normalize_mount("/data/"), "/data");
        assert_eq!(normalize_mount("  /data//  "), "/data");
        assert_eq!(normalize_mount(""), "");
    }

    #[test]
    fn byte_helpers_are_sane() {
        assert_eq!(bytes_to_mib(0), 0);
        assert_eq!(bytes_to_mib(1024 * 1024), 1);
        assert_eq!(bytes_to_mib(8 * 1024 * 1024 * 1024), 8192);
        assert_eq!(bytes_to_gib(1024 * 1024 * 1024), 1.0);
        assert_eq!(bytes_to_gib(0), 0.0);
        assert_eq!(round2(1.23456), 1.23);
        assert_eq!(round2(1.235), 1.24);
        assert_eq!(round2(f64::NAN), 0.0);
        assert_eq!(round2(f64::INFINITY), 0.0);
    }

    /// WSL / 真 Linux 上都能真跑：内存总量、uptime、挂载点过滤。
    #[tokio::test]
    async fn collect_reports_real_metrics_and_filters_mounts() {
        let missing = temp_mount_name();
        let mut collector = SysMetricsCollector::new(vec!["/".to_string(), missing.clone()]);

        let outcome = collector.collect(&ctx()).await;
        assert!(
            outcome.is_empty(),
            "sys_metrics 不该产出 alerts/services/git，实际: {outcome:?}"
        );

        let metrics = super::super::read_metrics(collector.shared());
        assert!(
            metrics.mem_total_mb > 0,
            "mem_total_mb 必须 > 0，实际 {}",
            metrics.mem_total_mb
        );
        assert!(
            metrics.mem_used_mb <= metrics.mem_total_mb,
            "mem_used_mb({}) 不能超过 mem_total_mb({})",
            metrics.mem_used_mb,
            metrics.mem_total_mb
        );
        assert!(metrics.uptime_sec > 0, "uptime_sec 必须 > 0");
        assert!(
            metrics.load_avg.iter().all(|v| v.is_finite() && *v >= 0.0),
            "load_avg 必须是三个非负有限值，实际 {:?}",
            metrics.load_avg
        );

        assert_eq!(
            metrics.disks.len(),
            1,
            "只有配置里存在且命中的挂载点才上报，实际 {:?}",
            metrics.disks
        );
        assert_eq!(metrics.disks[0].mount, "/", "mount 必须是配置里的字面值");
        assert!(
            metrics.disks[0].total_gb > 0.0,
            "根分区 total_gb 必须 > 0，实际 {}",
            metrics.disks[0].total_gb
        );
        assert!(metrics.disks[0].used_gb <= metrics.disks[0].total_gb);
        assert!(metrics.disks[0].used_gb >= 0.0);

        // 首轮没有网络基线
        assert!(metrics.net.is_none(), "首轮 net 必须是 None（无基线）");
        // 首轮 CPU 只是打点
        assert_eq!(metrics.cpu_percent, 0.0, "首轮 cpu_percent 必须是 0.0");
    }

    /// 第二轮起：CPU 有真实读数，网络有增量速率。
    #[tokio::test]
    async fn second_round_has_cpu_and_net_baseline() {
        let mut collector = SysMetricsCollector::new(vec!["/".to_string()]);
        let _ = collector.collect(&ctx()).await;
        let first = super::super::read_metrics(collector.shared());
        assert!(first.net.is_none());

        // 必须跨过 MINIMUM_CPU_UPDATE_INTERVAL
        tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL + std::time::Duration::from_millis(50))
            .await;

        let _ = collector.collect(&ctx()).await;
        let second = super::super::read_metrics(collector.shared());

        let cpu = second.cpu_percent;
        assert!(
            (0.0..=100.0).contains(&cpu) && cpu.is_finite(),
            "第二轮 cpu_percent 必须是 0..=100 的有限值，实际 {cpu}"
        );
        let net = second.net.expect("第二轮必须有网络吞吐基线");
        assert!(net.rx_kbps >= 0.0 && net.rx_kbps.is_finite(), "{net:?}");
        assert!(net.tx_kbps >= 0.0 && net.tx_kbps.is_finite(), "{net:?}");
        assert!(second.mem_total_mb > 0);
        assert!(second.uptime_sec > 0);
    }

    /// 不在白名单里的挂载点绝不出现在上报里（即便本机确实有别的分区）。
    #[tokio::test]
    async fn unconfigured_mounts_are_never_reported() {
        let mut collector = SysMetricsCollector::new(vec![temp_mount_name()]);
        let _ = collector.collect(&ctx()).await;
        let metrics = super::super::read_metrics(collector.shared());
        assert!(
            metrics.disks.is_empty(),
            "配置里没有任何真实挂载点时 disks 必须为空，实际 {:?}",
            metrics.disks
        );
        // 其余指标不受影响
        assert!(metrics.mem_total_mb > 0);
    }

    /// 采集器名字用于日志与告警 source，必须稳定。
    #[test]
    fn collector_name_is_stable() {
        let collector = SysMetricsCollector::new(vec!["/".to_string()]);
        assert_eq!(Collector::name(&collector), "sys_metrics");
    }
}
