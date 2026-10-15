//! Bark 推送（SPEC §7 `alerter.rs` / docs/04 §1 §3 §5）。
//!
//! - 请求形态：`POST <endpoint>/<key>/<title>/<body>?level=<level>&group=<group>`
//! - 级别映射：`critical` → `level=critical&volume=8&call=1`；
//!   `warning` → `level=timeSensitive`；`info` → `level=active`
//! - 失败重试：指数退避 3 次（1s / 4s / 16s），仍失败写 relay 日志
//! - 全局限流：同一 host 每分钟最多 `rate_limit_per_min` 条，超出**合并为一条摘要**
//! - 去重：`(source, title)` active 集合，恢复前不重复推送（集合落在 `store::state`）
//!
//! 推送目标就是配置里的 `bark.endpoint`：生产是 `https://api.day.app`，
//! e2e 是本地 mock `http://127.0.0.1:9200`。

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ssm_common::prelude::*;

use crate::net::PushTransport;

/// 一次推送的失败记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushFailure {
    pub title: String,
    pub attempts: usize,
    pub reason: String,
}

/// 一次推送的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushReport {
    /// 实际发出的 HTTP 请求次数（含重试）。
    pub requests: usize,
    /// 成功送达的推送条数。
    pub delivered: usize,
    /// 被限流并暂存、等待合并进摘要的条数。
    pub deferred: usize,
    /// 合并摘要的发送次数。
    pub summaries: usize,
    pub failures: Vec<PushFailure>,
}

impl PushReport {
    pub fn merge(&mut self, other: PushReport) {
        self.requests += other.requests;
        self.delivered += other.delivered;
        self.deferred += other.deferred;
        self.summaries += other.summaries;
        self.failures.extend(other.failures);
    }
}

/// 限流窗口长度（docs/04 §5：每分钟）。
pub const RATE_WINDOW_SECS: i64 = 60;

/// 退避序列（秒）：1s / 4s / 16s，共 3 次尝试。
pub const BACKOFF_SECS: [u64; 3] = [1, 4, 16];

/// 一次推送请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest<'a> {
    pub host_id: &'a str,
    pub level: AlertLevel,
    pub source: &'a str,
    pub title: &'a str,
    /// 仅用于日志（Bark 的正文由 `source:title` 组成）。
    pub summary: &'a str,
}

/// 退避等待函数。
///
/// 生产用 `tokio::time::sleep`；单测注入「记录型」实现，从而断言退避序列
/// 而**不需要真的睡 1s/4s/16s**（`cargo test` 不会卡 21 秒）。
pub type SleepFn = Arc<
    dyn Fn(u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// 默认退避：真的等。
pub fn real_sleep() -> SleepFn {
    Arc::new(|secs: u64| Box::pin(tokio::time::sleep(Duration::from_secs(secs))))
}

/// 告警推送器。
pub struct Alerter {
    endpoint: String,
    key: String,
    group: String,
    transport: Arc<dyn PushTransport>,
    sleep: SleepFn,
    /// 每个 host 的推送时间戳（滚动 60s 窗口）。
    recent: std::collections::HashMap<String, Vec<DateTime<Utc>>>,
    /// 被限流暂存、等待合并为摘要的推送。
    pending: Vec<PendingPush>,
}

#[derive(Debug, Clone)]
struct PendingPush {
    host_id: String,
    level: AlertLevel,
    source: String,
    title: String,
}

impl std::fmt::Debug for Alerter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alerter")
            .field("endpoint", &self.endpoint)
            .field("group", &self.group)
            .field("key_configured", &!self.key.is_empty())
            .finish()
    }
}

impl Alerter {
    pub fn new(bark: &BarkConfig, transport: Arc<dyn PushTransport>) -> Self {
        Self::with_sleep(bark, transport, real_sleep())
    }

    /// 注入自定义退避函数（单测用）。
    pub fn with_sleep(
        bark: &BarkConfig,
        transport: Arc<dyn PushTransport>,
        sleep: SleepFn,
    ) -> Self {
        Self {
            endpoint: bark.endpoint.trim_end_matches('/').to_string(),
            key: bark.key.clone(),
            group: bark.group.clone(),
            transport,
            sleep,
            recent: std::collections::HashMap::new(),
            pending: Vec::new(),
        }
    }

