//! HTTP 接口（SPEC §3 / §7 `api.rs` / docs/03 §4）。
//!
//! | 路由 | 鉴权 | 说明 |
//! |---|---|---|
//! | `POST /report` | HMAC 签名（`X-SSM-Timestamp` + `X-SSM-Signature`） | 验签 → 去重落库 → 更新台账 → 即时告警通道 |
//! | `GET /status` | `Authorization: Bearer <status_token>` | 返回 [`StatusSnapshot`] |
//! | `GET /healthz` | 无 | UptimeRobot 兜底 |
//!
//! 无签名 / 坏签名 / 时间戳超窗一律 **401**；`/status` 无 Token 或 Token 不符一律 **401**。

use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Serialize;
use ssm_common::prelude::*;

use crate::alerter::Alerter;
use crate::relay::{Clock, IngestError, Relay, SharedClock};

/// 请求体上限：心跳包带日志摘要也就几 KB，给 1 MiB 足够且能挡住巨包。
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 共享状态。
///
/// `relay` 与 `alerter` **分开加锁**：relay 的所有方法都是同步的（锁只在短临界区里
/// 持有），Bark 推送则在 alerter 自己的锁下 await，两者互不阻塞、也不会把锁带进 await。
#[derive(Clone)]
pub struct AppState {
    relay: Arc<Mutex<Relay>>,
    alerter: Arc<tokio::sync::Mutex<Alerter>>,
    secrets: Arc<SecretsConfig>,
    clock: SharedClock,
    /// `relay.toml` 里 `[[hosts]].host_id` 的顺序（`/status` 选主机的依据）。
    known_hosts: Arc<Vec<String>>,
    probe_transport: Arc<dyn crate::net::ProbeTransport>,
}

impl AppState {
    pub fn new(
        relay: Relay,
        alerter: Alerter,
        secrets: Arc<SecretsConfig>,
        known_hosts: Vec<String>,
        clock: SharedClock,
        probe_transport: Arc<dyn crate::net::ProbeTransport>,
    ) -> Self {
        Self {
            relay: Arc::new(Mutex::new(relay)),
            alerter: Arc::new(tokio::sync::Mutex::new(alerter)),
            secrets,
            clock,
            known_hosts: Arc::new(known_hosts),
            probe_transport,
        }
    }

    /// 加锁访问 relay（锁只在同步逻辑内持有，绝不在 await 点上跨越）。
    pub fn relay(&self) -> std::sync::MutexGuard<'_, Relay> {
        self.relay.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 加锁访问推送器。**任何 async 上下文都必须用 [`AppState::alerter_lock`]**。
    pub async fn alerter_lock(&self) -> tokio::sync::MutexGuard<'_, Alerter> {
        self.alerter.lock().await
    }

    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    pub fn probe_transport(&self) -> Arc<dyn crate::net::ProbeTransport> {
        self.probe_transport.clone()
    }

    /// `/status` 面向的主机（配置顺序第一台）。
    pub fn primary_host(&self) -> Option<String> {
        self.relay().primary_host(&self.known_hosts)
    }
}

/// 组装路由。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handle_healthz))
        .route("/status", get(handle_status))
        .route("/report", post(handle_report))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// GET /healthz
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Healthz {
    status: &'static str,
}

/// 无鉴权存活探针。
async fn handle_healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(Healthz { status: "ok" }))
}

// ---------------------------------------------------------------------------
// GET /status
// ---------------------------------------------------------------------------

