//! HTTP 传输抽象。
//!
//! 把「真的发 HTTP」这件事收敛到两个 trait 后面，业务逻辑（探测矩阵、告警分级、
//! 退避重试）就能在单测里用假传输驱动，不必起网络、也不必真的等 1s/4s/16s。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::de::DeserializeOwned;

/// 装箱的 future（让下面的两个 trait 保持 dyn-compatible）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 统一的 HTTP 响应视图。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpResponse {
    /// 拿到响应时的状态码；连接失败/超时为 `None`。
    pub status: Option<u16>,
    /// 传输层错误描述（超时、连接拒绝、TLS 失败……）。非 2xx 时也可能携带。
    pub error: Option<String>,
    /// 响应体；调用方按需解析。
    pub body: Option<String>,
}

impl HttpResponse {
    #[cfg(test)]
    pub fn ok(status: u16, body: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            error: None,
            body: Some(body.into()),
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            status: None,
            error: Some(message.into()),
            body: None,
        }
    }

    /// 是否 2xx。
    pub fn is_success(&self) -> bool {
        matches!(self.status, Some(s) if (200..300).contains(&s))
    }

    /// 人类可读的结果描述（日志/告警 detail 用）。
    pub fn describe(&self) -> String {
        match (self.status, &self.error) {
            (Some(s), Some(e)) => format!("HTTP {s}: {e}"),
            (Some(s), None) => format!("HTTP {s}"),
            (None, Some(e)) => e.clone(),
            (None, None) => "无响应".to_string(),
        }
    }

    /// 解析 JSON 响应体。
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        let body = self.body.as_deref().ok_or("响应体为空")?;
        serde_json::from_str(body).map_err(|e| format!("响应体不是合法 JSON: {e}"))
    }
}

/// 告警推送传输（`alerter` 用）。返回 `Ok(())` 表示这次推送成功。
///
/// 返回 `BoxFuture` 而不是 `impl Future`：`impl Trait` 返回类型会让 trait 失去
/// dyn-compatibility，而我们要用 `Arc<dyn PushTransport>`（生产真发、测试假发）。
pub trait PushTransport: Send + Sync {
    fn post<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), HttpResponse>>;
}

/// 探测传输（`prober` 用）。
pub trait ProbeTransport: Send + Sync {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, HttpResponse>;
}

/// 真实 HTTP 客户端（reqwest 0.13，rustls，无 OpenSSL —— SPEC §0.5）。
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            // SPEC §11：`rustls` 特性已装好 aws-lc-rs provider，不要手工 install_default()。
            client: reqwest::Client::builder()
                .user_agent(concat!("ssm-relay/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }
}

impl PushTransport for HttpTransport {
    fn post<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), HttpResponse>> {
        Box::pin(async move {
            // SPEC §11：`send()` 必须在 RequestBuilder 上调用；先 `.build()` 拿 Request 再 send 会编译失败。
            let resp = self
                .client
                .post(url)
                .send()
                .await
                .map_err(|e| HttpResponse::failure(format!("请求失败: {e}")))?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(HttpResponse {
                    status: Some(status),
                    error: None,
                    body: Some(body),
                })
            }
        })
    }
}

impl ProbeTransport for HttpTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, HttpResponse> {
        Box::pin(async move {
            match self.client.get(url).timeout(timeout).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.ok();
                    HttpResponse {
                        status: Some(status),
                        error: None,
                        body,
                    }
                }
                Err(e) => HttpResponse::failure(format!("请求失败: {e}")),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// 测试用假传输
// ---------------------------------------------------------------------------

/// 记录每一次推送尝试、可编排成功/失败的假推送传输。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakePush {
    inner: std::sync::Mutex<FakePushState>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct FakePushState {
    /// 每次 `post` 调用的 URL。
    pub calls: Vec<String>,
    /// 脚本化的失败次数：前 N 次调用返回失败，之后成功。
    pub fail_first: usize,
}

#[cfg(test)]
impl FakePush {
    pub fn new() -> Self {
        Self::default()
    }

    /// 前 `n` 次调用失败，之后成功。
    pub fn failing_first(n: usize) -> Self {
        let s = Self::default();
        s.inner.lock().unwrap().fail_first = n;
        s
    }

    pub fn urls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }

    pub fn call_count(&self) -> usize {
        self.inner.lock().unwrap().calls.len()
    }
}

#[cfg(test)]
impl PushTransport for FakePush {
    fn post<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), HttpResponse>> {
        Box::pin(async move {
            let mut st = self.inner.lock().unwrap();
            st.calls.push(url.to_string());
            if st.calls.len() <= st.fail_first {
                return Err(HttpResponse::failure("脚本化失败"));
            }
            Ok(())
        })
    }
}

/// 按 URL 返回预设响应的假探测传输。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeProbe {
    routes: std::sync::Mutex<Vec<(String, HttpResponse)>>,
    pub calls: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// 为 URL 设置响应。
    pub fn route(self, url: &str, resp: HttpResponse) -> Self {
        self.routes.lock().unwrap().push((url.to_string(), resp));
        self
    }

    pub fn called_urls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl ProbeTransport for FakeProbe {
    fn get<'a>(&'a self, url: &'a str, _timeout: Duration) -> BoxFuture<'a, HttpResponse> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(url.to_string());
            self.routes
                .lock()
                .unwrap()
                .iter()
                .find(|(u, _)| u == url)
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| HttpResponse::failure("没有为该 URL 配置响应"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_helpers() {
        let ok = HttpResponse::ok(200, r#"{"ok":true}"#);
        assert!(ok.is_success());
        assert_eq!(ok.describe(), "HTTP 200");
        assert_eq!(ok.json::<serde_json::Value>().unwrap()["ok"], true);

        let not_found = HttpResponse {
            status: Some(404),
            error: None,
            body: Some("nope".into()),
        };
        assert!(!not_found.is_success());
        assert!(not_found.json::<serde_json::Value>().is_err());

        let timeout = HttpResponse::failure("请求失败: timeout");
        assert!(!timeout.is_success());
        assert_eq!(timeout.describe(), "请求失败: timeout");
        assert_eq!(HttpResponse::default().describe(), "无响应");

        let mixed = HttpResponse {
            status: Some(500),
            error: Some("内部错误".into()),
            body: None,
        };
        assert_eq!(mixed.describe(), "HTTP 500: 内部错误");
    }

    #[tokio::test]
    async fn fake_probe_returns_configured_route() {
        let probe = FakeProbe::new()
            .route(
                "http://a/probe",
                HttpResponse::ok(200, r#"{"ok":true,"host_id":"h","uptime_sec":1}"#),
            )
            .route("http://b/probe", HttpResponse::failure("拒绝连接"));
        assert!(probe
            .get("http://a/probe", Duration::from_secs(1))
            .await
            .is_success());
        assert!(!probe
            .get("http://b/probe", Duration::from_secs(1))
            .await
            .is_success());
        assert_eq!(probe.called_urls().len(), 2);
    }

    #[tokio::test]
    async fn fake_push_fails_scripted_number_of_times() {
        let push = FakePush::failing_first(2);
        assert!(push.post("http://x/1").await.is_err());
        assert!(push.post("http://x/2").await.is_err());
        assert!(push.post("http://x/3").await.is_ok());
        assert_eq!(push.call_count(), 3);
    }
}
