//! 双路握手探测（SPEC §7 `prober.rs` / docs/03 §2）。
//!
//! relay → agent：`GET http://<agent-ip>:9101/probe`，超时 `handshake_timeout_sec`。
//! HTTP 非 2xx、超时、连接失败均计为**一次握手失败**。
//!
//! `hosts[].paths` 里的多条路径**并发**探测（每条路径一个任务），结果按
//! [`Lanes::verdict`] 的判定矩阵汇总。并发用 `JoinSet`：它天然是 `Send`，且不需要
//! 额外引入 `futures` 依赖。

use std::sync::Arc;
use std::time::Duration;

use ssm_common::prelude::*;

use crate::fsm::{LaneResult, Lanes, ProbeVerdict};
use crate::net::ProbeTransport;

/// 探测参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeTiming {
    /// 单次握手超时（秒）——`handshake_timeout_sec`。
    pub handshake_timeout_sec: u64,
}

impl ProbeTiming {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.handshake_timeout_sec.max(1))
    }
}

/// 一条路径的探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneOutcome {
    pub kind: PathKind,
    pub url: String,
    pub result: LaneResult,
    pub latency_ms: u64,
    pub detail: String,
}

/// 一轮双路探测的完整结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub host_id: String,
    pub verdict: ProbeVerdict,
    pub lanes: Vec<LaneOutcome>,
}

impl Judgement {
    /// 构造一个与 `verdict` 自洽的判定（测试与降级路径用）。
    #[cfg(test)]
    pub fn synthesized(host_id: &str, verdict: ProbeVerdict) -> Self {
        let (lan, ts) = Self::lanes_for_verdict(verdict);
        let mut lanes: Vec<LaneOutcome> = Vec::new();
        for (kind, result) in [(PathKind::Lan, lan), (PathKind::Tailscale, ts)] {
            if let Some(result) = result {
                lanes.push(LaneOutcome {
                    kind,
                    url: format!("http://probe.invalid/{kind:?}"),
                    result,
                    latency_ms: 0,
                    detail: String::new(),
                });
            }
        }
        Self {
            host_id: host_id.to_string(),
            verdict,
            lanes,
        }
    }

    /// 便捷构造：由判定矩阵的**结论**反推出两条车道的原始结果。
    ///
    /// FSM 只吃 `lanes`（再自己算 `verdict`），所以测试/降级路径必须保证两者自洽。
    #[cfg(test)]
    fn lanes_for_verdict(verdict: ProbeVerdict) -> (Option<LaneResult>, Option<LaneResult>) {
        let up = LaneResult::Reachable;
        let down = LaneResult::Unreachable;
        match verdict {
            ProbeVerdict::Online => (Some(up), Some(up)),
            ProbeVerdict::OnlineTunnelDown => (Some(up), Some(down)),
            ProbeVerdict::Offline => (Some(down), Some(down)),
            ProbeVerdict::OnlineLanAnomaly => (Some(down), Some(up)),
        }
    }

    /// 汇总后的车道教给 FSM 的形式。
    pub fn lane_results(&self) -> Lanes {
        let mut lanes = Lanes::default();
        for l in &self.lanes {
            match l.kind {
                PathKind::Lan => lanes.lan = Some(l.result),
                PathKind::Tailscale => lanes.tailscale = Some(l.result),
            }
        }
        lanes
    }

    /// 是否至少一条路径可达。
    #[cfg(test)]
    pub fn any_reachable(&self) -> bool {
        self.lanes.iter().any(|l| l.result == LaneResult::Reachable)
    }

