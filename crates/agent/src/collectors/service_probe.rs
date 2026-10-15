//! 服务存活探针：`http | tcp | process | tailscale`（SPEC §7、docs/04 §2.2、docs/01 F3）。
//!
//! # 行为契约
//!
//! - **每轮全量快照**：无论成功失败，每个探针都产出一条 [`ServiceStatus`] 进 `services[]`。
//!   relay 的 `/status` 依赖这个数组画服务灯，所以**不能只报失败的**。
//! - **连续 `fail_threshold` 次失败才告警**：单周期抖动只累计计数、不产 Alert（docs/04 §2.2）。
//! - **条件型告警（SPEC §13.2，勿再违反）**：探针一旦进入失败态（连续失败数 ≥
//!   `fail_threshold`），**此后每一轮都必须把同一条告警放进 `alerts[]`**，直到探针真的恢复。
//!   relay 把「本轮 `alerts[]` 里没有它」判定为「条件已恢复」，所以只在前沿报一次会造成
//!   **假恢复 + 永久失声**（SPEC §13.1 的 e2e 实测 bug）。重复出现不会造成推送风暴：
//!   relay 的 `(source, title)` active 集合负责去重（SPEC §13.2、docs/04 §5）。
//! - **不产出「已恢复」告警**：恢复通知由 relay 依据「该告警从 `alerts[]` 中消失」推导
//!   （SPEC §13.2）。agent 侧既没有 `service:<name>:recovered`，也不会有任何 `*:recovered`。
//! - **`first_seen` = 首次进入失败态的时刻**：失败态持续期间**保持不变**，不每轮刷新成当前时间。
//! - **运行期错误一律进 `detail`，绝不 panic**：探针失败是常态（服务没起来、命令没装），
//!   不是 agent 的异常。
//!
//! # 去重键（`Alert.id` 与 `(source, title)`）
//!
//! | id | level | 含义 |
//! |---|---|---|
//! | `service:<name>` | critical | 连续失败达阈值（失败态持续期间每轮都上报） |
//!
//! `title`（`【服务不可用】<name> 探测失败`）在失败态持续期间**必须逐字不变**：relay 的
//! 恢复判定用的是 `(source, title)`，标题里塞进逐轮变化的计数会让 relay 每轮都当成新告警
//! 推送、并给旧标题推一条假的「已恢复」。动态内容（连续失败次数、探测 detail）一律进 `summary`。
//!
//! # `process` 探针的匹配语义
//!
//! `target` 是**进程名**（Linux 上即 `/proc/<pid>/stat` 第二列 `comm`），不是命令行、
//! 也不是可执行文件路径：`python3 app.py` 的进程名是 `python3`；内核把 `comm` 截到
//! 15 字节，所以 `nginx: worker process` 这类名字会变成 `nginx`。匹配**大小写不敏感**。

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ssm_common::config::{ProbeConfig, ProbeKind};
use ssm_common::protocol::{Alert, AlertLevel, ServiceKind, ServiceStatus};
use sysinfo::{ProcessesToUpdate, System};

use super::{CollectCtx, CollectOutcome, Collector};

/// 告警来源标识（SPEC §4）。
const SOURCE: &str = "service_probe";

/// 探针单项超时的兜底值（配置写 0 时用）。
const MIN_TIMEOUT_SEC: u64 = 1;

/// HTTP 客户端级超时相对单项探针超时的余量。
const CLIENT_TIMEOUT_HEADROOM_SEC: u64 = 1;

/// 一次探测的原始结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub ok: bool,
    /// 本次探测耗时；`process` / `tailscale` 这类「查状态」型探针不填。
    pub latency_ms: Option<u64>,
    pub detail: String,
}

impl ProbeOutcome {
    fn ok(latency_ms: Option<u64>, detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            latency_ms,
            detail: detail.into(),
        }
    }

    fn fail(latency_ms: Option<u64>, detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            latency_ms,
            detail: detail.into(),
        }
    }
}

/// 一个探针的配置快照 + 连续失败状态机。
#[derive(Debug)]
struct ProbeEntry {
    name: String,
    kind: ServiceKind,
    /// 线协议里原样上报的 `target`（trim 过）。
    target: String,
    /// 规范化后的探测目标（进程名小写等）。
    needle: String,
    fail_threshold: u32,
    timeout: Duration,
    /// 连续失败次数（成功即清零）。
    consecutive_failures: u32,
    /// **首次进入失败态**（连续失败数 ≥ `fail_threshold`）的时刻；失败态持续期间不变，
    /// 既是告警的 `first_seen`，也是「当前是否处于失败态」的判据（`None` = 不在失败态）。
    failing_since: Option<DateTime<Utc>>,
}

/// 服务探针采集器。
pub struct ServiceProbeCollector {
    probes: Vec<ProbeEntry>,
    http: reqwest::Client,
}