/// Bearer Token 校验（不匹配一律 401）。
fn status_token_ok(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
    else {
        return false;
    };
    // 常量时间比较，避免逐字节比较泄漏 token。
    constant_time_eq(token.trim().as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn handle_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !status_token_ok(&headers, &state.secrets.status_token) {
        tracing::warn!("/status 鉴权失败：缺少或错误的 Bearer Token");
        return unauthorized("缺少或错误的 Bearer Token");
    }

    let now = state.relay().now();
    let Some(host_id) = state.primary_host() else {
        return ApiError {
            status: StatusCode::NOT_FOUND,
            message: "relay 尚未登记任何主机（检查 relay.toml 的 [[hosts]]）",
        }
        .into_response();
    };

    let relay = state.relay();
    let snapshot = build_snapshot(&relay, &host_id, now);
    (StatusCode::OK, Json(snapshot)).into_response()
}

/// 把台账拼成 [`StatusSnapshot`]（字段严格按 SPEC §4，勿增删）。
pub fn build_snapshot(relay: &Relay, host_id: &str, now: DateTime<Utc>) -> StatusSnapshot {
    let host = relay.host(host_id);
    let state = relay
        .machine_of(host_id)
        .map(|m| m.state())
        .or_else(|| host.map(|h| h.state))
        .unwrap_or(HostState::Online);
    StatusSnapshot {
        host_id: host_id.to_string(),
        state,
        last_heartbeat: host.and_then(|h| h.last_heartbeat),
        metrics: host.and_then(|h| h.snapshot_metrics()),
        services: host.map(|h| h.service_briefs()).unwrap_or_default(),
        active_alerts: host.map(|h| h.active_alerts).unwrap_or(0),
        generated_at: now,
    }
}

// ---------------------------------------------------------------------------
// POST /report
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ReportAck {
    status: &'static str,
    host_id: String,
    seq: u64,
}

/// 处理上报：验签 → 解析 → 去重落库 → 台账 → 即时告警通道 → 推送 Bark。
async fn handle_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let secret = state.secrets.shared_secret.as_bytes();

    // ---- 1) 验签（无签名/坏签名/超窗一律 401，SPEC §5）----
    let Some(ts_header) = headers.get(TIMESTAMP_HEADER) else {
        tracing::warn!("拒绝未签名的 /report（缺少 {TIMESTAMP_HEADER}）");
        return unauthorized("缺少 X-SSM-Timestamp");
    };
    let Some(sig_header) = headers.get(SIGNATURE_HEADER) else {
        tracing::warn!("拒绝未签名的 /report（缺少 {SIGNATURE_HEADER}）");
        return unauthorized("缺少 X-SSM-Signature");
    };
    let (Ok(ts_raw), Ok(sig)) = (ts_header.to_str(), sig_header.to_str()) else {
        return unauthorized("签名头不是合法 UTF-8");
    };
    let Ok(timestamp) = ts_raw.trim().parse::<i64>() else {
        tracing::warn!(value = ts_raw, "拒绝 /report：时间戳头不是十进制秒");
        return unauthorized("X-SSM-Timestamp 不是十进制 unix 秒");
    };

    let now = state.relay().now();
    if let Err(e) = verify(secret, timestamp, &body, sig, now.timestamp()) {
        tracing::warn!(error = %e, timestamp, "拒绝 /report：签名校验失败");
        return unauthorized("签名校验失败");
    }

    // ---- 2) 入库（重复包在这里被静默丢弃）----
    let ingest = {
        let mut relay = state.relay();
        relay.ingest_report(&body, now)
    };

    let ingest = match ingest {
        Ok(v) => v,
        Err(IngestError::Decode(msg)) => {
            tracing::warn!(error = %msg, "拒绝 /report：请求体不是合法 ReportPacket");
            return ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "请求体不是合法 ReportPacket",
            }
            .into_response();
        }
        Err(IngestError::Store(e)) => {
            tracing::error!(error = %e, "落库失败");
            return ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "落库失败",
            }
            .into_response();
        }
    };

    if !ingest.accepted {
        tracing::debug!(host = %ingest.host_id, seq = ingest.seq, "重复包已忽略");
        return (
            StatusCode::OK,
            Json(ReportAck {
                status: "duplicate",
                host_id: ingest.host_id,
                seq: ingest.seq,
            }),
        )
            .into_response();
    }

    tracing::info!(
        host = %ingest.host_id,
        seq = ingest.seq,
        alerts = ingest.alerts.len(),
        "收到心跳"
    );

    // ---- 3) 即时告警通道（网络 IO 在 relay 锁外 await）----
    if !ingest.alerts.is_empty() {
        let queued = state.relay().drain_queued();
        let now = state.clock().now();
        let limit = state.relay().params().rate_limit_per_min;
        let mut alerter = state.alerter_lock().await;
        let mut report = crate::alerter::PushReport::default();
        for alert in queued {
            let r = alerter
                .dispatch(
                    crate::alerter::PushRequest {
                        host_id: &alert.host_id,
                        level: alert.level,
                        source: &alert.source,
                        title: &alert.title,
                        summary: &alert.summary,
                    },
                    now,
                    limit,
                )
                .await;
            report.merge(r);
        }
        drop(alerter);
        if !report.failures.is_empty() {
            tracing::error!(failures = report.failures.len(), "部分 Bark 推送最终失败");
        }
    }

    (
        StatusCode::OK,
        Json(ReportAck {
            status: "ok",
            host_id: ingest.host_id,
            seq: ingest.seq,
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// 错误响应
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "status": "error",
                "message": self.message,
            })),
        )
            .into_response()
    }
}

