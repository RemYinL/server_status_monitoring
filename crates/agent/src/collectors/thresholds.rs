//! 资源阈值：磁盘 / CPU / 内存（SPEC §7、docs/04 §2.4、docs/06 T8）。
//!
//! 指标来自 [`SharedMetrics`]——`sys_metrics` 每轮把本轮 [`Metrics`] 写进去，
//! 本采集器读同一个句柄，所以 `main.rs` 里 `sys_metrics` **必须第一个注册**。
//!
//! # 三档规则与时间语义
//!
//! | 规则 | 触发 | 级别 | 产出节奏 |
//! |---|---|---|---|
//! | 磁盘 `used/total > disk_percent` | 立即 | warning | 超阈期间**每轮**都产出 |
//! | CPU `> cpu_percent` **持续** `sustained_sec` | 见下 | warning | 达标后**每轮**都产出 |
//! | 内存 `> mem_percent` **持续** `sustained_sec` | 见下 | warning | 达标后**每轮**都产出 |
//!
//! ## 「持续 N 秒」是**时间**，不是轮数
//!
//! 记 `over_since = 第一轮超阈的 `CollectCtx::now``，只有当
//! `now - over_since >= sustained_sec` 才产出告警。中途任何一轮回到阈值以下就把计时清零。
//! 计时全部用注入的 `now`，所以测试里可以把时间快进 5 分钟而不用真的等。
//!
//! ⚠️ **`sustained_sec` 只是「进入告警态的门槛」，不是「重复提醒节奏」**（SPEC §13.2）：
//! 一旦达标，超阈期间**每一轮都要产出**同一条告警，`first_seen` 保持首次超阈时刻不变。
//! 老实现「每 `sustained_sec` 才报一次」会制造**假恢复窗口**（报一次 → 下一轮缺席 →
//! relay 判定为已恢复 → 再报 → 再「恢复」），已在本次修复中删除。
//!
//! 重复提醒的节奏交给 **relay** 的 `rate_limit_per_min` / `repeat_alert_min` 决定
//! （SPEC §13.2）：relay 按 `(source, title)` 去重，所以这里每轮重复上报**不会**造成推送风暴。
//! 同理，`disk_throttle_sec` 自 SPEC §13.2 起**不再用于 agent 侧抑制**（保留配置字段以兼容
//! 既有 `agent.toml`，但本采集器不再读它）。
//!
//! ## 恢复
//!
//! 指标回到阈值以下 → 计时清零，**下一轮起不再产出**该告警。本采集器**不产出任何
//! 「已恢复」告警**：恢复通知由 relay 依据「条件型告警从 `alerts[]` 中消失」推导（SPEC §13.2）。
//!
//! # 去重键（`Alert.id` 与 `(source, title)`）
//!
//! `threshold:disk:<mount>` / `threshold:cpu` / `threshold:mem`。
//! **没有** `…:recovered`。
//!
//! relay 的恢复判定用 `(source, title)`，所以 `title` 在条件存续期间必须**逐字不变**：
//! 它只放配置常量（挂载点、阈值），逐轮变化的实测值一律进 `summary`。

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ssm_common::config::ThresholdConfig;
use ssm_common::protocol::{Alert, AlertLevel, Metrics};

use super::{read_metrics, CollectCtx, CollectOutcome, Collector, SharedMetrics};

/// 告警来源标识（SPEC §4）。
const SOURCE: &str = "thresholds";

/// 磁盘告警的追踪状态。
///
/// 磁盘是**立即**告警（不需要 `sustained_sec`），所以这里只需要记住「第一次超阈的时刻」，
/// 用来让 `first_seen` 在超阈期间保持稳定（SPEC §13.2；relay 也用它判定补报是否过期）。
struct DiskState {
    /// 首次超阈（= 本轮起条件成立）的时刻；`None` = 当前没有超阈。
    over_since: Option<DateTime<Utc>>,
}

/// CPU / 内存这类「需要持续超阈」的规则状态。
#[derive(Default)]
struct SustainedState {
    /// 本轮超阈区间的起点（`None` = 当前没有超阈）。
    over_since: Option<DateTime<Utc>>,
    /// 本区间内是否已经产出过告警（只用于区分首报与续报的日志级别；产出与否不受它影响）。
    reported: bool,
}

/// 资源阈值采集器。
pub struct ThresholdsCollector {
    config: ThresholdConfig,
    shared: SharedMetrics,
    disks: HashMap<String, DiskState>,
    cpu: SustainedState,
    mem: SustainedState,
}