impl ServiceProbeCollector {
    /// 从 `agent.toml` 的 `[[probes]]` 构造。
    ///
    /// 不返回 `Result`：`target` 的格式校验已在 `main.rs` 的启动期校验里做过（T2），
    /// 这里遇到不合法的配置只 `warn` 并跳过该探针，绝不拦住 agent 启动。
    pub fn new(probes: &[ProbeConfig]) -> Self {
        let max_timeout_sec = probes
            .iter()
            .map(|p| p.timeout_sec.max(MIN_TIMEOUT_SEC))
            .max()
            .unwrap_or(MIN_TIMEOUT_SEC)
            .saturating_add(CLIENT_TIMEOUT_HEADROOM_SEC);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(max_timeout_sec))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            probes: probes.iter().filter_map(Self::build_entry).collect(),
            http,
        }
    }

    /// 实际生效的探针数量（**仅测试可用**，生产路径不需要）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.probes.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    fn build_entry(cfg: &ProbeConfig) -> Option<ProbeEntry> {
        let target = cfg.target.trim().to_string();
        // 阈值 0 会让状态机永不告警，按 1 处理（语义上等于「一次失败就报」）。
        let fail_threshold = cfg.fail_threshold.max(1);
        let timeout = Duration::from_secs(cfg.timeout_sec.max(MIN_TIMEOUT_SEC));

        let (kind, needle) = match cfg.kind {
            ProbeKind::Http => {
                let lower = target.to_ascii_lowercase();
                if !lower.starts_with("http://") && !lower.starts_with("https://") {
                    tracing::warn!(
                        probe = %cfg.name,
                        target = %target,
                        "http 探针的 target 不是 http:// 或 https:// 开头，已跳过该探针"
                    );
                    return None;
                }
                (ServiceKind::Http, target.clone())
            }
            ProbeKind::Tcp => (ServiceKind::Tcp, target.clone()),
            ProbeKind::Process => (ServiceKind::Process, target.to_ascii_lowercase()),
            ProbeKind::Tailscale => (ServiceKind::Tailscale, target.to_ascii_lowercase()),
        };

        Some(ProbeEntry {
            name: cfg.name.clone(),
            kind,
            target,
            needle,
            fail_threshold,
            timeout,
            consecutive_failures: 0,
            failing_since: None,
        })
    }
}

#[async_trait]
impl Collector for ServiceProbeCollector {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn collect(&mut self, ctx: &CollectCtx) -> CollectOutcome {
        let mut outcome = CollectOutcome::empty();
        let mut failed = 0_usize;

        for entry in &mut self.probes {
            let probe = probe_once(&self.http, entry).await;

            // ---- 每轮全量：成功失败都进 services[]（relay 靠它画服务灯）----
            outcome.services.push(ServiceStatus {
                name: entry.name.clone(),
                kind: entry.kind,
                target: entry.target.clone(),
                ok: probe.ok,
                latency_ms: probe.latency_ms,
                detail: Some(probe.detail.clone()),
            });

            if probe.ok {
                // ---- 条件消失：下一轮起不再出现该告警（**不产出「已恢复」**，SPEC §13.2）----
                if let Some(since) = entry.failing_since.take() {
                    tracing::info!(
                        probe = %entry.name,
                        target = %entry.target,
                        failed_for_sec = ctx
                            .now
                            .signed_duration_since(since)
                            .num_seconds()
                            .max(0),
                        "服务探针已恢复健康，本轮起不再上报该告警（恢复通知由 relay 依据告警缺席推导）"
                    );
                }
                entry.consecutive_failures = 0;
            } else {
                failed += 1;
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);

                if entry.consecutive_failures < entry.fail_threshold {
                    // 抖动：只记录，不告警（docs/04 §2.2）
                    tracing::debug!(
                        probe = %entry.name,
                        consecutive_failures = entry.consecutive_failures,
                        fail_threshold = entry.fail_threshold,
                        detail = %probe.detail,
                        "服务探针失败但未达阈值，仅记录"
                    );
                    continue;
                }

                // ---- 条件型：失败态存续期间**每一轮都上报同一条告警**（SPEC §13.2）----
                // `id` 与 `title` 都逐字稳定，relay 的 `(source, title)` active 集合负责去重；
                // 只有 `summary` 允许带逐轮变化的数字。
                let first_report = entry.failing_since.is_none();
                let first_seen = *entry.failing_since.get_or_insert(ctx.now);

                outcome.alerts.push(Alert {
                    id: format!("service:{}", entry.name),
                    level: AlertLevel::Critical,
                    source: SOURCE.to_string(),
                    title: format!("【服务不可用】{} 探测失败", entry.name),
                    summary: format!(
                        "连续 {} 次探测失败（阈值 {}）：{} → {}",
                        entry.consecutive_failures,
                        entry.fail_threshold,
                        entry.target,
                        probe.detail
                    ),
                    first_seen,
                });

                if first_report {
                    tracing::warn!(
                        probe = %entry.name,
                        target = %entry.target,
                        consecutive_failures = entry.consecutive_failures,
                        fail_threshold = entry.fail_threshold,
                        detail = %probe.detail,
                        "服务探针连续失败达阈值，进入失败态并产出 critical 告警"
                    );
                } else {
                    tracing::info!(
                        probe = %entry.name,
                        target = %entry.target,
                        consecutive_failures = entry.consecutive_failures,
                        round = ctx.round,
                        "服务探针仍处于失败态，本轮继续上报该告警（条件型，每轮必报）"
                    );
                }
            }
        }

        tracing::debug!(
            probes = self.probes.len(),
            failed,
            round = ctx.round,
            "服务探针采集完成"
        );
        outcome
    }
}

// ---------------------------------------------------------------------------
// 各类型探针实现
// ---------------------------------------------------------------------------

/// 执行单个探针（纯函数形式，避免与 `&mut self.probes` 的借用冲突）。
async fn probe_once(http: &reqwest::Client, entry: &ProbeEntry) -> ProbeOutcome {
    match entry.kind {
        ServiceKind::Http => probe_http(http, entry).await,
        ServiceKind::Tcp => probe_tcp(entry).await,
        ServiceKind::Process => probe_process(entry),
        ServiceKind::Tailscale => probe_tailscale(entry).await,
    }
}

async fn probe_http(http: &reqwest::Client, entry: &ProbeEntry) -> ProbeOutcome {
    let started = std::time::Instant::now();
    // SPEC §11：`Request` 上没有 `send()`，必须在 `RequestBuilder` 上 send。
    let sent = http.get(&entry.target).timeout(entry.timeout).send().await;
    let elapsed = started.elapsed();

    match sent {
        Ok(response) => {
            let status = response.status();
            let latency = Some(to_millis(elapsed));
            if status.is_success() {
                ProbeOutcome::ok(
                    latency,
                    format!("{} {}", status.as_u16(), status_text(status)),
                )
            } else {
                ProbeOutcome::fail(
                    latency,
                    format!("HTTP {} {}", status.as_u16(), status_text(status)),
                )
            }
        }
        Err(err) => ProbeOutcome::fail(None, describe_reqwest_error(&err)),
    }
}

