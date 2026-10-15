//! relay 核心装配：把 store / ledger / fsm / prober / alerter 串成一个可测对象。
//!
//! **时间是通过 [`Clock`] 注入的**，判定逻辑不直接读 `Utc::now()`，因此单测可以
//! 直接喂时间戳（`Relay::ingest_report` / `Relay::tick` 都接受 `now`），
//! 不需要真的睡 90 秒。
//!
//! 数据流：
//!
//! ```text
//! POST /report ──► ingest_report()  ──► 去重 → 落库 → 台账 → FSM(on_heartbeat)
//!                                       └─► alerts[] 即时告警通道（按 sent_at 过滤过期）
//! tick(now)    ──► FSM(tick)        ──► 需要握手 → 交回调用方 spawn 探测任务
//! on_probe_result() ─► FSM(on_handshake) ─► 判定矩阵结论 → warning 告警 / 恢复通知
//! ```

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ssm_common::prelude::*;

use crate::fsm::{AlertKind, Fsm, FsmEvent, FsmTiming, ProbeVerdict};
use crate::ledger::{HostLedger, Ledger};
use crate::prober::{self, Judgement, ProbeTiming};
use crate::store::{fmt_time, HeartbeatRow, HostStateRow, Store};

/// 可注入时钟。
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// 系统时钟。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// 固定时钟（测试用，保证时间可注入）。
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

#[cfg(test)]
impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// 时钟句柄：可被 `Relay` 与后台任务同时持有。
#[derive(Debug, Clone)]
pub struct SharedClock(Arc<dyn Clock>);

impl SharedClock {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self(clock)
    }
}

impl Clock for SharedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0.now()
    }
}

/// relay 运行参数（由 `relay.toml` 换算而来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayParams {
    pub timing: FsmTiming,
    pub probe: ProbeTiming,
    /// 补报过期阈值：`sent_at` 早于该秒数的告警只入库不推送。
    pub lag_threshold_sec: u64,
    pub rate_limit_per_min: u32,
}

impl RelayParams {
    /// 从 `relay.toml` 构造。
    pub fn from_config(cfg: &RelayConfig) -> Self {
        Self {
            timing: FsmTiming {
                heartbeat_timeout_sec: cfg.heartbeat_timeout_sec,
                handshake_retries: cfg.handshake_retries,
                handshake_interval_sec: cfg.handshake_interval_sec,
                repeat_alert_min: cfg.repeat_alert_min,
                startup_grace_periods: cfg.startup_grace_periods,
            },
            probe: ProbeTiming {
                handshake_timeout_sec: cfg.handshake_timeout_sec,
            },
            lag_threshold_sec: cfg.lag_threshold_sec,
            rate_limit_per_min: cfg.rate_limit_per_min,
        }
    }
}

/// 一条待推送的告警（已过去重与过期判断）。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAlert {
    pub host_id: String,
    pub level: AlertLevel,
    pub source: String,
    pub title: String,
    pub summary: String,
}

/// 告警的生命周期类别（**SPEC §13.2 是唯一权威**）。
///
/// 这个分类决定 relay 怎么解释「本轮 `alerts[]` 里没有它」：
///
/// | 类别 | `source` | 语义 | relay 行为 |
/// |---|---|---|---|
/// | 条件型 | `service_probe`、`thresholds`、`relay`、`prober` | 一个持续存在的状态，条件存续期间 agent **每轮都上报**（同一个稳定 id） | 缺席且此前 active → 推「已恢复」并移出 active 集合 |
/// | 事件型 | `log_watcher`、`git_tracker` | 一个**已经发生**的点事件，只在发生的那一轮出现一次 | 缺席 → 静默移出 active 集合，**绝不**推「已恢复」 |
///
/// 为什么必须分两类（e2e 实测抓出的真实 bug，SPEC §13.1）：日志命中、提交 commit
/// 都是**点事件**，从 `alerts[]` 里消失谈不上「恢复」；而对条件型告警不推恢复，
/// 又会让运维以为故障仍在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertLifecycle {
    /// 条件型：缺席 = 条件消失 → 推 1 条「已恢复」。
    Condition,
    /// 事件型：缺席 = 本轮没有新事件 → 静默移出 active 集合，不推任何通知。
    Event,
}

impl AlertLifecycle {
    /// 稳定字符串（日志字段用）。
    pub fn as_str(self) -> &'static str {
        match self {
            AlertLifecycle::Condition => "condition",
            AlertLifecycle::Event => "event",
        }
    }
}

/// `source` → 生命周期类别（SPEC §13.2 表）。
///
/// **未知 `source` 默认 [`AlertLifecycle::Event`]**：
/// relay 没有任何证据证明它会「条件存续期间每轮重复上报」，而推一条语义错误的
/// 「已恢复」是**主动误导**（SPEC §13.1 后果 1），代价远大于少推一条恢复通知；
/// 默认事件型时本体告警与 active 去重都不受影响，缺席仍会清掉 active 键，
/// 于是同一个事件再次发生时可以重新推送。新增**条件型** source 必须显式登记在这里
/// （`relay::tests::lifecycle_of_*` 钉住了这个默认值）。
pub fn lifecycle_of(source: &str) -> AlertLifecycle {
    match source {
        // 条件型：agent 每轮重复上报；`relay` / `prober` 是 relay 自生成的结论，
        // 由 relay 自己的 FSM 管 active 与恢复，语义同样是「条件」。
        "service_probe" | "thresholds" | "relay" | "prober" => AlertLifecycle::Condition,
        // 事件型：点事件。
        "log_watcher" | "git_tracker" => AlertLifecycle::Event,
        // 未登记 → 按事件型兜底（少推一条恢复通知好过推一条语义错误的）。
        _ => AlertLifecycle::Event,
    }
}

/// `source` 是否在 SPEC §13.2 表里显式登记过（未登记时多打一条 info 便于发现新采集器）。
pub fn is_registered_source(source: &str) -> bool {
    matches!(
        source,
        "service_probe" | "thresholds" | "log_watcher" | "git_tracker" | "relay" | "prober"
    )
}

/// 是否为 relay 自生成的 source。
///
/// 它们的 active 与恢复通知由 relay 自己的 FSM 负责（`mark_recovered` /
/// `resolve_verdict_alerts`），**不**参与「agent 本轮没上报 = 条件消失」这条推导——
/// 否则每来一个不带 relay 告警的上报包都会把「服务器离线」当成已恢复。
pub fn is_relay_generated_source(source: &str) -> bool {
    matches!(source, "relay" | "prober")
}

/// tick 的结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickReport {
    /// 需要发起握手探测的主机 id。
    pub probe_now: Vec<String>,
    /// 本次 tick 产生的告警。
    pub alerts: Vec<PendingAlert>,
    /// 覆盖到 OFFLINE 的主机（日志用）。
    pub went_offline: Vec<String>,
    /// 建议的下一轮 tick 时刻（`None` = 只需按固定周期轮询）。
    pub next_wake: Option<DateTime<Utc>>,
}

/// `/report` 的入库结果。
#[derive(Debug, Clone, PartialEq)]
pub struct IngestReport {
    /// `false` = 重复包（`(host_id, seq)` 已存在），已静默丢弃。
    pub accepted: bool,
    pub host_id: String,
    pub seq: u64,
    pub alerts: Vec<PendingAlert>,
}

/// relay 核心。
///
/// 注意：**Bark 推送器不在本结构体内**（见 `crate::alerter::Alerter`，由调用方用
/// `Arc<Mutex<..>>` 持有）。本结构体的所有方法都是同步的，调用方可以用
/// `MutexGuard` 短临界区访问它，绝不会把锁带进 `await`。
pub struct Relay {
    store: Store,
    ledger: Ledger,
    machines: std::collections::BTreeMap<String, Fsm>,
    params: RelayParams,
    clock: SharedClock,
    startup_at: DateTime<Utc>,
    pending: Vec<PendingAlert>,
    /// 待投递的推送队列（`ingest_report` / `tick` 只入队，不在这里发网络请求）。
    queue: Vec<PendingAlert>,
    /// 探测任务用的配置快照（`[[hosts]]` + 超时）。
    probe_spec: ProbeSpec,
}

/// 探测任务所需的全部输入（与 `Relay` 解耦，便于 `tokio::spawn`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSpec {
    pub hosts: Vec<HostConfig>,
    pub timing: ProbeTiming,
}

impl ProbeSpec {
    pub fn from_config(cfg: &RelayConfig) -> Self {
        Self {
            hosts: cfg.hosts.clone(),
            timing: ProbeTiming {
                handshake_timeout_sec: cfg.handshake_timeout_sec,
            },
        }
    }

    /// 对一台主机跑一轮真实探测。
    pub async fn probe(
        &self,
        transport: Arc<dyn crate::net::ProbeTransport>,
        host: HostConfig,
    ) -> Judgement {
        prober::probe_host(transport, host, self.timing).await
    }
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("hosts", &self.ledger.host_ids())
            .field("params", &self.params)
            .field("startup_at", &self.startup_at)
            .field("queue", &self.queue.len())
            .finish()
    }
}

