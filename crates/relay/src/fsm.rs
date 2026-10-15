//! 离线判定状态机（docs/03 §3 / docs/02 §2.2）。
//!
//! ```text
//! ONLINE --(>heartbeat_timeout)--> SUSPECT --(handshake_retries × handshake_interval)-->
//!   ├─ 任一握手成功 --> ONLINE（静默恢复，不计事件、不告警）
//!   └─ 全部失败     --> OFFLINE  --> critical「服务器离线」
//! OFFLINE --(收到心跳 / 握手成功)--> ONLINE --> info 恢复通知（附离线时长）
//! ```
//!
//! **时间完全可注入**：本模块不调用 `Utc::now()`，所有状态迁移都由调用方传入的
//! `now` 驱动，因此单测可以「喂时间戳」而不用真的睡 90 秒。
//!
//! 心跳超时被切成 `heartbeat_timeout_sec / 3` 一段（设计上 `heartbeat_timeout_sec`
//! 就是 3 个上报周期，SPEC §6 注释），因此 `startup_grace_periods` 天然可以换算成秒。

use chrono::{DateTime, Duration, Utc};
use ssm_common::prelude::*;

/// 一条探测路径的结果（docs/03 §2 判定矩阵的输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneResult {
    /// 2xx + 合法 JSON。
    Reachable,
    /// 超时 / 连接失败 / 非 2xx。
    Unreachable,
}

/// 双路探测结论。
///
/// 注意：`OnlineTunnelDown` / `OnlineLanAnomaly` **不进 `/status.state`**（SPEC §4 注），
/// 而是以 `warning` 级告警呈现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    Online,
    OnlineTunnelDown,
    OnlineLanAnomaly,
    Offline,
}

impl ProbeVerdict {
    /// 数据库/日志里的稳定字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeVerdict::Online => "online",
            ProbeVerdict::OnlineTunnelDown => "online_tunnel_down",
            ProbeVerdict::OnlineLanAnomaly => "online_lan_anomaly",
            ProbeVerdict::Offline => "offline",
        }
    }

    /// 从数据库字面量还原。
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "online" => Some(ProbeVerdict::Online),
            "online_tunnel_down" => Some(ProbeVerdict::OnlineTunnelDown),
            "online_lan_anomaly" => Some(ProbeVerdict::OnlineLanAnomaly),
            "offline" => Some(ProbeVerdict::Offline),
            _ => None,
        }
    }

    /// docs/03 §2 判定矩阵的 warning 告警文案（`None` 表示无需告警）。
    #[cfg(test)]
    pub fn alert_kind(self) -> Option<AlertKind> {
        match self {
            ProbeVerdict::OnlineTunnelDown => Some(AlertKind::TunnelDown),
            ProbeVerdict::OnlineLanAnomaly => Some(AlertKind::LanAnomaly),
            ProbeVerdict::Online | ProbeVerdict::Offline => None,
        }
    }
}

/// 双路探测的原始输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Lanes {
    pub lan: Option<LaneResult>,
    pub tailscale: Option<LaneResult>,
}

impl Lanes {
    #[cfg(test)]
    pub fn new(lan: Option<LaneResult>, tailscale: Option<LaneResult>) -> Self {
        Self { lan, tailscale }
    }

    fn up(lane: Option<LaneResult>) -> bool {
        lane == Some(LaneResult::Reachable)
    }

    /// docs/03 §2 判定矩阵。
    ///
    /// | lan | tailscale | 结论 |
    /// |---|---|---|
    /// | 通 | 通 | `Online` |
    /// | 通 | 不通 | `OnlineTunnelDown` |
    /// | 不通 | 不通 | `Offline` |
    /// | 不通 | 通 | `OnlineLanAnomaly` |
    ///
    /// 单路部署（只声明了一条路径）时退化为第 1、3 行：声明的那条通了就是 `Online`，
    /// 不通就是 `Offline`。
    pub fn verdict(self) -> ProbeVerdict {
        match (self.lan, self.tailscale) {
            (Some(_), Some(_)) => match (Self::up(self.lan), Self::up(self.tailscale)) {
                (true, true) => ProbeVerdict::Online,
                (true, false) => ProbeVerdict::OnlineTunnelDown,
                (false, true) => ProbeVerdict::OnlineLanAnomaly,
                (false, false) => ProbeVerdict::Offline,
            },
            (Some(lan), None) => match lan {
                LaneResult::Reachable => ProbeVerdict::Online,
                LaneResult::Unreachable => ProbeVerdict::Offline,
            },
            (None, Some(ts)) => match ts {
                LaneResult::Reachable => ProbeVerdict::Online,
                LaneResult::Unreachable => ProbeVerdict::Offline,
            },
            // 没有任何探测路径：无法判定，保守地当作不可达（由心跳继续兜底）。
            (None, None) => ProbeVerdict::Offline,
        }
    }
}