    /// 是否配置了 Bark key；未配置时只打日志、不发请求。
    pub fn is_configured(&self) -> bool {
        !self.key.is_empty()
    }

    /// 待合并的条数（测试断言/日志用）。
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 推送一条告警；超出限流时暂存并合并为摘要。
    pub async fn dispatch(
        &mut self,
        req: PushRequest<'_>,
        now: DateTime<Utc>,
        limit_per_min: u32,
    ) -> PushReport {
        let PushRequest {
            host_id,
            level,
            source,
            title,
            summary,
        } = req;
        let mut report = PushReport::default();

        if !self.is_configured() {
            tracing::warn!("未配置 bark.key，跳过推送（告警仍已入库）");
            return report;
        }

        if self.slots_used(host_id, now) >= limit_per_min as usize {
            tracing::warn!(
                host = host_id,
                limit = limit_per_min,
                title,
                "Bark 推送超出每分钟限流，合并入摘要（docs/04 §5）"
            );
            self.deferred(host_id, level, source, title);
            report.deferred += 1;
            // 攒下的摘要趁这一刻尝试补发（若窗口仍然满，就留到下一次）。
            let flush = self.flush_pending(now, limit_per_min).await;
            report.merge(flush);
            return report;
        }

        let body = format!("{source}: {summary}");
        let r = self.push(level, title, &body, now).await;
        report.merge(r);
        // 成功的推送才占用限流额度（失败的会被退避重试，不该再挤占配额）。
        if report.delivered > 0 {
            self.recent
                .entry(host_id.to_string())
                .or_default()
                .push(now);
        }
        report
    }

    /// 尝试把攒下的限流告警合并成一条摘要发出去。
    pub async fn flush_pending(&mut self, now: DateTime<Utc>, limit_per_min: u32) -> PushReport {
        let mut report = PushReport::default();
        if self.pending.is_empty() || !self.is_configured() {
            return report;
        }
        // 摘要是「整批」的，算在第一条待发告警所属的 host 上。
        let host_id = self.pending[0].host_id.clone();
        if self.slots_used(&host_id, now) >= limit_per_min as usize {
            return report;
        }

        let count = self.pending.len();
        let level = self
            .pending
            .iter()
            .map(|p| p.level)
            .max_by_key(|l| match l {
                AlertLevel::Critical => 3,
                AlertLevel::Warning => 2,
                AlertLevel::Info => 1,
            })
            .unwrap_or(AlertLevel::Info);
        let mut titles: Vec<String> = Vec::new();
        for p in &self.pending {
            let t = format!("{}：{}", p.source, p.title);
            if !titles.contains(&t) {
                titles.push(t);
            }
        }
        let title = format!("{host_id} 告警合并（{count} 条）");
        let body = truncate_chars(&titles.join("；"), 300);
        let taken = std::mem::take(&mut self.pending);

        let r = self.push(level, &title, &body, now).await;
        report.merge(r);
        report.summaries += 1;
        if !report.failures.is_empty() {
            // 推送失败：把内容放回队列，下个 tick 再试（避免丢掉告警）。
            let mut taken = taken;
            taken.extend(std::mem::take(&mut self.pending));
            self.pending = taken;
        } else {
            self.recent.entry(host_id).or_default().push(now);
        }
        report
    }

    fn deferred(&mut self, host_id: &str, level: AlertLevel, source: &str, title: &str) {
        self.pending.push(PendingPush {
            host_id: host_id.to_string(),
            level,
            source: source.to_string(),
            title: title.to_string(),
        });
    }

    fn slots_used(&mut self, host_id: &str, now: DateTime<Utc>) -> usize {
        let cutoff = now - chrono::Duration::seconds(RATE_WINDOW_SECS);
        // 用 `get_mut` 而不是 `entry().or_default()`：**不能**因为查一次限流
        // 就给这台 host 建出一条空记录（否则「失败的推送不占额度」这类断言会被污染）。
        let Some(entry) = self.recent.get_mut(host_id) else {
            return 0;
        };
        entry.retain(|t| *t > cutoff);
        entry.len()
    }

