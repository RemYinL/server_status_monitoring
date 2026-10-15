//! 协议契约（SPEC §4 / docs/03）。
//!
//! **本文件是线上 JSON 字段名的唯一权威，字段名严格对齐 SPEC §4，不得增删。**
//! 时间统一 `chrono::DateTime<Utc>`，JSON 为 RFC3339（`2026-10-15T01:30:00Z`）；
//! 枚举一律 `#[serde(rename_all = "lowercase")]`；可选集合字段一律 `#[serde(default)]`。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 协议版本，relay 按版本解析，向前兼容。
pub const PROTOCOL_VERSION: u32 = 1;

/// 上报数据包：agent → relay，`POST /report`（docs/03 §1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportPacket {
    /// 协议版本，见 [`PROTOCOL_VERSION`]。
    pub version: u32,
    pub host_id: String,
    pub sent_at: DateTime<Utc>,
    /// 单调递增序号；relay 仅用于检测丢包/乱序，不告警。
    pub seq: u64,
    pub metrics: Metrics,
    #[serde(default)]
    pub services: Vec<ServiceStatus>,
    #[serde(default)]
    pub git: Vec<GitStatus>,
    #[serde(default)]
    pub alerts: Vec<Alert>,
}

/// 系统指标快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub cpu_percent: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub disks: Vec<DiskUsage>,
    /// 1 / 5 / 15 分钟平均负载。
    pub load_avg: [f64; 3],
    pub uptime_sec: u64,
    pub net: Option<NetThroughput>,
}

/// 单个挂载点的磁盘占用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskUsage {
    pub mount: String,
    pub used_gb: f64,
    pub total_gb: f64,
}

/// 瞬时网络吞吐（KiB/s）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetThroughput {
    pub rx_kbps: f64,
    pub tx_kbps: f64,
}

/// 探针类型：`http | tcp | process | tailscale`（docs/03 §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Http,
    Tcp,
    Process,
    Tailscale,
}

/// 单个服务探针的探测结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub kind: ServiceKind,
    pub target: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub detail: Option<String>,
}

/// 指定 git 仓库的状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitStatus {
    pub repo: String,
    pub branch: String,
    pub head: String,
    /// 工作区是否有未提交改动。
    pub dirty: bool,
    /// 与上次采集相比 HEAD 是否变化（首次采集不置 true）。
    pub changed: bool,
}

/// 告警级别：`info | warning | critical`（docs/03 §1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertLevel {
    Info,
    Warning,
    Critical,
}

/// 一条告警。`id` 是稳定去重键，如 `"log:nginx:error"`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub id: String,
    pub level: AlertLevel,
    /// `"log_watcher" | "service_probe" | "git_tracker" | "thresholds"`。
    pub source: String,
    pub title: String,
    pub summary: String,
    pub first_seen: DateTime<Utc>,
}

/// agent `GET /probe` 响应（docs/03 §2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub ok: bool,
    pub host_id: String,
    pub uptime_sec: u64,
}

/// relay 侧主机状态，**只有三值**。
///
/// 双路探测结论 `ONLINE_TUNNEL_DOWN` / `ONLINE_LAN_ANOMALY` 不进 `state`，
/// 而是以 `warning` 级 [`Alert`] 呈现并计入 `active_alerts`（SPEC §4 注）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostState {
    Online,
    Suspect,
    Offline,
}

/// `/status` 中的精简指标。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotMetrics {
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub disk_percent: f32,
    pub uptime_sec: u64,
}

/// relay `GET /status` 响应（字段严格按 docs/03 §4，勿增删）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub host_id: String,
    pub state: HostState,
    pub last_heartbeat: Option<DateTime<Utc>>,
    /// 从未收到心跳时为 `None`。
    pub metrics: Option<SnapshotMetrics>,
    pub services: Vec<ServiceBrief>,
    pub active_alerts: usize,
    pub generated_at: DateTime<Utc>,
}

