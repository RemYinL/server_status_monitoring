//! `ssm-relay` —— 中转判定器（SPEC §3 CLI / §7 relay 模块表）。
//!
//! ```text
//! ssm-relay --config /etc/ssm-relay/relay.toml [--log-level info]
//! ```
//!
//! 后台任务：
//! - **判定 tick**（1s 粒度）：推进所有主机的离线状态机；需要握手时并发跑探测；
//! - **快照 tick**（`snapshot_interval_sec`）：把 tmpfs 里的台账原子快照到 SSD；
//! - **优雅退出**：`SIGTERM`/`SIGINT` → 停 HTTP、落一次快照、退出码 0。

mod alerter;
mod api;
mod config_check;
mod fsm;
mod ledger;
mod net;
mod prober;
mod relay;
mod settings;
mod store;

#[cfg(test)]
mod test_util;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use ssm_common::prelude::*;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

use crate::alerter::{Alerter, PushReport};
use crate::api::AppState;
use crate::net::HttpTransport;
use crate::relay::{Clock, Relay, SharedClock, SystemClock};
use crate::settings::RelaySettings;

/// 判定 tick 的固定粒度：状态机的时间比较都基于绝对时间戳，粒度只影响精度（1s 足够）。
const TICK_GRANULARITY: Duration = Duration::from_secs(1);

/// 日志级别（CLI 取值），与 `tracing` 的级别名一一对应。
///
/// 单独定义一个本地枚举而不是复用 `ssm_common::config::LogLevel`（那是**告警级别**的
/// 同形类型，且不实现 `Display`/`ValueEnum`；SPEC §11 明确二者不可混用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LogLevelArg {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for LogLevelArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LogLevelArg::Trace => "trace",
            LogLevelArg::Debug => "debug",
            LogLevelArg::Info => "info",
            LogLevelArg::Warn => "warn",
            LogLevelArg::Error => "error",
        })
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "ssm-relay",
    version,
    about = "SSM relay: heartbeat ledger, offline FSM, dual-path prober, Bark alerter"
)]
struct Cli {
    /// 配置文件路径（relay.toml）
    #[arg(short, long, value_name = "FILE")]
    config: std::path::PathBuf,

    /// 日志级别：trace|debug|info|warn|error
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log_level: LogLevelArg,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_level);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 tokio runtime 失败")?;
    rt.block_on(run(cli))
}