/// `host:port` 的 TCP 连通性（支持 IPv6 字面量 `[::1]:80`）。
async fn probe_tcp(entry: &ProbeEntry) -> ProbeOutcome {
    let started = std::time::Instant::now();
    match tokio::time::timeout(entry.timeout, tokio::net::TcpStream::connect(&entry.needle)).await {
        Ok(Ok(stream)) => {
            let latency = Some(to_millis(started.elapsed()));
            let local = stream
                .local_addr()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|_| "?".to_string());
            ProbeOutcome::ok(latency, format!("TCP 连接成功（本地 {local}）"))
        }
        Ok(Err(err)) => ProbeOutcome::fail(
            Some(to_millis(started.elapsed())),
            format!("TCP 连接失败：{}", io_error_text(&err)),
        ),
        Err(_) => ProbeOutcome::fail(
            None,
            format!("TCP 连接超时（{}s）", entry.timeout.as_secs()),
        ),
    }
}

/// 按**进程名**查系统进程表；大小写不敏感。
fn probe_process(entry: &ProbeEntry) -> ProbeOutcome {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);

    let mut matched: Vec<u32> = Vec::new();
    for process in system.processes().values() {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if name == entry.needle {
            matched.push(process.pid().as_u32());
        }
    }
    matched.sort_unstable();

    if matched.is_empty() {
        ProbeOutcome::fail(
            None,
            format!(
                "进程表中没有名为 {:?} 的进程（匹配的是进程名，不是命令行或可执行文件路径）",
                entry.target
            ),
        )
    } else {
        ProbeOutcome::ok(None, format!("进程存在，pid = {}", join_pids(&matched)))
    }
}

/// 执行 `tailscale status --json` 并判定目标节点是否在线。
async fn probe_tailscale(entry: &ProbeEntry) -> ProbeOutcome {
    match run_command("tailscale", &["status", "--json"], entry.timeout).await {
        Err(detail) => ProbeOutcome::fail(None, detail),
        Ok(output) => match serde_json::from_str::<serde_json::Value>(&output) {
            Err(err) => ProbeOutcome::fail(
                None,
                format!("`tailscale status --json` 的输出不是合法 JSON：{err}"),
            ),
            Ok(value) => evaluate_tailscale(&value, &entry.needle),
        },
    }
}

/// 解析 `tailscale status --json` 的结果（纯函数，便于注入测试）。
fn evaluate_tailscale(value: &serde_json::Value, needle: &str) -> ProbeOutcome {
    let backend = value
        .get("BackendState")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    if !backend.eq_ignore_ascii_case("running") {
        return ProbeOutcome::fail(
            None,
            format!("tailscale BackendState = {backend}（期望 Running，说明 tailscaled 未就绪）"),
        );
    }

    // 目标就是本机：`Self.Online` 恒为 false，只能按「守护进程 Running」判定。
    if let Some(node) = value.get("Self") {
        if node_matches(node, needle) {
            let host = node
                .get("HostName")
                .and_then(|v| v.as_str())
                .unwrap_or(needle);
            return ProbeOutcome::ok(
                None,
                format!("本机节点 {host} 在线（BackendState=Running）"),
            );
        }
    }

    if let Some(peers) = value.get("Peer").and_then(|v| v.as_object()) {
        for peer in peers.values() {
            if !node_matches(peer, needle) {
                continue;
            }
            let host = peer
                .get("HostName")
                .and_then(|v| v.as_str())
                .unwrap_or(needle);
            return if node_is_online(peer) {
                let direct = peer
                    .get("CurAddr")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                match direct {
                    Some(addr) => ProbeOutcome::ok(None, format!("{host} 在线（直连 {addr}）")),
                    None => {
                        let relay = peer
                            .get("Relay")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        ProbeOutcome::ok(None, format!("{host} 在线（经中继 {relay}）"))
                    }
                }
            } else {
                ProbeOutcome::fail(None, format!("节点 {host} 的在线状态为 false（离线）"))
            };
        }
    }

    ProbeOutcome::fail(
        None,
        format!("tailnet 中找不到目标节点 {needle:?}（既不是本机，也不在任何 Peer 里）"),
    )
}

/// 节点是否命中 `target`（比对 `TailscaleIPs` / `DNSName` / `HostName`）。
fn node_matches(node: &serde_json::Value, needle: &str) -> bool {
    for key in ["TailscaleIPs", "DNSName", "HostName"] {
        let hit = match node.get(key) {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str())
                .any(|s| host_matches(s, needle)),
            Some(serde_json::Value::String(s)) => host_matches(s, needle),
            _ => false,
        };
        if hit {
            return true;
        }
    }
    false
}

/// 单个主机名比对：`needle` 写 `100.64.0.1` / `node1` / `node1.tailnet.ts.net.` 都要能命中。
fn host_matches(candidate: &str, needle: &str) -> bool {
    let candidate = candidate.trim_end_matches('.').to_ascii_lowercase();
    if candidate == needle {
        return true;
    }
    // DNSName → 短名（`node1.tailnet.ts.net` 的 `node1`）
    candidate
        .split('.')
        .next()
        .map(|short| short == needle)
        .unwrap_or(false)
}

