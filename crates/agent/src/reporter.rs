//! 组包 + HMAC 签名 + `POST relay_url`（SPEC §5 / §7 `reporter.rs`）。
//!
//! # 硬性约定（照抄 SPEC 与任务书，不要"优化"）
//!
//! - 请求头 `X-SSM-Timestamp` + `X-SSM-Signature`，用 [`ssm_common::sign::sign`]，
//!   消息是 `timestamp 十进制 ASCII 字节 || raw body bytes`。
//! - **签名对「实际发送的那串 body 字节」计算**：先 `serde_json::to_vec` 得到 body，
//!   再签名，再用 `.body(body)` 原样发出（**不能**用 `.json()`——那会再序列化一次，
//!   与签名的字节可能不一致）。
//! - **补发保留原始 `sent_at`**：relay 靠 `(host_id, seq)` 去重、靠 `sent_at` 判断过期；
//!   但签名头的时间戳必须是**发送当刻**（relay 的 `verify` 有 ±300s 重放窗，
//!   离线 5 分钟以上的包用原始时间戳会被直接拒掉）。
//! - `seq` 单调递增并持久化到 `seq_state_path`，**崩溃重启后从文件续接**（不从头再来）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ssm_common::prelude::*;
use thiserror::Error;

use crate::buffer::ReportBuffer;
use crate::collectors::CollectOutcome;

/// 单次 POST 的总超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 连接超时：relay 不存在时要快速失败，不能拖住上报节奏。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 一轮最多补发多少条，避免长时间断网恢复瞬间打出上百个请求。
const MAX_RESEND_PER_ROUND: usize = 32;
/// 错误正文截断长度（日志里不要塞整个 HTML 错误页）。
const ERROR_BODY_LIMIT: usize = 200;

/// 上报过程中的错误。
#[derive(Debug, Error)]
pub enum ReportError {
    /// 连接/超时/TLS 等传输层失败。
    #[error("POST {url} 失败: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// relay 返回非 2xx。
    #[error("relay 返回 HTTP {code}: {body}")]
    Status { code: u16, body: String },

    /// 组包序列化失败（协议类型不可序列化时才会发生，属程序 bug）。
    #[error("序列化上报包失败: {0}")]
    Json(#[from] serde_json::Error),

    /// 本地状态文件（seq / buffer）读写失败。
    #[error("本地状态文件读写失败: {0}")]
    Io(#[from] std::io::Error),
}

/// 一轮上报的结果摘要（用于日志与测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundOutcome {
    /// 本轮新包的 `seq`。
    pub seq: u64,
    /// 本轮新包的 `sent_at`。
    pub sent_at: DateTime<Utc>,
    /// 本轮成功补发的历史包数量。
    pub resent: usize,
    /// 本轮结束时缓冲里还剩多少条。
    pub buffered: usize,
    /// 本轮新包是否送达 relay。
    pub delivered: bool,
    /// 连续失败轮数（送达后归零）。
    pub consecutive_failures: u64,
}

/// 上报器：持有序号、缓冲与 HTTP client。
pub struct Reporter {
    client: reqwest::Client,
    relay_url: String,
    host_id: String,
    secret: Vec<u8>,
    seq_state_path: PathBuf,
    seq: u64,
    buffer: ReportBuffer,
    consecutive_failures: u64,
}

impl Reporter {
    /// 用现成的 [`ReportBuffer`] 构造；`seq` 从 `seq_state_path` 读取续接。
    pub fn new(
        relay_url: impl Into<String>,
        host_id: impl Into<String>,
        shared_secret: impl Into<String>,
        seq_state_path: impl Into<PathBuf>,
        buffer: ReportBuffer,
    ) -> Result<Self, ReportError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|source| ReportError::Http {
                url: "<client build>".to_string(),
                source,
            })?;

        let seq_state_path = seq_state_path.into();
        let seq = load_seq(&seq_state_path);

