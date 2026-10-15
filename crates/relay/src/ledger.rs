//! 内存心跳台账（SPEC §7 `ledger.rs`）。
//!
//! 每台主机保留：
//! - 最新完整 [`ReportPacket`]（`/status` 的 `metrics` / `services` 都从这里来）；
//! - `last_heartbeat`（用包的 `sent_at`，docs/03 §1：补报包保留原始时间）；
//! - 判定状态、探测结论、离线起点等 FSM 上下文（重启后从 `store::state` 恢复）；
//! - 活跃告警计数。
//!
//! [`Ledger::restore`] 是 e2e T11 的关键：relay 重启后台账必须带着 `last_heartbeat`
//! 回来，否则「重启」会被误判成「离线」（docs/03 §5）。

use chrono::{DateTime, Utc};
use ssm_common::prelude::*;

use crate::fsm::ProbeVerdict;
use crate::store::HostStateRow;

/// 每台主机的台账条目。
#[derive(Debug, Clone, PartialEq)]
pub struct HostLedger {
    pub host_id: String,
    pub last_heartbeat: Option<DateTime<Utc>>,
    /// 最新一次心跳包（含 metrics / services）。
    pub last_packet: Option<ReportPacket>,
    pub state: HostState,
    pub probe_verdict: Option<ProbeVerdict>,
    pub offline_since: Option<DateTime<Utc>>,
    pub active_alerts: usize,
    pub last_received_at: Option<DateTime<Utc>>,
}