/// 节点在线判定：优先 `Online`，缺失时退回 `Active`。
fn node_is_online(node: &serde_json::Value) -> bool {
    if let Some(online) = node.get("Online").and_then(|v| v.as_bool()) {
        return online;
    }
    node.get("Active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 子进程 + 错误文案
// ---------------------------------------------------------------------------

/// 跑一条命令并收集 stdout，带超时。失败时返回「人能直接看懂的原因」。
async fn run_command(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let spawned = tokio::process::Command::new(program).args(args).output();

    let output = if timeout.is_zero() {
        spawned
            .await
            .map_err(|err| format!("无法执行 `{program}`：{}", io_error_text(&err)))?
    } else {
        match tokio::time::timeout(timeout, spawned).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => return Err(format!("无法执行 `{program}`：{}", io_error_text(&err))),
            Err(_) => {
                return Err(format!(
                    "执行 `{program} {}` 超时（{}s）",
                    args.join(" "),
                    timeout.as_secs()
                ));
            }
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let tail = if stderr.is_empty() {
            String::new()
        } else {
            format!("：{}", first_line(stderr))
        };
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "被信号终止".to_string());
        return Err(format!(
            "`{program} {}` 退出码 {code}{tail}",
            args.join(" ")
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 200 {
        let truncated: String = line.chars().take(200).collect();
        format!("{truncated}…")
    } else {
        line.to_string()
    }
}

/// `io::Error` 的文案：把「命令不存在」这类最常见的坑说清楚。
fn io_error_text(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => format!("命令或目标不存在（{err}）"),
        std::io::ErrorKind::PermissionDenied => format!("权限不足（{err}）"),
        _ => err.to_string(),
    }
}

fn describe_reqwest_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "HTTP 请求超时".to_string()
    } else if err.is_connect() {
        format!("HTTP 连接失败：{}", strip_url(&err.to_string()))
    } else if err.is_body() {
        format!("HTTP 响应体读取失败：{}", strip_url(&err.to_string()))
    } else if err.is_request() {
        format!("HTTP 请求构造/发送失败：{}", strip_url(&err.to_string()))
    } else {
        strip_url(&err.to_string())
    }
}

/// reqwest 的错误文案会重复带上完整 URL（target 已在 summary 里出现过），这里截掉。
fn strip_url(text: &str) -> String {
    match text.find(" for url") {
        Some(index) => text[..index].to_string(),
        None => text.to_string(),
    }
}

fn status_text(status: reqwest::StatusCode) -> &'static str {
    status.canonical_reason().unwrap_or("Unknown")
}

fn to_millis(elapsed: Duration) -> u64 {
    elapsed.as_millis().min(u64::MAX as u128) as u64
}