impl ThresholdsCollector {
    /// 用 `[thresholds]` 配置与共享指标句柄构造。
    pub fn new(config: &ThresholdConfig, shared: SharedMetrics) -> Self {
        Self {
            config: config.clone(),
            shared,
            disks: HashMap::new(),
            cpu: SustainedState::default(),
            mem: SustainedState::default(),
        }
    }

    /// 读一次本轮指标（**仅测试可用**；生产路径在 `collect` 里直接读共享句柄）。
    #[cfg(test)]
    pub fn metrics(&self) -> Metrics {
        read_metrics(&self.shared)
    }

    fn sustained(&self) -> Duration {
        Duration::from_secs(self.config.sustained_sec)
    }

    /// 计算某个挂载点的使用率（百分比）；`total_gb` 非正或非有限时返回 `None`。
    fn disk_percent_of(used_gb: f64, total_gb: f64) -> Option<f64> {
        if !used_gb.is_finite() || !total_gb.is_finite() || total_gb <= 0.0 {
            return None;
        }
        Some(used_gb / total_gb * 100.0)
    }

    fn evaluate_disks(
        &mut self,
        metrics: &Metrics,
        ctx: &CollectCtx,
        outcome: &mut CollectOutcome,
    ) {
        let threshold = self.config.disk_percent;

        for disk in &metrics.disks {
            let Some(percent) = Self::disk_percent_of(disk.used_gb, disk.total_gb) else {
                tracing::debug!(
                    mount = %disk.mount,
                    used_gb = disk.used_gb,
                    total_gb = disk.total_gb,
                    "磁盘总量无效（<=0），本轮跳过该挂载点"
                );
                continue;
            };

            let state = self
                .disks
                .entry(disk.mount.clone())
                .or_insert(DiskState { over_since: None });

            if percent <= threshold {
                // 条件消失：计时清零，**下一轮起不再产出**（恢复通知由 relay 推导，SPEC §13.2）
                if let Some(since) = state.over_since.take() {
                    tracing::info!(
                        mount = %disk.mount,
                        percent = percent,
                        over_for_sec = ctx
                            .now
                            .signed_duration_since(since)
                            .num_seconds()
                            .max(0),
                        "磁盘使用率已回落到阈值以下，本轮起不再上报该告警（恢复通知由 relay 推导）"
                    );
                }
                continue;
            }

            // ---- 条件型：超阈期间**每一轮都产出**同一条告警（SPEC §13.2）----
            // `title` 只含挂载点与阈值（都是常量），逐轮变化的实测值放 `summary`：
            // relay 按 `(source, title)` 去重与判定恢复，标题一抖动就会假恢复 + 推送风暴。
            let first_report = state.over_since.is_none();
            let first_seen = *state.over_since.get_or_insert(ctx.now);

            outcome.alerts.push(Alert {
                id: format!("threshold:disk:{}", disk.mount),
                level: AlertLevel::Warning,
                source: SOURCE.to_string(),
                title: format!("【磁盘告警】{} 使用率超过 {:.1}%", disk.mount, threshold),
                summary: format!(
                    "挂载点 {} 使用率 {:.1}%（阈值 {:.1}%）：{:.1} / {:.1} GB",
                    disk.mount, percent, threshold, disk.used_gb, disk.total_gb
                ),
                first_seen,
            });

            if first_report {
                tracing::warn!(
                    mount = %disk.mount,
                    percent = percent,
                    threshold = threshold,
                    "磁盘使用率超阈，产出 warning 告警"
                );
            } else {
                tracing::info!(
                    mount = %disk.mount,
                    percent = percent,
                    round = ctx.round,
                    "磁盘仍超阈，本轮继续上报该告警（条件型，每轮必报）"
                );
            }
        }
    }
}