        Ok(Self {
            client,
            relay_url: relay_url.into(),
            host_id: host_id.into(),
            secret: shared_secret.into().into_bytes(),
            seq_state_path,
            seq,
            buffer,
            consecutive_failures: 0,
        })
    }

    /// 从 `agent.toml` + `secrets.toml` 装配：加载本地缓冲 + 续接 seq。
    pub fn from_config(cfg: &AgentConfig, secrets: &SecretsConfig) -> Result<Self, ReportError> {
        let buffer = ReportBuffer::load(&cfg.buffer_path, cfg.buffer_max)?;
        Self::new(
            cfg.relay_url.clone(),
            cfg.host_id.clone(),
            secrets.shared_secret.clone(),
            cfg.seq_state_path.clone(),
            buffer,
        )
    }

    /// 当前已发出的最大序号（启动时来自 `seq_state_path`）。
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// 待补发条数。
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// 本地缓冲（`main.rs` 用它打印实际使用的缓冲文件路径；测试用它做断言）。
    pub fn buffer(&self) -> &ReportBuffer {
        &self.buffer
    }

    /// 组一个包：`seq` 自增并**先落盘**再返回（崩溃也不会重号）。
    ///
    /// `sent_at` 由调用方注入（`main.rs` 传本轮 `CollectCtx.now`），补发时保持原值。
    pub fn build_packet(
        &mut self,
        sent_at: DateTime<Utc>,
        metrics: Metrics,
        outcome: CollectOutcome,
    ) -> ReportPacket {
        self.seq += 1;
        if let Err(err) = persist_seq(&self.seq_state_path, self.seq) {
            tracing::error!(
                path = %self.seq_state_path.display(),
                seq = self.seq,
                error = %err,
                "seq 落盘失败，重启后可能重号"
            );
        }

        ReportPacket {
            version: PROTOCOL_VERSION,
            host_id: self.host_id.clone(),
            sent_at,
            seq: self.seq,
            metrics,
            services: outcome.services,
            git: outcome.git,
            alerts: outcome.alerts,
        }
    }

    /// 发一个包：序列化一次 → 用当前时间戳签名 → 原样发送。
    pub async fn send_once(&self, packet: &ReportPacket) -> Result<(), ReportError> {
        let body = serde_json::to_vec(packet)?;
        let timestamp = Utc::now().timestamp();
        let signature = ssm_common::sign::sign(&self.secret, timestamp, &body);

        let response = self
            .client
            .post(&self.relay_url)
            .header(TIMESTAMP_HEADER, timestamp.to_string())
            .header(SIGNATURE_HEADER, signature)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|source| ReportError::Http {
                url: self.relay_url.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ReportError::Status {
                code: status.as_u16(),
                body: truncate(&body, ERROR_BODY_LIMIT),
            });
        }
        Ok(())
    }

    /// 一轮上报（SPEC §7）：
    ///
    /// 1. 先按 `sent_at` 从旧到新补发缓冲里的包（成功一批就从队首移除）；
    /// 2. 再发本轮新包；
    /// 3. 任何一步失败 → 新包入本地缓冲，下轮继续。
    ///
    /// relay 不可达时**本轮只打一次请求**（补发第一次失败就停手，新包直接入缓冲），
    /// 避免断网期间每轮把上百条缓冲各超时一遍。
    pub async fn report_round(
        &mut self,
        sent_at: DateTime<Utc>,
        metrics: Metrics,
        outcome: CollectOutcome,
    ) -> RoundOutcome {
        let packet = self.build_packet(sent_at, metrics, outcome);
        // 入缓冲会 move 掉 packet，先把要回显的字段取出来。
        let packet_seq = packet.seq;
        let packet_sent_at = packet.sent_at;

        let mut relay_reachable = true;
        let mut resent = 0_usize;

        if !self.buffer.is_empty() {
            let budget = self.buffer.len().min(MAX_RESEND_PER_ROUND);
            for index in 0..budget {
                // 先 clone 出来，避免在 await 期间持有 &self.buffer 借用
                let Some(queued) = self.buffer.pending().get(index).cloned() else {
                    break;
                };
                match self.send_once(&queued).await {
                    Ok(()) => resent += 1,
                    Err(err) => {
                        relay_reachable = false;
                        tracing::warn!(
                            seq = queued.seq,
                            sent_at = %queued.sent_at,
                            error = %err,
                            "补发历史上报包失败，本轮的补发到此为止"
                        );
                        break;
                    }
                }
            }
            if resent > 0 {
                // 全补发完就直接清空（连文件一起删掉，不留空壳文件）；
                // 只发了一部分则从队首丢弃已发的那几条。
                let drained = if resent == self.buffer.len() {
                    self.buffer.clear()
                } else {
                    self.buffer.drop_front(resent)
                };
                if let Err(err) = drained {
                    tracing::error!(error = %err, "从本地缓冲移除已补发条目失败");
                }
            }
        }

        let mut delivered = false;
        if relay_reachable {
            match self.send_once(&packet).await {
                Ok(()) => delivered = true,
                Err(err) => {
                    // 具体错误由 main.rs 的每轮汇总日志（warn 级）带出去，
                    // 这里只留 debug 细节，避免同一件事打两遍。
                    tracing::debug!(
                        seq = packet.seq,
                        error = %err,
                        "本轮心跳上报失败"
                    );
                }
            }
        } else {
            tracing::debug!(seq = packet.seq, "relay 不可达，本轮心跳直接入本地缓冲");
        }

        if delivered {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures += 1;
            if let Err(err) = self.buffer.push(packet) {
                tracing::error!(error = %err, "写入本地缓冲失败，这条上报包已丢失");
            }
        }

        if delivered {
            tracing::debug!(
                seq = self.seq,
                resent,
                buffered = self.buffer.len(),
                "心跳已上报"
            );
        } else {
            tracing::warn!(
                seq = self.seq,
                resent,
                buffered = self.buffer.len(),
                consecutive_failures = self.consecutive_failures,
                relay_url = %self.relay_url,
                "心跳未送达，已入本地缓冲，下轮按原始 sent_at 补发"
            );
        }

        RoundOutcome {
            seq: packet_seq,
            sent_at: packet_sent_at,
            resent,
            buffered: self.buffer.len(),
            delivered,
            consecutive_failures: self.consecutive_failures,
        }
    }

    /// 优雅退出：把内存里的缓冲与 seq 落盘。
    pub fn flush(&mut self) -> Result<(), ReportError> {
        self.buffer.persist()?;
        persist_seq(&self.seq_state_path, self.seq)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// seq 持久化
// ---------------------------------------------------------------------------

/// 读 `seq_state_path`；不存在 / 内容非法都退化为 0（并 warn），不让 agent 起不来。
fn load_seq(path: &Path) -> u64 {
    match std::fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<u64>() {
            Ok(seq) => {
                tracing::info!(path = %path.display(), seq, "已从磁盘续接上报序号 seq");
                seq
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    raw = %text.trim(),
                    error = %err,
                    "seq 状态文件内容非法，从 0 重新开始（relay 侧只会看到断号）"
                );
                0
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "seq 状态文件不存在，从 0 开始计数");
            0
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "读取 seq 状态文件失败，从 0 开始计数");
            0
        }
    }
}