impl HostLedger {
    pub fn new(host_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            last_heartbeat: None,
            last_packet: None,
            state: HostState::Online,
            probe_verdict: None,
            offline_since: None,
            active_alerts: 0,
            last_received_at: None,
        }
    }

    /// `/status` 用的精简指标；从未收到心跳时为 `None`。
    ///
    /// `disk_percent` 取**根挂载点**（`/`）的使用率；没有 `/` 时退化为第一个挂载点；
    /// 一个磁盘都没有则为 `0.0`。
    pub fn snapshot_metrics(&self) -> Option<SnapshotMetrics> {
        let packet = self.last_packet.as_ref()?;
        let m = &packet.metrics;
        let mem_percent = if m.mem_total_mb == 0 {
            0.0
        } else {
            (m.mem_used_mb as f64 / m.mem_total_mb as f64 * 100.0) as f32
        };
        let disk_percent = m
            .disks
            .iter()
            .find(|d| d.mount == "/")
            .or_else(|| m.disks.first())
            .map(|d| {
                if d.total_gb <= 0.0 {
                    0.0
                } else {
                    (d.used_gb / d.total_gb * 100.0) as f32
                }
            })
            .unwrap_or(0.0);
        Some(SnapshotMetrics {
            cpu_percent: m.cpu_percent,
            mem_percent,
            disk_percent,
            uptime_sec: m.uptime_sec,
        })
    }

    /// `/status.services`：取最新心跳包里的服务灯。
    pub fn service_briefs(&self) -> Vec<ServiceBrief> {
        self.last_packet
            .as_ref()
            .map(|p| {
                p.services
                    .iter()
                    .map(|s| ServiceBrief {
                        name: s.name.clone(),
                        ok: s.ok,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 距离 `now` 有多久没收到心跳了。
    #[cfg(test)]
    pub fn age(&self, now: DateTime<Utc>) -> Option<chrono::Duration> {
        self.last_heartbeat.map(|hb| now.signed_duration_since(hb))
    }
}

/// 全量内存台账。
#[derive(Debug, Default)]
pub struct Ledger {
    hosts: std::collections::BTreeMap<String, HostLedger>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记主机（已存在则原样保留）。
    pub fn ensure_host(&mut self, host_id: &str) -> &mut HostLedger {
        self.hosts
            .entry(host_id.to_string())
            .or_insert_with(|| HostLedger::new(host_id))
    }

    /// 主机是否已登记。
    pub fn knows(&self, host_id: &str) -> bool {
        self.hosts.contains_key(host_id)
    }

    pub fn get(&self, host_id: &str) -> Option<&HostLedger> {
        self.hosts.get(host_id)
    }

    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// 已登记的主机 id（按字典序，`BTreeMap` 保证顺序稳定）。
    pub fn host_ids(&self) -> Vec<String> {
        self.hosts.keys().cloned().collect()
    }

    /// 记录一次心跳：更新最新包、`last_heartbeat`、服务灯、接收时间。
    ///
    /// 不在这里改 `state`——状态迁移统一由 `relay::Relay` 交给 FSM 处理。
    pub fn record_heartbeat(
        &mut self,
        packet: ReportPacket,
        received_at: DateTime<Utc>,
    ) -> &mut HostLedger {
        let entry = self.ensure_host(&packet.host_id);
        entry.last_heartbeat = Some(packet.sent_at);
        entry.last_received_at = Some(received_at);
        entry.last_packet = Some(packet);
        entry
    }

    /// 写入判定上下文。
    pub fn set_fsm(
        &mut self,
        host_id: &str,
        state: HostState,
        verdict: Option<ProbeVerdict>,
        offline_since: Option<DateTime<Utc>>,
    ) {
        let entry = self.ensure_host(host_id);
        entry.state = state;
        entry.probe_verdict = verdict;
        entry.offline_since = offline_since;
    }

    pub fn set_active_alerts(&mut self, host_id: &str, count: usize) {
        self.ensure_host(host_id).active_alerts = count;
    }

    /// 从数据库恢复台账（relay 启动时调用一次）。
    ///
    /// 心跳包直接反序列化 `heartbeats.payload`；`state` 表提供 FSM 上下文。
    /// 任何一行坏数据都**不能**拖垮启动：反序列化失败时保留元数据、丢掉包体。
    pub fn restore(heartbeats: &[crate::store::HeartbeatRow], states: &[HostStateRow]) -> Self {
        let mut ledger = Ledger::new();

        for hb in heartbeats {
            let entry = ledger.ensure_host(&hb.host_id);
            entry.last_heartbeat = Some(hb.sent_at);
            entry.last_received_at = Some(hb.received_at);
            match serde_json::from_str::<ReportPacket>(&hb.payload) {
                Ok(packet) => entry.last_packet = Some(packet),
                Err(e) => tracing::warn!(
                    host = %hb.host_id,
                    seq = hb.seq,
                    error = %e,
                    "快照里的心跳包无法解析，仅恢复时间戳"
                ),
            }
        }

        for st in states {
            let verdict = st.probe_verdict.as_deref().and_then(ProbeVerdict::from_str);
            let state = match st.state.as_str() {
                "offline" => HostState::Offline,
                "suspect" => HostState::Suspect,
                _ => HostState::Online,
            };
            let entry = ledger.ensure_host(&st.host_id);
            // `state.last_heartbeat` 比 payload 更权威（它由 FSM 直接写）
            if st.last_heartbeat.is_some() {
                entry.last_heartbeat = st.last_heartbeat;
            }
            entry.state = state;
            entry.probe_verdict = verdict;
            entry.offline_since = st.offline_since;
        }

        ledger
    }

    /// 序列化整个台账（调试/诊断用）。
    #[cfg(test)]
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        #[derive(serde::Serialize)]
        struct Item<'a> {
            host_id: &'a str,
            last_heartbeat: Option<String>,
            state: HostState,
            last_packet: Option<&'a ReportPacket>,
        }
        let items: Vec<Item<'_>> = self
            .hosts
            .values()
            .map(|h| Item {
                host_id: &h.host_id,
                last_heartbeat: h.last_heartbeat.map(crate::store::fmt_time),
                state: h.state,
                last_packet: h.last_packet.as_ref(),
            })
            .collect();
        serde_json::to_string_pretty(&items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::test_util::{t, TempDir};
    use chrono::Duration;

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
                disks: vec![
                    DiskUsage {
                        mount: "/data".into(),
                        used_gb: 50.0,
                        total_gb: 100.0,
                    },
                    DiskUsage {
                        mount: "/".into(),
                        used_gb: 42.1,
                        total_gb: 100.0,
                    },
                ],
                load_avg: [0.3, 0.4, 0.4],
                uptime_sec: 1_234_567,
                net: None,
            },
            services: vec![
                ServiceStatus {
                    name: "web-frontend".into(),
                    kind: ServiceKind::Http,
                    target: "https://example.com".into(),
                    ok: true,
                    latency_ms: Some(145),
                    detail: Some("200 OK".into()),
                },
                ServiceStatus {
                    name: "tailscale".into(),
                    kind: ServiceKind::Tailscale,
                    target: "100.x.x.x".into(),
                    ok: false,
                    latency_ms: None,
                    detail: Some("无响应".into()),
                },
            ],
            git: vec![],
            alerts: vec![],
        }
    }

    #[test]
    fn record_heartbeat_updates_last_heartbeat_and_packet() {
        let mut ledger = Ledger::new();
        let sent = t("2026-10-15T01:30:00Z");
        let recv = t("2026-10-15T01:30:02Z");
        ledger.record_heartbeat(packet("h1", 1, sent), recv);

        let e = ledger.get("h1").unwrap();
        assert_eq!(e.last_heartbeat, Some(sent));
        assert_eq!(e.last_received_at, Some(recv));
        assert_eq!(e.last_packet.as_ref().unwrap().seq, 1);

        // 后到的包覆盖前一个
        let sent2 = t("2026-10-15T01:30:30Z");
        ledger.record_heartbeat(packet("h1", 2, sent2), sent2);
        let e = ledger.get("h1").unwrap();
        assert_eq!(e.last_heartbeat, Some(sent2));
        assert_eq!(e.last_packet.as_ref().unwrap().seq, 2);
    }

    #[test]
    fn snapshot_metrics_maps_protocol_fields() {
        let mut ledger = Ledger::new();
        ledger.record_heartbeat(
            packet("h1", 1, t("2026-10-15T01:30:00Z")),
            t("2026-10-15T01:30:00Z"),
        );
        let m = ledger.get("h1").unwrap().snapshot_metrics().unwrap();
        assert_eq!(m.cpu_percent, 12.4);
        assert_eq!(m.uptime_sec, 1_234_567);
        assert!(
            (m.mem_percent - 38.085_938).abs() < 0.01,
            "{}",
            m.mem_percent
        );
        assert!((m.disk_percent - 42.1).abs() < 0.001, "{}", m.disk_percent);
    }

    #[test]
    fn snapshot_metrics_falls_back_to_first_disk_then_zero() {
        let mut p = packet("h1", 1, t("2026-10-15T01:30:00Z"));
        p.metrics.disks.retain(|d| d.mount != "/");
        let mut ledger = Ledger::new();
        ledger.record_heartbeat(p, t("2026-10-15T01:30:00Z"));
        let m = ledger.get("h1").unwrap().snapshot_metrics().unwrap();
        assert!((m.disk_percent - 50.0).abs() < 0.001);

        let mut p = packet("h2", 1, t("2026-10-15T01:30:00Z"));
        p.metrics.disks.clear();
        p.metrics.mem_total_mb = 0;
        let mut ledger = Ledger::new();
        ledger.record_heartbeat(p, t("2026-10-15T01:30:00Z"));
        let m = ledger.get("h2").unwrap().snapshot_metrics().unwrap();
        assert_eq!(m.disk_percent, 0.0);
        assert_eq!(m.mem_percent, 0.0);
    }

    #[test]
    fn never_heard_host_has_no_metrics() {
        let mut ledger = Ledger::new();
        ledger.ensure_host("h1");
        let e = ledger.get("h1").unwrap();
        assert!(e.snapshot_metrics().is_none());
        assert!(e.service_briefs().is_empty());
        assert_eq!(e.state, HostState::Online);
        assert!(e.age(t("2026-10-15T01:30:00Z")).is_none());
    }

    #[test]
    fn service_briefs_track_latest_packet() {
        let mut ledger = Ledger::new();
        ledger.record_heartbeat(
            packet("h1", 1, t("2026-10-15T01:30:00Z")),
            t("2026-10-15T01:30:00Z"),
        );
        let briefs = ledger.get("h1").unwrap().service_briefs();
        assert_eq!(
            briefs,
            vec![
                ServiceBrief {
                    name: "web-frontend".into(),
                    ok: true
                },
                ServiceBrief {
                    name: "tailscale".into(),
                    ok: false
                },
            ]
        );
    }

    #[test]
    fn age_uses_sent_at_not_received_at() {
        let mut ledger = Ledger::new();
        let sent = t("2026-10-15T01:00:00Z");
        let recv = t("2026-10-15T01:30:00Z"); // 补报：30 分钟后才到
        ledger.record_heartbeat(packet("h1", 1, sent), recv);
        assert_eq!(
            ledger.get("h1").unwrap().age(recv),
            Some(Duration::minutes(30)),
            "补报包必须按 sent_at 计时（docs/03 §1）"
        );
    }

    #[test]
    fn restore_rebuilds_packet_and_fsm_context_from_store() {
        let dir = TempDir::new("ledger-restore");
        let store = Store::open(dir.path().join("l.db"), dir.path().join("s.db")).unwrap();

        let sent = t("2026-10-15T01:30:00Z");
        let p = packet("h1", 9, sent);
        store
            .insert_heartbeat("h1", 9, sent, sent, &serde_json::to_string(&p).unwrap())
            .unwrap();
        store
            .upsert_host_state(&crate::store::HostStateUpsert {
                host_id: "h1".into(),
                state: "offline".into(),
                last_heartbeat: Some(sent),
                probe_verdict: Some("offline".into()),
                offline_since: Some(t("2026-10-15T01:35:00Z")),
                updated_at: sent,
            })
            .unwrap();
        // 另一台主机只有 state 行，没有心跳
        store
            .upsert_host_state(&crate::store::HostStateUpsert {
                host_id: "h2".into(),
                state: "suspect".into(),
                last_heartbeat: None,
                probe_verdict: Some("online_tunnel_down".into()),
                offline_since: None,
                updated_at: sent,
            })
            .unwrap();

        let ledger = Ledger::restore(
            &store.latest_heartbeats().unwrap(),
            &store.all_host_states().unwrap(),
        );

        let h1 = ledger.get("h1").expect("h1 必须被恢复");
        assert_eq!(h1.last_heartbeat, Some(sent));
        assert_eq!(
            h1.last_packet.as_ref().map(|p| p.seq),
            Some(9),
            "必须还原完整 ReportPacket"
        );
        assert_eq!(h1.last_packet.as_ref().unwrap().services.len(), 2);
        assert_eq!(h1.state, HostState::Offline);
        assert_eq!(h1.probe_verdict, Some(ProbeVerdict::Offline));
        assert_eq!(h1.offline_since, Some(t("2026-10-15T01:35:00Z")));
        assert!(h1.snapshot_metrics().is_some());

        let h2 = ledger.get("h2").unwrap();
        assert_eq!(h2.state, HostState::Suspect);
        assert_eq!(h2.probe_verdict, Some(ProbeVerdict::OnlineTunnelDown));
        assert!(h2.last_heartbeat.is_none());
        assert!(h2.last_packet.is_none());
    }

    #[test]
    fn restore_survives_corrupt_payload() {
        let dir = TempDir::new("ledger-corrupt");
        let store = Store::open(dir.path().join("l.db"), dir.path().join("s.db")).unwrap();
        let sent = t("2026-10-15T01:30:00Z");
        store
            .insert_heartbeat("h1", 1, sent, sent, "{ 这不是 JSON")
            .unwrap();
        let ledger = Ledger::restore(&store.latest_heartbeats().unwrap(), &[]);
        let h1 = ledger.get("h1").unwrap();
        assert_eq!(h1.last_heartbeat, Some(sent), "坏包也不能丢掉时间戳");
        assert!(h1.last_packet.is_none());
    }

    #[test]
    fn ledger_json_dump_contains_hosts() {
        let mut ledger = Ledger::new();
        ledger.record_heartbeat(
            packet("h1", 1, t("2026-10-15T01:30:00Z")),
            t("2026-10-15T01:30:00Z"),
        );
        ledger.set_active_alerts("h1", 3);
        let json = ledger.to_json().unwrap();
        assert!(json.contains("\"host_id\": \"h1\""));
        assert!(json.contains("\"state\": \"online\""));
        assert_eq!(ledger.len(), 1);
        assert!(!ledger.is_empty());
        assert_eq!(ledger.host_ids(), vec!["h1".to_string()]);
        assert!(ledger.knows("h1"));
        assert!(!ledger.knows("h2"));
    }
}