/// CPU / 内存共用的「持续超阈」判定。
///
/// 写成自由函数而不是 `&self` 方法：`collect` 里要同时把 `&mut self.cpu` / `&mut self.mem`
/// 借出去，方法形式会撞上借用检查（`&self` vs `&mut self.cpu`）。
///
/// 语义（SPEC §13.2 条件型）：
/// - 未满 `window`（`sustained_sec`）→ **不产出**（防瞬时峰值误报，docs/04 §2.4）；
/// - 已满 `window` → **每一轮都产出**同一条告警，`first_seen` 保持首次超阈时刻；
/// - 回落到阈值以下 → 计时清零，下一轮起不再产出；
/// - **绝不产出「已恢复」告警**（由 relay 推导）。
#[allow(clippy::too_many_arguments)]
fn evaluate_sustained(
    window: Duration,
    key: &'static str,
    label: &'static str,
    value: f64,
    threshold: f64,
    state: &mut SustainedState,
    ctx: &CollectCtx,
    outcome: &mut CollectOutcome,
) {
    if value <= threshold {
        // 条件消失：计时清零（下一轮起不再产出该告警）
        if let Some(since) = state.over_since.take() {
            tracing::info!(
                metric = %label,
                value = value,
                over_for_sec = ctx
                    .now
                    .signed_duration_since(since)
                    .num_seconds()
                    .max(0),
                "资源使用率已回落到阈值以下，本轮起不再上报该告警（恢复通知由 relay 推导）"
            );
        }
        state.reported = false;
        return;
    }

    let since = *state.over_since.get_or_insert(ctx.now);
    let elapsed = ctx
        .now
        .signed_duration_since(since)
        .to_std()
        .unwrap_or(Duration::ZERO);

    if elapsed < window {
        // 防瞬时峰值误报：还没满 sustained_sec，**本轮不产出**（docs/04 §2.4）
        tracing::debug!(
            metric = %label,
            value = value,
            threshold = threshold,
            over_for_sec = elapsed.as_secs(),
            sustained_sec = window.as_secs(),
            "资源超阈但未达持续时长，本轮不产出告警"
        );
        return;
    }

    // ---- 条件型：达标后**每一轮都产出**（`first_seen` 恒为首次超阈时刻）----
    // 注意这里**不再**重置 `over_since`，也不做任何节流：老实现「每 sustained_sec 才报一次」
    // 会制造假恢复窗口（报一次 → 下一轮缺席 → relay 判为恢复 → 再报 → 再「恢复」）。
    let first_report = !state.reported;
    state.reported = true;

    outcome.alerts.push(Alert {
        id: format!("threshold:{key}"),
        level: AlertLevel::Warning,
        source: SOURCE.to_string(),
        // title 只放配置常量（label 与阈值）：relay 按 `(source, title)` 去重与判恢复。
        title: format!("【{label}告警】持续高于 {threshold:.1}%"),
        summary: format!(
            "{label}使用率 {value:.1}% 已持续 {}s 超过阈值 {threshold:.1}%（判定窗口 {}s）",
            elapsed.as_secs(),
            window.as_secs()
        ),
        first_seen: since,
    });

    if first_report {
        tracing::warn!(
            metric = %label,
            value = value,
            threshold = threshold,
            sustained_sec = window.as_secs(),
            "资源持续超阈达标，产出 warning 告警"
        );
    } else {
        tracing::info!(
            metric = %label,
            value = value,
            round = ctx.round,
            "资源仍持续超阈，本轮继续上报该告警（条件型，每轮必报）"
        );
    }
}

#[async_trait]
impl Collector for ThresholdsCollector {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn collect(&mut self, ctx: &CollectCtx) -> CollectOutcome {
        let metrics = read_metrics(&self.shared);
        let window = self.sustained();
        let cpu_threshold = self.config.cpu_percent;
        let mem_threshold = self.config.mem_percent;
        let mut outcome = CollectOutcome::empty();

        self.evaluate_disks(&metrics, ctx, &mut outcome);

        let cpu = metrics.cpu_percent as f64;
        evaluate_sustained(
            window,
            "cpu",
            "CPU",
            cpu,
            cpu_threshold,
            &mut self.cpu,
            ctx,
            &mut outcome,
        );

        // 内存：`mem_total_mb == 0`（sys_metrics 还没采过）时无法算百分比，跳过
        let mem_percent = if metrics.mem_total_mb > 0 {
            Some(metrics.mem_used_mb as f64 / metrics.mem_total_mb as f64 * 100.0)
        } else {
            None
        };
        match mem_percent {
            Some(percent) => evaluate_sustained(
                window,
                "mem",
                "内存",
                percent,
                mem_threshold,
                &mut self.mem,
                ctx,
                &mut outcome,
            ),
            None => {
                tracing::debug!("mem_total_mb = 0（sys_metrics 尚未采集），本轮跳过内存阈值判定")
            }
        }

        tracing::debug!(
            cpu_percent = cpu,
            mem_percent = mem_percent.unwrap_or(0.0),
            disks = metrics.disks.len(),
            alerts = outcome.alerts.len(),
            round = ctx.round,
            "资源阈值判定完成"
        );
        outcome
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::{new_shared_metrics, write_metrics, zero_metrics};
    use chrono::TimeZone;
    use ssm_common::protocol::DiskUsage;

    fn base_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap()
    }

    fn ctx_at(secs: i64) -> CollectCtx {
        CollectCtx::new(
            "test-host",
            100,
            1,
            base_time() + chrono::Duration::seconds(secs),
        )
    }