impl Relay {
    /// 打开台账并装配 relay（含快照恢复与 FSM 上下文恢复）。
    ///
    /// `db_path`（tmpfs 热台账）与 `snapshot_path`（持久快照）都来自 `RelayConfig`。
    pub fn boot(
        cfg: &RelayConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, crate::store::StoreError> {
        let store = Store::open(&cfg.db_path, &cfg.snapshot_path)?;
        Self::boot_with_store(cfg, store, clock)
    }

    /// 用一个已打开的 store 装配（测试直接用内存库）。
    pub fn boot_with_store(
        cfg: &RelayConfig,
        store: Store,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, crate::store::StoreError> {
        let params = RelayParams::from_config(cfg);
        let probe_spec = ProbeSpec::from_config(cfg);
        let now = clock.now();
        let startup_at = load_startup_at(&store, now)?;
        let clock = SharedClock::new(clock);

        let heartbeats: Vec<HeartbeatRow> = store.latest_heartbeats()?;
        let states: Vec<HostStateRow> = store.all_host_states()?;
        let ledger = Ledger::restore(&heartbeats, &states);

        let mut relay = Self {
            store,
            ledger,
            machines: std::collections::BTreeMap::new(),
            params,
            clock,
            startup_at,
            pending: Vec::new(),
            queue: Vec::new(),
            probe_spec,
        };

        // 1) 配置里声明的主机（`/status` 的候选顺序）
        for host in &cfg.hosts {
            relay.register_host(&host.host_id, now);
        }
        // 2) 台账里出现过的主机（快照恢复出来的也要继续判定）
        for hb in &heartbeats {
            relay.register_host(&hb.host_id, now);
        }
        for st in &states {
            relay.register_host(&st.host_id, now);
        }

        // 3) 把持久化的 FSM 上下文灌回状态机
        for id in relay.ledger.host_ids() {
            let restored = relay.store.host_state(&id)?;
            if let Some(row) = restored {
                let verdict = row
                    .probe_verdict
                    .as_deref()
                    .and_then(ProbeVerdict::from_str);
                let state = parse_host_state(&row.state);
                let machine = relay
                    .machines
                    .get_mut(&id)
                    .expect("register_host 已保证机器存在");
                machine.restore(state, row.last_heartbeat, verdict, row.offline_since);
            }
            let count = relay.store.active_alert_count(&id)?;
            relay.ledger.set_active_alerts(&id, count);
        }

        tracing::info!(
            hosts = relay.ledger.len(),
            startup_at = %fmt_time(startup_at),
            heartbeat_timeout_sec = relay.params.timing.heartbeat_timeout_sec,
            "relay 台账已就绪（宽限期 {}s）",
            relay.params.timing.startup_grace().num_seconds()
        );
        Ok(relay)
    }

    /// 登记一台主机（幂等）。
    pub fn register_host(&mut self, host_id: &str, now: DateTime<Utc>) {
        if self.machines.contains_key(host_id) {
            return;
        }
        self.ledger.ensure_host(host_id);
        self.machines
            .insert(host_id.to_string(), Fsm::new(self.params.timing, now));
    }

    fn machine(&mut self, host_id: &str) -> &mut Fsm {
        let timing = self.params.timing;
        let startup = self.startup_at;
        self.machines
            .entry(host_id.to_string())
            .or_insert_with(|| Fsm::new(timing, startup))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    #[cfg(test)]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn params(&self) -> &RelayParams {
        &self.params
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    #[cfg(test)]
    pub fn host_count(&self) -> usize {
        self.ledger.len()
    }

    /// 取某主机的状态机快照（只读）。
    pub fn machine_of(&self, host_id: &str) -> Option<&Fsm> {
        self.machines.get(host_id)
    }

    /// `/status` 选定的主机：配置顺序里的第一台，否则用字典序第一台。
    pub fn primary_host(&self, configured: &[String]) -> Option<String> {
        for id in configured {
            if self.ledger.knows(id) {
                return Some(id.clone());
            }
        }
        self.ledger.host_ids().into_iter().next()
    }

    // -----------------------------------------------------------------------
    // 上报入口
    // -----------------------------------------------------------------------

    /// `POST /report` 的核心。
    ///
    /// 顺序严格：去重 → 落库 → 台账 → FSM → 即时告警通道。
    /// 重复包（`(host_id, seq)` 已存在）直接返回 `accepted = false`，**不产生任何告警**。
    pub fn ingest_report(
        &mut self,
        raw: &[u8],
        received_at: DateTime<Utc>,
    ) -> Result<IngestReport, IngestError> {
        let packet: ReportPacket =
            serde_json::from_slice(raw).map_err(|e| IngestError::Decode(e.to_string()))?;
        let host_id = packet.host_id.clone();
        let seq = packet.seq;
        let sent_at = packet.sent_at;

        if !self.store.insert_heartbeat(
            &host_id,
            seq,
            sent_at,
            received_at,
            &String::from_utf8_lossy(raw),
        )? {
            tracing::debug!(host = %host_id, seq, "重复包，按 (host_id, seq) 去重（不告警）");
            return Ok(IngestReport {
                accepted: false,
                host_id,
                seq,
                alerts: Vec::new(),
            });
        }

        self.register_host(&host_id, received_at);
        let previous_state = self
            .machines
            .get(&host_id)
            .map(|m| m.state())
            .unwrap_or(HostState::Online);

        self.ledger
            .record_heartbeat(packet.clone(), received_at)
            .state = HostState::Online;

        let events = self.machine(&host_id).on_heartbeat(sent_at, received_at);
        for event in &events {
            match event {
                FsmEvent::Recovered {
                    offline_since,
                    offline_duration,
                } => {
                    let summary = format!(
                        "{host_id} 已上线，离线时长 {}",
                        humanize_duration(*offline_duration)
                    );
                    let offline_at = *offline_since;
                    self.mark_recovered(&host_id, received_at);
                    self.emit(
                        host_id.clone(),
                        AlertKind::Recovered,
                        summary,
                        format!("{host_id} 上次心跳 {}", relative(offline_at)),
                        received_at,
                    );
                }
                FsmEvent::SilentRecover => {
                    tracing::info!(host = %host_id, "SUSPECT 期间心跳恢复：静默恢复，不告警");
                }
                _ => {}
            }
        }
        if previous_state == HostState::Offline && !events.iter().any(is_recovered) {
            // `on_heartbeat` 在 OFFLINE 时必然产生 Recovered，这里只是兜底。
            tracing::warn!(host = %host_id, "OFFLINE 收到心跳但未产生恢复事件");
        }

        let mut alerts = Vec::new();
        self.collect_agent_alerts(&packet, received_at, &mut alerts);
        self.sync_ledger(&host_id);
        self.persist_state(&host_id, received_at)?;
        // 待推送队列：`alerts` 是本次要交给调用方的基础设施，
        // `self.queue` 是「攒着等下一次 deliver」的持久队列。
        // 两者都追加，最后由 `mem::take(&mut self.pending)` 统一返回（不能提前 take，
        // 否则会把 emit/重复提醒已经放进 `pending` 的告警丢掉）。
        self.queue.extend(alerts.iter().cloned());
        self.pending.extend(alerts);

        Ok(IngestReport {
            accepted: true,
            host_id,
            seq,
            alerts: std::mem::take(&mut self.pending),
        })
    }

    /// 处理随包上报的 `alerts[]`（即时告警通道）。
    fn collect_agent_alerts(
        &mut self,
        packet: &ReportPacket,
        now: DateTime<Utc>,
        out: &mut Vec<PendingAlert>,
    ) {
        let host_id = &packet.host_id;
        let lag = Duration::seconds(self.params.lag_threshold_sec as i64);
        let mut seen: Vec<(String, String)> = Vec::new();

        // `lag_threshold_sec` 判的是**上报包本身**的新鲜度（docs/04 §5：agent 重启/网络恢复
        // 后的补报包按 `sent_at` 去重，过期告警只入库不推送），**不是**「告警条件已经持续了
        // 多久」。判据必须是 `packet.sent_at`：
        //
        // - 条件型告警的 `first_seen` 在条件存续期间**恒定不变**（SPEC §13.2）。若拿它判过期，
        //   「relay 重启（台账为空）+ 服务已挂超过 lag_threshold_sec」的首轮告警就会被判过期
        //   → 只入库、永不推送，条件此后一直成立也再不会响（永久失声）。
        // - agent 补发时**保留原始 `sent_at`**（签名用发送当刻的时间戳），所以 `sent_at` 是
        //   区分「实时包」与「补报包」的可靠信号。
        let backfilled = now.signed_duration_since(packet.sent_at) > lag;

        for alert in &packet.alerts {
            let source = alert.source.clone();
            let title = alert.title.clone();
            seen.push((source.clone(), title.clone()));

            if backfilled {
                // 补报的过期包：**只入库**、不推送（docs/04 §5）。
                //
                // 注意这里**不**给告警建 active 键：active 集合是**推送去重**台账
                // （docs/04 §5「同一 (source,title) 告警在恢复前不重复推送」），语义是
                // 「已推送且未恢复」。给一条从未推送过的告警建 active 键会有两个恶果：
                //   1. 条件仍然成立时，之后的实时轮次全被判去重 → **永久失声**；
                //   2. 条件真正消失时，会凭空推一条运维从未见过的「X 已恢复」。
                // 已经 active 的键不受影响（本来也只是刷新，`activate_alert` 对已存在的
                // 键是纯 no-op），所以「补报不重置去重状态」这一性质保持不变。
                tracing::info!(
                    host = %host_id,
                    id = %alert.id,
                    source = %source,
                    sent_at = %fmt_time(packet.sent_at),
                    "补报包只入库不推送（sent_at 早于 lag_threshold_sec），不建 active 键"
                );
                self.store_alert(host_id, alert, now, false);
                continue;
            }

            let first_time = match self.store.activate_alert(&crate::store::ActiveAlertUpsert {
                host_id: host_id.clone(),
                alert_id: alert.id.clone(),
                source: source.clone(),
                title: title.clone(),
                level: alert.level,
                first_seen: alert.first_seen,
                updated_at: now,
            }) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "写入活跃告警集合失败");
                    false
                }
            };

            if first_time {
                out.push(PendingAlert {
                    host_id: host_id.clone(),
                    level: alert.level,
                    source: source.clone(),
                    title: title.clone(),
                    summary: alert.summary.clone(),
                });
                self.store_alert(host_id, alert, now, true);
            } else {
                tracing::debug!(host = %host_id, id = %alert.id, "活跃告警去重：恢复前不重复推送");
                self.store_alert(host_id, alert, now, false);
            }
        }

        // 本轮没再出现、但之前活跃的 agent 告警 → 按生命周期类别处理（SPEC §13.2）：
        //   - **条件型**：缺席 = 条件消失 → 推 1 条「已恢复」并移出 active 集合（解除去重）；
        //   - **事件型**：缺席 = 本轮没有新事件，点事件没有恢复语义 → **静默**移出 active
        //     集合，绝不推任何「已恢复」通知。
        // relay 自生成的告警（`relay` / `prober`）由 relay 自己的 FSM 管 active 与恢复，
        // 不参与这条「agent 本轮没上报 = 恢复」的推导。
        let active = match self.store.active_alerts() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "读取活跃告警集合失败");
                return;
            }
        };
        for row in active {
            if row.host_id != *host_id || is_relay_generated_source(&row.source) {
                continue;
            }
            let key = (row.source.clone(), row.title.clone());
            if seen.contains(&key) {
                continue;
            }
            if let Err(e) = self.store.resolve_alert(host_id, &row.source, &row.title) {
                tracing::error!(error = %e, "清除活跃告警失败");
                continue;
            }
            match lifecycle_of(&row.source) {
                AlertLifecycle::Condition => {
                    tracing::info!(
                        host = %host_id,
                        source = %row.source,
                        lifecycle = AlertLifecycle::Condition.as_str(),
                        title = %row.title,
                        "条件型告警已恢复，推恢复通知并重新允许推送"
                    );
                    out.push(PendingAlert {
                        host_id: host_id.clone(),
                        level: AlertLevel::Info,
                        source: row.source.clone(),
                        title: format!("{} 已恢复", row.title),
                        summary: "agent 侧不再上报该条件".to_string(),
                    });
                }
                AlertLifecycle::Event => {
                    // 点事件没有恢复语义：**一条推送都不产生**。
                    if is_registered_source(&row.source) {
                        tracing::debug!(
                            host = %host_id,
                            source = %row.source,
                            lifecycle = AlertLifecycle::Event.as_str(),
                            title = %row.title,
                            "事件型告警已从 active 集合移除，按契约不推恢复通知"
                        );
                    } else {
                        tracing::info!(
                            host = %host_id,
                            source = %row.source,
                            lifecycle = AlertLifecycle::Event.as_str(),
                            title = %row.title,
                            "未登记的 source 默认按事件型处理：已从 active 集合移除，不推恢复通知（SPEC §13.2）"
                        );
                    }
                }
            }
        }
    }

    fn store_alert(&self, host_id: &str, alert: &Alert, now: DateTime<Utc>, pushed: bool) {
        if let Err(e) = self.store.insert_alert(&crate::store::AlertInsert {
            host_id: host_id.to_string(),
            alert_id: alert.id.clone(),
            level: alert.level,
            source: alert.source.clone(),
            title: alert.title.clone(),
            summary: alert.summary.clone(),
            first_seen: alert.first_seen,
            received_at: now,
            pushed,
        }) {
            tracing::error!(error = %e, "告警落库失败");
        }
    }

    // -----------------------------------------------------------------------
    // 定时推进
    // -----------------------------------------------------------------------

    /// 定时 tick：推进所有状态机，返回需要触发的握手与告警。
    pub fn tick(&mut self, now: DateTime<Utc>) -> TickReport {
        let mut report = TickReport::default();
        let ids = self.ledger.host_ids();

        for host_id in ids {
            let out = self.machine(&host_id).tick(now);
            if let Some(at) = out.next_handshake_at {
                report.next_wake = Some(match report.next_wake {
                    Some(prev) => prev.min(at),
                    None => at,
                });
            }
            for event in out.events {
                match event {
                    FsmEvent::Handshake { attempt } => {
                        tracing::info!(
                            host = %host_id,
                            attempt,
                            retries = self.params.timing.handshake_retries,
                            "心跳超时，发起第 {attempt} 次握手探测"
                        );
                        report.probe_now.push(host_id.clone());
                    }
                    FsmEvent::Offline {
                        last_heartbeat,
                        attempts,
                    } => {
                        tracing::error!(
                            host = %host_id,
                            attempts,
                            retries = self.params.timing.handshake_retries,
                            "握手全部失败，判定离线"
                        );
                        report.went_offline.push(host_id.clone());
                        let summary = offline_summary(&host_id, last_heartbeat, attempts);
                        self.emit(
                            host_id.clone(),
                            AlertKind::Offline,
                            summary,
                            String::new(),
                            now,
                        );
                    }
                    FsmEvent::RepeatOffline { attempts, .. } => {
                        tracing::warn!(host = %host_id, "持续离线，发送重复提醒");
                        let summary =
                            format!("{host_id} 仍处于离线状态（已连续 {attempts} 次握手失败）");
                        self.emit_repeat_offline(host_id.clone(), summary);
                    }
                    FsmEvent::VerdictChanged(verdict) => {
                        let mut extra = Vec::new();
                        self.handle_verdict_change(&host_id, verdict, now, &mut extra);
                        report.alerts.append(&mut extra);
                    }
                    _ => {}
                }
            }
            // 状态变更落库
            let state = self.machines.get(&host_id).map(|m| m.state());
            let verdict = self.machines.get(&host_id).map(|m| m.verdict());
            let offline_since = self.machines.get(&host_id).and_then(|m| m.offline_since());
            self.ledger.set_fsm(
                &host_id,
                state.unwrap_or(HostState::Online),
                verdict,
                offline_since,
            );
            if let Err(e) = self.persist_state(&host_id, now) {
                tracing::error!(host = %host_id, error = %e, "写入 FSM 上下文失败");
            }
        }

        // 启动时间戳要跨重启保持稳定（宽限期基准），每次 tick 顺手刷新。
        if let Err(e) = save_startup_at(&self.store, self.startup_at) {
            tracing::error!(error = %e, "写入 startup_at 失败");
        }

        report.alerts.append(&mut self.pending);
        report
    }

    /// 一次握手的判定结果（判定矩阵 + 恢复）。
    pub fn on_probe_result(&mut self, judgement: Judgement, now: DateTime<Utc>) -> TickReport {
        let mut report = TickReport::default();
        let host_id = judgement.host_id.clone();
        let offline_before = self
            .machines
            .get(&host_id)
            .map(|m| m.state())
            .unwrap_or(HostState::Online)
            == HostState::Offline;

        tracing::info!(
            host = %host_id,
            verdict = judgement.verdict.as_str(),
            lanes = %judgement.describe(),
            "握手探测完成"
        );

        let out = self
            .machine(&host_id)
            .on_handshake(judgement.lane_results(), now);

        for event in out.events {
            match event {
                FsmEvent::SilentRecover => {
                    tracing::info!(host = %host_id, "任一握手成功：静默恢复（不告警）");
                }
                FsmEvent::Recovered {
                    offline_duration, ..
                } => {
                    let summary = format!(
                        "{host_id} 已上线，离线时长 {}",
                        humanize_duration(offline_duration)
                    );
                    self.mark_recovered(&host_id, now);
                    self.emit(
                        host_id.clone(),
                        AlertKind::Recovered,
                        summary,
                        String::new(),
                        now,
                    );
                }
                FsmEvent::Offline {
                    last_heartbeat,
                    attempts,
                } => {
                    // 第 `handshake_retries` 次握手失败当刻即判离线（docs/03 §3）。
                    // 这条事件可能来自 `tick`，也可能直接来自 `on_probe_result`。
                    tracing::error!(
                        host = %host_id,
                        attempts,
                        retries = self.params.timing.handshake_retries,
                        "握手全部失败，判定离线"
                    );
                    let summary = offline_summary(&host_id, last_heartbeat, attempts);
                    self.emit(
                        host_id.clone(),
                        AlertKind::Offline,
                        summary,
                        String::new(),
                        now,
                    );
                    report.went_offline.push(host_id.clone());
                }
                FsmEvent::RepeatOffline { attempts, .. } => {
                    tracing::warn!(host = %host_id, "持续离线，发送重复提醒");
                    let summary =
                        format!("{host_id} 仍处于离线状态（已连续 {attempts} 次握手失败）");
                    self.emit_repeat_offline(host_id.clone(), summary);
                }
                FsmEvent::VerdictChanged(verdict) => {
                    let mut extra = Vec::new();
                    self.handle_verdict_change(&host_id, verdict, now, &mut extra);
                    report.alerts.append(&mut extra);
                }
                _ => {}
            }
        }
        if offline_before
            && self.machines.get(&host_id).map(|m| m.state()) == Some(HostState::Online)
        {
            tracing::info!(host = %host_id, "握手成功，已从 OFFLINE 恢复");
        }

        // 状态 + 活跃告警计数都要同步进内存台账（`/status` 直接读台账）。
        self.sync_ledger(&host_id);

        let state = self.machines.get(&host_id).map(|m| m.state());
        let verdict = self.machines.get(&host_id).map(|m| m.verdict());
        let offline_since = self.machines.get(&host_id).and_then(|m| m.offline_since());
        self.ledger.set_fsm(
            &host_id,
            state.unwrap_or(HostState::Online),
            verdict,
            offline_since,
        );
        if let Err(e) = self.persist_state(&host_id, now) {
            tracing::error!(host = %host_id, error = %e, "写入 FSM 上下文失败");
        }

        report.alerts.append(&mut self.pending);
        report
    }

    /// 判定矩阵结论 → warning 告警 / 恢复（SPEC §4 注：结论不进 `/status.state`）。
    fn handle_verdict_change(
        &mut self,
        host_id: &str,
        verdict: ProbeVerdict,
        now: DateTime<Utc>,
        report: &mut Vec<PendingAlert>,
    ) {
        let offline = self.machines.get(host_id).map(|m| m.state()) == Some(HostState::Offline);
        match verdict {
            ProbeVerdict::OnlineTunnelDown => {
                tracing::warn!(host = %host_id, "双路探测：内网通、Tailscale 不通 → 隧道断开");
                self.emit(
                    host_id.to_string(),
                    AlertKind::TunnelDown,
                    format!("{host_id} 内网可达但 Tailscale 路径握手失败"),
                    String::new(),
                    now,
                );
            }
            ProbeVerdict::OnlineLanAnomaly => {
                tracing::warn!(host = %host_id, "双路探测：内网不通、Tailscale 通 → 内网异常");
                self.emit(
                    host_id.to_string(),
                    AlertKind::LanAnomaly,
                    format!("{host_id} Tailscale 可达但内网直连失败，检查网口/交换机"),
                    String::new(),
                    now,
                );
            }
            ProbeVerdict::Online => {
                self.resolve_verdict_alerts(host_id, now, report);
                if !offline {
                    tracing::info!(host = %host_id, "双路探测恢复正常");
                }
            }
            ProbeVerdict::Offline => {
                // 机器可能仍在线但两条路都不通：收敛掉此前的隧道/内网异常告警，
                // 整机离线由 FSM 的 3 次握手全失败负责判定。
                self.resolve_verdict_alerts(host_id, now, report);
            }
        }
    }

    /// 清除探测类 warning 告警（恢复后重新允许推送）。
    fn resolve_verdict_alerts(
        &mut self,
        host_id: &str,
        now: DateTime<Utc>,
        out: &mut Vec<PendingAlert>,
    ) {
        for kind in [AlertKind::TunnelDown, AlertKind::LanAnomaly] {
            match self
                .store
                .resolve_alert(host_id, kind.source(), kind.title())
            {
                Ok(true) => {
                    tracing::info!(host = %host_id, title = kind.title(), "探测异常解除");
                    out.push(PendingAlert {
                        host_id: host_id.to_string(),
                        level: AlertLevel::Info,
                        source: kind.source().to_string(),
                        title: format!("{} 已恢复", kind.title()),
                        summary: "双路探测恢复正常".to_string(),
                    });
                }
                Ok(false) => {}
                Err(e) => tracing::error!(error = %e, "清除探测告警失败"),
            }
        }
        let _ = now;
    }

    /// 机器从离线状态恢复：清掉离线/重复提醒留下的活跃键。
    ///
    /// `now` 由调用方注入——**不能**读 `self.clock.now()`：单测里的固定时钟可能停在
    /// 过去，用它去写 `offline_since` 会把离线时长抹成 0。
    fn mark_recovered(&mut self, host_id: &str, now: DateTime<Utc>) {
        if let Err(e) = self.store.resolve_alert(
            host_id,
            AlertKind::Offline.source(),
            AlertKind::Offline.title(),
        ) {
            tracing::error!(error = %e, "清除离线告警失败");
        }
        self.resolve_verdict_alerts(host_id, now, &mut Vec::new());
    }

    /// 组装 + 去重 + 入队一条 relay 侧告警。
    fn emit(
        &mut self,
        host_id: String,
        kind: AlertKind,
        summary: String,
        extra: String,
        now: DateTime<Utc>,
    ) {
        let source = kind.source().to_string();
        let title = kind.title().to_string();
        let summary = if extra.is_empty() {
            summary
        } else {
            format!("{summary}；{extra}")
        };

        let alert_id = format!("relay:{}", kind_key(kind));
        let first_time = match self.store.activate_alert(&crate::store::ActiveAlertUpsert {
            host_id: host_id.clone(),
            alert_id: alert_id.clone(),
            source: source.clone(),
            title: title.clone(),
            level: kind.level(),
            first_seen: now,
            updated_at: now,
        }) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "写入活跃告警集合失败");
                false
            }
        };

        if let Err(e) = self.store.insert_alert(&crate::store::AlertInsert {
            host_id: host_id.clone(),
            alert_id: alert_id.clone(),
            level: kind.level(),
            source: source.clone(),
            title: title.clone(),
            summary: summary.clone(),
            first_seen: now,
            received_at: now,
            pushed: first_time,
        }) {
            tracing::error!(error = %e, "告警落库失败");
        }

        if !first_time {
            tracing::info!(host = %host_id, title, "活跃告警去重：恢复前不重复推送");
            return;
        }

        let alert = PendingAlert {
            host_id: host_id.clone(),
            level: kind.level(),
            source,
            title,
            summary,
        };
        tracing::info!(
            host = %host_id,
            level = ?alert.level,
            title = %alert.title,
            "产生告警"
        );
        self.queue.push(alert.clone());
        self.pending.push(alert);
    }

    /// 持续离线重复提醒（不受 `(source,title)` 去重约束）。
    fn emit_repeat_offline(&mut self, host_id: String, summary: String) {
        let alert = PendingAlert {
            host_id: host_id.clone(),
            level: AlertKind::Offline.level(),
            source: AlertKind::Offline.source().to_string(),
            title: AlertKind::Offline.title().to_string(),
            summary,
        };
        if let Err(e) = self.store.insert_alert(&crate::store::AlertInsert {
            host_id: host_id.clone(),
            alert_id: "relay:offline:repeat".to_string(),
            level: alert.level,
            source: alert.source.clone(),
            title: alert.title.clone(),
            summary: alert.summary.clone(),
            first_seen: self.clock.now(),
            received_at: self.clock.now(),
            pushed: true,
        }) {
            tracing::error!(error = %e, "重复提醒落库失败");
        }
        self.queue.push(alert.clone());
        self.pending.push(alert);
    }

    // -----------------------------------------------------------------------
    // 推送
    // -----------------------------------------------------------------------

    /// 取出所有排队的告警（调用方在**锁外**用 `Alerter` 真正推送）。
    pub fn drain_queued(&mut self) -> Vec<PendingAlert> {
        std::mem::take(&mut self.queue)
    }

    /// 排队但尚未推送的条数（测试断言用）。
    #[cfg(test)]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    // -----------------------------------------------------------------------
    // 落库辅助
    // -----------------------------------------------------------------------

    fn sync_ledger(&mut self, host_id: &str) {
        let state = self.machines.get(host_id).map(|m| m.state());
        let verdict = self.machines.get(host_id).map(|m| m.verdict());
        let offline_since = self.machines.get(host_id).and_then(|m| m.offline_since());
        self.ledger.set_fsm(
            host_id,
            state.unwrap_or(HostState::Online),
            verdict,
            offline_since,
        );
        match self.store.active_alert_count(host_id) {
            Ok(n) => self.ledger.set_active_alerts(host_id, n),
            Err(e) => tracing::error!(error = %e, "统计活跃告警失败"),
        }
    }

    fn persist_state(
        &self,
        host_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), crate::store::StoreError> {
        let Some(machine) = self.machines.get(host_id) else {
            return Ok(());
        };
        let state = match machine.state() {
            HostState::Online => "online",
            HostState::Suspect => "suspect",
            HostState::Offline => "offline",
        };
        self.store
            .upsert_host_state(&crate::store::HostStateUpsert {
                host_id: host_id.to_string(),
                state: state.to_string(),
                last_heartbeat: machine.last_heartbeat(),
                probe_verdict: Some(machine.verdict().as_str().to_string()),
                offline_since: machine.offline_since(),
                updated_at: now,
            })
    }

    /// 取主机台账（`/status` 用）。
    pub fn host(&self, host_id: &str) -> Option<&HostLedger> {
        self.ledger.get(host_id)
    }

    /// 探测任务用的配置快照。
    pub fn probe_spec(&self) -> &ProbeSpec {
        &self.probe_spec
    }

    /// 某主机的探测路径（`relay.toml` 里的 `[[hosts]]`）。
    pub fn host_config(&self, host_id: &str) -> Option<&HostConfig> {
        self.probe_spec.hosts.iter().find(|h| h.host_id == host_id)
    }
}

