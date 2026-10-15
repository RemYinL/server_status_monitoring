//! 日志规则采集：journald 追随 + 文件 tail + 正则匹配（SPEC §7、docs/04 §2.3、docs/01 F4）。
//!
//! # 两种来源（`LogSource`，线格式是纯字符串）
//!
//! - `journald:<unit>` → 常驻子进程 `journalctl -u <unit> -f -o cat`，异步按行读 stdout
//!   （SPEC §7 明确**不要**依赖 `systemd` crate）。子进程在 `Drop` 时会被杀掉，不留僵尸
//!   `journalctl`。命令不存在（WSL 没装、unit 名写错）只 `warn` 一次并优雅退化为「无日志」。
//! - `file:<绝对路径>` → 自己 tail：记录 offset，**截断/轮转（inode 变化或 size 变小）
//!   会自动重头读**；文件不存在只 `warn` 一次，下一轮继续重试。
//!
//! # 脱敏（docs/04 §2.3，**硬约束**）
//!
//! `include_raw = false` 时 summary **绝不能含原始日志行**——只放规则名与匹配计数。
//! `include_raw = true` 才附最多 `max_lines` 行原文。
//!
//! # 限频
//!
//! `throttle_sec` 窗口内同一规则只产出一条 Alert（按 `CollectCtx::now` 计时，不是按轮数）。
//!
//! # 事件型告警（SPEC §13.2）
//!
//! 「日志命中一次」是**已经发生的点事件**，不是持续状态：
//!
//! - **只在命中那一轮出现一次**（再受 `throttle_sec` 限频）；后续没有新命中的轮次里
//!   `alerts[]` **不含**它 —— 这是**正确**行为，不是恢复；
//! - 与之配套，relay 对事件型告警缺席时**静默**从 active 集合移除，不推「已恢复」
//!   （SPEC §13.2）。因此本采集器**绝不产出** `xxx:recovered` 这类告警：
//!   点事件谈不上「恢复」，`throttle_sec` 只是限频，与恢复语义无关。

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ssm_common::config::{LogLevel, LogRule, LogSource};
use ssm_common::protocol::{Alert, AlertLevel};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use super::{CollectCtx, CollectOutcome, Collector};

/// 告警来源标识（SPEC §4）。
const SOURCE: &str = "log_watcher";

/// journald 行缓冲上限（防止 unit 刷屏时把内存吃光）。
const JOURNALD_BUFFER_LINES: usize = 4096;

/// 每次采集最多处理的 journald 行数（防止一轮吃掉几万行）。
const JOURNALD_MAX_LINES_PER_ROUND: usize = 2000;

/// journald 子进程退出后的重试间隔。
const JOURNALD_RESPAWN_BACKOFF: Duration = Duration::from_secs(60);

/// `Child::try_wait` 的重试参数（杀掉子进程后等它被回收）。
const KILL_WAIT_ATTEMPTS: usize = 5;
const KILL_WAIT_INTERVAL: Duration = Duration::from_millis(100);

/// 单轮从文件里最多读多少行（防止日志风暴把一轮上报拖死）。
const FILE_MAX_LINES_PER_ROUND: usize = 2000;

/// journald 子进程包装：stdout → channel 的读取任务 + 子进程句柄。
struct JournaldReader {
    rx: mpsc::Receiver<String>,
    /// 读取任务句柄；`None` 表示还没启动或已经被收掉。
    task: Option<tokio::task::JoinHandle<()>>,
    child: Option<tokio::process::Child>,
    started: bool,
    last_spawn_attempt: Option<std::time::Instant>,
    /// 「命令不存在」这类永久性失败只报一次，且本轮之后不再重试（省得每轮刷日志）。
    spawn_failed: bool,
    /// 子进程是否活着（stdout 关闭或子进程退出即置 false）。
    child_alive: bool,
    /// 是否已经就「子进程结束/命令缺失」告警过，避免刷屏。
    complained: bool,
    /// 可注入的 `journalctl` 命令（生产恒为 `"journalctl"`；测试注入假脚本）。
    binary: Arc<std::sync::Mutex<String>>,
}

impl JournaldReader {
    fn new(binary: Arc<std::sync::Mutex<String>>) -> Self {
        let (_tx, rx) = mpsc::channel(JOURNALD_BUFFER_LINES);
        Self {
            rx,
            task: None,
            child: None,
            started: false,
            last_spawn_attempt: None,
            spawn_failed: false,
            child_alive: false,
            complained: false,
            binary,
        }
    }

    /// 取回本轮的日志行（非阻塞）。
    fn drain(&mut self, unit: &str) -> Vec<String> {
        self.maybe_spawn(unit);

        let mut lines = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(line) => {
                    if lines.len() < JOURNALD_MAX_LINES_PER_ROUND {
                        lines.push(line);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.child_alive = false;
                    break;
                }
            }
        }