/// 状态机产生的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsmEvent {
    /// 进入 SUSPECT，发起第 `attempt` 次握手。
    Handshake { attempt: u32 },
    /// 连续 `handshake_retries` 次握手全失败 → OFFLINE。
    Offline {
        last_heartbeat: Option<DateTime<Utc>>,
        attempts: u32,
    },
    /// 静默恢复（SUSPECT → ONLINE，不告警）。
    SilentRecover,
    /// OFFLINE → ONLINE，需推 info 恢复通知。
    Recovered {
        offline_since: DateTime<Utc>,
        offline_duration: Duration,
    },
    /// 判定矩阵结论变化（含 `Online` 表示异常解除）。
    VerdictChanged(ProbeVerdict),
    /// 持续离线时的重复提醒（`repeat_alert_min`）。
    RepeatOffline {
        last_heartbeat: Option<DateTime<Utc>>,
        attempts: u32,
    },
}

/// relay 自己产生的告警种类（agent 随包上报的告警不经过这里）。
///
/// 级别映射按 docs/04 §1：离线 = critical，隧道断开 / 内网异常 = warning，恢复 = info。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// 「服务器离线」（3 次握手全失败）。
    Offline,
    /// 「服务器恢复上线」。
    Recovered,
    /// 「服务器在线，Tailscale 隧道断开」。
    TunnelDown,
    /// 「内网直连异常（Tailscale 可达）」。
    LanAnomaly,
}

impl AlertKind {
    pub fn level(self) -> AlertLevel {
        match self {
            AlertKind::Offline => AlertLevel::Critical,
            AlertKind::Recovered => AlertLevel::Info,
            AlertKind::TunnelDown | AlertKind::LanAnomaly => AlertLevel::Warning,
        }
    }

    /// 告警来源（`Alert.source`）。
    pub fn source(self) -> &'static str {
        match self {
            AlertKind::Offline | AlertKind::Recovered => "relay",
            AlertKind::TunnelDown | AlertKind::LanAnomaly => "prober",
        }
    }

    /// 稳定标题；同时充当 `(source, title)` 去重键。
    pub fn title(self) -> &'static str {
        match self {
            AlertKind::Offline => "服务器离线",
            AlertKind::Recovered => "服务器恢复",
            AlertKind::TunnelDown => "服务器在线，Tailscale 隧道断开",
            AlertKind::LanAnomaly => "内网直连异常（Tailscale 可达）",
        }
    }
}

/// 状态机对外暴露的状态与下一步动作。
#[derive(Debug, Clone, PartialEq)]
pub struct FsmOutput {
    pub state: HostState,
    pub next_handshake_at: Option<DateTime<Utc>>,
    pub offline_since: Option<DateTime<Utc>>,
    pub verdict: ProbeVerdict,
    pub events: Vec<FsmEvent>,
}

/// 驱动状态机的时间参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsmTiming {
    pub heartbeat_timeout_sec: u64,
    pub handshake_retries: u32,
    pub handshake_interval_sec: u64,
    pub repeat_alert_min: u64,
    pub startup_grace_periods: u32,
}

impl FsmTiming {
    /// 一个上报周期的时长（`heartbeat_timeout_sec` 按契约 = 3 个上报周期）。
    pub fn period(&self) -> Duration {
        Duration::seconds((self.heartbeat_timeout_sec / 3).max(1) as i64)
    }

    /// 启动宽限期总时长 = `period × startup_grace_periods`。
    pub fn startup_grace(&self) -> Duration {
        self.period() * i32::try_from(self.startup_grace_periods).unwrap_or(i32::MAX)
    }

    /// 两次握手之间的间隔。
    pub fn handshake_interval(&self) -> Duration {
        Duration::seconds(self.handshake_interval_sec as i64)
    }