fn join_pids(pids: &[u32]) -> String {
    const MAX: usize = 5;
    let head: Vec<String> = pids.iter().take(MAX).map(|p| p.to_string()).collect();
    if pids.len() > MAX {
        format!("{} 等 {} 个", head.join(", "), pids.len())
    } else {
        head.join(", ")
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ctx(round: u64) -> CollectCtx {
        CollectCtx::new(
            "test-host",
            100,
            round,
            Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap(),
        )
    }

    /// 每轮时间**不同**的 ctx：验证 `first_seen` 的「保持不变 / 重新开始」必须用它，
    /// 否则 `ctx(round)` 的时刻恒定，断言 `first_seen` 不变就成了恒真的假阳性测试。
    fn ctx_at(round: u64, secs: i64) -> CollectCtx {
        CollectCtx::new(
            "test-host",
            100,
            round,
            Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap() + chrono::Duration::seconds(secs),
        )
    }

    fn probe(name: &str, kind: ProbeKind, target: &str, fail_threshold: u32) -> ProbeConfig {
        ProbeConfig {
            name: name.to_string(),
            kind,
            target: target.to_string(),
            fail_threshold,
            timeout_sec: 2,
        }
    }

    /// 起一个本地 HTTP 服务，恒返回 `status`，返回 `http://127.0.0.1:<port>/`。
    async fn spawn_http_mock(status: u16) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::get;

        let app = axum::Router::new().route(
            "/",
            get(move || async move { (axum::http::StatusCode::from_u16(status).unwrap(), "mock") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle)
    }

    /// 起一个**可切换成败**的本地 HTTP 服务：`up == true` 回 200，否则回 500。
    ///
    /// 用它来跑状态机测试，避免「先占端口再放掉、然后再绑同一个端口」那种有时序竞争的写法。
    async fn spawn_toggle_http_mock(
        up: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::get;

        let app = axum::Router::new().route(
            "/",
            get(move || {
                let up = up.clone();
                async move {
                    let code = if up.load(std::sync::atomic::Ordering::SeqCst) {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    };
                    (code, "mock")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle)
    }

    /// 取一个**确定连不上**的 `host:port`。
    ///
    /// 不能简单地「绑一个端口再放掉」：本进程里并行跑的其它测试可能刚好重新绑上那个端口，
    /// 于是「必然失败」的断言偶发地变成成功（本项目的测试真的这样飘过一次）。
    /// 这里先取一个空闲端口，再**实测确认它确实连不上**，连得上就换一个重试（最多 20 次）。
    ///
    /// 不用 RFC5737 的 TEST-NET-1（`192.0.2.x`）：那些地址是**静默丢包**的，
    /// 每次都要等满整个 `timeout_sec`，会让测试平白慢好几秒。
    async fn unreachable_target() -> String {
        for _ in 0..20 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);

            let candidate = format!("127.0.0.1:{port}");
            let failed = matches!(
                tokio::time::timeout(
                    Duration::from_millis(300),
                    tokio::net::TcpStream::connect(&candidate)
                )
                .await,
                Ok(Err(_)) | Err(_)
            );
            if failed {
                return candidate;
            }
        }
        panic!("找不到连不上的测试目标（本机端口分配异常）");
    }

    #[tokio::test]
    async fn http_probe_reports_ok_with_latency_and_status_line() {
        let (url, server) = spawn_http_mock(200).await;
        let mut collector =
            ServiceProbeCollector::new(&[probe("web-frontend", ProbeKind::Http, &url, 2)]);
        assert_eq!(collector.len(), 1);

        let outcome = collector.collect(&ctx(1)).await;
        assert!(
            outcome.alerts.is_empty(),
            "成功不该告警: {:?}",
            outcome.alerts
        );
        assert_eq!(outcome.services.len(), 1, "每轮必须产出全量 ServiceStatus");
        let svc = &outcome.services[0];
        assert_eq!(svc.name, "web-frontend");
        assert_eq!(svc.kind, ServiceKind::Http);
        assert!(svc.ok, "200 必须 ok=true，detail={:?}", svc.detail);
        assert!(svc.latency_ms.is_some(), "http 成功必须记 latency_ms");
        assert_eq!(svc.detail.as_deref(), Some("200 OK"));

        server.abort();
    }

    #[tokio::test]
    async fn http_probe_5xx_is_failure_and_records_detail() {
        let (url, server) = spawn_http_mock(503).await;
        let mut collector =
            ServiceProbeCollector::new(&[probe("web-frontend", ProbeKind::Http, &url, 2)]);

        let outcome = collector.collect(&ctx(1)).await;
        assert!(outcome.alerts.is_empty(), "第 1 次失败未达阈值，不该告警");
        let svc = &outcome.services[0];
        assert!(!svc.ok, "503 必须 ok=false");
        assert_eq!(
            svc.detail.as_deref(),
            Some("HTTP 503 Service Unavailable"),
            "detail 必须写清 HTTP 状态"
        );

        server.abort();
    }

    #[tokio::test]
    async fn http_probe_connection_refused_is_failure_with_detail() {
        let target = unreachable_target().await;
        let mut collector = ServiceProbeCollector::new(&[probe(
            "web-frontend",
            ProbeKind::Http,
            &format!("http://{target}/"),
            2,
        )]);

        let outcome = collector.collect(&ctx(1)).await;
        let svc = &outcome.services[0];
        assert!(!svc.ok, "连不上的目标必须 ok=false: {:?}", svc.detail);
        assert!(svc.latency_ms.is_none(), "连不上时没有 latency");
        let detail = svc.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("连接失败") || detail.contains("超时"),
            "detail 必须说明是连接失败/超时，实际 {detail:?}"
        );
    }

    #[tokio::test]
    async fn tcp_probe_reflects_listener_state() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let down_target = unreachable_target().await;

        let mut collector = ServiceProbeCollector::new(&[
            probe("db-up", ProbeKind::Tcp, &addr.to_string(), 2),
            probe("db-down", ProbeKind::Tcp, &down_target, 2),
        ]);
        let outcome = collector.collect(&ctx(1)).await;
        assert_eq!(outcome.services.len(), 2);

        let up = outcome
            .services
            .iter()
            .find(|s| s.name == "db-up")
            .expect("db-up 必须出现在 services[]");
        assert!(up.ok, "监听中的端口必须 ok=true，detail={:?}", up.detail);
        assert!(up.latency_ms.is_some());
        assert!(up.detail.as_deref().unwrap_or_default().contains("成功"));

        let down = outcome
            .services
            .iter()
            .find(|s| s.name == "db-down")
            .expect("db-down 必须出现在 services[]");
        assert!(!down.ok);
        let detail = down.detail.as_deref().unwrap_or_default();
        // 关掉的本地端口通常立刻 RST（连接失败）；某些环境下会走完超时。两者都算「探测失败」。
        assert!(
            detail.contains("失败") || detail.contains("超时"),
            "detail 必须说明失败原因，实际 {detail:?}"
        );

        drop(listener);
    }

    #[tokio::test]
    async fn tcp_probe_timeout_when_unroutable() {
        // 10.255.255.1 是常见黑洞地址：连不上也不立即 RST，用来验证超时分支
        let mut collector = ServiceProbeCollector::new(&[ProbeConfig {
            name: "blackhole".to_string(),
            kind: ProbeKind::Tcp,
            target: "10.255.255.1:9".to_string(),
            fail_threshold: 1,
            timeout_sec: 1,
        }]);

        let outcome = collector.collect(&ctx(1)).await;
        let svc = &outcome.services[0];
        assert!(!svc.ok);
        let detail = svc.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("超时") || detail.contains("失败"),
            "detail 必须是超时或失败，实际 {detail:?}"
        );
    }

    /// **SPEC §13.2 条件型契约（本次修复的核心回归测试）**：
    /// 探针持续失败 6 轮 → **每一轮** `alerts[]` 都必须含该告警，`id` 每轮相同、
    /// `first_seen` 不变；恢复后**下一轮起不再含**，且**从不产出 `*:recovered`**。
    ///
    /// 这条测试就是 e2e 那个 bug 的最小复现：老实现只在前沿报一次，
    /// 第 4 轮 `alerts[]` 变空 → relay 推「已恢复」→ 此后永久失声。
    #[tokio::test]
    async fn failure_state_reports_every_round_then_stops_after_recovery() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let up = Arc::new(AtomicBool::new(false));
        let (url, server) = spawn_toggle_http_mock(up.clone()).await;
        let mut collector = ServiceProbeCollector::new(&[ProbeConfig {
            name: "web-frontend".to_string(),
            kind: ProbeKind::Http,
            target: url,
            fail_threshold: 3, // 阈值 3：前两次失败只记录
            timeout_sec: 2,
        }]);

        // ---- 第 1、2 轮：失败（500）但未达阈值 → 不告警（单周期抖动只记录，docs/04 §2.2）----
        for round in 1..=2 {
            let outcome = collector.collect(&ctx_at(round, round as i64 * 30)).await;
            assert!(
                outcome.alerts.is_empty(),
                "第 {round} 轮失败次数 < 阈值 3，不该告警: {:?}",
                outcome.alerts
            );
            assert_eq!(outcome.services.len(), 1, "失败也必须产出 ServiceStatus");
            assert!(!outcome.services[0].ok, "500 必须判定为失败");
        }

        // ---- 第 3 轮：达阈值 → 进入失败态，产出第一条 critical ----
        let entered = collector.collect(&ctx_at(3, 90)).await;
        assert_eq!(
            entered.alerts.len(),
            1,
            "达阈值必须告警: {:?}",
            entered.alerts
        );
        let first = &entered.alerts[0];
        assert_eq!(first.id, "service:web-frontend");
        assert_eq!(first.level, AlertLevel::Critical);
        assert_eq!(first.source, "service_probe");
        assert!(first.title.contains("web-frontend"), "{}", first.title);
        assert!(
            first.summary.contains('3'),
            "summary 要含失败次数: {}",
            first.summary
        );
        // 首次进入失败态的时刻 = 第 3 轮这一轮；此后必须保持不变
        let expected_first_seen = ctx_at(3, 90).now;
        assert_eq!(
            first.first_seen, expected_first_seen,
            "first_seen 必须是首次进入失败态的时刻"
        );
        let stable_title = first.title.clone();

        // ---- 第 4..=8 轮：仍在失败态 → **每一轮都必须含该告警**（条件型，SPEC §13.2）----
        // 时间每轮都往前走 30s：这样 first_seen 的「不变」才是真的被验证过。
        for round in 4..=8 {
            let outcome = collector.collect(&ctx_at(round, round as i64 * 30)).await;
            assert_eq!(
                outcome.alerts.len(),
                1,
                "第 {round} 轮仍处于失败态，alerts[] 必须含该告警（缺席 = relay 眼中的假恢复）: {:?}",
                outcome.alerts
            );
            let alert = &outcome.alerts[0];
            assert_eq!(
                alert.id, "service:web-frontend",
                "第 {round} 轮 id 必须稳定（relay 按 (source,title) 去重）"
            );
            assert_eq!(
                alert.first_seen, expected_first_seen,
                "第 {round} 轮 first_seen 必须保持首次进入失败态的时刻，不能刷新成当前时间"
            );
            assert_ne!(
                outcome.alerts[0].first_seen,
                ctx_at(round, round as i64 * 30).now,
                "第 {round} 轮的 first_seen 不该等于当前时间（防止「每轮刷新」）"
            );
            assert_eq!(
                alert.title, stable_title,
                "第 {round} 轮 title 必须逐字稳定，否则 relay 会当成新告警并给旧标题推假恢复"
            );
            assert_eq!(alert.level, AlertLevel::Critical);
            assert!(!outcome.services[0].ok, "第 {round} 轮服务确实还是挂的");
        }

        // ---- 恢复：本采集器**不产出**任何告警（恢复通知由 relay 推导）----
        up.store(true, Ordering::SeqCst);
        let recovered = collector.collect(&ctx_at(9, 270)).await;
        assert!(
            recovered.alerts.is_empty(),
            "恢复的那一轮不该产出任何告警（尤其是 *:recovered）: {:?}",
            recovered.alerts
        );
        assert!(recovered.services[0].ok, "服务必须已恢复");

        // ---- 恢复后：下一轮起仍然不含（条件已消失）----
        for round in 10..=12 {
            let outcome = collector.collect(&ctx_at(round, round as i64 * 30)).await;
            assert!(
                outcome.alerts.is_empty(),
                "恢复后的第 {round} 轮不该再含该告警: {:?}",
                outcome.alerts
            );
            assert!(outcome.services[0].ok);
        }

        server.abort();
    }

    /// 再次失败 → 重新进入失败态：`first_seen` 是**新的**一次失败态起点（状态被正确重置）。
    #[tokio::test]
    async fn second_failure_episode_starts_a_fresh_first_seen() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let up = Arc::new(AtomicBool::new(false));
        let (url, server) = spawn_toggle_http_mock(up.clone()).await;
        let mut collector = ServiceProbeCollector::new(&[ProbeConfig {
            name: "web-frontend".to_string(),
            kind: ProbeKind::Http,
            target: url,
            fail_threshold: 1,
            timeout_sec: 2,
        }]);

        let first_episode = collector.collect(&ctx_at(1, 0)).await;
        assert_eq!(first_episode.alerts.len(), 1);
        assert_eq!(first_episode.alerts[0].first_seen, ctx_at(1, 0).now);
        assert!(!collector.collect(&ctx_at(2, 30)).await.alerts.is_empty());

        // 恢复一轮（不在失败态）
        up.store(true, Ordering::SeqCst);
        assert!(collector.collect(&ctx_at(3, 60)).await.alerts.is_empty());

        // 再挂 → 新的失败态，first_seen 必须是第 4 轮（新的时刻）
        up.store(false, Ordering::SeqCst);
        let second_episode = collector.collect(&ctx_at(4, 90)).await;
        assert_eq!(second_episode.alerts.len(), 1, "再次失败必须重新告警");
        assert_eq!(
            second_episode.alerts[0].first_seen,
            ctx_at(4, 90).now,
            "新一次失败态的 first_seen 必须是新的起点"
        );
        assert_ne!(
            second_episode.alerts[0].first_seen,
            ctx_at(1, 0).now,
            "两次失败态的 first_seen 必须不同（时刻确实前进过）"
        );

        server.abort();
    }

    /// 抖动：失败 1 次后立刻成功 —— 不告警，也**不该**产生恢复通知。
    #[tokio::test]
    async fn single_cycle_flap_is_silent() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let up = Arc::new(AtomicBool::new(false));
        let (url, server) = spawn_toggle_http_mock(up.clone()).await;
        let mut collector = ServiceProbeCollector::new(&[ProbeConfig {
            name: "web-frontend".to_string(),
            kind: ProbeKind::Http,
            target: url,
            fail_threshold: 2,
            timeout_sec: 2,
        }]);

        // 失败 1 次：未达阈值
        let first = collector.collect(&ctx(1)).await;
        assert!(first.alerts.is_empty());
        assert!(!first.services[0].ok);

        // 立刻恢复
        up.store(true, Ordering::SeqCst);
        let second = collector.collect(&ctx(2)).await;
        assert!(
            second.alerts.is_empty(),
            "从未进入失败态的抖动，恢复也必须静默: {:?}",
            second.alerts
        );
        assert!(second.services[0].ok);

        // 再跑两轮也不该凭空冒出告警
        for round in 3..=4 {
            let outcome = collector.collect(&ctx(round)).await;
            assert!(outcome.alerts.is_empty(), "{:?}", outcome.alerts);
        }
        server.abort();
    }

    /// 阈值容错：`fail_threshold = 0` 当成 1，否则永不告警；且失败态期间每轮都报。
    #[tokio::test]
    async fn zero_fail_threshold_is_treated_as_one() {
        let target = unreachable_target().await;
        let mut collector = ServiceProbeCollector::new(&[probe("web", ProbeKind::Tcp, &target, 0)]);

        for round in 1..=3 {
            let outcome = collector.collect(&ctx_at(round, round as i64 * 30)).await;
            assert_eq!(
                outcome.alerts.len(),
                1,
                "阈值 0 必须当成 1（第 {round} 轮）"
            );
            assert_eq!(outcome.alerts[0].level, AlertLevel::Critical);
            assert_eq!(outcome.alerts[0].id, "service:web");
            assert_eq!(
                outcome.alerts[0].first_seen,
                ctx_at(1, 30).now,
                "第 {round} 轮 first_seen 必须保持首轮时刻"
            );
        }
    }

    /// 多探针：`services[]` 一条都不能少，顺序与配置一致（失败项也不是「消失」）。
    #[tokio::test]
    async fn all_probes_appear_in_services_every_round() {
        let (url, server) = spawn_http_mock(200).await;
        let down_target = unreachable_target().await;
        let mut collector = ServiceProbeCollector::new(&[
            probe("web-frontend", ProbeKind::Http, &url, 2),
            probe("db", ProbeKind::Tcp, &down_target, 2),
            probe(
                "ghost",
                ProbeKind::Process,
                "ssm-definitely-no-such-process",
                2,
            ),
        ]);

        for round in 1..=3 {
            let outcome = collector.collect(&ctx(round)).await;
            let names: Vec<&str> = outcome.services.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(
                names,
                ["web-frontend", "db", "ghost"],
                "第 {round} 轮 services[] 必须是全量快照且顺序稳定"
            );
            assert!(
                outcome.services[0].ok,
                "第 {round} 轮：mock 在监听，必须 ok=true"
            );
            assert!(
                !outcome.services[1].ok,
                "第 {round} 轮：连不上的 tcp 目标必须 ok=false，detail={:?}",
                outcome.services[1].detail
            );
            assert!(
                !outcome.services[2].ok,
                "第 {round} 轮：不存在的进程必须 ok=false"
            );
        }
        server.abort();
    }

    /// `process` 探针：用**本进程的真实进程名**验证命中路径（不是恒真断言）。
    #[tokio::test]
    async fn process_probe_matches_own_process_name() {
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let me = sysinfo::Pid::from_u32(std::process::id());
        let own_name = system
            .process(me)
            .map(|p| p.name().to_string_lossy().to_string())
            .expect("必须能在进程表里找到自己");
        assert!(!own_name.is_empty());

        let mut collector =
            ServiceProbeCollector::new(&[probe("self", ProbeKind::Process, &own_name, 2)]);
        let outcome = collector.collect(&ctx(1)).await;
        let svc = &outcome.services[0];
        assert!(
            svc.ok,
            "按自己的进程名 {own_name:?} 必须命中，detail={:?}",
            svc.detail
        );
        assert!(svc.latency_ms.is_none(), "process 探针不报 latency");
        assert!(
            svc.detail.as_deref().unwrap_or_default().contains("pid"),
            "detail 应带 pid，实际 {:?}",
            svc.detail
        );

        // 大小写不敏感
        let upper = own_name.to_ascii_uppercase();
        let mut collector =
            ServiceProbeCollector::new(&[probe("self-upper", ProbeKind::Process, &upper, 2)]);
        let outcome = collector.collect(&ctx(1)).await;
        assert!(outcome.services[0].ok, "进程名匹配必须大小写不敏感");
    }

    #[tokio::test]
    async fn process_probe_missing_process_is_failure_with_clear_detail() {
        let mut collector = ServiceProbeCollector::new(&[probe(
            "ghost",
            ProbeKind::Process,
            "ssm-no-such-process-xyz",
            2,
        )]);
        let outcome = collector.collect(&ctx(1)).await;
        let svc = &outcome.services[0];
        assert!(!svc.ok);
        let detail = svc.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("进程表"),
            "detail 要说清查的是什么: {detail}"
        );
        assert!(
            detail.contains("ssm-no-such-process-xyz"),
            "detail 要点出目标名: {detail}"
        );
    }

    /// `tailscale` 探针：WSL 里没装 tailscale → 必须优雅失败，不能 panic，detail 清晰。
    #[tokio::test]
    async fn tailscale_probe_handles_missing_binary() {
        let mut collector = ServiceProbeCollector::new(&[probe(
            "tailscale",
            ProbeKind::Tailscale,
            "100.64.0.1",
            2,
        )]);
        let outcome = collector.collect(&ctx(1)).await;
        assert_eq!(
            outcome.services.len(),
            1,
            "探针无论成败都要有 ServiceStatus"
        );
        let svc = &outcome.services[0];
        let detail = svc.detail.clone().unwrap_or_default();
        assert!(!detail.is_empty(), "detail 必须有内容");
        if svc.ok {
            // 机器上真的装了 tailscale 且该节点在线：也是合法结果，但要能自证
            assert!(
                detail.contains("在线"),
                "ok=true 时 detail 必须说明在线理由: {detail}"
            );
        } else {
            assert!(
                detail.contains("tailscale"),
                "失败时 detail 必须点名 tailscale，实际 {detail:?}"
            );
        }
    }

    // ---------------- tailscale JSON 解析（纯函数，可完全注入）----------------

    #[test]
    fn tailscale_json_running_self_and_peer_online() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
              "BackendState": "Running",
              "Self": {"HostName": "relay", "DNSName": "relay.tail.ts.net.", "TailscaleIPs": ["100.64.0.1"], "Online": false},
              "Peer": {
                "nodekey:abc": {"HostName": "kebei-prod-01", "DNSName": "kebei-prod-01.tail.ts.net.",
                                "TailscaleIPs": ["100.64.0.9"], "Online": true, "CurAddr": "192.168.1.10:41641", "Relay": "tok"}
              }
            }"#,
        )
        .unwrap();

        // 目标 = 本机 → 按 Self 判定（Self.Online 恒 false，不能当离线）
        let own = evaluate_tailscale(&json, "100.64.0.1");
        assert!(own.ok, "本机节点必须判定在线: {own:?}");

        // 目标 = 在线 peer，按 IP 命中
        let peer = evaluate_tailscale(&json, "100.64.0.9");
        assert!(peer.ok, "{peer:?}");
        assert!(peer.detail.contains("直连"), "detail={}", peer.detail);

        // 按 DNSName 短名命中
        assert!(evaluate_tailscale(&json, "kebei-prod-01").ok);

        // 不存在的节点必须失败
        let missing = evaluate_tailscale(&json, "100.64.0.99");
        assert!(!missing.ok);
        assert!(
            missing.detail.contains("找不到"),
            "detail={}",
            missing.detail
        );
    }

    #[test]
    fn tailscale_json_offline_peer_and_stopped_backend() {
        let stopped: serde_json::Value = serde_json::from_str(
            r#"{
              "BackendState": "Stopped",
              "Self": {"HostName": "relay", "TailscaleIPs": ["100.64.0.1"]},
              "Peer": {"k": {"HostName": "box", "TailscaleIPs": ["100.64.0.20"], "Online": false, "Relay": "tok"}}
            }"#,
        )
        .unwrap();
        let outcome = evaluate_tailscale(&stopped, "box");
        assert!(!outcome.ok);
        assert!(
            outcome.detail.contains("BackendState"),
            "detail 必须点出 BackendState: {}",
            outcome.detail
        );

        let running_offline_peer: serde_json::Value = serde_json::from_str(
            r#"{"BackendState": "Running",
                "Self": {"HostName": "relay", "TailscaleIPs": ["100.64.0.1"]},
                "Peer": {"k": {"HostName": "box", "TailscaleIPs": ["100.64.0.20"], "Online": false, "Relay": "tok"}}}"#,
        )
        .unwrap();
        let outcome = evaluate_tailscale(&running_offline_peer, "box");
        assert!(!outcome.ok);
        assert!(outcome.detail.contains("离线"), "detail={}", outcome.detail);
        assert!(
            !outcome.detail.contains("BackendState"),
            "Running 时不该再报 BackendState: {}",
            outcome.detail
        );
    }

    #[test]
    fn host_matches_handles_dns_suffix_and_case() {
        assert!(host_matches("node1.tailnet.ts.net.", "node1"));
        assert!(host_matches("Node1", "node1"));
        assert!(host_matches("100.64.0.1", "100.64.0.1"));
        assert!(!host_matches("node2.tailnet.ts.net.", "node1"));
        assert!(!host_matches("", "node1"));
    }

    // ---------------- 命令执行辅助 ----------------

    #[tokio::test]
    async fn run_command_reports_missing_binary_without_panicking() {
        let err = run_command(
            "ssm-no-such-binary-xyz",
            &["--version"],
            Duration::from_secs(2),
        )
        .await
        .expect_err("不存在的命令必须返回 Err");
        assert!(err.contains("ssm-no-such-binary-xyz"), "{err}");
        assert!(err.contains("无法执行"), "{err}");
    }

    #[tokio::test]
    async fn run_command_times_out_and_reports_program() {
        let err = run_command("sleep", &["5"], Duration::from_millis(200))
            .await
            .expect_err("超时必须返回 Err");
        assert!(err.contains("超时"), "{err}");
        assert!(err.contains("sleep 5"), "{err}");
    }

    #[tokio::test]
    async fn run_command_propagates_exit_code_and_stderr() {
        let err = run_command(
            "bash",
            &["-c", "echo boom >&2; exit 3"],
            Duration::from_secs(5),
        )
        .await
        .expect_err("非 0 退出码必须返回 Err");
        assert!(err.contains("退出码 3"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[tokio::test]
    async fn run_command_returns_stdout_on_success() {
        let out = run_command("bash", &["-c", "echo hello"], Duration::from_secs(5))
            .await
            .expect("成功必须返回 stdout");
        assert_eq!(out.trim(), "hello");
    }

    // ---------------- 构造期健壮性 ----------------

    #[test]
    fn invalid_http_target_is_skipped_not_fatal() {
        let collector = ServiceProbeCollector::new(&[
            probe("bad", ProbeKind::Http, "ftp://example.com", 2),
            probe("good", ProbeKind::Http, "https://example.com", 2),
        ]);
        assert_eq!(collector.len(), 1, "非 http(s) 的 http 探针必须被跳过");
        assert!(!collector.is_empty());
    }

    #[test]
    fn collector_name_and_empty_registry() {
        let collector = ServiceProbeCollector::new(&[]);
        assert_eq!(Collector::name(&collector), "service_probe");
        assert!(collector.is_empty());
    }

    #[tokio::test]
    async fn empty_probe_list_yields_empty_outcome() {
        let mut collector = ServiceProbeCollector::new(&[]);
        let outcome = collector.collect(&ctx(1)).await;
        assert!(outcome.services.is_empty());
        assert!(outcome.alerts.is_empty());
        assert!(outcome.git.is_empty());
    }
}