    /// 单条推送 + 指数退避重试。
    async fn push(
        &self,
        level: AlertLevel,
        title: &str,
        body: &str,
        _now: DateTime<Utc>,
    ) -> PushReport {
        let mut report = PushReport::default();
        if !self.is_configured() {
            return report;
        }
        let url = bark_url(&self.endpoint, &self.key, &self.group, title, body, level);
        let mut last_err = String::new();
        // 尝试次数 = 退避表长度（1s / 4s / 16s 三次），第 1 次不等待。
        let attempts = BACKOFF_SECS.len() + 1;

        for attempt in 0..attempts {
            if attempt > 0 {
                // 第 1→2 次之间等 1s，第 2→3 次之间等 4s（进入等待前把 16s 也算进去，
                // 这样「最终失败」的日志里也能体现完整退避预算）。
                let delay = backoff_delay(attempt);
                tracing::debug!(attempt, delay_secs = delay, "Bark 推送退避等待");
                (self.sleep)(delay).await;
            }
            report.requests += 1;
            match self.transport.post(&url).await {
                Ok(()) => {
                    report.delivered += 1;
                    return report;
                }
                Err(e) => {
                    last_err = e.describe();
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = attempts,
                        error = %last_err,
                        "Bark 推送失败，准备重试"
                    );
                }
            }
        }

        tracing::error!(title, error = %last_err, "Bark 推送最终失败（已指数退避 3 次）");
        report.failures.push(PushFailure {
            title: title.to_string(),
            attempts: report.requests,
            reason: last_err,
        });
        report
    }
}

/// 第 `attempt` 次尝试（0 基）之前应等待的秒数。
///
/// - `attempt = 0` → `0`（首次立刻发）
/// - `attempt = 1` → `1`
/// - `attempt = 2` → `4`
/// - `attempt = 3` → `16`
pub fn backoff_delay(attempt: usize) -> u64 {
    match attempt {
        0 => 0,
        n => BACKOFF_SECS.get(n - 1).copied().unwrap_or(0),
    }
}

/// 按 docs/04 §1 构造 Bark 请求 URL。
pub fn bark_url(
    endpoint: &str,
    key: &str,
    group: &str,
    title: &str,
    body: &str,
    level: AlertLevel,
) -> String {
    let mut url = format!(
        "{}/{}/{}/{}?{}",
        endpoint.trim_end_matches('/'),
        encode_path_segment(key),
        encode_path_segment(title),
        encode_path_segment(body),
        level_query(level, group)
    );
    if level == AlertLevel::Critical {
        // critical：穿透静音 + 强提醒
        url.push_str("&volume=8&call=1");
    }
    url
}

/// 级别 → Bark 查询参数（docs/04 §1）。
pub fn level_query(level: AlertLevel, group: &str) -> String {
    let level = match level {
        AlertLevel::Critical => "critical",
        AlertLevel::Warning => "timeSensitive",
        AlertLevel::Info => "active",
    };
    format!("level={level}&group={}", encode_query_component(group))
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";

fn hex_escape(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0f) as usize] as char);
}

/// path segment 编码：`/` 会截断路径、`?`/`#` 会截断 URL，统一换成 `-`；
/// 其余非 unreserved 字符 percent-encode（UTF-8 逐字节，中文不会破坏 URL）。
pub fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b == b'/' || b == b'?' || b == b'#' {
            out.push('-');
        } else if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            hex_escape(&mut out, b);
        }
    }
    out
}

/// query value 编码：严格 percent-encode（`/` 也要编掉，避免被当成路径分隔符）。
pub fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            hex_escape(&mut out, b);
        }
    }
    out
}