    /// 重复提醒间隔（`0` = 不重复）。
    pub fn repeat_alert(&self) -> Option<Duration> {
        if self.repeat_alert_min == 0 {
            None
        } else {
            Some(Duration::minutes(self.repeat_alert_min as i64))
        }
    }
}

/// 离线判定状态机。
#[derive(Debug, Clone)]
pub struct Fsm {
    timing: FsmTiming,
    state: HostState,
    last_heartbeat: Option<DateTime<Utc>>,
    offline_since: Option<DateTime<Utc>>,
    last_offline_alert: Option<DateTime<Utc>>,
    /// 本段 SUSPECT 已发起的握手次数。
    attempts: u32,
    /// 下一次握手时间（`None` = 不需要握手）。
    next_probe_at: Option<DateTime<Utc>>,
    /// 最近一次探测的车道结果（用于判定矩阵）。
    lanes: Lanes,
    /// 最近一次判定矩阵结论。
    verdict: ProbeVerdict,
    /// 启动宽限期的截止时间。
    grace_until: DateTime<Utc>,
}

impl Fsm {
    /// `startup_at` 是 relay 启动时刻；宽限期从这一刻起算。
    pub fn new(timing: FsmTiming, startup_at: DateTime<Utc>) -> Self {
        Self {
            timing,
            state: HostState::Online,
            last_heartbeat: None,
            offline_since: None,
            last_offline_alert: None,
            attempts: 0,
            next_probe_at: None,
            lanes: Lanes::default(),
            verdict: ProbeVerdict::Online,
            grace_until: startup_at + timing.startup_grace(),
        }
    }

    /// 宽限期截止时间（测试断言用）。
    #[cfg(test)]
    pub fn grace_until(&self) -> DateTime<Utc> {
        self.grace_until
    }

    pub fn state(&self) -> HostState {
        self.state
    }

    pub fn last_heartbeat(&self) -> Option<DateTime<Utc>> {
        self.last_heartbeat
    }

    pub fn offline_since(&self) -> Option<DateTime<Utc>> {
        self.offline_since
    }

    pub fn verdict(&self) -> ProbeVerdict {
        self.verdict
    }

    #[cfg(test)]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    #[cfg(test)]
    pub fn next_probe_at(&self) -> Option<DateTime<Utc>> {
        self.next_probe_at
    }

    /// 恢复持久化的判定上下文（relay 从快照重启）。
    pub fn restore(
        &mut self,
        state: HostState,
        last_heartbeat: Option<DateTime<Utc>>,
        verdict: Option<ProbeVerdict>,
        offline_since: Option<DateTime<Utc>>,
    ) {
        self.state = state;
        self.last_heartbeat = last_heartbeat;
        self.verdict = verdict.unwrap_or(ProbeVerdict::Online);
        self.offline_since = offline_since;
        // 恢复出来的 OFFLINE 必须补一次提醒窗口，避免重启后永远不再提醒。
        self.last_offline_alert = offline_since;
    }

    /// 收到心跳：任何状态都直接回 ONLINE。
    ///
    /// 不在此处产生「恢复」事件——调用方在 `ingest` 里根据**迁移前**的状态决定是否
    /// 推恢复通知（见 `Fsm::state_before_heartbeat` 用法），这样 SUSPECT 的静默恢复
    /// 不会误报（测试 1）。
    pub fn on_heartbeat(&mut self, sent_at: DateTime<Utc>, now: DateTime<Utc>) -> Vec<FsmEvent> {
        let was = self.state;
        self.last_heartbeat = Some(sent_at);
        self.state = HostState::Online;
        self.attempts = 0;
        self.next_probe_at = None;
        // 心跳说明机器活着，但探测结论要等下一次握手才更新；先按「未知」处理，
        // 让先前因隧道/内网异常置起的 warning 能在下一轮探测后被收敛。
        if self.verdict != ProbeVerdict::Online {
            self.verdict = ProbeVerdict::Online;
            self.lanes = Lanes::default();
        }

        let mut events = Vec::new();
        match was {
            HostState::Offline => {
                let since = self.offline_since.unwrap_or(sent_at);
                events.push(FsmEvent::Recovered {
                    offline_since: since,
                    offline_duration: now.signed_duration_since(since),
                });
                self.offline_since = None;
                self.last_offline_alert = None;
            }
            HostState::Suspect => {
                // 静默恢复：不计事件、不告警（docs/03 §3）。
                events.push(FsmEvent::SilentRecover);
            }
            HostState::Online => {}
        }
        events
    }