/// 原子写入十进制 seq（临时文件 + rename）。
fn persist_seq(path: &Path, seq: u64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let temp = path.with_file_name(name);

    std::fs::write(&temp, format!("{seq}\n"))?;
    std::fs::rename(&temp, path)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::CollectOutcome;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use chrono::TimeZone;
    use ssm_common::protocol::{Alert, AlertLevel, ServiceKind, ServiceStatus};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ssm-agent-reporter-{tag}-{}-{}",
                std::process::id(),
                UNIQUE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("创建临时目录");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ------------------------------------------------------------ mock relay

    #[derive(Debug, Clone)]
    struct Captured {
        timestamp: String,
        signature: String,
        body: Vec<u8>,
    }

    impl Captured {
        fn packet(&self) -> ReportPacket {
            serde_json::from_slice(&self.body).expect("body 必须能反序列化回 ReportPacket")
        }
        fn timestamp_secs(&self) -> i64 {
            self.timestamp
                .parse()
                .expect("X-SSM-Timestamp 必须是十进制秒")
        }
    }

    #[derive(Clone)]
    struct MockState {
        healthy: Arc<AtomicBool>,
        captured: Arc<Mutex<Vec<Captured>>>,
    }

    struct MockRelay {
        addr: std::net::SocketAddr,
        healthy: Arc<AtomicBool>,
        captured: Arc<Mutex<Vec<Captured>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockRelay {
        async fn spawn() -> Self {
            let healthy = Arc::new(AtomicBool::new(true));
            let captured = Arc::new(Mutex::new(Vec::new()));
            let state = MockState {
                healthy: Arc::clone(&healthy),
                captured: Arc::clone(&captured),
            };

            let app = Router::new()
                .route("/report", post(mock_report))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("绑定 mock relay");
            let addr = listener.local_addr().expect("取本地地址");
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            Self {
                addr,
                healthy,
                captured,
                handle,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/report", self.addr)
        }

        fn set_healthy(&self, healthy: bool) {
            self.healthy.store(healthy, Ordering::SeqCst);
        }

        fn captured(&self) -> Vec<Captured> {
            self.captured.lock().expect("mock 记录锁").clone()
        }
    }

    impl Drop for MockRelay {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn mock_report(
        State(state): State<MockState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        state.captured.lock().expect("mock 记录锁").push(Captured {
            timestamp: header(TIMESTAMP_HEADER),
            signature: header(SIGNATURE_HEADER),
            body: body.to_vec(),
        });

        if state.healthy.load(Ordering::SeqCst) {
            (StatusCode::OK, "{\"ok\":true}").into_response()
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, "relay down").into_response()
        }
    }

    // ------------------------------------------------------------ fixtures

    const SECRET: &str = "unit-test-shared-secret";

    fn test_config(dir: &TempDir, relay_url: &str) -> AgentConfig {
        let mut cfg: AgentConfig =
            toml::from_str("host_id = \"test-host\"\nrelay_url = \"http://127.0.0.1:1/report\"\n")
                .expect("最小 agent.toml 必须可解析");
        cfg.relay_url = relay_url.to_string();
        cfg.seq_state_path = dir.path().join("seq");
        cfg.buffer_path = dir.path().join("buffer.jsonl");
        cfg.buffer_max = 10;
        cfg
    }

    fn test_secrets() -> SecretsConfig {
        toml::from_str(&format!(
            "shared_secret = \"{SECRET}\"\nstatus_token = \"t\"\n"
        ))
        .expect("最小 secrets.toml 必须可解析")
    }

    /// 一个「一定连不上」的 relay 地址：绑定后立刻释放端口，连接必然被拒。
    async fn dead_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定临时端口");
        let addr = listener.local_addr().expect("取本地地址");
        drop(listener);
        format!("http://{addr}/report")
    }

    fn metrics() -> Metrics {
        Metrics {
            cpu_percent: 3.5,
            mem_used_mb: 111,
            mem_total_mb: 222,
            disks: vec![DiskUsage {
                mount: "/".to_string(),
                used_gb: 1.5,
                total_gb: 10.0,
            }],
            load_avg: [0.1, 0.2, 0.3],
            uptime_sec: 999,
            net: Some(NetThroughput {
                rx_kbps: 1.0,
                tx_kbps: 2.0,
            }),
        }
    }

    fn outcome(service_name: &str, alert_id: &str) -> CollectOutcome {
        CollectOutcome {
            alerts: vec![Alert {
                id: alert_id.to_string(),
                level: AlertLevel::Warning,
                source: "log_watcher".to_string(),
                title: "t".to_string(),
                summary: "s".to_string(),
                first_seen: Utc.timestamp_opt(1_700_000_000, 0).single().unwrap(),
            }],
            services: vec![ServiceStatus {
                name: service_name.to_string(),
                kind: ServiceKind::Http,
                target: "http://127.0.0.1:9/".to_string(),
                ok: false,
                latency_ms: None,
                detail: Some("connection refused".to_string()),
            }],
            git: vec![],
        }
    }

    // ------------------------------------------------------------ 测试

    /// 签名头必须对「实际发送的那串字节」成立：用独立重算的 HMAC 断言。
    #[tokio::test]
    async fn sends_signed_body_and_headers_verify_independently() {
        let dir = TempDir::new("sign");
        let mock = MockRelay::spawn().await;
        let cfg = test_config(&dir, &mock.url());
        let secrets = test_secrets();
        let mut reporter = Reporter::from_config(&cfg, &secrets).expect("装配 reporter");

        let now = Utc::now();
        let round = reporter
            .report_round(now, metrics(), outcome("svc-1", "alert:1"))
            .await;
        assert!(round.delivered, "mock relay 健康时本轮必须送达");
        assert_eq!(round.seq, 1);
        assert_eq!(round.sent_at, now);
        assert_eq!(round.resent, 0);
        assert_eq!(round.buffered, 0);
        assert_eq!(round.consecutive_failures, 0);

        let captured = mock.captured();
        assert_eq!(captured.len(), 1, "一轮只发一个请求");
        let sent = &captured[0];

        // 1) 时间戳头是发送当刻（重放窗内）
        let ts = sent.timestamp_secs();
        assert!(
            (Utc::now().timestamp() - ts).abs() <= 10,
            "X-SSM-Timestamp 必须是发送当刻，实际 {ts}"
        );

        // 2) 独立重算 HMAC：message = "timestamp" || raw body
        let expected = ssm_common::sign::sign(SECRET.as_bytes(), ts, &sent.body);
        assert_eq!(sent.signature, expected, "签名必须对实际发送的 body 成立");
        assert_eq!(sent.signature.len(), 64);
        assert!(
            sent.signature
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "签名必须是十六进制小写"
        );

        // 3) 用 common 的 verify 再走一遍完整校验路径
        assert_eq!(
            ssm_common::sign::verify(SECRET.as_bytes(), ts, &sent.body, &sent.signature, ts),
            Ok(()),
            "relay 侧 verify 必须通过"
        );

        // 4) body 反序列化回 ReportPacket，字段逐一一致
        let packet = sent.packet();
        assert_eq!(packet.version, PROTOCOL_VERSION);
        assert_eq!(packet.host_id, "test-host");
        assert_eq!(packet.seq, 1);
        assert_eq!(packet.sent_at, now);
        assert_eq!(packet.metrics, metrics());
        assert_eq!(packet.services.len(), 1);
        assert_eq!(packet.services[0].name, "svc-1");
        assert!(!packet.services[0].ok);
        assert_eq!(packet.alerts.len(), 1);
        assert_eq!(packet.alerts[0].id, "alert:1");
        assert!(packet.git.is_empty());

        // 5) 顶层字段名与契约一致（防协议漂移）
        let value: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("上报包必须是 JSON 对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["alerts", "git", "host_id", "metrics", "sent_at", "seq", "services", "version"]
        );
    }

    /// `seq` 落盘并在重启后**续接**（不是从 0 重来）。
    #[tokio::test]
    async fn seq_is_persisted_and_resumed_after_restart() {
        let dir = TempDir::new("seq");
        let mock = MockRelay::spawn().await;
        let cfg = test_config(&dir, &mock.url());
        let secrets = test_secrets();

        {
            let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();
            assert_eq!(reporter.seq(), 0, "全新机器从 0 开始");
            assert_eq!(
                reporter
                    .report_round(Utc::now(), metrics(), CollectOutcome::empty())
                    .await
                    .seq,
                1
            );
            assert_eq!(
                reporter
                    .report_round(Utc::now(), metrics(), CollectOutcome::empty())
                    .await
                    .seq,
                2
            );
            assert_eq!(
                std::fs::read_to_string(&cfg.seq_state_path)
                    .expect("seq 文件必须存在")
                    .trim(),
                "2",
                "每轮 seq 必须立刻落盘"
            );
        }

        let mut restarted = Reporter::from_config(&cfg, &secrets).unwrap();
        assert_eq!(restarted.seq(), 2, "重启后必须从磁盘续接");
        let round = restarted
            .report_round(Utc::now(), metrics(), CollectOutcome::empty())
            .await;
        assert_eq!(round.seq, 3);

        let seqs: Vec<u64> = mock.captured().iter().map(|c| c.packet().seq).collect();
        assert_eq!(seqs, [1, 2, 3], "seq 必须跨重启单调递增");
    }

    #[test]
    fn load_seq_tolerates_missing_and_broken_state_file() {
        let dir = TempDir::new("seq-broken");
        let path = dir.path().join("seq");
        assert_eq!(load_seq(&path), 0, "文件不存在 => 0");

        std::fs::write(&path, "  41 \n").unwrap();
        assert_eq!(load_seq(&path), 41, "容忍首尾空白");

        std::fs::write(&path, "not-a-number").unwrap();
        assert_eq!(load_seq(&path), 0, "内容非法 => 0 而不是 panic");

        std::fs::write(&path, "-3").unwrap();
        assert_eq!(load_seq(&path), 0, "负数不是合法 u64");

        // 目录（读取会失败）也不能 panic
        assert_eq!(load_seq(dir.path()), 0);
    }

    #[test]
    fn persist_seq_is_atomic_and_creates_parent_dir() {
        let dir = TempDir::new("seq-write");
        let path = dir.path().join("nested/deeper/seq");
        persist_seq(&path, 7).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "7");
        assert!(
            !path.with_file_name("seq.tmp").exists(),
            "原子替换后不能残留 .tmp"
        );
    }

    /// 上报失败 → 入 buffer；relay 恢复后下轮补发，且**原始 `sent_at` 不变**。
    #[tokio::test]
    async fn failed_round_is_buffered_then_resent_with_original_sent_at() {
        let dir = TempDir::new("resend");
        let mock = MockRelay::spawn().await;
        let cfg = test_config(&dir, &mock.url());
        let secrets = test_secrets();
        let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();

        // 真实过去时间，远超 ±300s 重放窗——用来验证补发必须重新签名
        let t1 = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        assert!(
            Utc::now().timestamp() - t1.timestamp() > MAX_SKEW_SEC,
            "测试前提：t1 必须在重放窗之外"
        );

        mock.set_healthy(false);
        let first = reporter
            .report_round(t1, metrics(), outcome("svc-1", "alert:1"))
            .await;
        assert!(!first.delivered, "500 必须算失败");
        assert_eq!(first.buffered, 1);
        assert_eq!(first.consecutive_failures, 1);
        assert_eq!(reporter.buffered(), 1);
        assert!(cfg.buffer_path.exists(), "失败必须落盘");
        assert_eq!(reporter.buffer().front().unwrap().sent_at, t1);
        assert_eq!(reporter.buffer().front().unwrap().seq, 1);
        assert_eq!(reporter.buffer().front().unwrap().services[0].name, "svc-1");

        // relay 恢复
        mock.set_healthy(true);
        let t2 = t1 + chrono::Duration::seconds(30);
        let second = reporter
            .report_round(t2, metrics(), outcome("svc-2", "alert:2"))
            .await;
        assert!(second.delivered);
        assert_eq!(second.seq, 2);
        assert_eq!(second.resent, 1, "上一轮的包必须被补发");
        assert_eq!(second.buffered, 0);
        assert_eq!(second.consecutive_failures, 0);
        assert!(reporter.buffer().is_empty());

        let captured = mock.captured();
        assert_eq!(captured.len(), 3, "1 次失败 + 1 次补发 + 1 次本轮新包");

        // 补发包：body 里 sent_at 保持原始时间，签名时间戳是「现在」
        let resent = captured[1].packet();
        assert_eq!(resent.seq, 1, "补发的必须是被缓存的旧包");
        assert_eq!(resent.sent_at, t1, "补发必须保留原始 sent_at");
        assert_eq!(resent.services[0].name, "svc-1");
        let resent_ts = captured[1].timestamp_secs();
        assert!(
            (Utc::now().timestamp() - resent_ts).abs() <= 10,
            "补发必须用当前时间戳重新签名（否则超 300s 会被 relay 拒），实际 {resent_ts}"
        );
        assert_eq!(
            captured[1].signature,
            ssm_common::sign::sign(SECRET.as_bytes(), resent_ts, &captured[1].body),
            "补发包的签名同样必须对实际发送的字节成立"
        );
        assert_eq!(
            ssm_common::sign::verify(
                SECRET.as_bytes(),
                resent_ts,
                &captured[1].body,
                &captured[1].signature,
                resent_ts
            ),
            Ok(()),
            "relay 侧必须能验过补发包"
        );

        // 本轮新包
        let fresh = captured[2].packet();
        assert_eq!(fresh.seq, 2);
        assert_eq!(fresh.sent_at, t2);
        assert_eq!(fresh.services[0].name, "svc-2");
    }

    /// 断网期间每轮只打一次请求（不是每条缓冲各超时一遍），且缓冲条数受 `buffer_max` 约束。
    #[tokio::test]
    async fn unreachable_relay_buffers_without_bursting() {
        let dir = TempDir::new("unreachable");
        let cfg = test_config(&dir, &dead_url().await);
        let secrets = test_secrets();
        let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();

        let mut base = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        for round in 1..=4_u64 {
            base += chrono::Duration::seconds(30);
            let outcome = reporter
                .report_round(base, metrics(), CollectOutcome::empty())
                .await;
            assert!(!outcome.delivered);
            assert_eq!(outcome.consecutive_failures, round);
            assert_eq!(outcome.buffered, round as usize);
        }
        assert_eq!(reporter.buffered(), 4);

        // 落盘的 JSONL 行数 = 缓冲条数（重启可续）
        let raw = std::fs::read_to_string(&cfg.buffer_path).unwrap();
        assert_eq!(raw.lines().count(), 4);

        // buffer_max 生效：淘汰最旧
        for _ in 0..20 {
            base += chrono::Duration::seconds(30);
            let _ = reporter
                .report_round(base, metrics(), CollectOutcome::empty())
                .await;
        }
        assert_eq!(reporter.buffered(), cfg.buffer_max, "不能超过 buffer_max");
        assert_eq!(
            reporter.buffer().front().map(|p| p.seq),
            Some(reporter.seq() - cfg.buffer_max as u64 + 1),
            "淘汰的必须是最旧的"
        );
    }

    /// 缓冲跨进程重启（模拟 agent 被 kill -9 后重启）。
    #[tokio::test]
    async fn buffered_packets_survive_restart_and_are_resent() {
        let dir = TempDir::new("restart-buffer");
        let mock = MockRelay::spawn().await;
        let cfg = test_config(&dir, &mock.url());
        let secrets = test_secrets();

        let t1 = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        {
            mock.set_healthy(false);
            let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();
            let round = reporter
                .report_round(t1, metrics(), outcome("svc-old", "alert:old"))
                .await;
            assert!(!round.delivered);
            assert_eq!(reporter.buffered(), 1);
        } // 进程"崩溃"，没有任何 flush

        mock.set_healthy(true);
        let mut restarted = Reporter::from_config(&cfg, &secrets).unwrap();
        assert_eq!(restarted.buffered(), 1, "启动时必须从磁盘加载缓冲");
        assert_eq!(restarted.seq(), 1, "seq 也必须续接");

        let t2 = t1 + chrono::Duration::seconds(60);
        let round = restarted
            .report_round(t2, metrics(), CollectOutcome::empty())
            .await;
        assert!(round.delivered);
        assert_eq!(round.resent, 1);
        assert_eq!(round.buffered, 0);

        let captured = mock.captured();
        let resent = captured.last().expect("至少有一次成功请求").packet();
        let restored = captured
            .iter()
            .map(|c| c.packet())
            .find(|p| p.seq == 1)
            .expect("补发的旧包必须在请求里");
        assert_eq!(restored.sent_at, t1, "跨重启补发也必须保留原始 sent_at");
        assert_eq!(restored.services[0].name, "svc-old");
        assert_eq!(resent.sent_at, t2);
        assert_eq!(resent.seq, 2);
    }

    /// 健康 relay 下缓冲按顺序补发（最旧先发），且一轮最多补发 32 条。
    #[tokio::test]
    async fn resend_is_oldest_first_and_capped_per_round() {
        let dir = TempDir::new("drain-order");
        let mock = MockRelay::spawn().await;
        let mut cfg = test_config(&dir, &mock.url());
        cfg.buffer_max = 80;
        let secrets = test_secrets();

        mock.set_healthy(false);
        let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();
        let mut base = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        for _ in 0..40 {
            base += chrono::Duration::seconds(30);
            let _ = reporter
                .report_round(base, metrics(), CollectOutcome::empty())
                .await;
        }
        assert_eq!(reporter.buffered(), 40);

        mock.set_healthy(true);
        // 断网期间每轮都会重试队首那一包，所以只统计「恢复之后」发出的请求。
        let after_recovery = mock.captured().len();

        base += chrono::Duration::seconds(30);
        let first = reporter
            .report_round(base, metrics(), CollectOutcome::empty())
            .await;
        assert_eq!(first.resent, 32, "一轮最多补发 MAX_RESEND_PER_ROUND 条");
        assert_eq!(first.buffered, 8);
        assert!(first.delivered, "本轮新包仍应发出");

        base += chrono::Duration::seconds(30);
        let second = reporter
            .report_round(base, metrics(), CollectOutcome::empty())
            .await;
        assert_eq!(second.resent, 8, "剩下的 8 条下一轮补完");
        assert_eq!(second.buffered, 0);

        // 补发顺序：seq 1..=40 必须严格递增（最旧先发）；过滤掉本轮的新包（41/42）
        let captured = mock.captured();
        let resent_seqs: Vec<u64> = captured[after_recovery..]
            .iter()
            .map(|c| c.packet().seq)
            .filter(|seq| *seq <= 40)
            .collect();
        assert_eq!(resent_seqs, (1..=40).collect::<Vec<u64>>());
    }

    /// 4xx 也算失败并进缓冲（数据不丢，等 relay 侧修好）。
    #[tokio::test]
    async fn non_2xx_status_is_reported_as_error() {
        let dir = TempDir::new("status");
        let mock = MockRelay::spawn().await;
        let cfg = test_config(&dir, &mock.url());
        let secrets = test_secrets();
        let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();

        mock.set_healthy(false);
        let round = reporter
            .report_round(Utc::now(), metrics(), CollectOutcome::empty())
            .await;
        assert!(!round.delivered);
        assert_eq!(round.buffered, 1);

        // 直接调用 send_once 时错误里必须带状态码
        let packet = reporter.buffer().front().cloned().expect("缓冲里应有一条");
        match reporter.send_once(&packet).await {
            Err(ReportError::Status { code, body }) => {
                assert_eq!(code, 500);
                assert!(body.contains("relay down"), "实际: {body}");
            }
            other => panic!("期望 ReportError::Status，得到 {other:?}"),
        }
    }

    /// 优雅退出：flush 把缓冲与 seq 都落盘。
    #[tokio::test]
    async fn flush_persists_buffer_and_seq() {
        let dir = TempDir::new("flush");
        let cfg = test_config(&dir, &dead_url().await);
        let secrets = test_secrets();
        let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();

        let _ = reporter
            .report_round(Utc::now(), metrics(), CollectOutcome::empty())
            .await;
        reporter.flush().expect("flush 必须成功");

        assert_eq!(
            std::fs::read_to_string(&cfg.seq_state_path).unwrap().trim(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(&cfg.buffer_path)
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 10), "abc");
        // 多字节字符不能被截成半个（"服务器离线" 每字 3 字节）
        assert_eq!(truncate("服务器离线", 7), "服务…");
        assert_eq!(truncate("服务器离线", 9), "服务器…");
    }
}