/// SPEC §3：日志输出到 **stderr**，格式含时间戳/级别/模块名。
fn init_tracing(level: LogLevelArg) {
    // 兜底 `warn`：第三方依赖（hyper/h2）在 info 级别噪音很大，本项目自身的
    // `info!` 已经足够说明心跳/判定轨迹。
    let filter = EnvFilter::new(format!("{level},hyper=warn,h2=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true)
        .init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // ---------------- 配置加载 ----------------
    let settings = RelaySettings::load(&cli.config)
        .with_context(|| format!("加载 relay 配置失败: {}", cli.config.display()))?;
    let cfg = settings.config.clone();
    let secrets = SecretsConfig::load(&cfg.secrets_path).with_context(|| {
        format!(
            "加载 secrets 失败: {}（由 relay.toml 的 secrets_path 指定）",
            cfg.secrets_path.display()
        )
    })?;

    // 宽松契约的代价：secrets 里的键名拼错会静默取默认值（例如 bark.endpont
    // 会退回官方 api.day.app）。这里把原文再读一遍，对未知键打警告但**不退出**。
    match settings::read_secrets_text(&cfg.secrets_path) {
        Ok(text) => {
            let unknown = settings::unknown_secret_keys(&text);
            settings::log_unknown_secret_keys(&cfg.secrets_path, &unknown);
        }
        Err(e) => tracing::warn!(error = %e, "无法读取 secrets 原文，跳过未知键检查"),
    }

    // ---------------- 启动期校验（fail fast）----------------
    let issues = settings.validate(&secrets);
    config_check::log_issues(&issues, &cfg.secrets_path);
    if config_check::has_errors(&issues) {
        anyhow::bail!(
            "配置校验未通过（{} 项错误），拒绝带病启动；请按上面的提示修正后重试",
            issues.iter().filter(|i| i.is_error()).count()
        );
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config.display(),
        listen = %cfg.listen,
        db_path = %cfg.db_path.display(),
        snapshot_path = %cfg.snapshot_path.display(),
        hosts = cfg.hosts.len(),
        "ssm-relay 启动中"
    );
    // 生效的 Bark 配置：**不打印 key 本身**，只说明「已配置/为空」。
    // 这样 `endpont` 之类拼错时，日志里一眼能看到 endpoint 还是官方地址。
    tracing::info!(
        bark_endpoint = %secrets.bark.endpoint,
        bark_group = %secrets.bark.group,
        bark_key = if secrets.bark.key.is_empty() { "未配置（不会推送）" } else { "已配置" },
        "生效的 Bark 推送配置"
    );

    // ---------------- 装配 ----------------
    let clock = SharedClock::new(Arc::new(SystemClock));
    let relay = Relay::boot(&cfg, Arc::new(clock.clone()))
        .with_context(|| format!("打开台账失败: {}", cfg.db_path.display()))?;

    let push_transport: Arc<dyn net::PushTransport> =
        Arc::new(HttpTransport::new().context("初始化 HTTP 客户端失败")?);
    let alerter = Alerter::new(&secrets.bark, push_transport);

    let probe_transport: Arc<dyn net::ProbeTransport> =
        Arc::new(HttpTransport::new().context("初始化探测 HTTP 客户端失败")?);

    let state = AppState::new(
        relay,
        alerter,
        Arc::new(secrets),
        settings::host_ids(&cfg),
        clock,
        probe_transport,
    );

    // ---------------- 后台任务 ----------------
    let shutdown = Arc::new(Notify::new());
    let tick_state = state.clone();
    let tick_task = tokio::spawn(tick_loop(tick_state, shutdown.clone()));

    let snapshot_task = {
        let store = state.relay().store().clone();
        let interval = Duration::from_secs(cfg.snapshot_interval_sec.max(1));
        let shutdown = shutdown.clone();
        tokio::spawn(snapshot_loop(store, interval, shutdown))
    };

    // ---------------- HTTP ----------------
    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("监听 {} 失败（端口被占用？）", cfg.listen))?;
    let local_addr = listener.local_addr().context("读取监听地址失败")?;
    tracing::info!(addr = %local_addr, "/healthz /status /report 已就绪");

    let app = api::router(state.clone());
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Err(e) = serve_result {
        tracing::error!(error = %e, "HTTP 服务异常退出");
    }

    // ---------------- 收尾 ----------------
    tracing::info!("收到退出信号，开始优雅退出");
    // `notify_one` 会存一个 permit：即使后台任务此刻正忙于 spawn_blocking，
    // 也不会丢掉退出信号（`notify_waiters` 不存 permit，有竞态）。
    shutdown.notify_one();
    // tick 任务可能正卡在探测里，给它一点时间收尾，超时就放弃（不阻塞退出）。
    let _ = tokio::time::timeout(Duration::from_secs(3), tick_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), snapshot_task).await;

    match state.relay().store().snapshot() {
        Ok(path) => tracing::info!(path = %path.display(), "退出前快照已落盘"),
        Err(e) => tracing::warn!(error = %e, "退出前快照失败（不影响退出）"),
    }
    tracing::info!("ssm-relay 已退出");
    Ok(())
}

/// `SIGTERM` / `SIGINT` 等待器。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "无法注册 SIGTERM 处理");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 SIGINT（Ctrl-C）"),
        _ = terminate => tracing::info!("收到 SIGTERM"),
    }
}

/// 判定 tick：推进状态机 → 需要握手就并发探测 → 把告警推出去。
async fn tick_loop(state: AppState, shutdown: Arc<Notify>) {
    let mut probes: tokio::task::JoinSet<prober::Judgement> = tokio::task::JoinSet::new();
    let mut ticker = tokio::time::interval(TICK_GRANULARITY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            _ = ticker.tick() => {}
        }

        // ---- 1) 同步推进（短临界区，绝不跨越 await）----
        let (tick, host_cfgs, spec) = {
            let mut relay = state.relay();
            let now = relay.now();
            let spec = relay.probe_spec().clone();
            let report = relay.tick(now);
            let cfgs: Vec<HostConfig> = report
                .probe_now
                .iter()
                .filter_map(|id| relay.host_config(id).cloned())
                .collect();
            (report, cfgs, spec)
        };

        for host_id in &tick.went_offline {
            tracing::error!(host = %host_id, "状态机判定：离线");
        }

        // ---- 2) 并发发起握手探测（放锁外）----
        for host in host_cfgs {
            let spec = spec.clone();
            let transport = state.probe_transport();
            probes.spawn(async move { spec.probe(transport, host).await });
        }

        // ---- 3) 收取已完成的探测 ----
        while let Some(joined) = probes.try_join_next() {
            match joined {
                Ok(judgement) => {
                    let mut relay = state.relay();
                    let now = relay.now();
                    let report = relay.on_probe_result(judgement, now);
                    for host_id in report.went_offline {
                        tracing::error!(host = %host_id, "状态机判定：离线");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "握手探测任务异常结束"),
            }
        }

        // ---- 4) 推送（网络 IO，锁外 await）----
        deliver(&state).await;
    }

    // 退出前把还没推完的告警尽力发出去
    deliver(&state).await;
    tracing::info!("判定 tick 已停止");
}