    /// 定时推进：心跳超时 → SUSPECT 并安排握手；握手全部失败 → OFFLINE；重复提醒。
    ///
    /// 注意：**从 ONLINE 掉进 SUSPECT 的那一次 tick 必须同时发起第 1 次握手**
    /// （docs/03 §3 时序：「relay 发起第 1 次握手」紧随进入 SUSPECT）。
    /// 因此这里用 `entered_suspect` 让同一轮 tick 继续走到 Suspect 分支。
    pub fn tick(&mut self, now: DateTime<Utc>) -> FsmOutput {
        let mut events = Vec::new();
        let mut entered_suspect = false;

        match self.state {
            HostState::Online => {
                if let Some(hb) = self.last_heartbeat {
                    if now.signed_duration_since(hb) > self.timing.heartbeat_timeout() {
                        self.state = HostState::Suspect;
                        self.attempts = 0;
                        self.next_probe_at = None; // 立即发起第 1 次握手
                        entered_suspect = true;
                        tracing::warn!(
                            host_last_heartbeat = %hb.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                            "心跳超时，进入 SUSPECT"
                        );
                    }
                } else if now >= self.grace_until {
                    // 从未收到过心跳且已过启动宽限：同样进入 SUSPECT 走握手流程。
                    // 「从未收到心跳」这条路径由宽限期兜底，否则 relay 刚起就会误判离线。
                    self.state = HostState::Suspect;
                    self.attempts = 0;
                    self.next_probe_at = None;
                    entered_suspect = true;
                    tracing::warn!("启动宽限期结束仍无心跳，进入 SUSPECT");
                }
            }
            HostState::Suspect => {}
            HostState::Offline => {
                // 持续离线：按 repeat_alert_min 重复提醒（0 = 不重复）。
                if let Some(interval) = self.timing.repeat_alert() {
                    let baseline = self.last_offline_alert.or(self.offline_since);
                    if let Some(base) = baseline {
                        if now.signed_duration_since(base) >= interval {
                            self.last_offline_alert = Some(now);
                            events.push(FsmEvent::RepeatOffline {
                                last_heartbeat: self.last_heartbeat,
                                attempts: self.attempts,
                            });
                        }
                    }
                }
            }
        }

        if entered_suspect || self.state == HostState::Suspect {
            if self.next_probe_at.is_none() {
                // 该发起（或继续）握手了。
                if self.attempts >= self.timing.handshake_retries {
                    self.go_offline(now, &mut events);
                } else {
                    self.attempts += 1;
                    self.next_probe_at = Some(now + self.timing.handshake_interval());
                    events.push(FsmEvent::Handshake {
                        attempt: self.attempts,
                    });
                }
            } else if self.next_probe_at.is_some_and(|at| now >= at) {
                // 上一轮握手的结果迟迟没回来（探测任务卡住）→ 当作失败继续推进。
                if self.attempts >= self.timing.handshake_retries {
                    self.go_offline(now, &mut events);
                } else {
                    self.attempts += 1;
                    self.next_probe_at = Some(now + self.timing.handshake_interval());
                    events.push(FsmEvent::Handshake {
                        attempt: self.attempts,
                    });
                }
            }
        }

        self.output(events)
    }

    /// 一次握手的结果（任一路径成功即视为机器在线）。
    pub fn on_handshake(&mut self, lanes: Lanes, now: DateTime<Utc>) -> FsmOutput {
        let mut events = Vec::new();
        let verdict = lanes.verdict();
        let verdict_changed = verdict != self.verdict;
        self.lanes = lanes;
        self.verdict = verdict;

        let was = self.state;
        let offline_since = self.offline_since;

        let reachable = matches!(
            verdict,
            ProbeVerdict::Online | ProbeVerdict::OnlineTunnelDown | ProbeVerdict::OnlineLanAnomaly
        );

        if reachable {
            // 任一成功 → 静默恢复（docs/03 §3）。
            self.state = HostState::Online;
            self.attempts = 0;
            self.next_probe_at = None;
            match was {
                HostState::Suspect => events.push(FsmEvent::SilentRecover),
                HostState::Offline => {
                    let since = offline_since.unwrap_or(now);
                    events.push(FsmEvent::Recovered {
                        offline_since: since,
                        offline_duration: now.signed_duration_since(since),
                    });
                    self.offline_since = None;
                    self.last_offline_alert = None;
                }
                HostState::Online => {}
            }
        } else {
            // 本次握手**失败**：`handshake_retries` 次全失败即判离线
            // （docs/03 §3：第 3 次握手失败 → OFFLINE，不再等下一轮 tick）；
            // 否则记下间隔，等 `handshake_interval_sec` 之后发起下一次。
            if self.attempts >= self.timing.handshake_retries {
                self.go_offline(now, &mut events);
            } else {
                self.next_probe_at = Some(now + self.timing.handshake_interval());
            }
        }

        if verdict_changed {
            events.push(FsmEvent::VerdictChanged(verdict));
        }

        self.output(events)
    }

