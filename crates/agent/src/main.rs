//! ssm-agent 入口：装配 + 定时循环（SPEC §3 CLI / §7 模块表 / §10 采集器注册表）。
//!
//! 运行形态：
//!
//! ```text
//! ┌─ probe_server（axum GET /probe，独立 task）──────┐
//! │                                                    │
//! └─ 主循环：每 report_interval_sec ─┐                 │
//!     1. 跑一遍 Vec<Box<dyn Collector>> 并 merge        │
//!     2. 读共享 Metrics                                │
//!     3. reporter.report_round()：补发 → 签名 → POST   │
//!     SIGTERM/SIGINT → flush buffer → 退出码 0 ────────┘
//! ```
//!
//! # T3 扩展点（只需两步）
//!
//! 1. 在 `collectors/mod.rs` 里取消对应 `pub mod` 行的注释；
//! 2. 在下面的 [`build_collectors`] 里 `registry.push(Box::new(...))` 一行。
//!    其它任何地方都不需要改。

mod buffer;
mod collectors;
mod probe_server;
mod reporter;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, ValueEnum};
use ssm_common::prelude::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

use crate::collectors::sys_metrics::SysMetricsCollector;
use crate::collectors::{
    new_shared_metrics, read_metrics, CollectCtx, CollectorRegistry, SharedMetrics,
};
use crate::reporter::Reporter;

/// 退出前等待 probe server 关停的上限。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// CLI 契约（SPEC §3）：`ssm-agent --config <PATH> [--log-level <LEVEL>]`。
#[derive(Debug, Parser)]
#[command(
    name = "ssm-agent",
    version,
    about = "SSM 被监控端：系统指标采集 + HMAC 签名上报 + /probe 握手应答"
)]
struct Cli {
    /// 配置文件路径（agent.toml）
    #[arg(short = 'c', long = "config", value_name = "PATH")]
    config: PathBuf,

    /// 日志级别（环境变量 RUST_LOG 优先级更高）
    #[arg(
        long = "log-level",
        value_enum,
        default_value = "info",
        value_name = "LEVEL"
    )]
    log_level: CliLogLevel,
}

/// CLI 的日志级别（`trace|debug|info|warn|error`）。
///
/// 注意：与 [`ssm_common::config::LogLevel`]（日志**规则**的告警级别 `info|warning|critical`）
/// 是完全不同的两个类型，不要混用（SPEC §11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl CliLogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_level);

    match run(&cli).await {
        Ok(()) => {
            tracing::info!("ssm-agent 已优雅退出");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("ssm-agent: {err:#}");
            tracing::error!(error = format!("{err:#}"), "ssm-agent 异常退出");
            ExitCode::FAILURE
        }
    }
}

/// `tracing` 一律写 **stderr**，格式含时间戳 / 级别 / 模块名（SPEC §3）。
fn init_tracing(level: CliLogLevel) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // RUST_LOG 未设置时：本 crate 按 --log-level，其它依赖只留 warn，避免噪音。
        EnvFilter::new(format!("ssm_agent={},warn", level.as_str()))
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .init();
}