        if !self.child_alive && !self.complained && self.started {
            self.complained = true;
            tracing::warn!(
                unit = %unit,
                "journalctl 子进程已结束（unit 不存在、journald 未运行或命令缺失）：该日志规则本轮起无输入，\
                 60s 后会自动重试"
            );
        }
        lines
    }

    fn maybe_spawn(&mut self, unit: &str) {
        if self.spawn_failed || self.child_alive {
            return;
        }
        if let Some(last) = self.last_spawn_attempt {
            if last.elapsed() < JOURNALD_RESPAWN_BACKOFF {
                return;
            }
        }
        // 先把上一轮残留的子进程收干净，避免留下孤儿 journalctl。
        self.kill_child();
        self.last_spawn_attempt = Some(std::time::Instant::now());

        let binary = self
            .binary
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());

        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("-u")
            .arg(unit)
            .arg("-f")
            .arg("-o")
            .arg("cat")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // 独立进程组：杀掉子进程时不会波及 agent 自己（SPEC §7 的「不留僵尸」）。
            command.process_group(0);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                self.spawn_failed = true;
                tracing::warn!(
                    unit = %unit,
                    command = %binary,
                    error = %err,
                    "无法启动 journalctl（命令不存在或没有权限）：该日志规则已禁用，其余规则不受影响"
                );
                return;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            self.spawn_failed = true;
            tracing::warn!(unit = %unit, "journalctl 没有 stdout 管道，无法读取日志");
            let _ = child.start_kill();
            return;
        };

        let (tx, rx) = mpsc::channel::<String>(JOURNALD_BUFFER_LINES);
        let task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        if tx.send(line).await.is_err() {
                            break; // 采集器已经不再读取
                        }
                    }
                    Ok(None) => break, // stdout EOF：子进程结束
                    Err(_) => break,
                }
            }
        });

        self.rx = rx;
        self.task = Some(task);
        self.child = Some(child);
        self.child_alive = true;
        self.started = true;
        self.complained = false;
        tracing::debug!(unit = %unit, command = %binary, "journalctl 追随子进程已启动");
    }

    /// 杀掉并回收子进程（同步，供 `Drop` 使用）。幂等。
    fn kill_child(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let Some(mut child) = self.child.take() else {
            self.child_alive = false;
            return;
        };

        // 先 SIGTERM（tokio 内部对 unix 用 kill），再轮询回收。
        let _ = child.start_kill();
        for _ in 0..KILL_WAIT_ATTEMPTS {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.child_alive = false;
                    return;
                }
                Ok(None) => std::thread::sleep(KILL_WAIT_INTERVAL),
                Err(_) => break,
            }
        }
        let _ = child.start_kill();
        let _ = child.try_wait();
        self.child_alive = false;
        tracing::warn!("journalctl 子进程未在预期时间内退出，已再次发送终止信号");
    }
}

impl Drop for JournaldReader {
    fn drop(&mut self) {
        self.kill_child();
    }
}

/// 文件 tail 游标（含轮转/截断检测）。
#[derive(Debug)]
struct FileCursor {
    path: PathBuf,
    offset: u64,
    /// 上次读到 EOF 时停在半行上：下次读到的第一段要拼进那一行。
    pending_partial: bool,
    /// 当前文件标识（Unix 下是 inode），用于识别「同名新文件」。
    file_id: u64,
    /// 「文件不存在」只告警一次。
    warned_missing: bool,
    warned_rotated: bool,
}

impl FileCursor {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            pending_partial: false,
            file_id: 0,
            warned_missing: false,
            warned_rotated: false,
        }
    }

    /// 读取自上次以来新增的完整行（非阻塞，同步 IO）。文件不存在返回 `Ok(vec![])`。
    fn read_new_lines(&mut self) -> std::io::Result<Vec<String>> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !self.warned_missing {
                    self.warned_missing = true;
                    tracing::warn!(
                        path = %self.path.display(),
                        "日志文件不存在，本轮跳过；下一轮自动重试（文件被创建后立即生效）"
                    );
                }
                return Ok(Vec::new());
            }
            Err(err) => return Err(err),
        };
        self.warned_missing = false;

        let file_id = file_id_of(&metadata);
        let size = metadata.len();
        if self.offset > 0 && (file_id != self.file_id || size < self.offset) {
            // 轮转（inode 变了）或截断（size 回退）→ 重头读
            if !self.warned_rotated {
                self.warned_rotated = true;
                tracing::info!(
                    path = %self.path.display(),
                    old_offset = self.offset,
                    new_size = size,
                    "检测到日志文件被轮转或截断，游标已重置到文件开头"
                );
            }
            self.offset = 0;
            self.pending_partial = false;
        }
        self.file_id = file_id;

        if size == 0 || size <= self.offset {
            return Ok(Vec::new());
        }

        let mut file = std::fs::File::open(&self.path)?;
        if self.offset > 0 {
            file.seek(SeekFrom::Start(self.offset))?;
        }
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        self.offset = self.offset.saturating_add(buffer.len() as u64);

        let text = String::from_utf8_lossy(&buffer);
        let ends_with_newline = text.ends_with('\n');

        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if ends_with_newline {
            // 结尾有换行时 `split` 会多出一个空元素
            lines.pop();
        } else {
            // 结尾没有换行 → 最后一段是**半行**，不算完整日志行，留到下一轮再看。
            // 严格来说应该和下一轮读到的续写段拼接，但拼出来的日志没有可读性，
            // 这里直接丢弃续写段（代价：极少数「一条日志跨两次写入」的行会漏匹配，
            // 换来的是不把半行误当成一条日志去匹配/上送）。
            lines.pop();
        }

        if self.pending_partial && !lines.is_empty() {
            lines.remove(0);
        }
        self.pending_partial = !ends_with_newline;

        Ok(lines)
    }
}