    fn go_offline(&mut self, now: DateTime<Utc>, events: &mut Vec<FsmEvent>) {
        self.state = HostState::Offline;
        self.offline_since = Some(now);
        self.last_offline_alert = Some(now);
        events.push(FsmEvent::Offline {
            last_heartbeat: self.last_heartbeat,
            attempts: self.attempts,
        });
    }

    fn output(&self, events: Vec<FsmEvent>) -> FsmOutput {
        FsmOutput {
            state: self.state,
            next_handshake_at: self.next_probe_at,
            offline_since: self.offline_since,
            verdict: self.verdict,
            events,
        }
    }
}

impl FsmTiming {
    /// 心跳超时阈值。
    pub fn heartbeat_timeout(&self) -> Duration {
        Duration::seconds(self.heartbeat_timeout_sec as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{t, T0};

    fn timing() -> FsmTiming {
        FsmTiming {
            heartbeat_timeout_sec: 90,
            handshake_retries: 3,
            handshake_interval_sec: 10,
            repeat_alert_min: 30,
            startup_grace_periods: 2,
        }
    }

    fn fsm() -> Fsm {
        Fsm::new(timing(), t(T0))
    }

    fn hands(fsm: &mut Fsm, at: &str, lanes: Lanes) -> FsmOutput {
        fsm.on_handshake(lanes, t(at))
    }

    fn both_up() -> Lanes {
        Lanes::new(Some(LaneResult::Reachable), Some(LaneResult::Reachable))
    }

    fn both_down() -> Lanes {
        Lanes::new(Some(LaneResult::Unreachable), Some(LaneResult::Unreachable))
    }

    /// 假 agent：在 `now` 时刻给一次心跳。
    fn beat(fsm: &mut Fsm, now: DateTime<Utc>) -> Vec<FsmEvent> {
        fsm.on_heartbeat(now, now)
    }

    /// 推进到 OFFLINE，返回全部事件。`handshake_retries = 3`。
    ///
    /// 时序（docs/03 §3）：hb 后第 91s 进入 SUSPECT **并当场发起第 1 次握手**，
    /// 之后每 10s 一次，第 3 次失败即 OFFLINE。
    fn drive_to_offline(fsm: &mut Fsm, hb: DateTime<Utc>) -> Vec<FsmEvent> {
        let mut events = beat(fsm, hb);
        for i in 0..3u32 {
            let at = hb + Duration::seconds(91 + i as i64 * 10);
            let out = fsm.tick(at);
            assert_eq!(
                out.events,
                vec![FsmEvent::Handshake { attempt: i + 1 }],
                "第 {} 次握手调度错误",
                i + 1
            );
            events.extend(out.events);
            events.extend(fsm.on_handshake(both_down(), at).events);
        }
        assert_eq!(fsm.state(), HostState::Offline);
        events
    }

    #[test]
    fn period_and_grace_derived_from_timeout() {
        let tm = timing();
        assert_eq!(tm.period(), Duration::seconds(30));
        assert_eq!(tm.startup_grace(), Duration::seconds(60));
        assert_eq!(tm.repeat_alert(), Some(Duration::minutes(30)));
        assert_eq!(
            FsmTiming {
                repeat_alert_min: 0,
                ..tm
            }
            .repeat_alert(),
            None
        );
    }

    #[test]
    fn heartbeat_keeps_online() {
        let mut fsm = fsm();
        beat(&mut fsm, t(T0));
        let out = fsm.tick(t(T0) + Duration::seconds(89));
        assert_eq!(out.state, HostState::Online);
        assert!(out.events.is_empty());
        assert!(out.next_handshake_at.is_none());
    }

    #[test]
    fn timeout_enters_suspect_and_schedules_first_handshake() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        beat(&mut fsm, hb);

        // 恰好 90s 不算超时（docs: 距上次心跳 > 90s）
        assert_eq!(
            fsm.tick(hb + Duration::seconds(90)).state,
            HostState::Online
        );

        let out = fsm.tick(hb + Duration::seconds(91));
        assert_eq!(out.state, HostState::Suspect);
        assert_eq!(
            out.events,
            vec![FsmEvent::Handshake { attempt: 1 }],
            "进入 SUSPECT 当刻即发起第 1 次握手"
        );
        assert_eq!(out.next_handshake_at, Some(hb + Duration::seconds(101)));
    }

    #[test]
    fn suspect_handshake_success_recovers_silently_without_alert() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        beat(&mut fsm, hb);
        let out = fsm.tick(hb + Duration::seconds(91));
        assert_eq!(out.state, HostState::Suspect);
        assert_eq!(out.events, vec![FsmEvent::Handshake { attempt: 1 }]);

        // 第 1 次握手成功 → ONLINE，事件是 SilentRecover（不含任何告警）
        let out = hands(&mut fsm, "2026-10-15T01:30:52Z", both_up());
        assert_eq!(out.state, HostState::Online);
        assert_eq!(out.events, vec![FsmEvent::SilentRecover]);
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, FsmEvent::Offline { .. } | FsmEvent::Recovered { .. })),
            "静默恢复不得产生离线/恢复告警"
        );
        assert_eq!(fsm.attempts(), 0);
        assert!(fsm.next_probe_at().is_none());
    }

    #[test]
    fn three_failed_handshakes_go_offline_with_critical_event() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        let events = drive_to_offline(&mut fsm, hb);

        let handshakes: Vec<u32> = events
            .iter()
            .filter_map(|e| match e {
                FsmEvent::Handshake { attempt } => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(handshakes, vec![1, 2, 3], "handshake_retries = 3");

        let offline: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                FsmEvent::Offline {
                    last_heartbeat,
                    attempts,
                } => Some((*last_heartbeat, *attempts)),
                _ => None,
            })
            .collect();
        assert_eq!(
            offline,
            vec![(Some(hb), 3)],
            "3 次全失败必须恰好产生一次 Offline 事件，且带最后心跳与尝试次数"
        );
        assert_eq!(fsm.state(), HostState::Offline);
        assert_eq!(fsm.offline_since(), Some(hb + Duration::seconds(111)));
        assert_eq!(AlertKind::Offline.level(), AlertLevel::Critical);
    }

    #[test]
    fn offline_then_heartbeat_recovers_with_duration() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        drive_to_offline(&mut fsm, hb);
        let offline_at = fsm.offline_since().expect("OFFLINE 必须记录 offline_since");

        // 收到心跳 → ONLINE + 恢复通知（附离线时长）
        let back = offline_at + Duration::seconds(300);
        let events = fsm.on_heartbeat(back, back);
        assert_eq!(fsm.state(), HostState::Online);
        assert_eq!(
            events,
            vec![FsmEvent::Recovered {
                offline_since: offline_at,
                offline_duration: Duration::seconds(300),
            }]
        );
        assert!(fsm.offline_since().is_none());
        assert_eq!(AlertKind::Recovered.level(), AlertLevel::Info);
    }

    #[test]
    fn offline_probe_success_also_recovers() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        drive_to_offline(&mut fsm, hb);
        // 第 3 次握手（hb+111s）失败当刻即判离线
        let offline_at = fsm.offline_since().unwrap();
        assert_eq!(offline_at, hb + Duration::seconds(111));

        // 80s 后握手成功 → 恢复，离线时长 80s（01:31:51 → 01:32:11）
        let out = hands(&mut fsm, "2026-10-15T01:32:11Z", both_up());
        assert_eq!(out.state, HostState::Online);
        assert_eq!(
            out.events,
            vec![
                FsmEvent::Recovered {
                    offline_since: offline_at,
                    offline_duration: Duration::seconds(20),
                },
                FsmEvent::VerdictChanged(ProbeVerdict::Online),
            ]
        );
    }

    #[test]
    fn repeat_alert_fires_after_interval_then_again() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        drive_to_offline(&mut fsm, hb);
        let offline_at = fsm.offline_since().unwrap();

        // 间隔未到 → 不重复
        let out = fsm.tick(offline_at + Duration::minutes(29));
        assert!(out.events.is_empty(), "未到 repeat_alert_min 不应提醒");

        // 到点 → 重复提醒
        let out = fsm.tick(offline_at + Duration::minutes(30));
        assert_eq!(
            out.events,
            vec![FsmEvent::RepeatOffline {
                last_heartbeat: fsm.last_heartbeat(),
                attempts: 3,
            }]
        );

        // 再过 30 分钟 → 再提醒一次
        let out = fsm.tick(offline_at + Duration::minutes(60));
        assert!(matches!(out.events[0], FsmEvent::RepeatOffline { .. }));
        // 两次提醒之间不会因多次 tick 而重复
        assert!(fsm
            .tick(offline_at + Duration::minutes(61))
            .events
            .is_empty());
    }

    #[test]
    fn repeat_alert_zero_disables_repeats() {
        let mut fsm = Fsm::new(
            FsmTiming {
                repeat_alert_min: 0,
                ..timing()
            },
            t(T0),
        );
        drive_to_offline(&mut fsm, t("2026-10-15T01:30:00Z"));
        let at = fsm.offline_since().unwrap() + Duration::hours(5);
        assert!(
            fsm.tick(at).events.is_empty(),
            "repeat_alert_min=0 必须不重复"
        );
    }

    #[test]
    fn startup_grace_suppresses_offline_when_no_heartbeat_ever() {
        let mut fsm = fsm(); // period 30s × 2 = 60s 宽限
        assert_eq!(fsm.grace_until(), t(T0) + Duration::seconds(60));

        for step in 0..60 {
            let out = fsm.tick(t(T0) + Duration::seconds(step));
            assert_eq!(out.state, HostState::Online, "宽限期内必须保持 ONLINE");
            assert!(!out
                .events
                .iter()
                .any(|e| matches!(e, FsmEvent::Offline { .. })));
        }
        // 宽限结束 → 才允许进入 SUSPECT
        let out = fsm.tick(t(T0) + Duration::seconds(60));
        assert_eq!(out.state, HostState::Suspect);
        assert!(matches!(out.events[0], FsmEvent::Handshake { attempt: 1 }));
    }

    #[test]
    fn startup_grace_does_not_delay_judgement_when_heartbeat_known() {
        // relay 从快照恢复：宽限期内已经有历史心跳，且心跳已远超阈值
        // → 宽限只针对「尚未收到心跳」的情形，不能让已知离线的主机一直显示 online。
        let mut fsm = fsm();
        let stale = t(T0) - Duration::minutes(10);
        fsm.on_heartbeat(stale, stale);
        let out = fsm.tick(t(T0) + Duration::seconds(1));
        assert_eq!(out.state, HostState::Suspect);
    }

    #[test]
    fn verdict_matrix_all_four_combinations() {
        for (lan, ts, expected) in [
            (true, true, ProbeVerdict::Online),
            (true, false, ProbeVerdict::OnlineTunnelDown),
            (false, false, ProbeVerdict::Offline),
            (false, true, ProbeVerdict::OnlineLanAnomaly),
        ] {
            let lanes = Lanes::new(
                Some(if lan {
                    LaneResult::Reachable
                } else {
                    LaneResult::Unreachable
                }),
                Some(if ts {
                    LaneResult::Reachable
                } else {
                    LaneResult::Unreachable
                }),
            );
            assert_eq!(lanes.verdict(), expected, "lan={lan} tailscale={ts}");
        }

        // 单路部署退化
        assert_eq!(
            Lanes::new(Some(LaneResult::Reachable), None).verdict(),
            ProbeVerdict::Online
        );
        assert_eq!(
            Lanes::new(Some(LaneResult::Unreachable), None).verdict(),
            ProbeVerdict::Offline
        );
        assert_eq!(
            Lanes::new(None, Some(LaneResult::Reachable)).verdict(),
            ProbeVerdict::Online
        );
        assert_eq!(
            Lanes::new(None, Some(LaneResult::Unreachable)).verdict(),
            ProbeVerdict::Offline
        );
        assert_eq!(Lanes::default().verdict(), ProbeVerdict::Offline);
    }

    #[test]
    fn verdict_change_is_reported_once() {
        let mut fsm = fsm();
        beat(&mut fsm, t(T0));

        let out = hands(
            &mut fsm,
            "2026-10-15T01:30:31Z",
            Lanes::new(Some(LaneResult::Reachable), Some(LaneResult::Unreachable)),
        );
        assert_eq!(
            out.events,
            vec![FsmEvent::VerdictChanged(ProbeVerdict::OnlineTunnelDown)]
        );

        let out = hands(
            &mut fsm,
            "2026-10-15T01:30:41Z",
            Lanes::new(Some(LaneResult::Reachable), Some(LaneResult::Unreachable)),
        );
        assert!(out.events.is_empty(), "结论未变不得重复产生事件");
    }

    #[test]
    fn verdict_strings_roundtrip() {
        for v in [
            ProbeVerdict::Online,
            ProbeVerdict::OnlineTunnelDown,
            ProbeVerdict::OnlineLanAnomaly,
            ProbeVerdict::Offline,
        ] {
            assert_eq!(ProbeVerdict::from_str(v.as_str()), Some(v));
        }
        assert_eq!(ProbeVerdict::from_str("bogus"), None);
        assert_eq!(
            ProbeVerdict::OnlineTunnelDown.alert_kind(),
            Some(AlertKind::TunnelDown)
        );
        assert_eq!(
            ProbeVerdict::OnlineLanAnomaly.alert_kind(),
            Some(AlertKind::LanAnomaly)
        );
        assert_eq!(ProbeVerdict::Online.alert_kind(), None);
        assert_eq!(ProbeVerdict::Offline.alert_kind(), None);
    }

    #[test]
    fn restore_keeps_offline_context_and_repeat_window() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:00:00Z");
        let off = t("2026-10-15T01:02:00Z");
        fsm.restore(
            HostState::Offline,
            Some(hb),
            Some(ProbeVerdict::Offline),
            Some(off),
        );
        assert_eq!(fsm.state(), HostState::Offline);
        assert_eq!(fsm.last_heartbeat(), Some(hb));
        assert_eq!(fsm.offline_since(), Some(off));

        // 重启后仍在重复提醒窗口内
        assert!(fsm.tick(off + Duration::minutes(10)).events.is_empty());
        assert!(matches!(
            fsm.tick(off + Duration::minutes(30)).events[0],
            FsmEvent::RepeatOffline { .. }
        ));
    }

    #[test]
    fn tick_is_idempotent_when_called_repeatedly_without_time_passing() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        beat(&mut fsm, hb);
        let at = hb + Duration::seconds(91);
        let first = fsm.tick(at);
        assert_eq!(first.events, vec![FsmEvent::Handshake { attempt: 1 }]);
        // 同一时刻重复 tick：不得重复发起握手
        let second = fsm.tick(at);
        assert!(second.events.is_empty());
        assert_eq!(fsm.attempts(), 1);
    }

    #[test]
    fn stalled_probe_does_not_freeze_the_fsm() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        beat(&mut fsm, hb);
        fsm.tick(hb + Duration::seconds(91)); // 握手 1
                                              // 探测结果永远不回来：时间跨过 interval 后 FSM 必须继续推进
        let out = fsm.tick(hb + Duration::seconds(101));
        assert_eq!(out.events, vec![FsmEvent::Handshake { attempt: 2 }]);
        let out = fsm.tick(hb + Duration::seconds(111));
        assert_eq!(out.events, vec![FsmEvent::Handshake { attempt: 3 }]);
        let out = fsm.tick(hb + Duration::seconds(121));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, FsmEvent::Offline { attempts: 3, .. })));
        assert_eq!(out.state, HostState::Offline);
    }

    #[test]
    fn heartbeat_during_suspect_before_first_probe_is_silent() {
        let mut fsm = fsm();
        let hb = t("2026-10-15T01:30:00Z");
        beat(&mut fsm, hb);
        fsm.tick(hb + Duration::seconds(91));
        let events = fsm.on_heartbeat(hb + Duration::seconds(92), hb + Duration::seconds(92));
        assert_eq!(
            events,
            vec![FsmEvent::SilentRecover],
            "SUSPECT 期间收到心跳属静默恢复"
        );
    }
}