    fn metrics(
        cpu: f32,
        mem_used_mb: u64,
        mem_total_mb: u64,
        disks: &[(&str, f64, f64)],
    ) -> Metrics {
        Metrics {
            cpu_percent: cpu,
            mem_used_mb,
            mem_total_mb,
            disks: disks
                .iter()
                .map(|(mount, used, total)| DiskUsage {
                    mount: mount.to_string(),
                    used_gb: *used,
                    total_gb: *total,
                })
                .collect(),
            load_avg: [0.0, 0.0, 0.0],
            uptime_sec: 42,
            net: None,
        }
    }

    /// 造一个「指标已写入共享句柄」的采集器。
    fn collector_with(cfg: ThresholdConfig, m: Metrics) -> ThresholdsCollector {
        let shared = new_shared_metrics();
        write_metrics(&shared, m);
        ThresholdsCollector::new(&cfg, shared)
    }

    fn cfg() -> ThresholdConfig {
        ThresholdConfig {
            disk_percent: 85.0,
            disk_throttle_sec: 21_600,
            cpu_percent: 95.0,
            mem_percent: 90.0,
            sustained_sec: 300,
        }
    }

    // ---------------- 磁盘 ----------------

    #[tokio::test]
    async fn disk_above_threshold_alerts_warning_with_mount_id() {
        let mut collector = collector_with(
            cfg(),
            metrics(1.0, 10, 100, &[("/", 90.0, 100.0), ("/data", 10.0, 100.0)]),
        );

        let outcome = collector.collect(&ctx_at(0)).await;
        assert_eq!(
            outcome.alerts.len(),
            1,
            "只有超阈的挂载点告警: {:?}",
            outcome.alerts
        );
        let alert = &outcome.alerts[0];
        assert_eq!(alert.id, "threshold:disk:/");
        assert_eq!(alert.level, AlertLevel::Warning, "磁盘必须是 warning");
        assert_eq!(alert.source, "thresholds");
        assert!(
            alert.title.contains('/'),
            "title 要点出挂载点: {}",
            alert.title
        );
        assert!(
            alert.summary.contains("90.0"),
            "实测使用率必须进 summary: {}",
            alert.summary
        );
        assert!(
            !alert.summary.contains("/data"),
            "不该提到未超阈的挂载点: {}",
            alert.summary
        );
        assert_eq!(alert.first_seen, base_time(), "first_seen 是首次超阈时刻");
    }

    /// 阈值边界：**等于**阈值不告警（`>`，不是 `>=`）。
    #[tokio::test]
    async fn disk_exactly_at_threshold_does_not_alert() {
        let mut collector = collector_with(cfg(), metrics(1.0, 1, 100, &[("/", 85.0, 100.0)]));
        let outcome = collector.collect(&ctx_at(0)).await;
        assert!(
            outcome.alerts.is_empty(),
            "正好 85.0% 等于阈值不该告警: {:?}",
            outcome.alerts
        );

        let mut collector = collector_with(cfg(), metrics(1.0, 1, 100, &[("/", 85.01, 100.0)]));
        let outcome = collector.collect(&ctx_at(0)).await;
        assert_eq!(outcome.alerts.len(), 1, "略高于阈值必须告警");
    }

    /// **SPEC §13.2 条件型契约（磁盘）**：超阈期间**每一轮**都产出该告警，
    /// `id` / `title` / `first_seen` 全部逐轮稳定；回落 → **下一轮起不含**。
    ///
    /// 老实现（`disk_throttle_sec` 节流 + `:recovered`）在本测试的时间线上会得到
    /// 「t=0 报一次 → t=3600 缺席 → t=21601 再报」——正是那个假恢复窗口。
    #[tokio::test]
    async fn disk_reports_every_round_while_over_threshold() {
        let mut collector = collector_with(cfg(), metrics(1.0, 1, 100, &[("/", 95.0, 100.0)]));

        let mut stable_title: Option<String> = None;
        // 连续 5 轮（跨越 6h 的 disk_throttle_sec 窗口，证明它不再抑制产出）
        for secs in [0_i64, 1, 3_600, 21_601, 100_000] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert_eq!(
                outcome.alerts.len(),
                1,
                "t={secs}s 仍超阈，必须每轮都产出（缺席 = relay 眼中的假恢复）: {:?}",
                outcome.alerts
            );
            let alert = &outcome.alerts[0];
            assert_eq!(alert.id, "threshold:disk:/", "t={secs}s id 必须稳定");
            assert_eq!(
                alert.first_seen,
                base_time(),
                "t={secs}s first_seen 必须保持首次超阈时刻"
            );
            match &stable_title {
                None => stable_title = Some(alert.title.clone()),
                Some(expected) => assert_eq!(
                    &alert.title, expected,
                    "t={secs}s title 必须逐字稳定（relay 按 (source,title) 去重与判恢复）"
                ),
            }
        }

