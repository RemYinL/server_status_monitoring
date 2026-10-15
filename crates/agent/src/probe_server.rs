//! `GET /probe` 应答（SPEC §7 `probe_server.rs`）。
//!
//! relay → agent 的握手探测：`GET http://<agent>:9101/probe`，无鉴权，仅用于存活判定
//! （docs/03 §2）。响应体是 [`ProbeResponse`]：
//!
//! ```json
//! {"ok": true, "host_id": "kebei-prod-01", "uptime_sec": 1234567}
//! ```
//!
//! 判定规则：HTTP 非 2xx、超时、TLS/连接失败均计为一次握手失败——所以这个 handler
//! **永远返回 200**，不做任何可能失败的事情（不读磁盘、不查 relay）。

use std::future::Future;

use anyhow::Context;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ssm_common::protocol::ProbeResponse;

/// `/probe` 路由（`main.rs` 与测试共用）。
pub fn router(host_id: impl Into<String>) -> Router {
    Router::new()
        .route("/probe", get(probe))
        .with_state(ProbeState {
            host_id: host_id.into(),
        })
}

#[derive(Clone)]
struct ProbeState {
    host_id: String,
}

async fn probe(State(state): State<ProbeState>) -> Json<ProbeResponse> {
    Json(ProbeResponse {
        ok: true,
        host_id: state.host_id,
        uptime_sec: sysinfo::System::uptime(),
    })
}

/// 在 `listen` 上跑 `/probe` 服务；`shutdown` 完成时优雅关停。
///
/// 绑定失败（端口被占 / 地址非法）返回错误，由 `main.rs` 决定是否致命。
pub async fn serve_with_shutdown<F>(
    listen: &str,
    host_id: String,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("无法绑定 probe_listen = {listen}"))?;
    let addr = listener.local_addr().context("读取 probe 监听地址失败")?;
    tracing::info!(%addr, host_id = %host_id, "probe server 已监听 GET /probe");

    axum::serve(listener, router(host_id))
        .with_graceful_shutdown(shutdown)
        .await
        .context("probe server 运行失败")?;
    tracing::info!("probe server 已停止");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 起真实 server，`GET /probe` 返回 200 且字段严格对齐 docs/03 §2。
    #[tokio::test]
    async fn probe_returns_200_and_documented_fields() {
        let app = router("kebei-prod-01");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定 127.0.0.1:0");
        let addr = listener.local_addr().expect("取本地地址");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = reqwest::get(format!("http://{addr}/probe"))
            .await
            .expect("GET /probe 必须可访问");
        assert_eq!(response.status(), 200);
        assert!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")),
            "Content-Type 必须是 application/json"
        );

        let body = response.bytes().await.expect("读取响应体");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("必须是 JSON");
        assert_eq!(value["ok"], serde_json::Value::Bool(true));
        assert_eq!(value["host_id"], "kebei-prod-01");
        let uptime = value["uptime_sec"].as_u64().expect("uptime_sec 必须是整数");
        assert!(uptime > 0, "uptime_sec 必须 > 0，实际 {uptime}");
        assert_eq!(
            value.as_object().expect("必须是 JSON 对象").len(),
            3,
            "字段必须只有 ok / host_id / uptime_sec，实际 {value}"
        );

        // 反序列化成协议类型后逐字段一致
        let parsed: ProbeResponse = serde_json::from_slice(&body).expect("必须是 ProbeResponse");
        assert_eq!(
            parsed,
            ProbeResponse {
                ok: true,
                host_id: "kebei-prod-01".to_string(),
                uptime_sec: uptime,
            }
        );

        server.abort();
    }

    /// 未注册的路径必须 404（relay 只探 `/probe`，其余不能误判为存活）。
    #[tokio::test]
    async fn unknown_path_is_404() {
        let app = router("h");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = reqwest::get(format!("http://{addr}/status"))
            .await
            .expect("请求应能发出");
        assert_eq!(response.status(), 404);
        server.abort();
    }

    /// `serve_with_shutdown` 在收到停止信号后必须自行退出（优雅退出路径）。
    #[tokio::test]
    async fn serve_with_shutdown_exits_on_signal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // 让 serve_with_shutdown 自己重新绑定

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            serve_with_shutdown(&addr.to_string(), "h".to_string(), async move {
                let _ = rx.await;
            })
            .await
        });

        // 等服务起来
        let mut ready = false;
        for _ in 0..50 {
            match reqwest::get(format!("http://{addr}/probe")).await {
                Ok(response) => {
                    assert_eq!(response.status(), 200);
                    ready = true;
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
        assert!(ready, "probe server 未能在 1s 内就绪");

        tx.send(()).expect("发送停止信号");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("serve_with_shutdown 必须在 5s 内退出")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "优雅关停不应报错: {result:?}");
    }

    /// 端口被占用时必须返回错误（而不是静默什么都不做）。
    #[tokio::test]
    async fn bind_failure_is_reported() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap();

        let result = serve_with_shutdown(&addr.to_string(), "h".to_string(), async {}).await;
        assert!(result.is_err(), "端口冲突必须报错");
        assert!(
            result.unwrap_err().to_string().contains("probe_listen"),
            "错误信息要能看出是 probe_listen 的问题"
        );
    }
}