fn unauthorized(message: &'static str) -> Response {
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        message,
    }
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{FakeProbe, FakePush};
    use crate::relay::{humanize_duration, FixedClock};
    use crate::store::Store;
    use crate::test_util::t;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn secrets() -> SecretsConfig {
        SecretsConfig {
            shared_secret: "e2e-secret-0123456789abcdef".to_string(),
            status_token: "e2e-token-abcdef".to_string(),
            bark: BarkConfig {
                endpoint: "http://127.0.0.1:9200".to_string(),
                key: "e2ekey".to_string(),
                group: "server_status".to_string(),
            },
        }
    }

    fn config() -> RelayConfig {
        // 与 ssm-e2e.sh 的 relay.toml 保持一致（握手间隔 1s，便于测试驱动时序）
        toml::from_str(
            r#"
heartbeat_timeout_sec  = 6
handshake_retries      = 3
handshake_interval_sec = 1
handshake_timeout_sec  = 2
repeat_alert_min       = 0
rate_limit_per_min     = 50
[[hosts]]
host_id = "e2e-host"
[[hosts.paths]]
kind = "lan"
url  = "http://127.0.0.1:9101/probe"
"#,
        )
        .unwrap()
    }

    fn app(start: DateTime<Utc>) -> (Router, AppState, Arc<FakePush>) {
        let push = Arc::new(FakePush::new());
        let store = Store::open_in_memory().unwrap();
        let clock = SharedClock::new(Arc::new(FixedClock(start)));
        let relay = Relay::boot_with_store(&config(), store, Arc::new(clock.clone())).unwrap();
        let alerter = Alerter::with_sleep(
            &secrets().bark,
            push.clone() as Arc<dyn crate::net::PushTransport>,
            // 记录型退避：测试不真的等 1s/4s/16s
            Arc::new(|_secs| Box::pin(std::future::ready(()))),
        );
        let probe_transport: Arc<dyn crate::net::ProbeTransport> = Arc::new(FakeProbe::new());
        let state = AppState::new(
            relay,
            alerter,
            Arc::new(secrets()),
            vec!["e2e-host".to_string()],
            clock,
            probe_transport,
        );
        (router(state.clone()), state, push)
    }

    fn signed_request(body: &[u8], ts: i64) -> Request<Body> {
        let sig = sign(b"e2e-secret-0123456789abcdef", ts, body);
        Request::builder()
            .method("POST")
            .uri("/report")
            .header("content-type", "application/json")
            .header(TIMESTAMP_HEADER, ts.to_string())
            .header(SIGNATURE_HEADER, sig)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    async fn body_of(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn packet(host: &str, seq: u64, sent_at: DateTime<Utc>) -> ReportPacket {
        ReportPacket {
            version: PROTOCOL_VERSION,
            host_id: host.to_string(),
            sent_at,
            seq,
            metrics: Metrics {
                cpu_percent: 12.4,
                mem_used_mb: 3120,
                mem_total_mb: 8192,
                disks: vec![DiskUsage {
                    mount: "/".into(),
                    used_gb: 42.1,
                    total_gb: 100.0,
                }],
                load_avg: [0.3, 0.4, 0.4],
                uptime_sec: 1_234_567,
                net: None,
            },
            services: vec![ServiceStatus {
                name: "web-frontend".into(),
                kind: ServiceKind::Http,
                target: "http://127.0.0.1:9299/".into(),
                ok: false,
                latency_ms: None,
                detail: Some("connection refused".into()),
            }],
            git: vec![],
            alerts: vec![],
        }
    }

    // ---------------- /healthz ----------------

    #[tokio::test]
    async fn healthz_is_open() {
        let (app, _, _) = app(t("2026-10-15T01:30:00Z"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (_, json) = body_of(resp).await;
        assert_eq!(json["status"], "ok");
    }

    // ---------------- /status ----------------

    #[tokio::test]
    async fn status_requires_bearer_token() {
        let (app, _, _) = app(t("2026-10-15T01:30:00Z"));

        // 无 Token → 401
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 错 Token → 401
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 非 Bearer 形态 → 401
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "e2e-token-abcdef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 正确 Token → 200
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer e2e-token-abcdef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_shape_matches_docs_section4() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, state, _) = app(start);

        // 从未收到心跳：metrics/last_heartbeat 必须是 null，state=online（宽限期内）
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer e2e-token-abcdef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (code, json) = body_of(resp).await;
        assert_eq!(code, StatusCode::OK);
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        for k in [
            "host_id",
            "state",
            "last_heartbeat",
            "metrics",
            "services",
            "active_alerts",
            "generated_at",
        ] {
            assert!(keys.contains(&k), "缺字段 {k}：{keys:?}");
        }
        assert_eq!(
            keys.len(),
            7,
            "字段必须严格等于 SPEC §4（勿增删）: {keys:?}"
        );
        assert_eq!(json["host_id"], "e2e-host");
        assert_eq!(json["state"], "online");
        assert!(json["last_heartbeat"].is_null());
        assert!(json["metrics"].is_null());
        assert_eq!(json["active_alerts"], 0);
        assert!(json["services"].as_array().unwrap().is_empty());

        // 心跳之后：字段全部填充
        let raw = serde_json::to_vec(&packet("e2e-host", 1, start)).unwrap();
        let resp = app
            .clone()
            .oneshot(signed_request(&raw, start.timestamp()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer e2e-token-abcdef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (_, json) = body_of(resp).await;
        assert_eq!(json["state"], "online");
        assert!(json["last_heartbeat"]
            .as_str()
            .unwrap()
            .starts_with("2026-10-15T01:30:00"));
        assert_eq!(json["metrics"]["cpu_percent"], 12.4);
        assert!((json["metrics"]["mem_percent"].as_f64().unwrap() - 38.08).abs() < 0.1);
        assert!((json["metrics"]["disk_percent"].as_f64().unwrap() - 42.1).abs() < 0.01);
        assert_eq!(json["metrics"]["uptime_sec"], 1_234_567);
        assert_eq!(json["services"][0]["name"], "web-frontend");
        assert_eq!(json["services"][0]["ok"], false);
        assert_eq!(state.relay().store().heartbeat_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn status_reports_suspect_then_offline() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, state, _) = app(start);
        let get_status = |app: Router| async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/status")
                        .header("authorization", "Bearer e2e-token-abcdef")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            body_of(resp).await.1
        };

        let raw = serde_json::to_vec(&packet("e2e-host", 1, start)).unwrap();
        app.clone()
            .oneshot(signed_request(&raw, start.timestamp()))
            .await
            .unwrap();

        // 手动推进 FSM（时间注入）：收到心跳后 91s → SUSPECT
        {
            let mut relay = state.relay();
            relay.tick(start + chrono::Duration::seconds(91));
            assert_eq!(
                relay.machine_of("e2e-host").unwrap().state(),
                HostState::Suspect
            );
        }
        assert_eq!(get_status(app.clone()).await["state"], "suspect");

        // 3 次握手失败 → OFFLINE
        // 时序（docs/03 §3）：心跳超时当刻（+91s）发起第 1 次，之后每 `handshake_interval_sec`
        // （e2e 配置 = 1s）一次 → 第 2 次 +92s、第 3 次 +93s。
        // 注意：+91s 那次握手已经在上面推进 SUSPECT 时发起了。
        {
            let mut relay = state.relay();
            for i in 0..3u32 {
                let at = start + chrono::Duration::seconds(91 + i as i64);
                if i > 0 {
                    assert_eq!(
                        relay.tick(at).probe_now,
                        vec!["e2e-host".to_string()],
                        "第 {} 次握手",
                        i + 1
                    );
                }
                relay.on_probe_result(
                    crate::prober::Judgement::synthesized(
                        "e2e-host",
                        crate::fsm::ProbeVerdict::Offline,
                    ),
                    at,
                );
            }
            assert_eq!(
                relay.machine_of("e2e-host").unwrap().state(),
                HostState::Offline
            );
        }
        let json = get_status(app.clone()).await;
        assert_eq!(json["state"], "offline");
        assert!(json["active_alerts"].as_u64().unwrap() >= 1);
        assert!(
            json["last_heartbeat"].as_str().is_some(),
            "离线也不能丢 last_heartbeat"
        );
    }

    // ---------------- /report 验签 ----------------

    #[tokio::test]
    async fn report_without_signature_is_401() {
        let (app, _, _) = app(t("2026-10-15T01:30:00Z"));
        let body = br#"{"version":1,"host_id":"x","seq":1}"#;

        // 完全无头
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 只有时间戳
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header(TIMESTAMP_HEADER, "1792000000")
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 只有签名
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header(SIGNATURE_HEADER, "deadbeef")
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn report_with_bad_signature_is_401() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, state, _) = app(start);
        let body = serde_json::to_vec(&packet("e2e-host", 1, start)).unwrap();

        // 完全错误的 hex
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header(TIMESTAMP_HEADER, start.timestamp().to_string())
                    .header(SIGNATURE_HEADER, "deadbeef")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 用错密钥签的合法 hex
        let wrong = sign(b"wrong-secret", start.timestamp(), &body);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header(TIMESTAMP_HEADER, start.timestamp().to_string())
                    .header(SIGNATURE_HEADER, wrong)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 时间戳头不是数字
        let sig = sign(b"e2e-secret-0123456789abcdef", start.timestamp(), &body);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header(TIMESTAMP_HEADER, "not-a-number")
                    .header(SIGNATURE_HEADER, sig)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        assert_eq!(state.relay().store().heartbeat_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn report_with_timestamp_301s_off_is_401_but_300s_ok() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, _, _) = app(start);
        let body = serde_json::to_vec(&packet("e2e-host", 1, start)).unwrap();

        // 301s 偏差 → 拒绝
        let resp = app
            .clone()
            .oneshot(signed_request(&body, start.timestamp() - 301))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 300s 偏差 → 通过
        let resp = app
            .clone()
            .oneshot(signed_request(&body, start.timestamp() - 300))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn report_with_correct_signature_is_accepted_and_deduped() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, state, _) = app(start);
        let body = serde_json::to_vec(&packet("e2e-host", 42, start)).unwrap();

        let resp = app
            .clone()
            .oneshot(signed_request(&body, start.timestamp()))
            .await
            .unwrap();
        let (code, json) = body_of(resp).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(json["status"], "ok");
        assert_eq!(json["seq"], 42);

        // 同一个 (host_id, seq) 再发一次 → duplicate，且不重复落库
        let resp = app
            .clone()
            .oneshot(signed_request(&body, start.timestamp()))
            .await
            .unwrap();
        let (code, json) = body_of(resp).await;
        assert_eq!(code, StatusCode::OK, "重复包不是错误");
        assert_eq!(json["status"], "duplicate");
        assert_eq!(state.relay().store().heartbeat_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn report_with_bad_json_after_valid_signature_is_400() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, _, _) = app(start);
        let body = b"{ not json".to_vec();

        let resp = app
            .oneshot(signed_request(&body, start.timestamp()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn report_tampered_body_after_signing_is_401() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, _, _) = app(start);
        let original = serde_json::to_vec(&packet("e2e-host", 1, start)).unwrap();
        let mut tampered = original.clone();
        let last = tampered.len() - 2;
        tampered[last] ^= 0x01;

        let sig = sign(b"e2e-secret-0123456789abcdef", start.timestamp(), &original);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/report")
                    .header(TIMESTAMP_HEADER, start.timestamp().to_string())
                    .header(SIGNATURE_HEADER, sig)
                    .body(Body::from(tampered))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn report_with_alerts_triggers_bark_push() {
        let start = t("2026-10-15T01:30:00Z");
        let (app, _, push) = app(start);
        let mut p = packet("e2e-host", 1, start);
        p.alerts.push(Alert {
            id: "log:test-error".into(),
            level: AlertLevel::Warning,
            source: "log_watcher".into(),
            title: "test-error 命中".into(),
            summary: "matched 1 lines".into(),
            first_seen: start,
        });
        let body = serde_json::to_vec(&p).unwrap();

        let resp = app
            .oneshot(signed_request(&body, start.timestamp()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(push.call_count(), 1, "即时告警通道必须真的推 Bark");
        assert!(push.urls()[0].contains("level=timeSensitive"));
    }

    #[test]
    fn constant_time_eq_matches_normal_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn humanize_duration_is_readable() {
        use chrono::Duration;
        assert_eq!(humanize_duration(Duration::seconds(45)), "45s");
        assert_eq!(humanize_duration(Duration::seconds(90)), "1m30s");
        assert_eq!(humanize_duration(Duration::seconds(3600)), "1h0m");
        assert_eq!(humanize_duration(Duration::seconds(-5)), "0s");
    }
}