async fn run(cli: &Cli) -> Result<()> {
    // ---------------------------------------------------------------- 配置
    let cfg = AgentConfig::load(&cli.config)
        .with_context(|| format!("加载 agent 配置失败：{}", cli.config.display()))?;
    let secrets = SecretsConfig::load(&cfg.secrets_path)
        .with_context(|| format!("加载 secrets 失败：{}", cfg.secrets_path.display()))?;

    // ---------------------------------------------------------------- 配置校验（fail fast）
    // 见 `validate_config` 的文档注释：能静默退化、且退化后极难排查的项，一律拒绝启动。
    let issues = validate_config(&cfg, &secrets);
    for warning in &issues.warnings {
        tracing::warn!("{warning}");
    }
    if issues.is_fatal() {
        for error in &issues.errors {
            tracing::error!("{error}");
        }
        bail!(
            "agent 配置校验未通过：{} 项致命问题（配置文件 {}，逐条原因见上方 error 日志）",
            issues.errors.len(),
            cli.config.display()
        );
    }

    let interval_secs = cfg.report_interval_sec;

    tracing::info!(
        host_id = %cfg.host_id,
        relay_url = %cfg.relay_url,
        report_interval_sec = interval_secs,
        probe_listen = %cfg.probe_listen,
        buffer_path = %cfg.buffer_path.display(),
        buffer_max = cfg.buffer_max,
        seq_state_path = %cfg.seq_state_path.display(),
        disks = ?cfg.disks,
        "ssm-agent 启动"
    );

    // ---------------------------------------------------------------- 停止信号
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // ---------------------------------------------------------------- probe server
    let probe_task = spawn_probe_server(&cfg, shutdown_rx);

    // ---------------------------------------------------------------- 采集器 + 上报器
    let metrics_handle = new_shared_metrics();
    let mut registry = build_collectors(&cfg, metrics_handle.clone());
    let mut reporter = Reporter::from_config(&cfg, &secrets).context("初始化上报器失败")?;

    tracing::info!(
        collectors = ?registry.names(),
        collector_count = registry.len(),
        seq = reporter.seq(),
        buffered = reporter.buffered(),
        buffer_path = %reporter.buffer().path().display(),
        secrets_path = %cfg.secrets_path.display(),
        "装配完成"
    );
    if registry.is_empty() {
        tracing::warn!(
            "采集器注册表为空：上报包将只带 seq / sent_at，没有任何指标（检查 build_collectors）"
        );
    }

    // ---------------------------------------------------------------- 主循环
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    // 断网/休眠后不要"补拍"堆积的 tick，直接以当前时刻为基准重新计时。
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 首拍立即到：启动即发一次心跳，relay 不用等一个周期（也避免误判刚上线的主机）。

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let mut round: u64 = 0;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                round += 1;
                run_round(&cfg, &mut registry, &metrics_handle, &mut reporter, round).await;
            }
            _ = &mut shutdown => {
                tracing::info!(round, "收到停止信号，开始优雅退出");
                break;
            }
        }
    }

    // ---------------------------------------------------------------- 优雅退出
    // 缓冲在每次入队时就已经落盘，这里再做一次整体原子落盘 + seq 落盘，
    // 保证"收到 SIGTERM → flush → 退出码 0"这条路径是确定性的。
    if let Err(err) = reporter.flush() {
        tracing::error!(error = %err, "退出前落盘本地缓冲失败");
        let _ = shutdown_tx.send(true);
        return Err(err).context("退出前落盘本地缓冲失败");
    }
    let _ = shutdown_tx.send(true);

    match tokio::time::timeout(SHUTDOWN_GRACE, probe_task).await {
        Ok(Ok(())) => tracing::info!("probe server 已优雅关停"),
        Ok(Err(err)) => tracing::warn!(error = %err, "probe server 任务异常结束"),
        Err(_) => tracing::warn!("probe server 未在宽限期内关停，直接退出"),
    }

    tracing::info!(
        seq = reporter.seq(),
        buffered = reporter.buffered(),
        "本地缓冲已落盘，退出码 0"
    );
    Ok(())
}