#[cfg(unix)]
fn file_id_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn file_id_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// 一条规则的静态配置 + 运行期状态。
struct Rule {
    name: String,
    source: LogSource,
    /// 已编译的正则；`None` = 不过滤（任何行都命中）。
    pattern: Option<regex::Regex>,
    pattern_text: Option<String>,
    level: AlertLevel,
    throttle: Duration,
    max_lines: usize,
    include_raw: bool,
    /// 上次产出告警的时间。
    last_alert_at: Option<DateTime<Utc>>,
    /// 上次产出告警时累计的命中数（用于 summary 里的「累计」）。
    total_matches: u64,
    cursor: Option<FileCursor>,
    journald: Option<JournaldReader>,
}

impl Rule {
    fn source_label(&self) -> String {
        self.source.as_config_str()
    }

    fn drain_lines(&mut self) -> std::io::Result<Vec<String>> {
        if let Some(reader) = self.journald.as_mut() {
            let unit = self.source.unit().unwrap_or("<unknown>").to_string();
            return Ok(reader.drain(&unit));
        }
        if let Some(cursor) = self.cursor.as_mut() {
            let mut lines = cursor.read_new_lines()?;
            if lines.len() > FILE_MAX_LINES_PER_ROUND {
                let dropped = lines.len() - FILE_MAX_LINES_PER_ROUND;
                lines.drain(..dropped);
                tracing::warn!(
                    path = %cursor.path.display(),
                    dropped,
                    "单轮读取的日志行数超过上限，已丢弃最旧的部分（其余仍会在下一轮之前的读取中处理完）"
                );
            }
            return Ok(lines);
        }
        Ok(Vec::new())
    }
}

/// 日志规则采集器。
pub struct LogWatcherCollector {
    rules: Vec<Rule>,
    /// 可注入的 `journalctl` 命令路径（生产恒为 `"journalctl"`）。
    ///
    /// 构造时只被 `clone` 给每条规则的 [`JournaldReader`]，采集器自己不再读它，
    /// 因此非测试构建下允许它是「死字段」（它是测试注入 `journalctl` 假脚本的入口）。
    #[cfg_attr(not(test), allow(dead_code))]
    journalctl_bin: Arc<std::sync::Mutex<String>>,
}

impl LogWatcherCollector {
    /// 从 `agent.toml` 的 `[[log_rules]]` 构造。
    ///
    /// 正则非法只 `warn` 并跳过该规则（`main.rs` 的启动期校验已经拦过一道）。
    pub fn new(rules: &[LogRule]) -> Self {
        let journalctl_bin = Arc::new(std::sync::Mutex::new("journalctl".to_string()));

        let rules = rules
            .iter()
            .filter_map(|cfg| Self::build_rule(cfg, journalctl_bin.clone()))
            .collect();

        Self {
            rules,
            journalctl_bin,
        }
    }

    /// 生效的规则数量（**仅测试可用**，生产路径不需要）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// **仅测试可用**：把 `journalctl` 换成假脚本，用来真跑子进程分支。
    #[cfg(test)]
    pub fn set_journalctl_bin(&self, bin: impl Into<String>) {
        let mut guard = self
            .journalctl_bin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = bin.into();
    }

    /// **仅测试可用**：读取内部状态，用于轮转/节流断言。
    #[cfg(test)]
    pub fn rule_offsets(&self) -> Vec<(String, u64)> {
        self.rules
            .iter()
            .map(|rule| {
                (
                    rule.name.clone(),
                    rule.cursor.as_ref().map(|c| c.offset).unwrap_or(0),
                )
            })
            .collect()
    }

    fn build_rule(cfg: &LogRule, journalctl_bin: Arc<std::sync::Mutex<String>>) -> Option<Rule> {
        let pattern = match cfg.pattern.as_deref() {
            None => None,
            Some(text) => match regex::Regex::new(text) {
                Ok(regex) => Some(regex),
                Err(err) => {
                    tracing::warn!(
                        rule = %cfg.name,
                        pattern = %text,
                        error = %err,
                        "日志规则的正则非法，已跳过该规则（其余规则不受影响）"
                    );
                    return None;
                }
            },
        };

        let (cursor, journald) = match &cfg.source {
            LogSource::File(path) => (Some(FileCursor::new(path.clone())), None),
            LogSource::Journald(_) => (None, Some(JournaldReader::new(journalctl_bin))),
        };

        Some(Rule {
            name: cfg.name.clone(),
            source: cfg.source.clone(),
            pattern,
            pattern_text: cfg.pattern.clone(),
            level: map_level(cfg.level),
            throttle: Duration::from_secs(cfg.throttle_sec),
            max_lines: cfg.max_lines,
            include_raw: cfg.include_raw,
            last_alert_at: None,
            total_matches: 0,
            cursor,
            journald,
        })
    }
}

#[async_trait]
impl Collector for LogWatcherCollector {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn collect(&mut self, ctx: &CollectCtx) -> CollectOutcome {
        let mut outcome = CollectOutcome::empty();