/// 按**字符**（不是字节）截断，避免切断 UTF-8。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::FakePush;
    use crate::test_util::t;

    fn bark() -> BarkConfig {
        BarkConfig {
            endpoint: "http://127.0.0.1:9200".to_string(),
            key: "e2ekey".to_string(),
            group: "server_status".to_string(),
        }
    }

    fn alerter(transport: Arc<FakePush>) -> Alerter {
        Alerter::new(&bark(), transport)
    }

    /// 记录型退避：不真的睡，只把「被要求等多少秒」记下来。
    #[derive(Clone, Default)]
    struct RecordingSleep(Arc<std::sync::Mutex<Vec<u64>>>);

    impl RecordingSleep {
        fn delays(&self) -> Vec<u64> {
            self.0.lock().unwrap().clone()
        }

        fn as_fn(&self) -> SleepFn {
            let store = self.0.clone();
            Arc::new(move |secs: u64| {
                store.lock().unwrap().push(secs);
                Box::pin(std::future::ready(()))
            })
        }
    }

    fn alerter_with(transport: Arc<dyn PushTransport>, sleep: &RecordingSleep) -> Alerter {
        Alerter::with_sleep(&bark(), transport, sleep.as_fn())
    }

    async fn push_one(a: &mut Alerter, title: &str, level: AlertLevel, at: &str) -> PushReport {
        a.dispatch(
            PushRequest {
                host_id: "e2e-host",
                level,
                source: "relay",
                title,
                summary: "summary",
            },
            t(at),
            5,
        )
        .await
    }

    // ---------------- URL 构造与级别映射 ----------------

    #[test]
    fn bark_url_shape_matches_docs() {
        let url = bark_url(
            "http://127.0.0.1:9200",
            "e2ekey",
            "server_status",
            "服务器离线",
            "relay: e2e-host 最后心跳",
            AlertLevel::Critical,
        );
        assert!(url.starts_with("http://127.0.0.1:9200/e2ekey/"));
        assert!(url.contains("?level=critical&group=server_status"));
        assert!(
            url.ends_with("&volume=8&call=1"),
            "critical 必须带 volume/call: {url}"
        );
        assert!(url.contains("%E6%9C%8D%E5%8A%A1%E5%99%A8%E7%A6%BB%E7%BA%BF"));
    }

    #[test]
    fn bark_url_level_mapping_for_all_three_levels() {
        for (level, expect) in [
            (
                AlertLevel::Critical,
                "level=critical&group=server_status&volume=8&call=1",
            ),
            (
                AlertLevel::Warning,
                "level=timeSensitive&group=server_status",
            ),
            (AlertLevel::Info, "level=active&group=server_status"),
        ] {
            let url = bark_url("https://api.day.app", "k", "server_status", "t", "b", level);
            assert!(url.ends_with(expect), "{level:?} → {url}");
        }
        // 只有 critical 带强提醒参数
        assert!(!bark_url("e", "k", "g", "t", "b", AlertLevel::Warning).contains("volume="));
        assert!(!bark_url("e", "k", "g", "t", "b", AlertLevel::Info).contains("call=1"));
    }

    #[test]
    fn bark_url_percent_encodes_path_and_query() {
        let url = bark_url(
            "http://127.0.0.1:9200/",
            "key",
            "server status",
            "a/b?c",
            "x y",
            AlertLevel::Info,
        );
        // endpoint 末尾斜杠不留双斜杠
        assert!(url.starts_with("http://127.0.0.1:9200/key/"));
        // 标题里的 / 与 ? 不得截断 URL
        assert!(url.contains("/a-b-c/"), "实际: {url}");
        assert!(url.contains("/x%20y?"), "实际: {url}");
        assert!(url.contains("group=server%20status"));
        assert_eq!(url.matches('?').count(), 1, "只允许一个 query 起始符");
    }

    #[test]
    fn encode_helpers_cover_utf8_and_reserved() {
        assert_eq!(encode_path_segment("abc-_.~"), "abc-_.~");
        assert_eq!(
            encode_path_segment("服务器离线"),
            "%E6%9C%8D%E5%8A%A1%E5%99%A8%E7%A6%BB%E7%BA%BF"
        );
        assert_eq!(encode_query_component("a/b"), "a%2Fb");
        assert_eq!(encode_query_component("server_status"), "server_status");
        assert_eq!(encode_path_segment("a:b"), "a%3Ab");
    }

    // ---------------- 退避序列 ----------------

    #[test]
    fn backoff_sequence_is_1_4_16() {
        assert_eq!(BACKOFF_SECS, [1, 4, 16], "docs/04 §3 的指数退避序列");
        assert_eq!(backoff_delay(0), 0, "第 1 次尝试立刻发，不等待");
        assert_eq!(backoff_delay(1), 1);
        assert_eq!(backoff_delay(2), 4);
        assert_eq!(backoff_delay(3), 16);
        assert_eq!(backoff_delay(99), 0);
    }

    /// 重试循环 + 假传输 + **记录型退避**：断言尝试次数、失败记录与退避序列。
    #[tokio::test]
    async fn retries_with_1s_4s_16s_backoff_then_records_failure() {
        let fake = Arc::new(FakePush::failing_first(usize::MAX));
        let sleep = RecordingSleep::default();
        let mut a = alerter_with(fake.clone(), &sleep);
        let report = push_one(
            &mut a,
            "服务器离线",
            AlertLevel::Critical,
            "2026-10-15T01:30:00Z",
        )
        .await;

        assert_eq!(fake.call_count(), 4, "首次 + 3 次重试 = 4 次尝试");
        assert_eq!(report.requests, 4);
        assert_eq!(report.delivered, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].attempts, 4);
        assert!(report.failures[0].title.contains("离线"));
        assert_eq!(
            sleep.delays(),
            vec![1, 4, 16],
            "退避必须严格是 1s / 4s / 16s（且没有真的睡）"
        );
        // 失败的推送不占用限流额度
        assert!(a.recent.is_empty());
    }

    #[tokio::test]
    async fn succeeds_on_second_attempt_without_spending_the_rest() {
        let fake = Arc::new(FakePush::failing_first(1));
        let sleep = RecordingSleep::default();
        let mut a = alerter_with(fake.clone(), &sleep);
        let report = push_one(
            &mut a,
            "服务器恢复",
            AlertLevel::Info,
            "2026-10-15T01:30:00Z",
        )
        .await;
        assert_eq!(fake.call_count(), 2);
        assert_eq!(report.delivered, 1);
        assert!(report.failures.is_empty());
        assert_eq!(sleep.delays(), vec![1], "成功即停止，不再退避");
    }

    /// 生产路径用的是真 `sleep`：验证 `real_sleep()` 可用且退避时长正确（不真的等）。
    #[tokio::test(start_paused = true)]
    async fn real_sleep_is_wired_for_production() {
        let fake = Arc::new(FakePush::failing_first(usize::MAX));
        let mut a = Alerter::new(&bark(), fake.clone());
        // 用虚拟时钟跑完整退避（1+4+16 = 21s 会被 tokio 自动跳过）
        let report = push_one(&mut a, "t", AlertLevel::Warning, "2026-10-15T01:30:00Z").await;
        assert_eq!(report.requests, 4);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(fake.call_count(), 4);
    }

    // ---------------- 限流与合并 ----------------

    #[tokio::test(start_paused = true)]
    async fn rate_limit_merges_excess_into_one_summary() {
        let fake = Arc::new(FakePush::new());
        let mut a = alerter(fake.clone());

        // rate_limit_per_min = 5：前 5 条直发
        for i in 1..=5 {
            let r = push_one(
                &mut a,
                &format!("告警{i}"),
                AlertLevel::Warning,
                "2026-10-15T01:30:00Z",
            )
            .await;
            assert_eq!(r.delivered, 1, "第 {i} 条应直发");
            assert_eq!(r.deferred, 0);
        }
        assert_eq!(fake.call_count(), 5);

        // 第 6 条被限流 → 暂存
        let r = push_one(&mut a, "告警6", AlertLevel::Warning, "2026-10-15T01:30:10Z").await;
        assert_eq!(r.deferred, 1);
        assert_eq!(r.delivered, 0);
        assert_eq!(r.summaries, 0, "窗口仍满，摘要要等下一分钟");
        assert_eq!(a.pending_len(), 1);
        assert_eq!(fake.call_count(), 5);

        // 窗口滑过 60s 后 flush → 合并成一条摘要
        let r = a.flush_pending(t("2026-10-15T01:31:11Z"), 5).await;
        assert_eq!(r.summaries, 1);
        assert_eq!(r.delivered, 1);
        assert_eq!(a.pending_len(), 0);

        let urls = fake.urls();
        assert_eq!(urls.len(), 6);
        let last = urls.last().unwrap();
        // 被合并进来的是一条 Warning 告警（"告警6"），摘要的级别取被合并者中最高的
        // → timeSensitive（docs/04 §1 的 warning 映射）。
        assert!(
            last.contains("level=timeSensitive"),
            "摘要级别应取被合并告警中的最高级别: {last}"
        );
        assert!(
            last.contains("/e2e-host%20%E5%91%8A%E8%AD%A6%E5%90%88%E5%B9%B6"),
            "摘要标题应含「<host> 告警合并」: {last}"
        );
        assert!(
            last.contains("%E5%91%8A%E8%AD%A66"),
            "摘要应列出被合并的告警: {last}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn summary_level_escalates_to_highest_deferred_level() {
        let fake = Arc::new(FakePush::new());
        let mut a = alerter(fake.clone());
        for _ in 0..5 {
            push_one(&mut a, "填充", AlertLevel::Info, "2026-10-15T01:30:00Z").await;
        }
        push_one(&mut a, "普通", AlertLevel::Warning, "2026-10-15T01:30:01Z").await;
        push_one(&mut a, "严重", AlertLevel::Critical, "2026-10-15T01:30:02Z").await;
        assert_eq!(a.pending_len(), 2);

        a.flush_pending(t("2026-10-15T01:31:30Z"), 5).await;
        let last = fake.urls().last().unwrap().clone();
        assert!(last.contains("level=critical"), "摘要取最高级别: {last}");
        assert!(last.contains("volume=8&call=1"));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_is_per_host() {
        let fake = Arc::new(FakePush::new());
        let mut a = alerter(fake.clone());
        for i in 1..=5 {
            a.dispatch(
                PushRequest {
                    host_id: "h1",
                    level: AlertLevel::Info,
                    source: "relay",
                    title: &format!("t{i}"),
                    summary: "s",
                },
                t("2026-10-15T01:30:00Z"),
                5,
            )
            .await;
        }
        // h1 满了，h2 不受影响
        let r = a
            .dispatch(
                PushRequest {
                    host_id: "h2",
                    level: AlertLevel::Info,
                    source: "relay",
                    title: "其它主机",
                    summary: "s",
                },
                t("2026-10-15T01:30:01Z"),
                5,
            )
            .await;
        assert_eq!(r.delivered, 1);
        assert_eq!(r.deferred, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_window_expires_after_60s() {
        let fake = Arc::new(FakePush::new());
        let mut a = alerter(fake.clone());
        for _ in 0..5 {
            push_one(&mut a, "x", AlertLevel::Info, "2026-10-15T01:30:00Z").await;
        }
        let r = push_one(&mut a, "y", AlertLevel::Info, "2026-10-15T01:30:30Z").await;
        assert_eq!(r.deferred, 1, "30s 时窗口仍满");

        // 61s 后旧记录过期
        let r = push_one(&mut a, "z", AlertLevel::Info, "2026-10-15T01:31:01Z").await;
        assert_eq!(r.delivered, 1);
        assert_eq!(r.deferred, 0);
    }

    // ---------------- 未配置 key ----------------

    #[tokio::test(start_paused = true)]
    async fn missing_key_skips_push_without_requests() {
        let fake = Arc::new(FakePush::new());
        let cfg = BarkConfig {
            endpoint: "http://127.0.0.1:9200".into(),
            key: String::new(),
            group: "server_status".into(),
        };
        let mut a = Alerter::new(&cfg, fake.clone());
        assert!(!a.is_configured());
        let r = push_one(&mut a, "t", AlertLevel::Critical, "2026-10-15T01:30:00Z").await;
        assert_eq!(fake.call_count(), 0);
        assert_eq!(r.deferred, 0);
        assert_eq!(r.delivered, 0);
    }

    #[test]
    fn truncate_chars_never_splits_utf8() {
        let s = "服务器离线服务器离线服务器离线";
        let cut = truncate_chars(s, 4);
        assert_eq!(cut, "服务器离…");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}