/// 起 `/probe` 服务（独立 task）。
///
/// 绑定失败**不致命**：上报链路仍然可用，relay 只是无法握手探测本机
/// （会被判离线），所以这里只打 error 日志继续跑。
fn spawn_probe_server(
    cfg: &AgentConfig,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let listen = cfg.probe_listen.clone();
    let host_id = cfg.host_id.clone();
    let mut rx = shutdown_rx;

    tokio::spawn(async move {
        let wait_for_shutdown = async move {
            loop {
                if *rx.borrow_and_update() {
                    break;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        };

        if let Err(err) =
            probe_server::serve_with_shutdown(&listen, host_id, wait_for_shutdown).await
        {
            tracing::error!(
                error = format!("{err:#}"),
                listen = %listen,
                "probe server 启动/运行失败：relay 将无法握手探测本机，可能被误判离线"
            );
        }
    })
}

/// 一轮：采集 → 组包 → 上报。
async fn run_round(
    cfg: &AgentConfig,
    registry: &mut CollectorRegistry,
    metrics_handle: &SharedMetrics,
    reporter: &mut Reporter,
    round: u64,
) {
    let now = Utc::now();
    let ctx = CollectCtx::new(cfg.host_id.as_str(), sysinfo::System::uptime(), round, now);

    let outcome = registry.collect_all(&ctx).await;
    let metrics = read_metrics(metrics_handle);

    let result = reporter.report_round(now, metrics, outcome).await;

    if result.delivered {
        let metrics = read_metrics(metrics_handle);
        tracing::info!(
            round,
            seq = result.seq,
            resent = result.resent,
            buffered = result.buffered,
            cpu_percent = metrics.cpu_percent,
            mem_used_mb = metrics.mem_used_mb,
            mem_total_mb = metrics.mem_total_mb,
            disks = metrics.disks.len(),
            "本轮心跳已送达 relay"
        );
    } else {
        // 具体错误已由 reporter 以 warn 级打出（含 relay_url 与 error），
        // 这里只补一个轮次维度的 record，避免同一件事打两遍。
        tracing::debug!(
            round,
            seq = result.seq,
            buffered = result.buffered,
            consecutive_failures = result.consecutive_failures,
            "本轮心跳未送达"
        );
    }
}

// ---------------------------------------------------------------------------
// 启动期配置校验（fail fast）
// ---------------------------------------------------------------------------

/// 配置校验结果：致命错误（拒绝启动）与「只告警不拦」两类。
#[derive(Debug, Default, PartialEq, Eq)]
struct ConfigIssues {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl ConfigIssues {
    /// 有任何致命错误就不允许启动。
    fn is_fatal(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// 启动期配置校验（fail fast）。
///
/// # 为什么必须有这一段
///
/// `ssm-common` 的配置加载**故意不加 `deny_unknown_fields`**（一份 `secrets.toml`
/// 要同时喂给 agent 和 relay），代价是**键名打错 / 值写空会静默退化**。
/// 最典型的症状是「所有上报恒 401」或「上报周期变成默认 30s」——都不是一眼能看出来的。
/// 所以这里在**开始采集/上报之前**，把「能静默退化且退化后极难排查」的项挨个查一遍。
///
/// 一次返回**全部**问题（而不是遇到第一个就返回），让一轮日志把话说完。
///
/// 分两档：
/// - `errors`：致命，`main` 打 error 日志后以非 0 退出码退出；
/// - `warnings`：只 warning 不拦（`disks` 里的挂载点在不同机器上本来就不一样，
///   硬失败会让 WSL 与真机之间没法共用配置）。
fn validate_config(cfg: &AgentConfig, secrets: &SecretsConfig) -> ConfigIssues {
    let mut issues = ConfigIssues::default();

    // ---- 1) shared_secret：空密钥 => 上报全部 401 ----
    if secrets.shared_secret.trim().is_empty() {
        issues.errors.push(format!(
            "secrets 校验失败：shared_secret 为空。实际读取的 secrets 文件是 {}。\
             后果：agent 会用空密钥签名，relay 对**每一次**上报都返回 401，\
             表现为「relay 日志里全是签名校验失败、/status 永远 offline」。\
             请确认该文件里有一行 `shared_secret = \"...\"`（非空），\
             且与 relay 侧 secrets.toml 的值完全一致；\
             若你写的是别的键名（例如 shared_secrett），serde 不会报未知键，只会在缺字段时报错。",
            cfg.secrets_path.display()
        ));
    }

    // ---- 2) relay_url：必须是非空 http(s) URL ----
    let relay_url = cfg.relay_url.trim();
    if relay_url.is_empty() {
        issues.errors.push(
            "agent.toml 校验失败：relay_url 为空。\
             请填 relay 的 POST /report 完整地址，例如 \
             `relay_url = \"http://127.0.0.1:9100/report\"`（注意带 /report 路径）。"
                .to_string(),
        );
    } else if !has_http_scheme(relay_url) {
        issues.errors.push(format!(
            "agent.toml 校验失败：relay_url = {relay_url:?} 不是 http:// 或 https:// 开头，\
             reqwest 只支持这两种 scheme。\
             正确写法例如 `relay_url = \"http://127.0.0.1:9100/report\"`。"
        ));
    }

    // ---- 3) report_interval_sec：0 会变成忙循环 ----
    if cfg.report_interval_sec == 0 {
        issues.errors.push(
            "agent.toml 校验失败：report_interval_sec = 0。\
             它会让上报循环退化成忙循环（每秒最多转一圈且疯狂打日志），\
             同时把 relay 的 heartbeat_timeout_sec（默认 90）挤爆。请填 >= 1，建议 30。"
                .to_string(),
        );
    }

    // ---- 4) probes：target 不能空；tcp 必须是 host:port ----
    for probe in &cfg.probes {
        let target = probe.target.trim();
        let kind = probe_kind_str(probe.kind);

        if target.is_empty() {
            issues.errors.push(format!(
                "agent.toml 校验失败：probes 里 name = {:?}（kind = {kind}）的 target 为空。\
                 各类型的写法：http 填完整 URL（https://example.com）；\
                 tcp 填 host:port（127.0.0.1:80）；process 填进程名；tailscale 填 100.x 地址。",
                probe.name
            ));
            continue;
        }

        if probe.kind == ProbeKind::Tcp && !is_host_port(target) {
            issues.errors.push(format!(
                "agent.toml 校验失败：probes 里 name = {:?} 的 tcp 探针 target = {target:?} \
                 不是 host:port 形式（IPv6 请写成 [::1]:80，端口必须是 1..=65535 的十进制数）。\
                 正确写法例如 `target = \"127.0.0.1:80\"`。",
                probe.name
            ));
        }
    }

    // ---- 5) log_rules：pattern 必须是合法正则 ----
    for rule in &cfg.log_rules {
        let Some(pattern) = rule.pattern.as_deref() else {
            continue; // 不写 pattern = 不过滤，合法
        };
        if let Err(err) = regex::Regex::new(pattern) {
            issues.errors.push(format!(
                "agent.toml 校验失败：log_rules 里 name = {:?} 的 pattern = {pattern:?} \
                 不是合法正则，log_watcher 将无法按它匹配日志（注意括号要转义为 \\( ）。\
                 正则引擎的报错如下：\n{err}",
                rule.name
            ));
        }
    }

    // ---- 6) disks：不存在只告警（WSL 与真机路径不同，硬失败会很难用）----
    if cfg.disks.is_empty() {
        issues.warnings.push(
            "agent.toml 的 disks 为空：不会上报任何磁盘使用率，磁盘阈值告警也会失效。\
             建议至少写 `disks = [\"/\"]`。"
                .to_string(),
        );
    }
    for mount in &cfg.disks {
        if !Path::new(mount.trim()).exists() {
            issues.warnings.push(format!(
                "agent.toml 的 disks 里 {mount:?} 在本机不存在，采集时会跳过该挂载点\
                 （只告警不退出：WSL 与真机挂载点不同属正常）。\
                 请确认真机上确实有这个名字的挂载点，否则 /status 的磁盘指标会缺这一项。"
            ));
        }
    }

    issues
}

/// `http://` / `https://` 前缀（大小写不敏感，容忍 URL 里写 `HTTP://`）。
fn has_http_scheme(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// `host:port` 形式（支持 IPv6 字面量 `[::1]:80`）；端口必须是 `1..=65535`。
fn is_host_port(target: &str) -> bool {
    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        match rest.split_once("]:") {
            Some(parts) => parts,
            None => return false,
        }
    } else {
        match target.rsplit_once(':') {
            Some(parts) => parts,
            None => return false,
        }
    };

    !host.is_empty()
        && port
            .parse::<u16>()
            .map(|parsed| parsed > 0)
            .unwrap_or(false)
}

/// 配置里的 `kind` 字面量（报错信息里要能直接对着配置改）。
fn probe_kind_str(kind: ProbeKind) -> &'static str {
    match kind {
        ProbeKind::Http => "http",
        ProbeKind::Tcp => "tcp",
        ProbeKind::Process => "process",
        ProbeKind::Tailscale => "tailscale",
    }
}

/// 构建采集器注册表。
///
/// **顺序有意义**：`sys_metrics` 必须第一个注册——它先把本轮 [`Metrics`] 写进
/// 共享单元，后面的 `thresholds`（T3）才能读到本轮值。
///
/// # T3 扩展点
///
/// 新增采集器只需两步，**不改动本函数的其它逻辑**：
/// 1. `collectors/mod.rs` 里取消对应 `pub mod` 行的注释；
/// 2. 在这里取消对应 `registry.push(...)` 行的注释。
fn build_collectors(cfg: &AgentConfig, metrics: SharedMetrics) -> CollectorRegistry {
    let mut registry = CollectorRegistry::new();

    registry.push(Box::new(SysMetricsCollector::with_shared(
        cfg.disks.clone(),
        metrics.clone(),
    )));

    // ↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓ T3 扩展点：新增采集器在这里 push 一行 ↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓↓
    registry.push(Box::new(
        collectors::service_probe::ServiceProbeCollector::new(&cfg.probes),
    ));
    registry.push(Box::new(collectors::log_watcher::LogWatcherCollector::new(
        &cfg.log_rules,
    )));
    registry.push(Box::new(collectors::git_tracker::GitTrackerCollector::new(
        &cfg.git_repos,
    )));
    registry.push(Box::new(collectors::thresholds::ThresholdsCollector::new(
        &cfg.thresholds,
        metrics,
    )));
    // ↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑ T3 扩展点：新增采集器在这里 push 一行 ↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑↑

    registry
}

/// 等 SIGTERM / SIGINT（SPEC §3：收到后优雅退出，退出码 0）。
///
/// systemd 停止服务发 SIGTERM；Ctrl-C / 容器停止发 SIGINT。
async fn shutdown_signal() {
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::error!(error = %err, "安装 SIGTERM 处理器失败");
            None
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::error!(error = %err, "安装 SIGINT 处理器失败");
            None
        }
    };

    if sigterm.is_none() && sigint.is_none() {
        tracing::error!("SIGTERM/SIGINT 处理器都装不上，本进程只能被 SIGKILL 结束");
        std::future::pending::<()>().await;
        return;
    }

    let mut got_sigterm = false;
    tokio::select! {
        _ = async {
            match sigterm.as_mut() {
                Some(stream) => { let _ = stream.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => got_sigterm = true,
        _ = async {
            match sigint.as_mut() {
                Some(stream) => { let _ = stream.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }

    if got_sigterm {
        tracing::info!("收到 SIGTERM");
    } else {
        tracing::info!("收到 SIGINT");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// SPEC §3 的 CLI 契约：`--config` 必填、`-c` 短选项、`--log-level` 默认 info。
    #[test]
    fn cli_matches_documented_contract() {
        let cli = Cli::try_parse_from(["ssm-agent", "--config", "/etc/ssm-agent/agent.toml"])
            .expect("--config 长选项必须可用");
        assert_eq!(cli.config, PathBuf::from("/etc/ssm-agent/agent.toml"));
        assert_eq!(
            cli.log_level,
            CliLogLevel::Info,
            "--log-level 默认必须是 info"
        );

        let cli = Cli::try_parse_from(["ssm-agent", "-c", "/tmp/a.toml", "--log-level", "debug"])
            .expect("-c 短选项必须可用");
        assert_eq!(cli.config, PathBuf::from("/tmp/a.toml"));
        assert_eq!(cli.log_level, CliLogLevel::Debug);

        for (text, expected) in [
            ("trace", CliLogLevel::Trace),
            ("debug", CliLogLevel::Debug),
            ("info", CliLogLevel::Info),
            ("warn", CliLogLevel::Warn),
            ("error", CliLogLevel::Error),
        ] {
            let cli = Cli::try_parse_from(["ssm-agent", "-c", "/tmp/a.toml", "--log-level", text])
                .unwrap_or_else(|e| panic!("--log-level {text} 必须可用: {e}"));
            assert_eq!(cli.log_level, expected);
            assert_eq!(cli.log_level.as_str(), text);
        }

        // 缺 --config 必须报错
        assert!(Cli::try_parse_from(["ssm-agent"]).is_err());
        // 非法级别必须报错
        assert!(Cli::try_parse_from(["ssm-agent", "-c", "a", "--log-level", "verbose"]).is_err());
        // --version / --help 可用
        let err = Cli::try_parse_from(["ssm-agent", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        let err = Cli::try_parse_from(["ssm-agent", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    fn minimal_config() -> AgentConfig {
        toml::from_str(
            r#"
host_id = "unit-test-host"
relay_url = "http://127.0.0.1:1/report"
disks = ["/"]
"#,
        )
        .expect("最小 agent.toml")
    }

    /// 采集器注册契约：`sys_metrics` **必须第一位**（SPEC §10 + `collectors/mod.rs`：
    /// 它先把本轮 `Metrics` 写进共享单元，`thresholds` 才读得到本轮值），
    /// 其余四个是 T3 在 `build_collectors` 里注册的，顺序固定为下面这个列表。
    #[test]
    fn build_collectors_registers_sys_metrics_first() {
        let cfg = minimal_config();
        let metrics = new_shared_metrics();
        let registry = build_collectors(&cfg, metrics);

        assert_eq!(
            registry.names(),
            vec![
                "sys_metrics",
                "service_probe",
                "log_watcher",
                "git_tracker",
                "thresholds"
            ],
            "注册顺序有意义：sys_metrics 必须第一个，thresholds 必须最后一个（它读本轮指标）"
        );
        assert_eq!(registry.len(), 5);
    }

    /// 一轮采集 + 组包 + 上报（relay 不存在）后：指标进共享单元、包进本地缓冲。
    #[tokio::test]
    async fn one_round_collects_metrics_and_buffers_when_relay_is_down() {
        let dir = std::env::temp_dir().join(format!("ssm-agent-main-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut cfg = minimal_config();
        cfg.seq_state_path = dir.join("seq");
        cfg.buffer_path = dir.join("buffer.jsonl");
        cfg.buffer_max = 5;
        let secrets: SecretsConfig =
            toml::from_str("shared_secret = \"s\"\nstatus_token = \"t\"\n").unwrap();

        let metrics_handle = new_shared_metrics();
        let mut registry = build_collectors(&cfg, metrics_handle.clone());
        let mut reporter = Reporter::from_config(&cfg, &secrets).unwrap();

        run_round(&cfg, &mut registry, &metrics_handle, &mut reporter, 1).await;

        let metrics = read_metrics(&metrics_handle);
        assert!(metrics.mem_total_mb > 0, "sys_metrics 必须真的采到内存");
        assert!(metrics.uptime_sec > 0);
        assert_eq!(metrics.disks.len(), 1);
        assert_eq!(metrics.disks[0].mount, "/");

        assert_eq!(reporter.buffered(), 1, "relay 不存在时本轮包必须进缓冲");
        assert_eq!(reporter.seq(), 1);
        assert_eq!(
            std::fs::read_to_string(&cfg.seq_state_path).unwrap().trim(),
            "1"
        );

        // 再跑一轮：序号递增，缓冲累积
        run_round(&cfg, &mut registry, &metrics_handle, &mut reporter, 2).await;
        assert_eq!(reporter.seq(), 2);
        assert_eq!(reporter.buffered(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- 配置校验

    fn valid_secrets() -> SecretsConfig {
        toml::from_str("shared_secret = \"s3cr3t\"\nstatus_token = \"t\"\n")
            .expect("最小 secrets.toml")
    }

    fn cfg_from(text: &str) -> AgentConfig {
        toml::from_str(text).unwrap_or_else(|e| panic!("测试配置必须可解析: {e}\n{text}"))
    }

    fn errors_text(issues: &ConfigIssues) -> String {
        issues.errors.join("\n---\n")
    }

    /// 一份完全合规的配置不得产生任何 error / warning（防止校验器误伤）。
    #[test]
    fn valid_config_has_no_issues() {
        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"
report_interval_sec = 30
disks = ["/"]

[[probes]]
name = "web"
kind = "http"
target = "https://example.com"

[[probes]]
name = "db"
kind = "tcp"
target = "127.0.0.1:5432"

[[probes]]
name = "v6"
kind = "tcp"
target = "[::1]:80"

[[probes]]
name = "worker"
kind = "process"
target = "ssm-agent"

[[probes]]
name = "ts"
kind = "tailscale"
target = "100.64.0.1"

[[log_rules]]
name = "nginx-error"
source = "file:/tmp/nginx.log"
pattern = "ERROR|upstream timed out"

[[git_repos]]
name = "repo"
path = "/tmp"
"#,
        );
        let issues = validate_config(&cfg, &valid_secrets());
        assert!(
            !issues.is_fatal(),
            "合规配置不该致命: {}",
            errors_text(&issues)
        );
        assert!(
            issues.warnings.is_empty(),
            "合规配置不该有告警: {:?}",
            issues.warnings
        );
    }

    /// 1) `shared_secret` 为空 → 致命，且提示里带出**实际读取的路径**与后果。
    #[test]
    fn empty_shared_secret_is_fatal_and_mentions_the_real_path() {
        let cfg = minimal_config();
        for raw in ["", "   ", "\t\n"] {
            let secrets: SecretsConfig =
                toml::from_str(&format!("shared_secret = {raw:?}\nstatus_token = \"t\"\n"))
                    .expect("TOML 必须可解析");
            let issues = validate_config(&cfg, &secrets);

            assert!(issues.is_fatal(), "shared_secret = {raw:?} 必须致命");
            let text = errors_text(&issues);
            assert!(text.contains("shared_secret"), "{text}");
            assert!(
                text.contains(&cfg.secrets_path.display().to_string()),
                "报错必须带出实际读取的 secrets 路径，实际: {text}"
            );
            assert!(
                text.contains("401"),
                "必须说明后果是「上报全部 401」: {text}"
            );
        }

        // 非空密钥不报错
        assert!(!validate_config(&cfg, &valid_secrets()).is_fatal());
    }

    /// 2) `relay_url` 空 / 非 http(s) → 致命；大小写 scheme 放行。
    #[test]
    fn bad_relay_url_is_fatal() {
        for bad in [
            "",
            "   ",
            "127.0.0.1:9100/report",
            "ftp://relay/report",
            "//relay/report",
            "ws://relay/report",
        ] {
            let mut cfg = minimal_config();
            cfg.relay_url = bad.to_string();
            let issues = validate_config(&cfg, &valid_secrets());
            assert!(issues.is_fatal(), "relay_url = {bad:?} 必须致命");
            assert!(
                issues.errors.iter().any(|e| e.contains("relay_url")),
                "报错必须点出 relay_url: {}",
                errors_text(&issues)
            );
        }

        for good in [
            "http://127.0.0.1:9100/report",
            "https://relay.example/report",
            "HTTP://relay/report",
            "  https://relay.example/report  ",
        ] {
            let mut cfg = minimal_config();
            cfg.relay_url = good.to_string();
            assert!(
                !validate_config(&cfg, &valid_secrets()).is_fatal(),
                "relay_url = {good:?} 应被接受"
            );
        }

        assert!(has_http_scheme("http://x"), "http:// 必须识别");
        assert!(has_http_scheme("HTTPS://x"), "scheme 大小写不敏感");
        assert!(!has_http_scheme("http:/x"), "单斜杠不放行");
        assert!(!has_http_scheme(""), "空串不放行");
    }

    /// 3) `report_interval_sec = 0` → 致命（否则上报循环变忙循环）。
    #[test]
    fn zero_report_interval_is_fatal() {
        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"
report_interval_sec = 0
"#,
        );
        let issues = validate_config(&cfg, &valid_secrets());
        assert!(issues.is_fatal(), "0 必须致命");
        assert!(
            issues
                .errors
                .iter()
                .any(|e| e.contains("report_interval_sec")),
            "报错必须点出 report_interval_sec: {}",
            errors_text(&issues)
        );

        // 1 是最小合法值
        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"
report_interval_sec = 1
"#,
        );
        assert!(!validate_config(&cfg, &valid_secrets()).is_fatal());
    }

    /// 4) probes：空 target / tcp 非 `host:port` → 致命。
    #[test]
    fn bad_probe_targets_are_fatal() {
        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"

[[probes]]
name = "empty-http"
kind = "http"
target = ""

[[probes]]
name = "empty-tcp"
kind = "tcp"
target = "   "

[[probes]]
name = "bad-tcp"
kind = "tcp"
target = "127.0.0.1"

[[probes]]
name = "bad-port"
kind = "tcp"
target = "127.0.0.1:70000"

[[probes]]
name = "zero-port"
kind = "tcp"
target = "127.0.0.1:0"
"#,
        );
        let issues = validate_config(&cfg, &valid_secrets());
        let text = errors_text(&issues);
        assert_eq!(issues.errors.len(), 5, "五个探针问题都要报出来: {text}");
        for name in [
            "empty-http",
            "empty-tcp",
            "bad-tcp",
            "bad-port",
            "zero-port",
        ] {
            assert!(text.contains(name), "报错必须点名 {name}: {text}");
        }
    }

    #[test]
    fn is_host_port_accepts_documented_forms() {
        for good in [
            "127.0.0.1:80",
            "localhost:5432",
            "example.com:443",
            "[::1]:80",
            "[2001:db8::1]:9101",
            " 127.0.0.1:80 ",
        ] {
            assert!(is_host_port(good.trim()), "{good:?} 应被接受");
        }
        for bad in [
            "",
            "127.0.0.1",
            ":80",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:70000",
            "127.0.0.1:abc",
            "[::1]",
            "[::1]80",
        ] {
            assert!(!is_host_port(bad), "{bad:?} 应被拒绝");
        }
    }

    /// 5) `pattern` 不是合法正则 → 致命；合法或未配置 → 通过。
    #[test]
    fn invalid_log_pattern_is_fatal() {
        for pattern in ["(", "[unclosed", "*bad", "\\"] {
            let cfg = cfg_from(&format!(
                r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"

[[log_rules]]
name = "r"
source = "file:/tmp/x.log"
pattern = {pattern:?}
"#
            ));
            let issues = validate_config(&cfg, &valid_secrets());
            assert!(issues.is_fatal(), "pattern = {pattern:?} 必须致命");
            let text = errors_text(&issues);
            assert!(text.contains("正则"), "报错要说明是正则问题: {text}");
            assert!(text.contains("\"r\""), "报错要点出规则名: {text}");
        }

        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"

[[log_rules]]
name = "ok"
source = "file:/tmp/x.log"
pattern = "ERROR|upstream timed out"

[[log_rules]]
name = "no-pattern"
source = "file:/tmp/y.log"

[[log_rules]]
name = "escaped-paren"
source = "file:/tmp/z.log"
pattern = "\\(root\\)"
"#,
        );
        let issues = validate_config(&cfg, &valid_secrets());
        assert!(
            !issues.is_fatal(),
            "合法正则不该被拦: {}",
            errors_text(&issues)
        );
    }

    /// 6) `disks` 里的挂载点不存在 → **只告警不退出**。
    #[test]
    fn missing_disk_mount_only_warns() {
        let missing = format!("/ssm-definitely-not-a-mount-{}", std::process::id());
        let cfg = cfg_from(&format!(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"
disks = ["/", "{missing}"]
"#
        ));
        let issues = validate_config(&cfg, &valid_secrets());
        assert!(
            !issues.is_fatal(),
            "挂载点不存在绝不能拦住启动: {}",
            errors_text(&issues)
        );
        assert!(issues.errors.is_empty());
        assert_eq!(
            issues.warnings.len(),
            1,
            "只该为不存在的那个挂载点告警，实际 {:?}",
            issues.warnings
        );
        assert!(
            issues.warnings[0].contains(&missing),
            "{:?}",
            issues.warnings
        );

        // disks 为空：同样只告警
        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "http://127.0.0.1:9100/report"
disks = []
"#,
        );
        let issues = validate_config(&cfg, &valid_secrets());
        assert!(!issues.is_fatal());
        assert_eq!(issues.warnings.len(), 1, "{:?}", issues.warnings);
    }

    /// 一次把**所有**问题都报出来（不是遇到第一个就返回），便于一轮修完。
    #[test]
    fn every_problem_is_reported_in_one_pass() {
        let cfg = cfg_from(
            r#"
host_id = "h"
relay_url = "relay:9100/report"
report_interval_sec = 0
disks = ["/ssm-no-such-mount"]

[[probes]]
name = "p"
kind = "tcp"
target = "nohost"

[[log_rules]]
name = "r"
source = "file:/tmp/x.log"
pattern = "("
"#,
        );
        let secrets: SecretsConfig =
            toml::from_str("shared_secret = \"\"\nstatus_token = \"t\"\n").unwrap();

        let issues = validate_config(&cfg, &secrets);
        assert_eq!(
            issues.errors.len(),
            5,
            "五个致命项都要露面: {}",
            errors_text(&issues)
        );
        assert_eq!(issues.warnings.len(), 1, "{:?}", issues.warnings);
    }
}
