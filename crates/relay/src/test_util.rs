//! 测试辅助：临时目录、固定时间基、假 HTTP 传输。
//!
//! 只在 `cfg(test)` 下编译。

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};

use crate::net::HttpResponse;

/// 固定时间基（2026-10-15T01:30:00Z，与文档示例同一时刻）。
pub fn t(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>()
        .unwrap_or_else(|e| panic!("测试时间戳 {s:?} 必须合法: {e}"))
}

/// 固定时间基的字面量。
pub const T0_STR: &str = "2026-10-15T01:30:00Z";

/// 固定时间基，便于 `use crate::test_util::T0;` 直接当 `&str` 用。
pub const T0: &str = T0_STR;

/// 进程内唯一的自增计数器（临时目录命名用）。
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 测试用临时目录，`Drop` 时递归删除。
pub struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    #[allow(clippy::new_without_default)]
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ssm-relay-test-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建测试临时目录");
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 一次失败的 HTTP 响应（测试断言用）。
pub fn http_error(status: Option<u16>, detail: &str) -> HttpResponse {
    HttpResponse {
        status,
        error: Some(detail.to_string()),
        body: None,
    }
}