        // 回落 → 下一轮起不含
        write_metrics(
            &collector.shared,
            metrics(1.0, 1, 100, &[("/", 40.0, 100.0)]),
        );
        let dropped = collector.collect(&ctx_at(100_001)).await;
        assert!(
            dropped.alerts.is_empty(),
            "回落后**不产出任何告警**（尤其不能有 *:recovered）: {:?}",
            dropped.alerts
        );
        let still_gone = collector.collect(&ctx_at(100_002)).await;
        assert!(still_gone.alerts.is_empty(), "{:?}", still_gone.alerts);

        // 再次超阈 → 新一轮条件，first_seen 重新计
        write_metrics(
            &collector.shared,
            metrics(1.0, 1, 100, &[("/", 96.0, 100.0)]),
        );
        let again = collector.collect(&ctx_at(100_003)).await;
        assert_eq!(again.alerts.len(), 1, "再次超阈必须重新产出");
        assert_ne!(
            again.alerts[0].first_seen,
            base_time(),
            "新一次超阈的 first_seen 必须是新的起点"
        );
    }

    /// 时钟跨越 `disk_throttle_sec` 也不影响节奏（配置字段保留但已不参与产出判定）。
    #[tokio::test]
    async fn disk_throttle_sec_no_longer_suppresses_output() {
        let mut cfg = cfg();
        cfg.disk_throttle_sec = 21_600;
        let mut collector = collector_with(cfg, metrics(1.0, 1, 100, &[("/", 95.0, 100.0)]));

        // 1h（远小于 6h 节流窗口）之后仍然必须产出
        assert_eq!(collector.collect(&ctx_at(0)).await.alerts.len(), 1);
        assert_eq!(
            collector.collect(&ctx_at(3_600)).await.alerts.len(),
            1,
            "1h < disk_throttle_sec(6h)：老实现会在这里静默，条件型必须继续上报"
        );
    }

    /// 没有告警过的低使用率是**完全静默**的（不该出现「恢复」噪音）。
    #[tokio::test]
    async fn disk_below_threshold_from_the_start_is_silent() {
        let mut collector = collector_with(cfg(), metrics(1.0, 1, 100, &[("/", 10.0, 100.0)]));
        for round in 0..3 {
            let outcome = collector.collect(&ctx_at(round)).await;
            assert!(outcome.alerts.is_empty(), "{:?}", outcome.alerts);
        }
    }

    /// `total_gb = 0`（sys_metrics 尚未采到挂载点）不能除零 panic。
    #[tokio::test]
    async fn disk_with_zero_total_is_ignored() {
        let mut collector = collector_with(cfg(), metrics(1.0, 1, 100, &[("/", 0.0, 0.0)]));
        let outcome = collector.collect(&ctx_at(0)).await;
        assert!(
            outcome.alerts.is_empty(),
            "总量为 0 必须跳过而不是告警/panic"
        );
    }

    // ---------------- CPU / 内存持续性 ----------------

    /// **核心**：不足 `sustained_sec` 不报，达到才报 —— 用注入时间验证。
    #[tokio::test]
    async fn cpu_alerts_only_after_sustained_window() {
        let mut collector = collector_with(
            cfg(),
            metrics(99.0, 10, 100, &[]), // CPU 99% 超阈，内存 10% 不超
        );

        // t=0：第一次超阈，只开始计时
        let first = collector.collect(&ctx_at(0)).await;
        assert!(
            first.alerts.is_empty(),
            "刚超阈不该告警（还要持续 300s）: {:?}",
            first.alerts
        );

        // t=299：还差 1 秒
        let second = collector.collect(&ctx_at(299)).await;
        assert!(
            second.alerts.is_empty(),
            "299s < sustained_sec 300s，不该告警: {:?}",
            second.alerts
        );

        // 未满窗口的中间几轮同样必须静默（持续性判定本身不能被「每轮都报」冲掉）
        for secs in [30_i64, 120, 250] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert!(
                outcome.alerts.is_empty(),
                "t={secs}s 仍未满 sustained_sec，不该产出: {:?}",
                outcome.alerts
            );
        }

        // t=300：达标
        let third = collector.collect(&ctx_at(300)).await;
        assert_eq!(third.alerts.len(), 1, "持续 300s 必须告警");
        let alert = &third.alerts[0];
        assert_eq!(alert.id, "threshold:cpu");
        assert_eq!(alert.level, AlertLevel::Warning);
        assert_eq!(alert.source, "thresholds");
        assert!(alert.title.contains("CPU"), "{}", alert.title);
        assert_eq!(
            alert.first_seen,
            base_time(),
            "first_seen 必须是**开始超阈**的时刻，不是告警时刻"
        );
    }

    /// 中途回落必须**清零计时**：99% → 10% → 99% 累计不到 300s 就不该报。
    #[tokio::test]
    async fn cpu_timer_resets_when_metric_drops_below_threshold() {
        let mut collector = collector_with(cfg(), metrics(99.0, 10, 100, &[]));
        assert!(collector.collect(&ctx_at(0)).await.alerts.is_empty());

        // 回落
        write_metrics(&collector.shared, metrics(10.0, 10, 100, &[]));
        let dropped = collector.collect(&ctx_at(200)).await;
        assert!(
            dropped.alerts.is_empty(),
            "回落时从未告警过 → 静默（不该有恢复噪音）: {:?}",
            dropped.alerts
        );

        // 再次超阈：计时从 t=200 重新开始
        write_metrics(&collector.shared, metrics(99.0, 10, 100, &[]));
        assert!(collector.collect(&ctx_at(200)).await.alerts.is_empty());
        let almost = collector.collect(&ctx_at(499)).await;
        assert!(
            almost.alerts.is_empty(),
            "499-200 = 299s < 300s，不该告警: {:?}",
            almost.alerts
        );
        let reached = collector.collect(&ctx_at(500)).await;
        assert_eq!(reached.alerts.len(), 1, "500-200 = 300s，必须告警");
        assert_eq!(reached.alerts[0].id, "threshold:cpu");
    }

    /// **SPEC §13.2 条件型契约（CPU）**：越过 `sustained_sec` 后**连续 5 轮**都必须含该告警，
    /// `id` / `title` / `first_seen` 逐轮稳定；回落后**下一轮起不含**，且从不产出 `:recovered`。
    #[tokio::test]
    async fn cpu_reports_every_round_after_sustained_window() {
        let mut collector = collector_with(cfg(), metrics(99.0, 10, 100, &[]));

        // 未满 sustained_sec（300s）：不产出
        for secs in [0_i64, 1, 150, 299] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert!(
                outcome.alerts.is_empty(),
                "t={secs}s 未满 sustained_sec 300s，不该产出（防瞬时峰值误报）: {:?}",
                outcome.alerts
            );
        }

        // 达标后连续 5 轮：**每一轮都含**
        let mut stable_title: Option<String> = None;
        for secs in [300_i64, 301, 302, 303, 304] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert_eq!(
                outcome.alerts.len(),
                1,
                "t={secs}s 已持续超阈，必须每轮都产出（缺席 = relay 眼中的假恢复）: {:?}",
                outcome.alerts
            );
            let alert = &outcome.alerts[0];
            assert_eq!(alert.id, "threshold:cpu", "t={secs}s id 必须稳定");
            assert_eq!(alert.level, AlertLevel::Warning);
            assert_eq!(alert.source, "thresholds");
            assert_eq!(
                alert.first_seen,
                base_time(),
                "t={secs}s first_seen 必须保持首次超阈时刻（不是告警时刻）"
            );
            match &stable_title {
                None => stable_title = Some(alert.title.clone()),
                Some(expected) => assert_eq!(
                    &alert.title, expected,
                    "t={secs}s title 必须逐字稳定（relay 按 (source,title) 判恢复）"
                ),
            }
        }
        assert!(stable_title.unwrap().contains("CPU"), "title 要点出指标名");

        // 回落到阈值以下 → **下一轮起不含**
        write_metrics(&collector.shared, metrics(20.0, 10, 100, &[]));
        let dropped = collector.collect(&ctx_at(305)).await;
        assert!(
            dropped.alerts.is_empty(),
            "回落后不产出任何告警（尤其不能有 *:recovered）: {:?}",
            dropped.alerts
        );
        let still_gone = collector.collect(&ctx_at(306)).await;
        assert!(still_gone.alerts.is_empty(), "{:?}", still_gone.alerts);
    }

    /// 老实现「每 sustained_sec 才报一次」的节奏必须消失：连续超阈期间**不留任何空档**。
    ///
    /// 这是假恢复窗口的直接判据：时间线 t=0,150,...,900（sustained_sec=300）下，
    /// 达标之后**每一轮**都必须有告警，一次都不能少。
    #[tokio::test]
    async fn cpu_has_no_gap_after_the_sustained_window_opens() {
        let mut collector = collector_with(cfg(), metrics(99.0, 10, 100, &[]));

        let mut reported_rounds = 0_usize;
        for secs in [0_i64, 150, 299, 300, 450, 599, 600, 750, 900] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            if secs >= 300 {
                assert_eq!(
                    outcome.alerts.len(),
                    1,
                    "t={secs}s 必须产出（老实现在 599/750 这类时刻是空的 → 假恢复）: {:?}",
                    outcome.alerts
                );
                reported_rounds += 1;
            } else {
                assert!(outcome.alerts.is_empty(), "t={secs}s 不该产出");
            }
        }
        assert_eq!(reported_rounds, 6, "达标后的 6 轮一轮都不能漏");
    }

    /// 阈值边界：等于阈值不算超（`>`）。
    #[tokio::test]
    async fn cpu_exactly_at_threshold_is_not_over() {
        let mut collector = collector_with(cfg(), metrics(95.0, 10, 100, &[]));
        assert!(collector.collect(&ctx_at(0)).await.alerts.is_empty());
        let later = collector.collect(&ctx_at(1_000)).await;
        assert!(
            later.alerts.is_empty(),
            "正好等于 cpu_percent 不该被判定为超阈: {:?}",
            later.alerts
        );
    }

    /// 内存：从 `mem_used_mb / mem_total_mb` 现算百分比。
    #[tokio::test]
    async fn mem_uses_computed_percent_and_respects_sustained() {
        // 9_500 / 10_000 = 95% > 90%
        let mut collector = collector_with(cfg(), metrics(1.0, 9_500, 10_000, &[]));
        assert!(collector.collect(&ctx_at(0)).await.alerts.is_empty());
        assert!(collector.collect(&ctx_at(299)).await.alerts.is_empty());

        let fired = collector.collect(&ctx_at(300)).await;
        assert_eq!(fired.alerts.len(), 1, "{:?}", fired.alerts);
        assert_eq!(fired.alerts[0].id, "threshold:mem");
        assert!(
            fired.alerts[0].title.contains("内存"),
            "{}",
            fired.alerts[0].title
        );
        assert!(
            fired.alerts[0].summary.contains("95.0"),
            "summary 必须含真实百分比: {}",
            fired.alerts[0].summary
        );

        // 仍持续超阈 → 下一轮照样产出（条件型）
        let again = collector.collect(&ctx_at(301)).await;
        assert_eq!(again.alerts.len(), 1, "持续超阈必须每轮都产出");
        assert_eq!(again.alerts[0].id, "threshold:mem");
    }

    /// **回归护栏（SPEC §13.2）**：本采集器**绝不**产出「已恢复」类告警 ——
    /// 恢复通知由 relay 依据「告警从 `alerts[]` 中消失」推导。
    /// 谁要是把 `threshold:*:recovered` 加回来，这条测试必须红。
    #[tokio::test]
    async fn thresholds_never_emit_recovered_alerts() {
        let mut collector =
            collector_with(cfg(), metrics(99.0, 9_500, 10_000, &[("/", 95.0, 100.0)]));

        let mut all_ids: Vec<String> = Vec::new();
        let mut all_titles: Vec<String> = Vec::new();

        // 完整生命周期：超阈（含未达 sustained 的几轮）→ 持续 5 轮 → 回落 → 回落后再看 3 轮
        for secs in [0_i64, 100, 300, 301, 302, 303, 304] {
            for alert in collector.collect(&ctx_at(secs)).await.alerts {
                all_ids.push(alert.id);
                all_titles.push(alert.title);
            }
        }
        write_metrics(
            &collector.shared,
            metrics(10.0, 1_000, 10_000, &[("/", 10.0, 100.0)]),
        );
        for secs in [305_i64, 306, 307] {
            for alert in collector.collect(&ctx_at(secs)).await.alerts {
                all_ids.push(alert.id);
                all_titles.push(alert.title);
            }
        }

        assert!(
            all_ids.iter().any(|id| id == "threshold:cpu"),
            "测试本身必须真的跑出过告警，否则护栏是恒真的: {all_ids:?}"
        );
        assert!(
            all_ids.iter().any(|id| id == "threshold:disk:/"),
            "磁盘告警也必须出现过: {all_ids:?}"
        );
        for id in &all_ids {
            assert!(
                !id.contains("recovered"),
                "agent 侧不得产出恢复告警（SPEC §13.2），实际 id = {id:?}（全部: {all_ids:?}）"
            );
        }
        for title in &all_titles {
            assert!(
                !title.contains("恢复"),
                "agent 侧不得产出恢复告警（SPEC §13.2），实际 title = {title:?}"
            );
        }
    }

    /// `mem_total_mb = 0`（sys_metrics 还没跑过）→ 跳过而不是误报 100%。
    #[tokio::test]
    async fn zero_mem_total_skips_memory_check() {
        let mut collector = collector_with(cfg(), metrics(1.0, 0, 0, &[]));
        for secs in [0, 1_000, 10_000] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert!(
                outcome.alerts.is_empty(),
                "mem_total_mb = 0 时不能误报内存告警: {:?}",
                outcome.alerts
            );
        }
    }

    // ---------------- 组合 ----------------

    #[tokio::test]
    async fn disk_cpu_mem_are_evaluated_independently() {
        let mut cfg = cfg();
        cfg.sustained_sec = 10;
        let mut collector =
            collector_with(cfg, metrics(99.0, 9_500, 10_000, &[("/", 95.0, 100.0)]));

        let first = collector.collect(&ctx_at(0)).await;
        assert_eq!(
            first.alerts.len(),
            1,
            "只有磁盘是立即告警: {:?}",
            first.alerts
        );
        assert_eq!(first.alerts[0].id, "threshold:disk:/");

        let second = collector.collect(&ctx_at(10)).await;
        let ids: Vec<&str> = second.alerts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["threshold:disk:/", "threshold:cpu", "threshold:mem"],
            "磁盘每轮都报；CPU/内存达标后各报一条，顺序稳定: {ids:?}"
        );
    }

    /// 采集器不产出 `services` / `git`（那是别的采集器的通道）。
    #[tokio::test]
    async fn thresholds_never_touch_services_or_git() {
        let mut collector =
            collector_with(cfg(), metrics(99.0, 9_500, 10_000, &[("/", 99.0, 100.0)]));
        let outcome = collector.collect(&ctx_at(0)).await;
        assert!(outcome.services.is_empty());
        assert!(outcome.git.is_empty());
    }

    /// 零指标 + 正常配置：一条告警都不该有（防止「0/0 被当 100%」这类假阳性）。
    #[tokio::test]
    async fn zero_metrics_produce_no_alerts() {
        let mut collector = ThresholdsCollector::new(&cfg(), new_shared_metrics());
        assert_eq!(collector.metrics(), zero_metrics());
        for secs in [0, 600, 100_000] {
            let outcome = collector.collect(&ctx_at(secs)).await;
            assert!(
                outcome.alerts.is_empty(),
                "全零指标不该产告警（第 {secs}s）: {:?}",
                outcome.alerts
            );
        }
    }

    /// 时钟回拨：不 panic，也不产生「负持续时长」的告警。
    #[tokio::test]
    async fn clock_going_backwards_is_tolerated() {
        let mut collector = collector_with(cfg(), metrics(99.0, 10, 100, &[]));
        assert!(collector.collect(&ctx_at(0)).await.alerts.is_empty());
        let back = collector.collect(&ctx_at(-5_000)).await;
        assert!(
            back.alerts.is_empty(),
            "时钟回拨不该导致告警: {:?}",
            back.alerts
        );
    }

    #[test]
    fn collector_name_is_thresholds() {
        let collector = ThresholdsCollector::new(&cfg(), new_shared_metrics());
        assert_eq!(Collector::name(&collector), "thresholds");
    }

    #[test]
    fn disk_percent_of_guards_against_bad_input() {
        assert_eq!(
            ThresholdsCollector::disk_percent_of(50.0, 100.0),
            Some(50.0)
        );
        assert_eq!(ThresholdsCollector::disk_percent_of(0.0, 100.0), Some(0.0));
        assert_eq!(ThresholdsCollector::disk_percent_of(1.0, 0.0), None);
        assert_eq!(ThresholdsCollector::disk_percent_of(f64::NAN, 100.0), None);
        assert_eq!(
            ThresholdsCollector::disk_percent_of(1.0, f64::INFINITY),
            None
        );
    }

    /// 读的必须是**共享句柄里的当前值**，而不是构造时快照（`sys_metrics` 先跑的前提）。
    #[tokio::test]
    async fn reads_latest_metrics_from_shared_handle() {
        let shared = new_shared_metrics();
        write_metrics(&shared, metrics(10.0, 1, 100, &[]));
        let mut collector = ThresholdsCollector::new(&cfg(), shared.clone());

        // 第 1 轮：CPU 10%，正常
        assert!(collector.collect(&ctx_at(0)).await.alerts.is_empty());

        // 第 2 轮开始前，别的采集器（sys_metrics）写入新指标
        write_metrics(&shared, metrics(99.0, 1, 100, &[]));
        assert_eq!(collector.metrics().cpu_percent, 99.0);
        // t=0 那一轮 CPU 正常 → 超阈区间从 t=300 这一轮才开始，此时不该告警
        assert!(
            collector.collect(&ctx_at(300)).await.alerts.is_empty(),
            "超阈计时尚未满 sustained_sec"
        );

        // t=600：300s（t=300 起算）已满 → 必须用共享句柄里的新值判定并告警
        let outcome = collector.collect(&ctx_at(600)).await;
        assert_eq!(outcome.alerts.len(), 1, "必须用共享句柄里的新值判定");
        assert_eq!(outcome.alerts[0].id, "threshold:cpu");
    }
}