        for rule in &mut self.rules {
            let lines = match rule.drain_lines() {
                Ok(lines) => lines,
                Err(err) => {
                    tracing::warn!(
                        rule = %rule.name,
                        source = %rule.source_label(),
                        error = %err,
                        "读取日志来源失败，本轮跳过该规则"
                    );
                    continue;
                }
            };
            if lines.is_empty() {
                continue;
            }

            let matched: Vec<&String> = match rule.pattern.as_ref() {
                Some(regex) => lines.iter().filter(|line| regex.is_match(line)).collect(),
                None => lines.iter().collect(),
            };
            if matched.is_empty() {
                tracing::trace!(
                    rule = %rule.name,
                    lines = lines.len(),
                    "本轮读取到日志行但没有命中规则"
                );
                continue;
            }

            let count = matched.len();
            rule.total_matches = rule.total_matches.saturating_add(count as u64);

            // ---- 节流：throttle_sec 内同一规则只报一次（按时间，不按轮数）----
            let throttled = rule
                .last_alert_at
                .map(|last| {
                    ctx.now
                        .signed_duration_since(last)
                        .to_std()
                        .map(|elapsed| elapsed < rule.throttle)
                        .unwrap_or(false) // now 早于 last（时钟回拨）→ 不节流
                })
                .unwrap_or(false);
            if throttled {
                tracing::debug!(
                    rule = %rule.name,
                    matched = count,
                    "日志规则命中但处于节流窗口内，不产出告警"
                );
                continue;
            }

            rule.last_alert_at = Some(ctx.now);
            outcome.alerts.push(Alert {
                id: format!("log:{}", rule.name),
                level: rule.level,
                source: SOURCE.to_string(),
                title: format!("【日志】{} 命中规则", rule.name),
                summary: render_summary(rule, &matched),
                first_seen: ctx.now,
            });
            tracing::info!(
                rule = %rule.name,
                matched = count,
                level = ?rule.level,
                source = %rule.source_label(),
                "日志规则命中，产出告警"
            );
        }

        outcome
    }
}

/// summary 渲染：**脱敏是硬约束**（docs/04 §2.3）。
fn render_summary(rule: &Rule, matched: &[&String]) -> String {
    let pattern = rule.pattern_text.as_deref().unwrap_or("<不过滤>");
    if !rule.include_raw {
        // 关键：只含规则名 + 计数 + 来源，绝不拼接任何原始日志行。
        return format!(
            "规则 {} 本周期命中 {} 行（累计 {} 行），来源 {}，pattern={}",
            rule.name,
            matched.len(),
            rule.total_matches,
            rule.source_label(),
            pattern
        );
    }

    let mut summary = format!(
        "规则 {} 本周期命中 {} 行（累计 {} 行），来源 {}",
        rule.name,
        matched.len(),
        rule.total_matches,
        rule.source_label()
    );
    for line in matched.iter().take(rule.max_lines) {
        // 原始行可能很长，截断到 200 字符（多字节安全）
        let text: String = line.chars().take(200).collect();
        summary.push_str("\n  ");
        summary.push_str(&text);
    }
    if matched.len() > rule.max_lines {
        summary.push_str(&format!(
            "\n  …另有 {} 行未附（max_lines = {}）",
            matched.len() - rule.max_lines,
            rule.max_lines
        ));
    }
    summary
}