    /// 人读摘要（日志用）。
    pub fn describe(&self) -> String {
        if self.lanes.is_empty() {
            return "未配置探测路径".to_string();
        }
        self.lanes
            .iter()
            .map(|l| {
                format!(
                    "{:?}={}{}",
                    l.kind,
                    if l.result == LaneResult::Reachable {
                        "通"
                    } else {
                        "不通"
                    },
                    if l.detail.is_empty() {
                        String::new()
                    } else {
                        format!("({})", l.detail)
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 并发探测一台主机的全部路径。
///
/// `host.paths` 为空时返回一个「无路径」判定（矩阵退化为 OFFLINE）——调用方负责决定
/// 是否把这种「配置缺失」当成真实离线。
pub async fn probe_host(
    transport: Arc<dyn ProbeTransport>,
    host: HostConfig,
    timing: ProbeTiming,
) -> Judgement {
    let mut set = tokio::task::JoinSet::new();
    for path in &host.paths {
        let transport = transport.clone();
        let kind = path.kind;
        let url = path.url.clone();
        let timeout = timing.timeout();
        set.spawn(async move { probe_lane(transport, kind, url, timeout).await });
    }

    let mut lanes = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(outcome) => lanes.push(outcome),
            Err(e) => tracing::warn!(error = %e, "握手探测任务 panic"),
        }
    }
    // JoinSet 完成顺序不确定 → 排序，保证日志与判定稳定。
    lanes.sort_by_key(|l| match l.kind {
        PathKind::Lan => 0,
        PathKind::Tailscale => 1,
    });

    let lanes_struct = {
        let mut ls = Lanes::default();
        for l in &lanes {
            match l.kind {
                PathKind::Lan => ls.lan = Some(l.result),
                PathKind::Tailscale => ls.tailscale = Some(l.result),
            }
        }
        ls
    };

    Judgement {
        host_id: host.host_id,
        verdict: lanes_struct.verdict(),
        lanes,
    }
}

/// 探测单条路径。
pub async fn probe_lane(
    transport: Arc<dyn ProbeTransport>,
    kind: PathKind,
    url: String,
    timeout: Duration,
) -> LaneOutcome {
    let started = std::time::Instant::now();
    let resp = transport.get(&url, timeout).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let (result, detail) = evaluate(&resp);
    LaneOutcome {
        kind,
        url,
        result,
        latency_ms,
        detail,
    }
}

/// 判定单次握手的成败（`GET /probe` 语义）。
fn evaluate(resp: &crate::net::HttpResponse) -> (LaneResult, String) {
    if !resp.is_success() {
        return (LaneResult::Unreachable, resp.describe());
    }
    // 2xx：按 docs/03 §2 解析 ProbeResponse。agent 的 `/probe` 就是它，
    // 只要明确告诉我们 `ok:false` 才算失败；响应体不是 JSON 时按「活着」处理
    // （协议演进时不要因为多了字段/换了格式就误判离线）。
    match resp.json::<ProbeResponse>() {
        Ok(p) if !p.ok => (
            LaneResult::Unreachable,
            format!("agent 自报 ok=false（uptime {}s）", p.uptime_sec),
        ),
        Ok(p) => (
            LaneResult::Reachable,
            format!("ok, uptime {}s", p.uptime_sec),
        ),
        Err(e) => (
            LaneResult::Reachable,
            format!("2xx 但响应体非 ProbeResponse: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{FakeProbe, HttpResponse};
    use crate::test_util::http_error;

    const LAN: &str = "http://192.168.1.10:9101/probe";
    const TS: &str = "http://100.64.0.10:9101/probe";

    fn host(paths: Vec<(PathKind, &str)>) -> HostConfig {
        HostConfig {
            host_id: "kebei-prod-01".to_string(),
            paths: paths
                .into_iter()
                .map(|(kind, url)| ProbePath {
                    kind,
                    url: url.to_string(),
                })
                .collect(),
        }
    }

    fn ok_body(host_id: &str) -> HttpResponse {
        HttpResponse::ok(
            200,
            format!(r#"{{"ok": true, "host_id": "{host_id}", "uptime_sec": 1234567}}"#),
        )
    }

    fn timing() -> ProbeTiming {
        ProbeTiming {
            handshake_timeout_sec: 5,
        }
    }

    async fn judge(paths: Vec<(PathKind, &str)>, lan: HttpResponse, ts: HttpResponse) -> Judgement {
        let probe = FakeProbe::new().route(LAN, lan).route(TS, ts);
        probe_host(Arc::new(probe), host(paths), timing()).await
    }

    /// docs/03 §2 判定矩阵四种组合。
    #[tokio::test]
    async fn matrix_online_when_both_paths_reachable() {
        let j = judge(
            vec![(PathKind::Lan, LAN), (PathKind::Tailscale, TS)],
            ok_body("kebei-prod-01"),
            ok_body("kebei-prod-01"),
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::Online);
        assert_eq!(
            j.lane_results(),
            Lanes::new(Some(LaneResult::Reachable), Some(LaneResult::Reachable))
        );
        assert!(j.any_reachable());
        assert_eq!(j.lanes.len(), 2);
        assert_eq!(j.lanes[0].kind, PathKind::Lan, "结果必须排序且 lan 在前");
        assert!(j.describe().contains("Lan=通"));
        assert!(j.lanes[0].detail.contains("uptime 1234567"));
    }

    #[tokio::test]
    async fn matrix_tunnel_down_when_lan_ok_tailscale_dead() {
        let j = judge(
            vec![(PathKind::Lan, LAN), (PathKind::Tailscale, TS)],
            ok_body("kebei-prod-01"),
            HttpResponse::failure("请求失败: connection refused"),
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::OnlineTunnelDown);
        assert!(j.any_reachable());
        assert!(j.describe().contains("Tailscale=不通"));
    }

    #[tokio::test]
    async fn matrix_offline_when_both_dead() {
        let j = judge(
            vec![(PathKind::Lan, LAN), (PathKind::Tailscale, TS)],
            HttpResponse::failure("超时"),
            http_error(Some(500), "agent 500"),
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::Offline);
        assert!(!j.any_reachable());
    }

    #[tokio::test]
    async fn matrix_lan_anomaly_when_lan_dead_tailscale_ok() {
        let j = judge(
            vec![(PathKind::Lan, LAN), (PathKind::Tailscale, TS)],
            HttpResponse::failure("超时"),
            ok_body("kebei-prod-01"),
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::OnlineLanAnomaly);
    }

    #[tokio::test]
    async fn single_path_deployment_degrades_to_online_or_offline() {
        let j = judge(
            vec![(PathKind::Tailscale, TS)],
            HttpResponse::failure("未配置"),
            ok_body("h"),
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::Online);
        assert_eq!(j.lanes.len(), 1);

        let j = judge(
            vec![(PathKind::Tailscale, TS)],
            HttpResponse::failure("未配置"),
            HttpResponse::failure("超时"),
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::Offline);
    }

    #[tokio::test]
    async fn no_paths_configured_yields_offline_without_requests() {
        let probe = FakeProbe::new();
        let fake = Arc::new(probe);
        let j = probe_host(fake.clone(), host(vec![]), timing()).await;
        assert_eq!(j.verdict, ProbeVerdict::Offline);
        assert!(j.lanes.is_empty());
        assert_eq!(j.describe(), "未配置探测路径");
        assert!(fake.called_urls().is_empty(), "没有路径就不该发请求");
    }

    #[tokio::test]
    async fn non_2xx_is_a_failed_handshake() {
        for status in [301_u16, 401, 403, 404, 500, 503] {
            let j = judge(
                vec![(PathKind::Lan, LAN)],
                http_error(Some(status), ""),
                ok_body("h"),
            )
            .await;
            assert_eq!(
                j.verdict,
                ProbeVerdict::Offline,
                "HTTP {status} 必须算握手失败"
            );
            assert!(j.lanes[0].detail.contains(&status.to_string()));
        }
    }

    #[tokio::test]
    async fn agent_reporting_ok_false_counts_as_failure() {
        let body = HttpResponse::ok(200, r#"{"ok": false, "host_id": "h", "uptime_sec": 3}"#);
        let j = judge(vec![(PathKind::Lan, LAN)], body, ok_body("h")).await;
        assert_eq!(j.verdict, ProbeVerdict::Offline);
        assert!(j.lanes[0].detail.contains("ok=false"));
    }

    #[tokio::test]
    async fn non_json_2xx_body_is_still_reachable() {
        // 协议演进（多字段/换格式）不应把在线机器判成离线
        let body = HttpResponse::ok(200, "<html>ok</html>");
        let j = judge(vec![(PathKind::Lan, LAN)], body, ok_body("h")).await;
        assert_eq!(j.verdict, ProbeVerdict::Online);
        assert!(j.lanes[0].detail.contains("非 ProbeResponse"));
    }

    #[tokio::test]
    async fn latency_is_measured() {
        let j = judge(vec![(PathKind::Lan, LAN)], ok_body("h"), ok_body("h")).await;
        assert_eq!(j.lanes.len(), 1);
        let _ = j.lanes[0].latency_ms; // 只要能被记录即可（不依赖真实耗时）
        assert!(j.lanes[0].url.ends_with("/probe"));
    }

    #[test]
    fn timing_timeout_has_a_floor_of_one_second() {
        assert_eq!(
            ProbeTiming {
                handshake_timeout_sec: 5
            }
            .timeout(),
            Duration::from_secs(5)
        );
        assert_eq!(
            ProbeTiming {
                handshake_timeout_sec: 0
            }
            .timeout(),
            Duration::from_secs(1)
        );
    }

    /// 真实 HTTP 传输对不可达地址必须超时失败（不 panic、不挂死）。
    #[tokio::test]
    async fn http_transport_reports_failure_for_dead_address() {
        let transport = crate::net::HttpTransport::new().unwrap();
        // 127.0.0.1:1 上不会有服务
        let out = probe_lane(
            Arc::new(transport),
            PathKind::Lan,
            "http://127.0.0.1:1/probe".to_string(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(out.result, LaneResult::Unreachable);
        assert!(!out.detail.is_empty());
    }

    /// 真实 HTTP 传输对真 agent（这里用最小 axum 服务模拟）必须判定可达。
    #[tokio::test]
    async fn http_transport_reaches_a_live_probe_endpoint() {
        use axum::{routing::get, Json, Router};

        let app = Router::new().route(
            "/probe",
            get(|| async {
                Json(ProbeResponse {
                    ok: true,
                    host_id: "kebei-prod-01".to_string(),
                    uptime_sec: 42,
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let transport = Arc::new(crate::net::HttpTransport::new().unwrap());
        let out = probe_lane(
            transport,
            PathKind::Lan,
            format!("http://{addr}/probe"),
            Duration::from_secs(2),
        )
        .await;
        server.abort();
        assert_eq!(out.result, LaneResult::Reachable, "detail={}", out.detail);
        assert!(out.detail.contains("uptime 42"));

        // 顺带覆盖「真实 HttpTransport + probe_host」这条组合
        let host = host(vec![(PathKind::Lan, "http://127.0.0.1:1/probe")]);
        let j = probe_host(
            Arc::new(crate::net::HttpTransport::new().unwrap()),
            host,
            ProbeTiming {
                handshake_timeout_sec: 1,
            },
        )
        .await;
        assert_eq!(j.verdict, ProbeVerdict::Offline);
    }
}