/// 把 relay 排队的告警经 `Alerter` 推送出去（网络 IO 全部在 relay 锁之外）。
async fn deliver(state: &AppState) {
    let queued = state.relay().drain_queued();
    let now = state.clock().now();
    let limit = state.relay().params().rate_limit_per_min;

    let mut report = PushReport::default();
    {
        let mut alerter = state.alerter_lock().await;
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
        report.merge(alerter.flush_pending(now, limit).await);
    }
    if report.delivered > 0 || report.summaries > 0 || report.deferred > 0 {
        tracing::info!(
            delivered = report.delivered,
            summaries = report.summaries,
            deferred = report.deferred,
            requests = report.requests,
            "Bark 推送统计"
        );
    }
    for failure in &report.failures {
        tracing::error!(
            title = %failure.title,
            attempts = failure.attempts,
            reason = %failure.reason,
            "Bark 推送最终失败"
        );
    }
}

/// 周期性把 tmpfs 台账快照到 SSD（e2e T11 的持久化来源）。
async fn snapshot_loop(store: store::Store, interval: Duration, shutdown: Arc<Notify>) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 消费掉 tokio interval 的「立即首次 tick」，让第一次快照在 interval 之后发生
    // （刚启动时台账还是空的，快不快照都一样）。
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            _ = ticker.tick() => {}
        }
        let store = store.clone();
        match tokio::task::spawn_blocking(move || store.snapshot()).await {
            Ok(Ok(path)) => tracing::debug!(path = %path.display(), "台账快照完成"),
            Ok(Err(e)) => tracing::error!(error = %e, "台账快照失败"),
            Err(e) => tracing::error!(error = %e, "快照任务 panic"),
        }
    }
    tracing::info!("快照任务已停止");
}

/// `main.rs` 内部的纯函数/任务级测试（二进制 crate 的 `#[cfg(test)]` 只覆盖本文件）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn cli_requires_config_and_has_default_log_level() {
        let cli = Cli::try_parse_from(["ssm-relay", "--config", "/tmp/relay.toml"]).unwrap();
        assert_eq!(cli.config, std::path::PathBuf::from("/tmp/relay.toml"));
        assert_eq!(cli.log_level, LogLevelArg::Info);
        assert_eq!(cli.log_level.to_string(), "info");

        let cli = Cli::try_parse_from(["ssm-relay", "-c", "/tmp/r.toml", "--log-level", "debug"])
            .unwrap();
        assert_eq!(cli.log_level, LogLevelArg::Debug);

        assert!(
            Cli::try_parse_from(["ssm-relay"]).is_err(),
            "--config 必填（SPEC §3）"
        );
        assert!(
            Cli::try_parse_from(["ssm-relay", "-c", "x", "--log-level", "verbose"]).is_err(),
            "非法日志级别必须报错"
        );
    }

    #[test]
    fn every_cli_log_level_maps_to_a_tracing_directive() {
        for (arg, text) in [
            (LogLevelArg::Trace, "trace"),
            (LogLevelArg::Debug, "debug"),
            (LogLevelArg::Info, "info"),
            (LogLevelArg::Warn, "warn"),
            (LogLevelArg::Error, "error"),
        ] {
            assert_eq!(arg.to_string(), text);
            // EnvFilter 必须能解析拼出来的过滤串（否则启动就 panic）
            let _ = EnvFilter::new(format!("{arg},hyper=warn,h2=warn"));
        }
    }

    #[test]
    fn cli_version_and_help_are_available() {
        let err = Cli::try_parse_from(["ssm-relay", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        let err = Cli::try_parse_from(["ssm-relay", "-h"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[tokio::test]
    async fn snapshot_loop_snapshots_repeatedly_and_stops_on_shutdown() {
        let dir = TempDir::new("snapshot-loop");
        let db = dir.path().join("ledger.db");
        let snap = dir.path().join("ledger.snapshot.db");
        let store = store::Store::open(&db, &snap).unwrap();

        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(snapshot_loop(
            store,
            Duration::from_millis(20),
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(snap.exists(), "定时快照必须落盘");
        // 用 notify_one（会存一个 permit）而不是 notify_waiters（不存、可能丢信号）
        shutdown.notify_one();
        let stopped = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(stopped.is_ok(), "收到 shutdown 后快照任务必须退出");
    }
}