/// 日志规则的告警级别（`ssm_common::config::LogLevel`）→ 协议级别（`AlertLevel`）。
///
/// 注意这是**两个独立类型**（SPEC §11）
fn map_level(level: LogLevel) -> AlertLevel {
    match level {
        LogLevel::Info => AlertLevel::Info,
        LogLevel::Warning => AlertLevel::Warning,
        LogLevel::Critical => AlertLevel::Critical,
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::Path;

    fn ctx_at(round: u64, secs: i64) -> CollectCtx {
        CollectCtx::new(
            "test-host",
            100,
            round,
            Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap() + chrono::Duration::seconds(secs),
        )
    }

    fn ctx(round: u64) -> CollectCtx {
        ctx_at(round, 0)
    }

    /// 临时目录守卫：`Drop` 时递归删除，**断言失败 panic 也会清理**。
    ///
    /// 用 RAII 而不是在测试末尾手动 `remove_dir_all`：手动清理在断言失败时会被跳过，
    /// 每跑一次失败的测试就在 `/tmp` 里留一堆垃圾（本项目踩过这个坑）。
    struct TempDir(PathBuf);

    /// 让 `&TempDir` 能自动当成 `&Path` 传给 `file_rule(name, &dir, ..)` 这类辅助函数。
    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 每个测试自己的临时目录（不污染仓库；`crates/agent` 没有 tempfile 依赖，这里手搓）。
    fn temp_dir(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "ssm-logwatch-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn file_rule(name: &str, path: &Path, extras: impl FnOnce(&mut LogRule)) -> LogRule {
        let mut rule = LogRule {
            name: name.to_string(),
            source: LogSource::File(path.to_path_buf()),
            pattern: Some("ERROR".to_string()),
            level: LogLevel::Warning,
            throttle_sec: 300,
            max_lines: 3,
            include_raw: false,
        };
        extras(&mut rule);
        rule
    }

    /// 读 `/proc/<pid>/stat` 的进程状态字母（`S`/`R`/`Z`…）；进程不存在返回 `None`。
    ///
    /// 用 `/proc` 而不是 `kill(pid, 0)`：僵尸进程仍然「存在」，但 `/proc` 能看出它是 `Z`
    /// （已被杀、只是还没被父进程回收），这正是「没留下在跑的 journalctl」的准确判据。
    #[cfg(unix)]
    fn process_state(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // 格式：`pid (comm) STATE ...`；comm 里可能含空格与括号，所以从最后一个 ')' 往后切
        let after_comm = stat.rsplit_once(')')?.1;
        after_comm.split_whitespace().next().map(|s| s.to_string())
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.flush().unwrap();
    }

    #[tokio::test]
    async fn matching_line_produces_alert_with_stable_id_and_level() {
        let dir = temp_dir("match");
        let log = dir.join("test.log");
        std::fs::write(&log, "").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |_| {})]);
        assert_eq!(collector.len(), 1);

        // 没有内容 → 没有告警
        let outcome = collector.collect(&ctx(1)).await;
        assert!(outcome.alerts.is_empty());

        append(&log, "2026-10-15T01:30:00+00:00 ERROR e2e injected\n");
        let outcome = collector.collect(&ctx(2)).await;
        assert_eq!(outcome.alerts.len(), 1, "命中必须产出一条告警");
        let alert = &outcome.alerts[0];
        assert_eq!(alert.id, "log:test-error", "id 必须稳定可去重");
        assert_eq!(alert.source, "log_watcher");
        assert_eq!(alert.level, AlertLevel::Warning, "级别必须取自规则");
        assert!(alert.title.contains("test-error"), "{}", alert.title);
        assert!(alert.summary.contains("test-error"), "{}", alert.summary);
        assert_eq!(
            alert.first_seen,
            Utc.with_ymd_and_hms(2026, 10, 15, 1, 30, 0).unwrap(),
            "first_seen 必须来自 CollectCtx::now"
        );

        // 不匹配的行不告警
        append(&log, "just an info line\n");
        let outcome = collector.collect(&ctx(3)).await;
        assert!(outcome.alerts.is_empty(), "{:?}", outcome.alerts);
    }

    /// **事件型语义（SPEC §13.2，回归护栏）**：命中一次 → **只有那一轮**含告警，
    /// 后续轮次**不含**（这是正确行为，不是恢复；relay 不推「已恢复」）。
    ///
    /// `throttle_sec = 0`（关掉限频）是刻意的：这样「第 2/3 轮不含」只能是事件型语义
    /// 造成的，不可能是限频掩盖出来的。
    #[tokio::test]
    async fn event_semantics_alert_appears_only_in_the_matching_round() {
        let dir = temp_dir("event-semantics");
        let log = dir.join("app.log");
        std::fs::write(&log, "").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.throttle_sec = 0; // 关掉限频，排除「被 throttle 掩盖」
        })]);

        // 第 1 轮之前写入命中行
        append(&log, "2026-10-15T01:30:00+00:00 ERROR injected once\n");

        let first = collector.collect(&ctx(1)).await;
        assert_eq!(
            first.alerts.len(),
            1,
            "命中那一轮必须含告警: {:?}",
            first.alerts
        );
        assert_eq!(first.alerts[0].id, "log:test-error");
        assert_eq!(first.alerts[0].source, "log_watcher");
        assert_eq!(first.alerts[0].level, AlertLevel::Warning);

        // 第 2、3 轮：没有新命中 → **不含**（事件型；缺席 ≠ 恢复）
        for round in 2..=3 {
            let outcome = collector.collect(&ctx(round)).await;
            assert!(
                outcome.alerts.is_empty(),
                "第 {round} 轮没有新命中，不该再含该告警（事件型只报一次）: {:?}",
                outcome.alerts
            );
        }

        // 第 4 轮再来一次新命中 → 新一轮事件，可以再报（且仍不是「恢复」类告警）
        append(&log, "ERROR injected again\n");
        let fourth = collector.collect(&ctx(4)).await;
        assert_eq!(
            fourth.alerts.len(),
            1,
            "新的命中是一个新事件: {:?}",
            fourth.alerts
        );
        assert_eq!(
            fourth.alerts[0].id, "log:test-error",
            "id 稳定（relay 据此去重）"
        );
        assert!(
            !fourth.alerts[0].id.contains("recovered"),
            "事件型告警没有恢复语义: {}",
            fourth.alerts[0].id
        );
        assert!(
            !fourth.alerts[0].title.contains("恢复"),
            "事件型告警不该产出恢复通知: {}",
            fourth.alerts[0].title
        );
    }

    /// **节流**：`throttle_sec` 窗口内第二次写入不再告警（e2e 会连写两次 ERROR）。
    #[tokio::test]
    async fn throttle_window_suppresses_second_alert_then_allows_after_expiry() {
        let dir = temp_dir("throttle");
        let log = dir.join("test.log");
        std::fs::write(&log, "").unwrap();

        let mut collector =
            LogWatcherCollector::new(&[file_rule("test-error", &log, |r| r.throttle_sec = 300)]);

        append(&log, "ERROR first\n");
        let first = collector.collect(&ctx_at(1, 0)).await;
        assert_eq!(first.alerts.len(), 1, "第一次必须告警");

        append(&log, "ERROR second\n");
        let second = collector.collect(&ctx_at(2, 10)).await;
        assert!(
            second.alerts.is_empty(),
            "10s 后（节流窗口 300s 内）第二次命中不该再告警: {:?}",
            second.alerts
        );

        append(&log, "ERROR third\n");
        let third = collector.collect(&ctx_at(3, 200)).await;
        assert!(
            third.alerts.is_empty(),
            "200s 仍在 300s 窗口内: {:?}",
            third.alerts
        );

        append(&log, "ERROR fourth\n");
        let fourth = collector.collect(&ctx_at(4, 301)).await;
        assert_eq!(
            fourth.alerts.len(),
            1,
            "超过 300s 必须重新告警（说明节流是基于时间不是基于轮数）"
        );
        assert!(
            fourth.alerts[0].summary.contains('4'),
            "{}",
            fourth.alerts[0].summary
        );
    }

    /// **脱敏硬约束**：`include_raw = false` 时 summary 绝不含原始日志行。
    #[tokio::test]
    async fn include_raw_false_never_leaks_raw_lines() {
        let dir = temp_dir("redact");
        let log = dir.join("test.log");
        std::fs::write(&log, "").unwrap();

        const SECRET: &str = "password=SUPER_SECRET_VALUE_42";

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.throttle_sec = 0;
            r.include_raw = false;
        })]);

        append(&log, &format!("ERROR login failed {SECRET}\n"));
        let outcome = collector.collect(&ctx(1)).await;
        assert_eq!(outcome.alerts.len(), 1);
        let summary = &outcome.alerts[0].summary;
        assert!(
            !summary.contains(SECRET),
            "include_raw=false 时 summary 绝不能含原始日志行，实际: {summary:?}"
        );
        assert!(
            !summary.contains("login failed"),
            "include_raw=false 时 summary 不能含任何原始行片段，实际: {summary:?}"
        );
        // 但必须仍然能看出「哪条规则命中了几行」
        assert!(summary.contains("test-error"), "{summary:?}");
        assert!(summary.contains('1'), "{summary:?}");
        assert!(summary.contains("file:"), "summary 应标明来源: {summary:?}");
    }

    /// `include_raw = true` 时才附原文，且受 `max_lines` 限制。
    #[tokio::test]
    async fn include_raw_true_appends_at_most_max_lines() {
        let dir = temp_dir("raw");
        let log = dir.join("test.log");
        std::fs::write(&log, "").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.include_raw = true;
            r.max_lines = 2;
            r.throttle_sec = 0;
        })]);

        append(&log, "ERROR alpha\nERROR beta\nERROR gamma\nERROR delta\n");
        let outcome = collector.collect(&ctx(1)).await;
        assert_eq!(outcome.alerts.len(), 1);
        let summary = &outcome.alerts[0].summary;
        assert!(summary.contains("ERROR alpha"), "{summary}");
        assert!(summary.contains("ERROR beta"), "{summary}");
        assert!(
            !summary.contains("ERROR gamma"),
            "超出 max_lines 的行不能出现: {summary}"
        );
        assert!(
            summary.contains("另有 2 行未附"),
            "必须说明还有多少行没附: {summary}"
        );
        assert!(summary.contains("命中 4 行"), "{summary}");
    }

    /// 文件被**轮转**（rename + 新文件）：必须检测到并重头读。
    #[tokio::test]
    async fn rotated_file_is_detected_and_read_from_scratch() {
        let dir = temp_dir("rotate");
        let log = dir.join("app.log");
        let rotated = dir.join("app.log.1");
        std::fs::write(&log, "ERROR before rotation\n").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.throttle_sec = 0;
        })]);

        let first = collector.collect(&ctx(1)).await;
        assert_eq!(first.alerts.len(), 1, "轮转前的 ERROR 必须告警");

        // logrotate 风格：改名 + 新文件
        std::fs::rename(&log, &rotated).unwrap();
        std::fs::write(&log, "ERROR after rotation\n").unwrap();

        let second = collector.collect(&ctx(2)).await;
        assert_eq!(
            second.alerts.len(),
            1,
            "轮转后新文件里的 ERROR 必须被读到（说明 inode 变化被检测到）"
        );
    }

    /// 文件被**截断**（`: > log`，size 回退）：同样要重头读。
    #[tokio::test]
    async fn truncated_file_is_read_from_scratch() {
        let dir = temp_dir("truncate");
        let log = dir.join("app.log");
        std::fs::write(&log, "ERROR first pass\n").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.throttle_sec = 0;
        })]);
        let first = collector.collect(&ctx(1)).await;
        assert_eq!(first.alerts.len(), 1);

        // 截断 + 写新内容（内容比原来短，size 回退）
        std::fs::write(&log, "ERROR x\n").unwrap();
        let second = collector.collect(&ctx(2)).await;
        assert_eq!(
            second.alerts.len(),
            1,
            "截断后必须从 0 重读（size 回退被检测到）"
        );

        // 再验证 offset 确实从 0 重新累计过
        let offsets = collector.rule_offsets();
        assert_eq!(offsets.len(), 1);
        assert_eq!(offsets[0].1, "ERROR x\n".len() as u64);
    }

    /// 只有**完整行**才参与匹配；不完整的尾行要等下一轮补齐。
    #[tokio::test]
    async fn partial_trailing_line_waits_for_completion() {
        let dir = temp_dir("partial");
        let log = dir.join("app.log");
        std::fs::write(&log, "").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.throttle_sec = 0;
        })]);

        // 没有换行 → 还没有完整行。注意行已经落盘，offset 会推进，
        // 但 pending_partial 会让下一轮丢弃续写段。
        append(&log, "ERROR incomplete without newline");
        let outcome = collector.collect(&ctx(1)).await;
        assert!(
            outcome.alerts.is_empty(),
            "没有换行的半行不该被当成完整日志行: {:?}",
            outcome.alerts
        );

        // 补齐换行 → 续写段被丢弃（不误报），但**新的一整行**仍会被匹配
        append(&log, "\nERROR complete line\n");
        let outcome = collector.collect(&ctx(2)).await;
        assert_eq!(outcome.alerts.len(), 1, "完整行必须被匹配");
    }

    /// 文件不存在：不 panic、不告警，下一轮文件出现后立刻生效。
    #[tokio::test]
    async fn missing_file_is_graceful_and_recovers_next_round() {
        let dir = temp_dir("missing");
        let log = dir.join("not-yet.log");

        let mut collector = LogWatcherCollector::new(&[file_rule("test-error", &log, |r| {
            r.throttle_sec = 0;
        })]);

        let outcome = collector.collect(&ctx(1)).await;
        assert!(outcome.alerts.is_empty(), "文件不存在时不该告警");
        assert!(outcome.services.is_empty());

        std::fs::write(&log, "ERROR file appeared\n").unwrap();
        let outcome = collector.collect(&ctx(2)).await;
        assert_eq!(outcome.alerts.len(), 1, "文件出现后必须立刻开始匹配");
    }

    /// 非法正则：构造期跳过该规则，不影响其它规则，不 panic。
    #[tokio::test]
    async fn invalid_regex_skips_rule_only() {
        let dir = temp_dir("badregex");
        let a = dir.join("a.log");
        let b = dir.join("b.log");
        std::fs::write(&a, "ERROR a\n").unwrap();
        std::fs::write(&b, "ERROR b\n").unwrap();

        let mut collector = LogWatcherCollector::new(&[
            file_rule("bad", &a, |r| r.pattern = Some("(".to_string())),
            file_rule("good", &b, |r| r.throttle_sec = 0),
        ]);
        assert_eq!(collector.len(), 1, "非法正则的规则必须被跳过");

        let outcome = collector.collect(&ctx(1)).await;
        assert_eq!(outcome.alerts.len(), 1);
        assert_eq!(outcome.alerts[0].id, "log:good");
    }

    /// 不写 `pattern` = 不过滤，任何行都命中。
    #[tokio::test]
    async fn rule_without_pattern_matches_every_line() {
        let dir = temp_dir("nopattern");
        let log = dir.join("a.log");
        std::fs::write(&log, "").unwrap();

        let mut collector = LogWatcherCollector::new(&[file_rule("all", &log, |r| {
            r.pattern = None;
            r.throttle_sec = 0;
        })]);

        append(&log, "nothing special here\n");
        let outcome = collector.collect(&ctx(1)).await;
        assert_eq!(outcome.alerts.len(), 1, "无 pattern 必须命中所有行");
        assert!(
            outcome.alerts[0].summary.contains("<不过滤>"),
            "{}",
            outcome.alerts[0].summary
        );
    }

    /// `level` 三档必须原样映射到协议级别（两个独立类型，别混用）。
    #[tokio::test]
    async fn alert_level_follows_rule_level() {
        for (level, expected) in [
            (LogLevel::Info, AlertLevel::Info),
            (LogLevel::Warning, AlertLevel::Warning),
            (LogLevel::Critical, AlertLevel::Critical),
        ] {
            let dir = temp_dir("level");
            let log = dir.join("a.log");
            std::fs::write(&log, "ERROR x\n").unwrap();

            let mut collector = LogWatcherCollector::new(&[file_rule("r", &log, |r| {
                r.level = level;
            })]);
            let outcome = collector.collect(&ctx(1)).await;
            assert_eq!(outcome.alerts.len(), 1);
            assert_eq!(
                outcome.alerts[0].level, expected,
                "level = {level:?} 映射错误"
            );
        }
    }

    /// 多规则互不干扰，各自独立节流。
    #[tokio::test]
    async fn rules_have_independent_throttle_state() {
        let dir = temp_dir("multi");
        let a = dir.join("a.log");
        let b = dir.join("b.log");
        std::fs::write(&a, "ERROR a\n").unwrap();
        std::fs::write(&b, "ERROR b\n").unwrap();

        let mut collector = LogWatcherCollector::new(&[
            file_rule("rule-a", &a, |_| {}),
            file_rule("rule-b", &b, |_| {}),
        ]);

        let outcome = collector.collect(&ctx(1)).await;
        let ids: Vec<&str> = outcome.alerts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["log:rule-a", "log:rule-b"], "两条规则都要告警");
    }

    #[test]
    fn collector_name_is_log_watcher() {
        let collector = LogWatcherCollector::new(&[]);
        assert_eq!(Collector::name(&collector), "log_watcher");
        assert!(collector.is_empty());
    }

    // ---------------- journald 分支 ----------------

    /// journald 命令缺失：不 panic、不告警、可重复采集。
    #[tokio::test]
    async fn journald_missing_command_is_graceful() {
        let rule = LogRule {
            name: "nginx-error".to_string(),
            source: LogSource::Journald("ssm-no-such-unit.service".to_string()),
            pattern: Some("ERROR".to_string()),
            level: LogLevel::Warning,
            throttle_sec: 300,
            max_lines: 3,
            include_raw: false,
        };

        let mut collector = LogWatcherCollector::new(&[rule]);
        collector.set_journalctl_bin("ssm-no-such-journalctl-xyz");

        for round in 1..=3 {
            let outcome = collector.collect(&ctx(round)).await;
            assert!(
                outcome.alerts.is_empty(),
                "journalctl 不存在时不该产告警（第 {round} 轮）: {:?}",
                outcome.alerts
            );
        }
    }

    /// journald 子进程分支**真跑**：注入假 journalctl 脚本，验证 stdout 被按行读取并匹配。
    #[cfg(unix)]
    #[tokio::test]
    async fn journald_subprocess_lines_are_read_and_matched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("journald");
        let script = dir.join("fake-journalctl");
        let pidfile = dir.join("pid");
        // 假 journalctl：先写出自己的 pid（供 Drop 断言用），吐一行 ERROR，然后靠后台
        // `sleep` 保持存活 —— 不用 `while read`，因为 `/bin/sh` 读 stdin 时会 fork 子 shell，
        // 那样写进 pidfile 的就不是被 spawn 的那个进程本身了。
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > \"{}\"\necho 'ERROR from fake journald'\necho 'all good'\nsleep 60 &\nwait\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let rule = LogRule {
            name: "test-error".to_string(),
            source: LogSource::Journald("fake.service".to_string()),
            pattern: Some("ERROR".to_string()),
            level: LogLevel::Warning,
            throttle_sec: 0,
            max_lines: 3,
            include_raw: true,
        };

        let mut collector = LogWatcherCollector::new(&[rule]);
        collector.set_journalctl_bin(script.display().to_string());

        // 第一轮启动子进程（此时还没有输出：
        // 脚本是启动后才吐行的）
        let first = collector.collect(&ctx(1)).await;
        assert!(first.alerts.is_empty(), "子进程刚起，还没有输出");

        // 等假 journalctl 吐行
        tokio::time::sleep(Duration::from_millis(800)).await;
        let second = collector.collect(&ctx(2)).await;
        assert_eq!(
            second.alerts.len(),
            1,
            "假 journalctl 的 ERROR 行必须被读到并匹配: {:?}",
            second.alerts
        );
        assert_eq!(second.alerts[0].id, "log:test-error");
        assert!(
            second.alerts[0]
                .summary
                .contains("ERROR from fake journald"),
            "include_raw=true 时应附原文: {}",
            second.alerts[0].summary
        );

        // 没有新行 → 不再重复告警
        let third = collector.collect(&ctx(3)).await;
        assert!(
            third.alerts.is_empty(),
            "没有新日志行时不该重复告警: {:?}",
            third.alerts
        );

        // ---- Drop 必须真的杀掉子进程 ----
        // 判据是「子进程真的没了」，而不是 shell 的 trap 标记（trap 与信号的交互依赖具体
        // shell）。注意 `/proc/<pid>/stat` 可能读到 `Z`：tokio 在 SIGCHLD 时回收子进程，
        // 所以「已经不在运行」既可能是进程消失、也可能是只剩一个僵尸。
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("假 journalctl 必须写出自己的 pid")
            .trim()
            .parse()
            .expect("pid 必须是数字");
        assert!(
            matches!(process_state(pid).as_deref(), Some("S") | Some("R")),
            "采集中子进程必须活着（pid {pid}，state={:?}）",
            process_state(pid)
        );

        drop(collector); // Drop 里必须杀子进程

        let mut killed = false;
        for _ in 0..30 {
            match process_state(pid) {
                // 已消失，或只剩等待回收的僵尸 → 都没有在跑的 journalctl 了
                None => {
                    killed = true;
                    break;
                }
                Some(state) if state == "Z" => {
                    killed = true;
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        assert!(
            killed,
            "Drop 必须把 journalctl 子进程杀掉（pid {pid} 仍然存活，state={:?}）",
            process_state(pid)
        );
    }

    /// 假 journalctl 立刻退出：不能 panic，之后不再反复拉起刷日志。
    #[cfg(unix)]
    #[tokio::test]
    async fn journald_child_exiting_immediately_is_handled() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("journald-exit");
        let script = dir.join("fake-journalctl");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let rule = LogRule {
            name: "test-error".to_string(),
            source: LogSource::Journald("fake.service".to_string()),
            pattern: Some("ERROR".to_string()),
            level: LogLevel::Warning,
            throttle_sec: 0,
            max_lines: 3,
            include_raw: false,
        };
        let mut collector = LogWatcherCollector::new(&[rule]);
        collector.set_journalctl_bin(script.display().to_string());

        for round in 1..=3 {
            let outcome = collector.collect(&ctx(round)).await;
            assert!(outcome.alerts.is_empty(), "第 {round} 轮不该有告警");
        }
    }
}