/// `/status` 中的服务灯。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceBrief {
    pub name: String,
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/03 §1 的字面示例 JSON（**不要改写**：这是防协议漂移的锚点）。
    pub(crate) const DOC_REPORT_PACKET_JSON: &str = r#"{
  "version": 1,
  "host_id": "kebei-prod-01",
  "sent_at": "2026-10-15T01:30:00Z",
  "seq": 10234,
  "metrics": {
    "cpu_percent": 12.4,
    "mem_used_mb": 3120,
    "mem_total_mb": 8192,
    "disks": [
      {"mount": "/", "used_gb": 42.1, "total_gb": 100.0}
    ],
    "load_avg": [0.32, 0.41, 0.38],
    "uptime_sec": 1234567,
    "net": {"rx_kbps": 120.5, "tx_kbps": 88.2}
  },
  "services": [
    {"name": "web-frontend", "kind": "http", "target": "https://example.com", "ok": true, "latency_ms": 145, "detail": "200 OK"},
    {"name": "tailscale", "kind": "tailscale", "target": "100.x.x.x", "ok": true, "latency_ms": 23, "detail": "direct"}
  ],
  "git": [
    {"repo": "kebei-web", "branch": "main", "head": "a1b2c3d", "dirty": false, "changed": false}
  ],
  "alerts": [
    {
      "id": "log:nginx:error:20261015-0130",
      "level": "warning",
      "source": "log_watcher",
      "title": "nginx 出现 ERROR 日志",
      "summary": "upstream timed out (matched 3 lines)",
      "first_seen": "2026-10-15T01:29:40Z"
    }
  ]
}"#;

    /// docs/03 §4 的字面示例 JSON（**不要改写**）。
    pub(crate) const DOC_STATUS_SNAPSHOT_JSON: &str = r#"{
  "host_id": "kebei-prod-01",
  "state": "online",
  "last_heartbeat": "2026-10-15T01:30:22Z",
  "metrics": { "cpu_percent": 12.4, "mem_percent": 38.1, "disk_percent": 42.1, "uptime_sec": 1234567 },
  "services": [{"name": "web-frontend", "ok": true}, {"name": "tailscale", "ok": true}],
  "active_alerts": 0,
  "generated_at": "2026-10-15T01:30:25Z"
}"#;

    /// docs/03 §2 的字面示例 JSON（**不要改写**）。
    pub(crate) const DOC_PROBE_RESPONSE_JSON: &str =
        r#"{"ok": true, "host_id": "kebei-prod-01", "uptime_sec": 1234567}"#;

    fn doc_rfc3339(s: &str) -> DateTime<Utc> {
        s.parse::<DateTime<Utc>>()
            .unwrap_or_else(|e| panic!("文档里的时间戳 {s:?} 必须是 RFC3339: {e}"))
    }

    /// RFC3339 往返：chrono 必须能解析文档写法，序列化回同样文字。
    #[test]
    fn rfc3339_sent_at_roundtrip_matches_doc_wire_format() {
        let ts = doc_rfc3339("2026-10-15T01:30:00Z");
        assert_eq!(
            ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "2026-10-15T01:30:00Z"
        );
        // serde 默认格式必须与文档一致（含 Z 后缀）
        assert_eq!(
            serde_json::to_string(&ts).unwrap(),
            "\"2026-10-15T01:30:00Z\""
        );
        let back: DateTime<Utc> = serde_json::from_str("\"2026-10-15T01:30:00Z\"").unwrap();
        assert_eq!(back, ts);
    }

    /// **关键测试**：docs/03 §1 字面 JSON → 逐字段断言。
    #[test]
    fn deserialize_docs_section1_report_packet_field_by_field() {
        let p: ReportPacket =
            serde_json::from_str(DOC_REPORT_PACKET_JSON).expect("docs/03 §1 JSON 必须可解析");

        assert_eq!(p.version, PROTOCOL_VERSION);
        assert_eq!(p.host_id, "kebei-prod-01");
        assert_eq!(p.sent_at, doc_rfc3339("2026-10-15T01:30:00Z"));
        assert_eq!(p.seq, 10234);

        assert_eq!(p.metrics.cpu_percent, 12.4_f32);
        assert_eq!(p.metrics.mem_used_mb, 3120);
        assert_eq!(p.metrics.mem_total_mb, 8192);
        assert_eq!(p.metrics.load_avg, [0.32, 0.41, 0.38]);
        assert_eq!(p.metrics.uptime_sec, 1_234_567);
        assert_eq!(p.metrics.disks.len(), 1);
        assert_eq!(p.metrics.disks[0].mount, "/");
        assert_eq!(p.metrics.disks[0].used_gb, 42.1);
        assert_eq!(p.metrics.disks[0].total_gb, 100.0);
        let net = p.metrics.net.expect("docs 示例中 net 非 null");
        assert_eq!(net.rx_kbps, 120.5);
        assert_eq!(net.tx_kbps, 88.2);

        assert_eq!(p.services.len(), 2);
        assert_eq!(p.services[0].name, "web-frontend");
        assert_eq!(p.services[0].kind, ServiceKind::Http);
        assert_eq!(p.services[0].target, "https://example.com");
        assert!(p.services[0].ok);
        assert_eq!(p.services[0].latency_ms, Some(145));
        assert_eq!(p.services[0].detail.as_deref(), Some("200 OK"));
        assert_eq!(p.services[1].name, "tailscale");
        assert_eq!(p.services[1].kind, ServiceKind::Tailscale);
        assert_eq!(p.services[1].target, "100.x.x.x");
        assert!(p.services[1].ok);
        assert_eq!(p.services[1].latency_ms, Some(23));
        assert_eq!(p.services[1].detail.as_deref(), Some("direct"));

        assert_eq!(p.git.len(), 1);
        assert_eq!(p.git[0].repo, "kebei-web");
        assert_eq!(p.git[0].branch, "main");
        assert_eq!(p.git[0].head, "a1b2c3d");
        assert!(!p.git[0].dirty);
        assert!(!p.git[0].changed);

        assert_eq!(p.alerts.len(), 1);
        assert_eq!(p.alerts[0].id, "log:nginx:error:20261015-0130");
        assert_eq!(p.alerts[0].level, AlertLevel::Warning);
        assert_eq!(p.alerts[0].source, "log_watcher");
        assert_eq!(p.alerts[0].title, "nginx 出现 ERROR 日志");
        assert_eq!(p.alerts[0].summary, "upstream timed out (matched 3 lines)");
        assert_eq!(p.alerts[0].first_seen, doc_rfc3339("2026-10-15T01:29:40Z"));
    }

    /// **关键测试**：docs/03 §4 字面 JSON → 逐字段断言。
    #[test]
    fn deserialize_docs_section4_status_snapshot_field_by_field() {
        let s: StatusSnapshot =
            serde_json::from_str(DOC_STATUS_SNAPSHOT_JSON).expect("docs/03 §4 JSON 必须可解析");

        assert_eq!(s.host_id, "kebei-prod-01");
        assert_eq!(s.state, HostState::Online);
        assert_eq!(s.last_heartbeat, Some(doc_rfc3339("2026-10-15T01:30:22Z")));
        assert_eq!(s.active_alerts, 0);
        assert_eq!(s.generated_at, doc_rfc3339("2026-10-15T01:30:25Z"));

        let m = s.metrics.expect("docs 示例中 metrics 非 null");
        assert_eq!(m.cpu_percent, 12.4_f32);
        assert_eq!(m.mem_percent, 38.1_f32);
        assert_eq!(m.disk_percent, 42.1_f32);
        assert_eq!(m.uptime_sec, 1_234_567);

        assert_eq!(s.services.len(), 2);
        assert_eq!(
            s.services[0],
            ServiceBrief {
                name: "web-frontend".into(),
                ok: true
            }
        );
        assert_eq!(
            s.services[1],
            ServiceBrief {
                name: "tailscale".into(),
                ok: true
            }
        );
    }

    /// docs/03 §2 的 `/probe` 响应字面 JSON。
    #[test]
    fn deserialize_docs_section2_probe_response_field_by_field() {
        let r: ProbeResponse =
            serde_json::from_str(DOC_PROBE_RESPONSE_JSON).expect("docs/03 §2 JSON 必须可解析");
        assert!(r.ok);
        assert_eq!(r.host_id, "kebei-prod-01");
        assert_eq!(r.uptime_sec, 1_234_567);
    }

    /// 枚举的线格式必须是 docs 里的小写字面量。
    #[test]
    fn enum_wire_format_is_lowercase() {
        for (kind, wire) in [
            (ServiceKind::Http, "http"),
            (ServiceKind::Tcp, "tcp"),
            (ServiceKind::Process, "process"),
            (ServiceKind::Tailscale, "tailscale"),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
            assert_eq!(
                serde_json::from_str::<ServiceKind>(&format!("\"{wire}\"")).unwrap(),
                kind
            );
        }
        for (level, wire) in [
            (AlertLevel::Info, "info"),
            (AlertLevel::Warning, "warning"),
            (AlertLevel::Critical, "critical"),
        ] {
            assert_eq!(
                serde_json::to_string(&level).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<AlertLevel>(&format!("\"{wire}\"")).unwrap(),
                level
            );
        }
        for (state, wire) in [
            (HostState::Online, "online"),
            (HostState::Suspect, "suspect"),
            (HostState::Offline, "offline"),
        ] {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<HostState>(&format!("\"{wire}\"")).unwrap(),
                state
            );
        }
    }

    #[test]
    fn report_packet_serde_roundtrip() {
        let packet: ReportPacket = serde_json::from_str(DOC_REPORT_PACKET_JSON).unwrap();
        let json = serde_json::to_string(&packet).unwrap();
        let back: ReportPacket = serde_json::from_str(&json).unwrap();
        assert_eq!(back, packet, "serde 往返必须无损");
        // 再序列化一次必须字节级一致（无隐藏状态）
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
        // 顶层字段名必须与契约一致
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["alerts", "git", "host_id", "metrics", "sent_at", "seq", "services", "version"]
        );
    }

    #[test]
    fn status_snapshot_serde_roundtrip() {
        let snap: StatusSnapshot = serde_json::from_str(DOC_STATUS_SNAPSHOT_JSON).unwrap();
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<StatusSnapshot>(&json).unwrap(), snap);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "active_alerts",
                "generated_at",
                "host_id",
                "last_heartbeat",
                "metrics",
                "services",
                "state"
            ]
        );
    }

    #[test]
    fn probe_response_serde_roundtrip() {
        let probe: ProbeResponse = serde_json::from_str(DOC_PROBE_RESPONSE_JSON).unwrap();
        let json = serde_json::to_string(&probe).unwrap();
        assert_eq!(serde_json::from_str::<ProbeResponse>(&json).unwrap(), probe);
    }

    #[test]
    fn metrics_and_children_serde_roundtrip() {
        let metrics = Metrics {
            cpu_percent: 7.5,
            mem_used_mb: 100,
            mem_total_mb: 200,
            disks: vec![
                DiskUsage {
                    mount: "/".into(),
                    used_gb: 1.0,
                    total_gb: 2.0,
                },
                DiskUsage {
                    mount: "/data".into(),
                    used_gb: 3.5,
                    total_gb: 9.0,
                },
            ],
            load_avg: [1.0, 2.0, 3.0],
            uptime_sec: 42,
            net: Some(NetThroughput {
                rx_kbps: 1.25,
                tx_kbps: 2.5,
            }),
        };
        let json = serde_json::to_string(&metrics).unwrap();
        assert_eq!(serde_json::from_str::<Metrics>(&json).unwrap(), metrics);

        // net = null 必须能解析（agent 首轮无基线时）
        let no_net = json.replace(r#""net":{"rx_kbps":1.25,"tx_kbps":2.5}"#, r#""net":null"#);
        assert!(
            no_net.contains(r#""net":null"#),
            "替换失败，检查 JSON 字段顺序: {no_net}"
        );
        let parsed: Metrics = serde_json::from_str(&no_net).unwrap();
        assert_eq!(parsed.net, None);
    }

    #[test]
    fn service_status_and_alert_and_git_roundtrip() {
        let svc = ServiceStatus {
            name: "web".into(),
            kind: ServiceKind::Tcp,
            target: "127.0.0.1:80".into(),
            ok: false,
            latency_ms: None,
            detail: Some("connection refused".into()),
        };
        let json = serde_json::to_string(&svc).unwrap();
        assert_eq!(serde_json::from_str::<ServiceStatus>(&json).unwrap(), svc);
        assert!(json.contains(r#""latency_ms":null"#));

        let alert = Alert {
            id: "log:nginx:error".into(),
            level: AlertLevel::Critical,
            source: "log_watcher".into(),
            title: "t".into(),
            summary: "s".into(),
            first_seen: doc_rfc3339("2026-10-15T01:29:40Z"),
        };
        let json = serde_json::to_string(&alert).unwrap();
        assert_eq!(serde_json::from_str::<Alert>(&json).unwrap(), alert);

        let git = GitStatus {
            repo: "kebei-web".into(),
            branch: "main".into(),
            head: "deadbee".into(),
            dirty: true,
            changed: true,
        };
        let json = serde_json::to_string(&git).unwrap();
        assert_eq!(serde_json::from_str::<GitStatus>(&json).unwrap(), git);

        let brief = ServiceBrief {
            name: "tailscale".into(),
            ok: true,
        };
        let json = serde_json::to_string(&brief).unwrap();
        assert_eq!(serde_json::from_str::<ServiceBrief>(&json).unwrap(), brief);
    }

    /// 可选集合字段必须 `#[serde(default)]`：缺省即空，不是报错。
    #[test]
    fn optional_collections_default_to_empty() {
        let json = r#"{
            "version": 1,
            "host_id": "kebei-prod-01",
            "sent_at": "2026-10-15T01:30:00Z",
            "seq": 0,
            "metrics": {
                "cpu_percent": 0.0,
                "mem_used_mb": 0,
                "mem_total_mb": 0,
                "disks": [],
                "load_avg": [0.0, 0.0, 0.0],
                "uptime_sec": 0,
                "net": null
            }
        }"#;
        let p: ReportPacket = serde_json::from_str(json).expect("services/git/alerts 应可缺省");
        assert!(p.services.is_empty());
        assert!(p.git.is_empty());
        assert!(p.alerts.is_empty());
    }

    /// `/status` 在从未收到心跳时的边界：metrics 为 null。
    #[test]
    fn status_snapshot_allows_null_metrics_and_heartbeat() {
        let json = r#"{
            "host_id": "kebei-prod-01",
            "state": "offline",
            "last_heartbeat": null,
            "metrics": null,
            "services": [],
            "active_alerts": 3,
            "generated_at": "2026-10-15T01:30:25Z"
        }"#;
        let s: StatusSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(s.state, HostState::Offline);
        assert!(s.last_heartbeat.is_none());
        assert!(s.metrics.is_none());
        assert_eq!(s.active_alerts, 3);
    }
}