/// 离线告警文案（docs/04 §2.1）：「<host_id> 最后心跳 HH:MM:SS，N/N 次握手均失败」。
fn offline_summary(host_id: &str, last_heartbeat: Option<DateTime<Utc>>, attempts: u32) -> String {
    let last = last_heartbeat
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "无（从未收到心跳）".to_string());
    format!("{host_id} 最后心跳 {last}，{attempts}/{attempts} 次握手均失败")
}

fn kind_key(kind: AlertKind) -> &'static str {
    match kind {
        AlertKind::Offline => "offline",
        AlertKind::Recovered => "recovered",
        AlertKind::TunnelDown => "tunnel_down",
        AlertKind::LanAnomaly => "lan_anomaly",
    }
}

fn is_recovered(e: &FsmEvent) -> bool {
    matches!(e, FsmEvent::Recovered { .. } | FsmEvent::SilentRecover)
}

fn parse_host_state(s: &str) -> HostState {
    match s {
        "offline" => HostState::Offline,
        "suspect" => HostState::Suspect,
        _ => HostState::Online,
    }
}

/// `2026-10-15T01:30:22Z` → `01:30:22Z`（告警文案里的相对时间）。
fn relative(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `5m30s` / `45s` 形式的人类可读时长。
pub fn humanize_duration(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// 从 `state` 表读 `startup_at`（宽限期基准必须跨重启保持稳定）。
fn load_startup_at(
    store: &Store,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, crate::store::StoreError> {
    if let Some(raw) = store.meta("startup_at")? {
        if let Ok(t) = crate::store::parse_time(&raw) {
            return Ok(t);
        }
    }
    save_startup_at(store, now)?;
    Ok(now)
}

fn save_startup_at(store: &Store, at: DateTime<Utc>) -> Result<(), crate::store::StoreError> {
    store.set_meta("startup_at", &fmt_time(at), at)
}

/// 入库失败原因。
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("请求体不是合法 ReportPacket: {0}")]
    Decode(String),
    #[error("存储错误: {0}")]
    Store(#[from] crate::store::StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerter::Alerter;
    use crate::net::FakePush;
    use crate::test_util::{t, T0};

    fn secrets() -> SecretsConfig {
        SecretsConfig {
            shared_secret: "e2e-secret".into(),
            status_token: "e2e-token".into(),
            bark: BarkConfig {
                endpoint: "http://127.0.0.1:9200".into(),
                key: "e2ekey".into(),
                group: "server_status".into(),
            },
        }
    }

    fn config() -> RelayConfig {
        toml::from_str(
            r#"
listen                 = "127.0.0.1:9100"
db_path                = ":memory:"
snapshot_path          = ":memory:"
heartbeat_timeout_sec  = 90
handshake_retries      = 3
handshake_interval_sec = 10
handshake_timeout_sec  = 2
repeat_alert_min       = 30
startup_grace_periods  = 2
rate_limit_per_min     = 50
snapshot_interval_sec  = 300
lag_threshold_sec      = 300

[[hosts]]
host_id = "h1"
[[hosts.paths]]
kind = "lan"
url  = "http://127.0.0.1:9101/probe"
[[hosts.paths]]
kind = "tailscale"
url  = "http://100.64.0.10:9101/probe"
"#,
        )
        .unwrap()
    }

    struct Rig {
        relay: Relay,
        alerter: Alerter,
        push: Arc<FakePush>,
    }

    impl Rig {
        /// 把 `relay` 排队的告警真正推出去（等价于 `main.rs` 里的 tick 任务逻辑）。
        async fn deliver_at(&mut self, now: DateTime<Utc>) -> crate::alerter::PushReport {
            let queued = self.relay.drain_queued();
            let mut report = crate::alerter::PushReport::default();
            for alert in queued {
                let r = self
                    .alerter
                    .dispatch(
                        crate::alerter::PushRequest {
                            host_id: &alert.host_id,
                            level: alert.level,
                            source: &alert.source,
                            title: &alert.title,
                            summary: &alert.summary,
                        },
                        now,
                        self.relay.params().rate_limit_per_min,
                    )
                    .await;
                report.merge(r);
            }
            report.merge(
                self.alerter
                    .flush_pending(now, self.relay.params().rate_limit_per_min)
                    .await,
            );
            report
        }

        async fn deliver(&mut self) -> crate::alerter::PushReport {
            self.deliver_at(t(T0)).await
        }
    }

    fn rig() -> Rig {
        rig_at(t(T0))
    }

    fn rig_with(start: DateTime<Utc>, cfg: &RelayConfig) -> Rig {
        let push = Arc::new(FakePush::new());
        let store = Store::open_in_memory().unwrap();
        let relay = Relay::boot_with_store(cfg, store, Arc::new(FixedClock(start))).unwrap();
        // 记录型退避：单测里推送都成功，但万一失败也不该让 cargo test 卡 21 秒。
        let sleep: crate::alerter::SleepFn = Arc::new(|_secs| Box::pin(std::future::ready(())));
        let alerter = Alerter::with_sleep(&secrets().bark, push.clone(), sleep);
        Rig {
            relay,
            alerter,
            push,
        }
    }

    fn rig_at(start: DateTime<Utc>) -> Rig {
        rig_with(start, &config())
    }

    fn packet(host: &str, seq: u64, sent_at: DateTime<Utc>, alerts: Vec<Alert>) -> ReportPacket {
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
                uptime_sec: 1000,
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
            alerts,
        }
    }

    /// 任意 `source` 的告警（生命周期分类由 `source` 决定，SPEC §13.2）。
    fn sourced_alert(
        id: &str,
        level: AlertLevel,
        source: &str,
        title: &str,
        first_seen: DateTime<Utc>,
    ) -> Alert {
        Alert {
            id: id.to_string(),
            level,
            source: source.to_string(),
            title: title.to_string(),
            summary: format!("{source} 采集结果"),
            first_seen,
        }
    }

    /// 事件型（点事件）：只在发生的那一轮上报一次。
    fn agent_alert(id: &str, level: AlertLevel, title: &str, first_seen: DateTime<Utc>) -> Alert {
        Alert {
            id: id.to_string(),
            level,
            source: "log_watcher".to_string(),
            title: title.to_string(),
            summary: "matched 3 lines".to_string(),
            first_seen,
        }
    }

    /// 条件型：条件存续期间**每轮都上报**（同一个稳定 id/title）。
    fn condition_alert(
        id: &str,
        level: AlertLevel,
        title: &str,
        first_seen: DateTime<Utc>,
    ) -> Alert {
        sourced_alert(id, level, "service_probe", title, first_seen)
    }

    fn ingest(rig: &mut Rig, p: &ReportPacket, at: DateTime<Utc>) -> IngestReport {
        let raw = serde_json::to_vec(p).unwrap();
        rig.relay.ingest_report(&raw, at).unwrap()
    }

    /// 上报一轮 + 真正推送到（假）Bark，返回**本轮实际发出的请求数**。
    ///
    /// 断言「只推 1 条」「推送总数不增加」必须看真实产出（`FakePush` 记录的 URL），
    /// 不能只看 `IngestReport.alerts`。
    async fn round(rig: &mut Rig, seq: u64, at: DateTime<Utc>, alerts: Vec<Alert>) -> usize {
        round_backfilled(rig, seq, at, at, alerts).await
    }

    /// 同上，但允许 `sent_at` 与接收时刻不同（补报包场景）。
    async fn round_backfilled(
        rig: &mut Rig,
        seq: u64,
        sent_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        alerts: Vec<Alert>,
    ) -> usize {
        let raw = serde_json::to_vec(&packet("h1", seq, sent_at, alerts)).unwrap();
        rig.relay.ingest_report(&raw, received_at).unwrap();
        let before = rig.push.call_count();
        rig.deliver_at(received_at).await;
        rig.push.call_count() - before
    }

    /// 假 Bark 是否收到过「已恢复」通知（URL 里 percent-encode 后的「已恢复」）。
    fn recovery_pushes(rig: &Rig) -> Vec<String> {
        rig.push
            .urls()
            .into_iter()
            .filter(|u| u.contains(RECOVERED_ENC))
            .collect()
    }

    /// 「已恢复」的 percent-encoded 形式（`encode_path_segment` 的 UTF-8 逐字节编码）。
    const RECOVERED_ENC: &str = "%E5%B7%B2%E6%81%A2%E5%A4%8D";

    /// 推进到 SUSPECT 并取出需要探测的主机。
    fn tick_to_suspect(rig: &mut Rig, at: DateTime<Utc>) -> TickReport {
        rig.relay.tick(at)
    }

    // ------------------------------------------------------------------
    // 基础
    // ------------------------------------------------------------------

    #[test]
    fn boot_registers_configured_hosts() {
        let rig = rig();
        assert_eq!(rig.relay.host_count(), 1);
        assert!(rig.relay.ledger().knows("h1"));
        let h = rig.relay.host("h1").unwrap();
        assert!(h.last_heartbeat.is_none());
        assert_eq!(h.state, HostState::Online);
        assert_eq!(
            rig.relay.primary_host(&["h1".to_string()]).as_deref(),
            Some("h1")
        );
    }

    #[test]
    fn ingest_accepts_and_persists_report() {
        let mut rig = rig();
        let now = t(T0);
        let r = ingest(&mut rig, &packet("h1", 1, now, vec![]), now);
        assert!(r.accepted);
        assert_eq!(r.seq, 1);
        assert!(r.alerts.is_empty());
        assert_eq!(rig.relay.store().heartbeat_count().unwrap(), 1);

        let h = rig.relay.host("h1").unwrap();
        assert_eq!(h.last_heartbeat, Some(now));
        assert_eq!(h.state, HostState::Online);
        let m = h.snapshot_metrics().unwrap();
        assert_eq!(m.cpu_percent, 12.4);
        assert_eq!(h.service_briefs().len(), 1);
        assert!(!h.service_briefs()[0].ok);
    }

    #[test]
    fn duplicate_packet_is_ignored_without_alerts() {
        let mut rig = rig();
        let now = t(T0);
        let p = packet(
            "h1",
            7,
            now,
            vec![agent_alert("log:x", AlertLevel::Critical, "严重", now)],
        );
        let first = ingest(&mut rig, &p, now);
        assert!(first.accepted);
        assert_eq!(first.alerts.len(), 1);

        let second = ingest(&mut rig, &p, now);
        assert!(!second.accepted, "同 (host_id, seq) 必须去重");
        assert!(second.alerts.is_empty(), "重复包不得产生任何告警");
        assert_eq!(rig.relay.store().heartbeat_count().unwrap(), 1);
        assert_eq!(rig.relay.store().alert_count().unwrap(), 1);
    }

    #[test]
    fn out_of_order_seq_is_stored_and_deduped_by_pair() {
        let mut rig = rig();
        let now = t(T0);
        ingest(&mut rig, &packet("h1", 5, now, vec![]), now);
        ingest(&mut rig, &packet("h1", 3, now, vec![]), now);
        assert_eq!(rig.relay.store().heartbeat_count().unwrap(), 2);
        // 台账保留「最后写入」的包（relay 只记乱序，不告警，docs/03 §1）
        assert_eq!(
            rig.relay
                .host("h1")
                .unwrap()
                .last_packet
                .as_ref()
                .unwrap()
                .seq,
            3
        );
    }

    #[test]
    fn malformed_body_is_rejected() {
        let mut rig = rig();
        let err = rig.relay.ingest_report(b"{not json", t(T0)).unwrap_err();
        assert!(matches!(err, IngestError::Decode(_)));
    }

    // ------------------------------------------------------------------
    // 即时告警通道
    // ------------------------------------------------------------------

    #[test]
    fn packet_alerts_are_pushed_immediately_with_their_level() {
        let mut rig = rig();
        let now = t(T0);
        let p = packet(
            "h1",
            1,
            now,
            vec![
                agent_alert(
                    "log:nginx",
                    AlertLevel::Warning,
                    "nginx 出现 ERROR 日志",
                    now,
                ),
                agent_alert(
                    "svc:web",
                    AlertLevel::Critical,
                    "web-frontend 连续失败",
                    now,
                ),
            ],
        );
        let r = ingest(&mut rig, &p, now);
        assert_eq!(r.alerts.len(), 2);
        assert_eq!(r.alerts[0].level, AlertLevel::Warning);
        assert_eq!(r.alerts[1].level, AlertLevel::Critical);
        assert_eq!(rig.relay.store().alert_count().unwrap(), 2);
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 2);
    }

    // ------------------------------------------------------------------
    // 告警生命周期（SPEC §13.2：条件型 vs 事件型）
    // ------------------------------------------------------------------

    /// SPEC §13.2 的分类表 + **未知 source 的默认类别**（钉死这个决策）。
    #[test]
    fn lifecycle_of_registers_spec_sources_and_defaults_unknown_to_event() {
        // 条件型：agent 每轮重复上报；relay / prober 是 relay 自生成的结论。
        assert_eq!(lifecycle_of("service_probe"), AlertLifecycle::Condition);
        assert_eq!(lifecycle_of("thresholds"), AlertLifecycle::Condition);
        assert_eq!(lifecycle_of("relay"), AlertLifecycle::Condition);
        assert_eq!(lifecycle_of("prober"), AlertLifecycle::Condition);
        // 事件型：点事件。
        assert_eq!(lifecycle_of("log_watcher"), AlertLifecycle::Event);
        assert_eq!(lifecycle_of("git_tracker"), AlertLifecycle::Event);
        // 未知 source → **默认事件型**：relay 没有证据证明它会每轮重复上报，
        // 而推一条语义错误的「已恢复」是主动误导（SPEC §13.1）。
        assert_eq!(lifecycle_of("brand_new_collector"), AlertLifecycle::Event);
        assert_eq!(lifecycle_of(""), AlertLifecycle::Event);
        assert_eq!(lifecycle_of("LOG_WATCHER"), AlertLifecycle::Event);

        assert!(!is_registered_source("brand_new_collector"));
        for known in [
            "service_probe",
            "thresholds",
            "log_watcher",
            "git_tracker",
            "relay",
            "prober",
        ] {
            assert!(is_registered_source(known), "{known} 应在 §13.2 表里登记");
        }
        // relay 自生成的 source 不走「agent 本轮没上报 = 恢复」的推导。
        assert!(is_relay_generated_source("relay"));
        assert!(is_relay_generated_source("prober"));
        assert!(!is_relay_generated_source("log_watcher"));
        assert!(!is_relay_generated_source("service_probe"));
        // 日志字段用的稳定字符串。
        assert_eq!(AlertLifecycle::Condition.as_str(), "condition");
        assert_eq!(AlertLifecycle::Event.as_str(), "event");
    }

    /// 测试要求 1：条件型同一 `(source,title)` 连续 8 轮出现 → **只推 1 条**，
    /// `active_alerts` 保持 1。跨度特意跨过 `lag_threshold_sec`（8 × 45s = 360s > 300s）。
    #[tokio::test(start_paused = true)]
    async fn condition_alert_repeated_eight_rounds_is_pushed_once() {
        let mut rig = rig();
        let mut now = t(T0);
        let detected = t(T0);
        let title = "【服务不可用】web-frontend 探测失败";

        let mut total = 0usize;
        for seq in 1..=8u64 {
            let alert = condition_alert(
                "service:web-frontend",
                AlertLevel::Critical,
                title,
                detected,
            );
            let pushes = round(&mut rig, seq, now, vec![alert]).await;
            assert_eq!(
                pushes,
                usize::from(seq == 1),
                "第 {seq} 轮推送数（条件存续期间只有首轮该推）"
            );
            total += pushes;
            assert_eq!(
                rig.relay.host("h1").unwrap().active_alerts,
                1,
                "第 {seq} 轮 active 必须保持 1"
            );
            now += Duration::seconds(45);
        }
        assert_eq!(total, 1, "8 轮合计只推 1 条 Bark");
        assert_eq!(rig.push.call_count(), 1, "假 Bark 实际收到的请求数");
        assert!(
            recovery_pushes(&rig).is_empty(),
            "条件存续期间不得有恢复通知"
        );
        // 每轮都入库留痕（8 条），但只有 1 条 pushed = true
        assert_eq!(rig.relay.store().alert_count().unwrap(), 8);
    }

    /// 测试要求 2：条件型随后缺席 1 轮 → **恰好 1 条**「已恢复」，`active_alerts` 归 0。
    #[tokio::test(start_paused = true)]
    async fn condition_alert_absence_pushes_exactly_one_recovery() {
        let mut rig = rig();
        let mut now = t(T0);
        let detected = t(T0);
        let title = "【服务不可用】web-frontend 探测失败";

        for seq in 1..=3u64 {
            let alert = condition_alert(
                "service:web-frontend",
                AlertLevel::Critical,
                title,
                detected,
            );
            let pushes = round(&mut rig, seq, now, vec![alert]).await;
            assert_eq!(pushes, usize::from(seq == 1));
            now += Duration::seconds(30);
        }
        assert_eq!(rig.push.call_count(), 1);

        // 缺席 1 轮 → 1 条「已恢复」
        let pushes = round(&mut rig, 4, now, vec![]).await;
        assert_eq!(pushes, 1, "条件型缺席必须推 1 条恢复通知");
        let rec = recovery_pushes(&rig);
        assert_eq!(rec.len(), 1, "恢复通知恰好 1 条: {rec:?}");
        assert!(
            rec[0].contains("level=active"),
            "恢复通知是 info 级: {}",
            rec[0]
        );
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 0);
        assert_eq!(rig.relay.store().active_alert_count("h1").unwrap(), 0);

        // 再缺席一轮 → 不得重复推恢复
        now += Duration::seconds(30);
        let pushes = round(&mut rig, 5, now, vec![]).await;
        assert_eq!(pushes, 0, "恢复通知不得重复推");
        assert_eq!(recovery_pushes(&rig).len(), 1);
    }

    /// 测试要求 4：事件型告警**首次出现仍要推送本体**（`log_watcher` / `git_tracker`）。
    #[tokio::test(start_paused = true)]
    async fn event_alerts_are_pushed_when_they_happen() {
        for (source, id, title) in [
            (
                "log_watcher",
                "log:test-error",
                "【日志】test-error 命中规则",
            ),
            (
                "git_tracker",
                "git:testrepo:changed",
                "【代码变动】testrepo master",
            ),
        ] {
            let mut rig = rig();
            let now = t(T0);
            let alert = sourced_alert(id, AlertLevel::Warning, source, title, now);
            let pushes = round(&mut rig, 1, now, vec![alert]).await;
            assert_eq!(pushes, 1, "{source} 首次出现必须推送本体");
            let url = rig.push.urls()[0].clone();
            assert!(url.contains("level=timeSensitive"), "{source} 级别: {url}");
            assert!(
                url.contains(&crate::alerter::encode_path_segment(title)),
                "{source} 标题必须出现在 URL 里: {url}"
            );
            assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 1);
        }
    }

    /// 测试要求 3（🔴 本次修复的核心）：事件型告警出现 1 轮后缺席 →
    /// **推送总数不增加（0 条恢复通知）**，active 静默清零。`log_watcher` 与 `git_tracker` 都要。
    #[tokio::test(start_paused = true)]
    async fn event_alert_absence_never_pushes_recovery() {
        for (source, id, title) in [
            (
                "log_watcher",
                "log:test-error",
                "【日志】test-error 命中规则",
            ),
            (
                "git_tracker",
                "git:testrepo:changed",
                "【代码变动】testrepo master",
            ),
        ] {
            let mut rig = rig();
            let now = t(T0);
            let alert = sourced_alert(id, AlertLevel::Warning, source, title, now);
            let pushes = round(&mut rig, 1, now, vec![alert]).await;
            assert_eq!(pushes, 1, "{source} 首次出现必须推送本体");

            // 下一轮不再出现 → 事件型不得推任何东西
            let later = now + Duration::seconds(30);
            let pushes = round(&mut rig, 2, later, vec![]).await;
            let rec = recovery_pushes(&rig);
            assert_eq!(pushes, 0, "{source} 缺席不得产生任何推送: {rec:?}");
            assert!(rec.is_empty(), "{source} 推了恢复通知: {rec:?}");
            assert_eq!(rig.push.call_count(), 1, "{source} 推送总数必须停在 1");
            assert_eq!(
                rig.relay.host("h1").unwrap().active_alerts,
                0,
                "{source} 缺席必须静默移出 active 集合"
            );
            assert_eq!(rig.relay.store().active_alert_count("h1").unwrap(), 0);

            // 同样的事件再次发生 → active 键已清，允许重新推送
            let again = later + Duration::seconds(30);
            let repeat = sourced_alert(id, AlertLevel::Warning, source, title, again);
            let pushes = round(&mut rig, 3, again, vec![repeat]).await;
            assert_eq!(pushes, 1, "{source} 再次发生必须重新推送");
        }
    }

    /// 测试要求 5：事件型重复出现也不重复推（`(source,title)` 去重对它同样生效）。
    #[tokio::test(start_paused = true)]
    async fn event_alert_repeated_rounds_are_deduped() {
        let mut rig = rig();
        let mut now = t(T0);
        let title = "【日志】test-error 命中规则";
        let alert = agent_alert("log:test-error", AlertLevel::Warning, title, now);

        for seq in 1..=3u64 {
            let pushes = round(&mut rig, seq, now, vec![alert.clone()]).await;
            assert_eq!(pushes, usize::from(seq == 1), "第 {seq} 轮推送数");
            now += Duration::seconds(30);
        }
        assert_eq!(rig.push.call_count(), 1, "3 轮合计只推 1 条");
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 1);
        assert!(recovery_pushes(&rig).is_empty());
    }

    /// 未登记 source 的**端到端**默认行为：本体照推、缺席静默移出、**一条恢复通知都不推**。
    #[tokio::test(start_paused = true)]
    async fn unregistered_source_defaults_to_event_end_to_end() {
        let mut rig = rig();
        let now = t(T0);
        let alert = sourced_alert(
            "x:1",
            AlertLevel::Warning,
            "brand_new_collector",
            "未来某个采集器的告警",
            now,
        );
        assert_eq!(round(&mut rig, 1, now, vec![alert]).await, 1);
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 1);

        let later = now + Duration::seconds(30);
        assert_eq!(round(&mut rig, 2, later, vec![]).await, 0);
        assert!(recovery_pushes(&rig).is_empty());
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 0);
    }

    /// 同一轮里两种类别共存：下一轮都缺席时，**只有条件型**推「已恢复」。
    #[tokio::test(start_paused = true)]
    async fn mixed_round_recovers_only_the_condition_source() {
        let mut rig = rig();
        let now = t(T0);
        let cond_title = "【服务不可用】web-frontend 探测失败";
        let event_title = "【日志】test-error 命中规则";
        let cond = condition_alert(
            "service:web-frontend",
            AlertLevel::Critical,
            cond_title,
            now,
        );
        let event = sourced_alert(
            "log:test-error",
            AlertLevel::Warning,
            "log_watcher",
            event_title,
            now,
        );
        assert_eq!(round(&mut rig, 1, now, vec![cond, event]).await, 2);
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 2);

        // 探针恢复 + 日志不再命中：两条都缺席
        let later = now + Duration::seconds(30);
        let pushes = round(&mut rig, 2, later, vec![]).await;
        let rec = recovery_pushes(&rig);
        assert_eq!(pushes, 1, "只应推探针那一条恢复通知: {rec:?}");
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert!(
            rec[0].contains(&crate::alerter::encode_path_segment(cond_title)),
            "恢复通知必须来自条件型告警: {}",
            rec[0]
        );
        assert!(
            !rec[0].contains(&crate::alerter::encode_path_segment(event_title)),
            "事件型不得出现在任何恢复通知里: {}",
            rec[0]
        );
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 0);
    }

    /// `(source, title)` 是去重键：**title 变了就是另一条告警**，必须重新推送。
    ///
    /// 这条把「title 必须稳定」钉在 relay 侧：agent 若把逐轮变化的值（如磁盘使用率）
    /// 放进 title，就会绕开 active 去重 → 每轮一条推送，而且旧 title 缺席还会推出一条
    /// 假恢复（正是 SPEC §13.1 的病症）。
    #[tokio::test(start_paused = true)]
    async fn changed_title_is_a_different_alert_and_pushes_again() {
        let mut rig = rig();
        let mut now = t(T0);
        let stable = condition_alert(
            "threshold:disk:/",
            AlertLevel::Warning,
            "【磁盘告警】/ 阈值 85%",
            now,
        );
        assert_eq!(round(&mut rig, 1, now, vec![stable]).await, 1);

        // 同一个 id，但 title 里塞了逐轮变化的使用率 → 被判为**新**告警
        now += Duration::seconds(30);
        let dynamic = condition_alert(
            "threshold:disk:/",
            AlertLevel::Warning,
            "【磁盘告警】/ 已用 90.0%",
            now,
        );
        let pushes = round(&mut rig, 2, now, vec![dynamic]).await;
        let rec = recovery_pushes(&rig);
        assert_eq!(
            pushes, 2,
            "新 title 重新推送 + 旧 title 假恢复 = 2 条（动态 title 的代价）: {rec:?}"
        );
        assert_eq!(rec.len(), 1, "旧 title 被判缺席 → 假恢复: {rec:?}");
        assert_eq!(rig.push.call_count(), 3);
    }

    #[test]
    fn condition_alert_is_not_pushed_twice_until_recovered() {
        let mut rig = rig();
        let now = t(T0);
        let alert = condition_alert(
            "svc:web",
            AlertLevel::Warning,
            "【服务不可用】web-frontend 探测失败",
            now,
        );
        let r1 = ingest(&mut rig, &packet("h1", 1, now, vec![alert.clone()]), now);
        assert_eq!(r1.alerts.len(), 1);

        // 下一次上报仍然带同一条告警（id 与 (source,title) 都相同）→ 不重复推送
        let later = now + Duration::seconds(30);
        let r2 = ingest(&mut rig, &packet("h1", 2, later, vec![alert]), later);
        assert!(r2.alerts.is_empty(), "恢复前不得重复推送（docs/04 §5）");
        // 但每次都入库（台账留痕）
        assert_eq!(rig.relay.store().alert_count().unwrap(), 2);

        // 条件型告警消失 → 视为恢复 → 推送恢复通知并解除去重
        let r3 = ingest(
            &mut rig,
            &packet("h1", 3, later + Duration::seconds(30), vec![]),
            later,
        );
        assert_eq!(r3.alerts.len(), 1);
        assert_eq!(r3.alerts[0].level, AlertLevel::Info);
        assert!(r3.alerts[0].title.contains("已恢复"));
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 0);

        // 恢复后同一个告警再次出现 → 允许重新推送
        let again = later + Duration::seconds(90);
        let r4 = ingest(
            &mut rig,
            &packet(
                "h1",
                4,
                again,
                vec![condition_alert(
                    "svc:web",
                    AlertLevel::Warning,
                    "【服务不可用】web-frontend 探测失败",
                    again,
                )],
            ),
            again,
        );
        assert_eq!(r4.alerts.len(), 1, "恢复后应重新允许推送");
    }

    /// 🔴 回归（SPEC §13.1 后果 1/2）：`lag_threshold_sec` 判的是**上报包的新鲜度**，
    /// 不是「告警条件已经持续了多久」。
    ///
    /// 场景：relay 重启（台账为空）+ 服务已经挂了 1 小时。agent 的**实时包**里那条
    /// 条件型告警 `first_seen` 是 1 小时前；旧口径拿 `first_seen` 判「过期」→ 只入库不推送
    /// → 条件此后一直成立也**永远不再推送**（永久失声）。
    #[tokio::test(start_paused = true)]
    async fn fresh_packet_with_hour_old_first_seen_is_pushed() {
        let mut rig = rig();
        let now = t(T0);
        let detected = now - Duration::hours(1);
        let title = "【服务不可用】web-frontend 探测失败";
        let alert = condition_alert("svc:web", AlertLevel::Critical, title, detected);

        let pushes = round(&mut rig, 1, now, vec![alert.clone()]).await;
        assert_eq!(pushes, 1, "实时包必须推送，与 first_seen 多老无关");
        assert!(recovery_pushes(&rig).is_empty());

        // 条件存续（first_seen 恒定不变），下一轮还在 → 去重，不重复推
        let later = now + Duration::seconds(30);
        let pushes = round(&mut rig, 2, later, vec![alert]).await;
        assert_eq!(pushes, 0, "条件存续期间不得重复推送");
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 1);
        assert_eq!(rig.push.call_count(), 1, "两轮合计只推 1 条");
    }

    /// 补报包（`sent_at` 明显早于接收时刻）→ **只入库、不推送**。
    #[tokio::test(start_paused = true)]
    async fn backfilled_packet_is_stored_but_not_pushed() {
        let mut rig = rig();
        let now = t(T0);
        let detected = now - Duration::minutes(30);
        let alert = condition_alert(
            "svc:web",
            AlertLevel::Critical,
            "【服务不可用】web-frontend 探测失败",
            detected,
        );

        let pushes = round_backfilled(&mut rig, 1, detected, now, vec![alert]).await;
        assert_eq!(pushes, 0, "补报包只入库不推送");
        assert_eq!(
            rig.relay.store().alert_count().unwrap(),
            1,
            "补报包仍要入库"
        );
        let rows = rig.relay.store().recent_alerts("h1", 10).unwrap();
        assert!(!rows[0].pushed, "补报包的告警必须记 pushed = false");
        assert!(
            rig.relay.store().active_alert_count("h1").unwrap() == 0,
            "从未推送过的告警不得占 active 键（否则条件存续期间会被永久去重）"
        );
    }

    /// 边界：**严格大于**才判补报；`sent_at` 恰好 `now - lag_threshold_sec` 仍算实时包。
    #[tokio::test(start_paused = true)]
    async fn lag_threshold_boundary_is_decided_by_packet_sent_at() {
        let now = t(T0);
        let old_first_seen = now - Duration::hours(1);

        // sent_at 恰好 now - 300s（= lag_threshold_sec）→ 实时包 → 推送
        let mut at_edge = rig();
        let edge = condition_alert(
            "svc:edge",
            AlertLevel::Warning,
            "边界-刚好 300s",
            old_first_seen,
        );
        assert_eq!(
            round_backfilled(
                &mut at_edge,
                1,
                now - Duration::seconds(300),
                now,
                vec![edge]
            )
            .await,
            1,
            "sent_at == now - lag_threshold_sec 仍按实时包推送"
        );

        // sent_at = now - 301s → 补报包 → 只入库不推送
        let mut over_edge = rig();
        let over = condition_alert(
            "svc:over",
            AlertLevel::Warning,
            "边界-超过 1s",
            old_first_seen,
        );
        assert_eq!(
            round_backfilled(
                &mut over_edge,
                1,
                now - Duration::seconds(301),
                now,
                vec![over]
            )
            .await,
            0,
            "sent_at 超出 lag_threshold_sec 1 秒即判补报"
        );
        assert_eq!(over_edge.relay.store().alert_count().unwrap(), 1);
    }

    /// 补报包不得给告警**建 active 键**：否则条件仍成立时后续实时轮次会被永久去重
    /// （永久失声），条件消失时又会凭空推一条运维从未见过的「已恢复」。
    #[tokio::test(start_paused = true)]
    async fn backfilled_packet_does_not_arm_dedup_for_later_fresh_rounds() {
        let mut rig = rig();
        let now = t(T0);
        let detected = now - Duration::minutes(30);
        let title = "【服务不可用】web-frontend 探测失败";
        let alert = condition_alert("svc:web", AlertLevel::Critical, title, detected);

        assert_eq!(
            round_backfilled(&mut rig, 1, detected, now, vec![alert]).await,
            0,
            "补报包只入库不推送"
        );
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 0);

        // 下一轮实时包仍带该条件（agent 每轮都上报）→ 必须真正推出去
        let later = now + Duration::seconds(30);
        let still_failing = condition_alert("svc:web", AlertLevel::Critical, title, detected);
        assert_eq!(
            round(&mut rig, 2, later, vec![still_failing]).await,
            1,
            "补报包不能把条件型告警的推送资格吃掉"
        );
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 1);
        assert!(
            recovery_pushes(&rig).is_empty(),
            "全程不得出现「已恢复」：{:?}",
            recovery_pushes(&rig)
        );
    }

    // ------------------------------------------------------------------
    // FSM 全路径（时间注入，不睡 90 秒）
    // ------------------------------------------------------------------

    #[test]
    fn silent_recovery_produces_no_alert() {
        let mut rig = rig();
        let mut now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);

        // 心跳超时 → SUSPECT + 第 1 次握手
        now += Duration::seconds(91);
        let tick = tick_to_suspect(&mut rig, now);
        assert_eq!(tick.probe_now, vec!["h1".to_string()]);
        assert_eq!(
            rig.relay.machine_of("h1").unwrap().state(),
            HostState::Suspect
        );
        assert!(tick.alerts.is_empty(), "进入 SUSPECT 不告警");

        // 第 1 次握手成功 → 静默恢复
        let out = rig
            .relay
            .on_probe_result(Judgement::synthesized("h1", ProbeVerdict::Online), now);
        assert_eq!(
            rig.relay.machine_of("h1").unwrap().state(),
            HostState::Online
        );
        assert!(out.alerts.is_empty(), "静默恢复不得产生告警");
        assert!(out.went_offline.is_empty());
        assert_eq!(rig.relay.store().alert_count().unwrap(), 0);
    }

    #[test]
    fn three_failed_probes_go_offline_and_emit_critical() {
        let mut rig = rig();
        let mut now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);

        now += Duration::seconds(91);
        assert_eq!(rig.relay.tick(now).probe_now, vec!["h1".to_string()]);

        let mut alerts = Vec::new();
        let mut offline_at = None;
        for i in 0..3 {
            let out = rig
                .relay
                .on_probe_result(Judgement::synthesized("h1", ProbeVerdict::Offline), now);
            alerts.extend(out.alerts);
            if !out.went_offline.is_empty() {
                offline_at = Some(now);
            }
            if i < 2 {
                now += Duration::seconds(10);
                let tick = rig.relay.tick(now);
                assert_eq!(
                    tick.probe_now,
                    vec!["h1".to_string()],
                    "第 {} 次握手",
                    i + 2
                );
            }
        }

        assert!(offline_at.is_some(), "3 次全失败必须判离线");
        assert_eq!(
            rig.relay.machine_of("h1").unwrap().state(),
            HostState::Offline
        );
        // 判离线当刻（第 3 次握手失败瞬间）即产出告警，不必等下一轮 tick
        assert_eq!(
            offline_at,
            Some(t("2026-10-15T01:30:00Z") + Duration::seconds(111))
        );
        assert_eq!(alerts.len(), 1, "离线只产生一条告警");
        let a = &alerts[0];
        assert_eq!(a.level, AlertLevel::Critical);
        assert_eq!(a.title, "服务器离线");
        assert!(a.summary.contains("h1"));
        assert!(a.summary.contains("3/3"));
        assert_eq!(rig.relay.host("h1").unwrap().state, HostState::Offline);
    }

    #[test]
    fn heartbeat_after_offline_emits_recovery() {
        let mut rig = rig();
        let mut now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);
        now += Duration::seconds(91);
        rig.relay.tick(now);
        for i in 0..3 {
            rig.relay
                .on_probe_result(Judgement::synthesized("h1", ProbeVerdict::Offline), now);
            if i < 2 {
                now += Duration::seconds(10);
                rig.relay.tick(now);
            }
        }
        assert_eq!(
            rig.relay.machine_of("h1").unwrap().state(),
            HostState::Offline
        );

        now += Duration::minutes(5);
        let r = ingest(&mut rig, &packet("h1", 2, now, vec![]), now);
        assert_eq!(
            rig.relay.machine_of("h1").unwrap().state(),
            HostState::Online
        );
        assert_eq!(r.alerts.len(), 1);
        assert_eq!(r.alerts[0].level, AlertLevel::Info);
        assert_eq!(r.alerts[0].title, "服务器恢复");
        assert!(
            r.alerts[0].summary.contains("5m"),
            "{}",
            r.alerts[0].summary
        );
    }

    #[test]
    fn repeat_alert_honours_repeat_alert_min() {
        let mut rig = rig();
        let mut now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);
        now += Duration::seconds(91);
        rig.relay.tick(now);
        for i in 0..3 {
            rig.relay
                .on_probe_result(Judgement::synthesized("h1", ProbeVerdict::Offline), now);
            if i < 2 {
                now += Duration::seconds(10);
                rig.relay.tick(now);
            }
        }
        let offline_at = now;

        assert!(rig
            .relay
            .tick(offline_at + Duration::minutes(29))
            .alerts
            .is_empty());
        let out = rig.relay.tick(offline_at + Duration::minutes(30));
        assert_eq!(out.alerts.len(), 1);
        assert_eq!(out.alerts[0].level, AlertLevel::Critical);
        assert!(out.alerts[0].summary.contains("仍处于离线状态"));
    }

    #[test]
    fn startup_grace_blocks_offline_judgement() {
        let mut rig = rig();
        // 宽限 = 90/3 × 2 = 60s；期内反复 tick 不得判离线、不得告警
        for secs in [0, 10, 30, 59] {
            let out = rig.relay.tick(t(T0) + Duration::seconds(secs));
            assert!(out.probe_now.is_empty(), "{secs}s 时不应发起握手");
            assert!(out.alerts.is_empty(), "{secs}s 时不应告警");
        }
        assert_eq!(
            rig.relay.machine_of("h1").unwrap().state(),
            HostState::Online
        );
        assert_eq!(rig.relay.store().alert_count().unwrap(), 0);

        // 宽限结束后才进 SUSPECT
        let out = rig.relay.tick(t(T0) + Duration::seconds(60));
        assert_eq!(out.probe_now, vec!["h1".to_string()]);
    }

    #[test]
    fn startup_grace_survives_reboot_but_still_judges_known_stale_host() {
        // relay 重启：state 表里留着 10 分钟前的心跳。宽限期只对「从未收到心跳」生效，
        // 已知心跳早过期的主机仍要正常进入判定流程（否则重启会把离线主机永久报成在线）。
        let store = Store::open_in_memory().unwrap();
        let start = t(T0);
        let stale = start - Duration::minutes(10);
        let raw = serde_json::to_vec(&packet("h1", 1, stale, vec![])).unwrap();
        store
            .insert_heartbeat("h1", 1, stale, stale, &String::from_utf8_lossy(&raw))
            .unwrap();
        store
            .upsert_host_state(&crate::store::HostStateUpsert {
                host_id: "h1".into(),
                state: "online".into(),
                last_heartbeat: Some(stale),
                probe_verdict: Some("online".into()),
                offline_since: None,
                updated_at: stale,
            })
            .unwrap();

        let mut relay =
            Relay::boot_with_store(&config(), store, Arc::new(FixedClock(start))).unwrap();
        assert_eq!(
            relay.host("h1").unwrap().last_heartbeat,
            Some(stale),
            "重启必须从库恢复 last_heartbeat"
        );
        // 启动后第 1 秒：宽限期还没过，但心跳早已过期 → 立刻进入 SUSPECT
        let out = relay.tick(start + Duration::seconds(1));
        assert_eq!(out.probe_now, vec!["h1".to_string()]);
        assert_eq!(relay.machine_of("h1").unwrap().state(), HostState::Suspect);
    }

    // ------------------------------------------------------------------
    // 判定矩阵（经 relay 全链路）
    // ------------------------------------------------------------------

    /// 推进到 SUSPECT 后喂一次探测结论，返回产生的告警。
    fn verdict_run(verdict: ProbeVerdict) -> (Vec<PendingAlert>, Relay) {
        let mut rig = rig();
        let now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);

        // 心跳超时当刻即发起第 1 次握手（docs/03 §3），随后把结论喂回去。
        let at = now + Duration::seconds(91);
        assert_eq!(rig.relay.tick(at).probe_now, vec!["h1".to_string()]);
        let out = rig
            .relay
            .on_probe_result(Judgement::synthesized("h1", verdict), at);
        (out.alerts, rig.relay)
    }

    #[test]
    fn matrix_both_up_is_silent() {
        let (alerts, relay) = verdict_run(ProbeVerdict::Online);
        assert!(alerts.is_empty());
        assert_eq!(relay.machine_of("h1").unwrap().state(), HostState::Online);
        assert_eq!(relay.store().active_alert_count("h1").unwrap(), 0);
    }

    #[test]
    fn matrix_tunnel_down_is_warning_and_counts_as_active_alert() {
        let (alerts, relay) = verdict_run(ProbeVerdict::OnlineTunnelDown);
        assert_eq!(alerts.len(), 1);
        assert_eq!(
            alerts[0].level,
            AlertLevel::Warning,
            "docs/03 §2 矩阵：隧道断开是 warning"
        );
        assert_eq!(alerts[0].source, "prober");
        assert!(alerts[0].title.contains("隧道断开"));
        // 关键：结论不进 state，只计入 active_alerts
        assert_eq!(relay.machine_of("h1").unwrap().state(), HostState::Online);
        assert_eq!(relay.host("h1").unwrap().active_alerts, 1);
    }

    #[test]
    fn matrix_lan_anomaly_is_warning() {
        let (alerts, _) = verdict_run(ProbeVerdict::OnlineLanAnomaly);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Warning);
        assert!(alerts[0].title.contains("内网直连异常"));
    }

    #[test]
    fn matrix_both_down_does_not_emit_warning_but_keeps_judging() {
        let (alerts, relay) = verdict_run(ProbeVerdict::Offline);
        assert!(alerts.is_empty(), "双不通由 FSM 判离线，不在这里出 warning");
        assert_eq!(relay.machine_of("h1").unwrap().state(), HostState::Suspect);
    }

    #[test]
    fn matrix_recovery_resolves_warning() {
        let mut rig = rig();
        let mut now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);
        now += Duration::seconds(91);
        rig.relay.tick(now);

        let out = rig.relay.on_probe_result(
            Judgement::synthesized("h1", ProbeVerdict::OnlineTunnelDown),
            now,
        );
        assert_eq!(out.alerts.len(), 1);

        // 同一结论重复上报 → 不重复推送
        now += Duration::seconds(10);
        let out = rig.relay.on_probe_result(
            Judgement::synthesized("h1", ProbeVerdict::OnlineTunnelDown),
            now,
        );
        assert!(out.alerts.is_empty(), "结论未变不得重复告警");

        // 隧道恢复 → info 恢复通知 + active 清零
        now += Duration::seconds(10);
        let out = rig
            .relay
            .on_probe_result(Judgement::synthesized("h1", ProbeVerdict::Online), now);
        assert_eq!(out.alerts.len(), 1);
        assert_eq!(out.alerts[0].level, AlertLevel::Info);
        assert!(out.alerts[0].title.contains("已恢复"));
        assert_eq!(rig.relay.host("h1").unwrap().active_alerts, 0);
    }

    // ------------------------------------------------------------------
    // 推送（fake transport）
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn deliver_queued_hits_bark_with_mapped_level() {
        let mut rig = rig();
        let now = t(T0);
        let p = packet(
            "h1",
            1,
            now,
            vec![agent_alert("log:x", AlertLevel::Critical, "严重告警", now)],
        );
        let r = ingest(&mut rig, &p, now);
        assert!(r.accepted);
        assert_eq!(r.alerts.len(), 1, "即时告警通道必须产生一条待推送: {r:?}");
        assert_eq!(rig.relay.queued_len(), 1);
        let report = rig.deliver().await;
        assert_eq!(report.delivered, 1);
        let urls = rig.push.urls();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("level=critical"));
        assert!(urls[0].contains("volume=8&call=1"));
        assert!(urls[0].contains("group=server_status"));
        assert_eq!(rig.relay.queued_len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn offline_alert_reaches_bark_as_critical() {
        let mut rig = rig();
        let mut now = t(T0);
        ingest(&mut rig, &packet("h1", 1, now, vec![]), now);
        now += Duration::seconds(91);
        rig.relay.tick(now);
        for i in 0..3 {
            rig.relay
                .on_probe_result(Judgement::synthesized("h1", ProbeVerdict::Offline), now);
            if i < 2 {
                now += Duration::seconds(10);
                rig.relay.tick(now);
            }
        }
        let report = rig.deliver().await;
        assert_eq!(report.delivered, 1);
        let url = rig.push.urls()[0].clone();
        assert!(url.contains("level=critical"));
        assert!(url.contains("volume=8&call=1"));
        assert!(url.contains("/e2ekey/"));
        // 中文标题必须 percent-encode 后进 URL（离线 = 「服务器离线」）
        assert!(url.contains("%E7%A6%BB%E7%BA%BF"));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_alerts_are_merged_into_summary_after_window() {
        let mut cfg = config();
        cfg.rate_limit_per_min = 2;
        let start = t(T0);
        let mut rig = rig_with(start, &cfg);

        // 一轮里塞 4 条不同告警：前 2 条直发，后 2 条合并
        let alerts = vec![
            agent_alert("a1", AlertLevel::Info, "告警一", start),
            agent_alert("a2", AlertLevel::Info, "告警二", start),
            agent_alert("a3", AlertLevel::Warning, "告警三", start),
            agent_alert("a4", AlertLevel::Warning, "告警四", start),
        ];
        let raw = serde_json::to_vec(&packet("h1", 1, start, alerts)).unwrap();
        rig.relay.ingest_report(&raw, start).unwrap();

        let report = rig.deliver().await;
        assert_eq!(report.delivered, 2, "限流内 2 条直发");
        assert_eq!(report.deferred, 2, "超出 2 条转入摘要");
        assert_eq!(report.summaries, 0, "窗口仍满，摘要留到下一分钟");
        assert_eq!(rig.push.call_count(), 2);

        // 60s 后 flush → 一条摘要（级别取被合并者中最高的 warning）
        let report = rig
            .alerter
            .flush_pending(start + Duration::seconds(61), cfg.rate_limit_per_min)
            .await;
        assert_eq!(report.summaries, 1);
        assert_eq!(report.delivered, 1);
        assert_eq!(rig.push.call_count(), 3);
        let url = rig.push.urls().last().unwrap().clone();
        assert!(url.contains("level=timeSensitive"), "摘要取最高级别: {url}");
        assert!(
            url.contains("%E5%91%8A%E8%AD%A6%E5%90%88%E5%B9%B6"),
            "摘要标题: {url}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_summary_is_not_sent_inside_the_window() {
        let mut cfg = config();
        cfg.rate_limit_per_min = 1;
        let start = t(T0);
        let mut rig = rig_with(start, &cfg);

        let alerts = vec![
            agent_alert("a1", AlertLevel::Info, "告警一", start),
            agent_alert("a2", AlertLevel::Critical, "告警二", start),
        ];
        let raw = serde_json::to_vec(&packet("h1", 1, start, alerts)).unwrap();
        rig.relay.ingest_report(&raw, start).unwrap();

        let r = rig.deliver().await;
        assert_eq!(r.delivered, 1);
        assert_eq!(r.deferred, 1);
        assert_eq!(r.summaries, 0);
        assert_eq!(rig.push.call_count(), 1);

        // 窗口内再 flush：不得绕限流
        let r = rig
            .alerter
            .flush_pending(start + Duration::seconds(30), cfg.rate_limit_per_min)
            .await;
        assert_eq!(r.summaries, 0);
        assert_eq!(rig.push.call_count(), 1, "窗口内不得绕限流");
        assert_eq!(rig.alerter.pending_len(), 1, "摘要必须留在队列里等窗口");
    }
}
