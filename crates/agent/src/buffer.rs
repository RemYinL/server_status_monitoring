//! 上报失败本地环形缓冲（SPEC §7 `buffer.rs`）。
//!
//! 语义：
//! - 落盘格式为 **JSONL**：一行一个 `ReportPacket`（`serde_json::to_string` 不产生换行，
//!   所以「一行一包」是安全的）。
//! - 追加写；超过 `buffer_max` 时**淘汰最旧**（环形），淘汰后整体重写文件。
//! - 补发成功的那批从队首移除（重写文件）；全部发完即文件为空。
//! - 启动时 [`ReportBuffer::load`] 从磁盘恢复，坏行（例如崩溃时被截断的最后一行）
//!   只记 warn 并跳过，不让整个 agent 起不来。
//!
//! 写文件一律走「临时文件 + rename」的原子替换，避免断电留下半个文件。

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use ssm_common::protocol::ReportPacket;

/// 临时文件名后缀（与目标文件同目录，保证 `rename` 是同一文件系统内的原子操作）。
const TEMP_SUFFIX: &str = ".tmp";

/// 上报失败缓冲（内存队列 + JSONL 落盘）。
#[derive(Debug)]
pub struct ReportBuffer {
    path: PathBuf,
    max: usize,
    pending: VecDeque<ReportPacket>,
}

impl ReportBuffer {
    /// 建一个空缓冲（不读盘）。`max` 为 0 时按 1 处理，否则「淘汰最旧」会退化成不留任何数据。
    pub fn new(path: impl Into<PathBuf>, max: usize) -> Self {
        Self {
            path: path.into(),
            max: max.max(1),
            pending: VecDeque::new(),
        }
    }

    /// 启动时从 `path` 恢复；文件不存在视为空缓冲。
    ///
    /// 只会保留**最后 `max` 条**（超出部分淘汰最旧），并在发生截断时把文件重写成一致状态。
    pub fn load(path: impl Into<PathBuf>, max: usize) -> std::io::Result<Self> {
        let mut buffer = Self::new(path, max);
        let path = buffer.path.clone();

        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(buffer),
            Err(err) => return Err(err),
        };

        let mut skipped = 0_usize;
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ReportPacket>(&line) {
                Ok(packet) => buffer.pending.push_back(packet),
                Err(err) => {
                    skipped += 1;
                    tracing::warn!(
                        path = %path.display(),
                        line = index + 1,
                        error = %err,
                        "本地缓冲里有无法解析的行，已跳过（崩溃截断属正常）"
                    );
                }
            }
        }

        let loaded = buffer.pending.len();
        let mut truncated = skipped > 0;
        while buffer.pending.len() > buffer.max {
            buffer.pending.pop_front();
            truncated = true;
        }
        if truncated {
            buffer.persist()?;
            tracing::warn!(
                path = %path.display(),
                loaded,
                kept = buffer.pending.len(),
                skipped,
                "本地缓冲已按 buffer_max 截断最旧记录"
            );
        }
        if !buffer.pending.is_empty() {
            tracing::info!(
                path = %path.display(),
                pending = buffer.pending.len(),
                "从磁盘恢复了未发出的上报包，本轮起会按原始 sent_at 补发"
            );
        }
        Ok(buffer)
    }

    /// 缓冲文件路径（`main.rs` 启动日志里会打出来，便于确认实际用的是哪个文件）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 上限（`agent.toml` 的 `buffer_max`）。**仅测试代码可用**。
    #[cfg(test)]
    pub fn max(&self) -> usize {
        self.max
    }

    /// 待补发的包，按 `sent_at` 从旧到新。
    pub fn pending(&self) -> &VecDeque<ReportPacket> {
        &self.pending
    }

    /// 队首（最旧的未发出包）。**仅测试代码可用**。
    #[cfg(test)]
    pub fn front(&self) -> Option<&ReportPacket> {
        self.pending.front()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// 入队并追加落盘；超出 `max` 时淘汰最旧并重写文件。
    pub fn push(&mut self, packet: ReportPacket) -> std::io::Result<()> {
        // 先追加（快路径：一行一次 append，不必重写整个文件）。
        self.append(&packet)?;
        self.pending.push_back(packet);

        if self.pending.len() > self.max {
            let mut evicted = 0_usize;
            while self.pending.len() > self.max {
                self.pending.pop_front();
                evicted += 1;
            }
            // 队首的几条已经写进文件了，必须重写文件才能与内存一致。
            self.persist()?;
            tracing::warn!(
                path = %self.path.display(),
                evicted,
                max = self.max,
                "本地缓冲已满，淘汰最旧的上报包"
            );
        }
        Ok(())
    }

    /// 丢弃队首 `count` 条（已补发成功），并重写文件。
    pub fn drop_front(&mut self, count: usize) -> std::io::Result<()> {
        let count = count.min(self.pending.len());
        for _ in 0..count {
            self.pending.pop_front();
        }
        if count > 0 {
            self.persist()?;
        }
        Ok(())
    }

    /// 清空内存与磁盘（全部补发成功 / 优雅退出前）。
    pub fn clear(&mut self) -> std::io::Result<()> {
        self.pending.clear();
        remove_if_exists(&self.path)?;
        remove_if_exists(&temp_path(&self.path))?;
        Ok(())
    }

    /// 把内存里的队列整体原子写回磁盘（优雅退出、截断、丢弃已发条目时调用）。
    pub fn persist(&self) -> std::io::Result<()> {
        let mut contents = String::new();
        for packet in &self.pending {
            contents.push_str(&to_jsonl_line(packet)?);
        }
        write_atomic(&self.path, &contents)
    }

    // ---------------------------------------------------------------- 内部

    fn append(&self, packet: &ReportPacket) -> std::io::Result<()> {
        ensure_parent_dir(&self.path)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(to_jsonl_line(packet)?.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }
}

/// 序列化成一行 JSONL（末尾带 `\n`）。
fn to_jsonl_line(packet: &ReportPacket) -> std::io::Result<String> {
    let mut line = serde_json::to_string(packet)?;
    line.push('\n');
    Ok(line)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(TEMP_SUFFIX);
    path.with_file_name(name)
}

fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// 临时文件 + `rename` 的原子替换；`rename` 在同一文件系统内是原子的。
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    ensure_parent_dir(path)?;
    let temp = temp_path(path);
    {
        let mut file = File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use ssm_common::protocol::{
        Alert, AlertLevel, Metrics, ServiceKind, ServiceStatus, PROTOCOL_VERSION,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    /// 独立临时目录，`Drop` 时清理——绝不写进仓库。
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ssm-agent-buffer-{tag}-{}-{}",
                std::process::id(),
                UNIQUE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).expect("创建临时目录");
            Self(dir)
        }

        pub(super) fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ts(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().expect("合法时间戳")
    }

    fn metrics() -> Metrics {
        Metrics {
            cpu_percent: 1.5,
            mem_used_mb: 100,
            mem_total_mb: 200,
            disks: vec![],
            load_avg: [0.1, 0.2, 0.3],
            uptime_sec: 42,
            net: None,
        }
    }

    fn packet(seq: u64, sent_at_secs: i64) -> ReportPacket {
        ReportPacket {
            version: PROTOCOL_VERSION,
            host_id: "test-host".to_string(),
            sent_at: ts(sent_at_secs),
            seq,
            metrics: metrics(),
            services: vec![ServiceStatus {
                name: format!("svc-{seq}"),
                kind: ServiceKind::Tcp,
                target: "127.0.0.1:80".to_string(),
                ok: true,
                latency_ms: Some(1),
                detail: None,
            }],
            git: vec![],
            alerts: vec![Alert {
                id: format!("alert:{seq}"),
                level: AlertLevel::Info,
                source: "test".to_string(),
                title: "t".to_string(),
                summary: "s".to_string(),
                first_seen: ts(sent_at_secs),
            }],
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .expect("缓冲文件应存在")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn push_appends_one_jsonl_line_per_packet() {
        let dir = TempDir::new("append");
        let path = dir.file("buffer.jsonl");
        let mut buffer = ReportBuffer::new(&path, 10);

        buffer.push(packet(1, 1_000)).unwrap();
        assert_eq!(buffer.len(), 1);
        assert!(!buffer.is_empty());
        assert_eq!(buffer.path(), path.as_path());
        assert_eq!(buffer.max(), 10);
        assert_eq!(lines(&path).len(), 1);

        buffer.push(packet(2, 1_001)).unwrap();
        buffer.push(packet(3, 1_002)).unwrap();
        let raw = lines(&path);
        assert_eq!(raw.len(), 3, "每包一行");
        assert!(
            !raw[0].contains('\n'),
            "序列化结果里不能有内嵌换行，否则 JSONL 无法逐行解析"
        );

        // 文件内容必须能逐行还原成 packet，且顺序 = 入队顺序
        let parsed: Vec<ReportPacket> = raw
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            parsed,
            vec![packet(1, 1_000), packet(2, 1_001), packet(3, 1_002)]
        );
        assert_eq!(
            buffer.pending().iter().cloned().collect::<Vec<_>>(),
            parsed,
            "内存队列与文件必须一致"
        );
        assert_eq!(buffer.front().map(|p| p.seq), Some(1));
    }

    #[test]
    fn push_beyond_max_evicts_oldest_and_keeps_file_in_sync() {
        let dir = TempDir::new("evict");
        let path = dir.file("buffer.jsonl");
        let mut buffer = ReportBuffer::new(&path, 3);

        for seq in 1..=5 {
            buffer.push(packet(seq, 1_000 + seq as i64)).unwrap();
        }

        assert_eq!(buffer.len(), 3, "不能超过 buffer_max");
        let seqs: Vec<u64> = buffer.pending().iter().map(|p| p.seq).collect();
        assert_eq!(seqs, [3, 4, 5], "淘汰的必须是最旧的");

        // 文件也必须只剩 3 行（否则重启会把淘汰掉的又读回来）
        let raw = lines(&path);
        assert_eq!(raw.len(), 3, "淘汰后文件必须被重写成一致状态: {raw:?}");
        let file_seqs: Vec<u64> = raw
            .iter()
            .map(|l| serde_json::from_str::<ReportPacket>(l).unwrap().seq)
            .collect();
        assert_eq!(file_seqs, [3, 4, 5]);

        // buffer_max 为 0 时按 1 处理，而不是把缓冲变成黑洞
        let zero = ReportBuffer::new(dir.file("zero.jsonl"), 0);
        assert_eq!(zero.max(), 1);
    }

    #[test]
    fn drop_front_rewrites_file_keeping_unsent_packets() {
        let dir = TempDir::new("drop");
        let path = dir.file("buffer.jsonl");
        let mut buffer = ReportBuffer::new(&path, 10);
        for seq in 1..=4 {
            buffer.push(packet(seq, 1_000 + seq as i64)).unwrap();
        }

        buffer.drop_front(3).unwrap();
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.front().map(|p| p.seq), Some(4));
        assert_eq!(lines(&path).len(), 1);
        assert_eq!(
            serde_json::from_str::<ReportPacket>(&lines(&path)[0])
                .unwrap()
                .seq,
            4
        );

        // 越界 drop 不 panic
        buffer.drop_front(99).unwrap();
        assert!(buffer.is_empty());
        assert_eq!(lines(&path), Vec::<String>::new());
    }

    #[test]
    fn clear_removes_file_and_memory() {
        let dir = TempDir::new("clear");
        let path = dir.file("buffer.jsonl");
        let mut buffer = ReportBuffer::new(&path, 10);
        buffer.push(packet(1, 1_000)).unwrap();
        buffer.push(packet(2, 1_001)).unwrap();
        assert!(path.exists());

        buffer.clear().unwrap();
        assert!(buffer.is_empty());
        assert!(!path.exists(), "成功后清空必须把文件也删掉");
        // 幂等：再清一次也不报错
        buffer.clear().unwrap();
    }

    #[test]
    fn load_restores_pending_after_restart() {
        let dir = TempDir::new("reload");
        let path = dir.file("buffer.jsonl");

        {
            let mut buffer = ReportBuffer::new(&path, 10);
            buffer.push(packet(7, 1_700_000_000)).unwrap();
            buffer.push(packet(8, 1_700_000_030)).unwrap();
        }

        let reloaded = ReportBuffer::load(&path, 10).unwrap();
        assert_eq!(reloaded.len(), 2);
        let seqs: Vec<u64> = reloaded.pending().iter().map(|p| p.seq).collect();
        assert_eq!(seqs, [7, 8]);
        // sent_at 原样恢复（补发靠它判断过期）
        assert_eq!(reloaded.pending()[0].sent_at, ts(1_700_000_000));
        assert_eq!(reloaded.pending()[1].sent_at, ts(1_700_000_030));
        assert_eq!(reloaded.pending()[0], packet(7, 1_700_000_000));

        // 不存在的文件 => 空缓冲，不报错
        let missing = ReportBuffer::load(dir.file("nope.jsonl"), 10).unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn load_skips_torn_lines_and_truncates_to_max() {
        let dir = TempDir::new("torn");
        let path = dir.file("buffer.jsonl");
        let mut raw = String::new();
        for seq in 1..=5 {
            raw.push_str(&serde_json::to_string(&packet(seq, 1_000 + seq as i64)).unwrap());
            raw.push('\n');
        }
        // 模拟崩溃：最后一行被截断成半个 JSON
        raw.push_str("{\"version\":1,\"host_id\":\"test-hos");
        fs::write(&path, raw).unwrap();

        let buffer = ReportBuffer::load(&path, 3).unwrap();
        assert_eq!(buffer.len(), 3, "坏行跳过 + 只保留最后 3 条");
        let seqs: Vec<u64> = buffer.pending().iter().map(|p| p.seq).collect();
        assert_eq!(seqs, [3, 4, 5]);

        // 截断后文件必须被重写成一致的 3 行
        let file_seqs: Vec<u64> = lines(&path)
            .iter()
            .map(|l| serde_json::from_str::<ReportPacket>(l).unwrap().seq)
            .collect();
        assert_eq!(file_seqs, [3, 4, 5]);

        // 纯垃圾文件也不能让 agent 起不来
        let garbage = dir.file("garbage.jsonl");
        fs::write(&garbage, "not json at all\n\n").unwrap();
        let buffer = ReportBuffer::load(&garbage, 10).unwrap();
        assert!(buffer.is_empty());
    }

    #[test]
    fn load_creates_parent_dir_on_first_push() {
        let dir = TempDir::new("mkdir");
        let path = dir.file("nested/deeper/buffer.jsonl");
        let mut buffer = ReportBuffer::new(&path, 10);
        buffer.push(packet(1, 1_000)).unwrap();
        assert!(path.exists(), "push 必须自动建父目录");
    }

    #[test]
    fn persist_is_atomic_and_leaves_no_temp_file() {
        let dir = TempDir::new("atomic");
        let path = dir.file("buffer.jsonl");
        let mut buffer = ReportBuffer::new(&path, 10);
        buffer.push(packet(1, 1_000)).unwrap();
        buffer.persist().unwrap();

        assert!(path.exists());
        assert!(!temp_path(&path).exists(), "原子替换后不能残留 .tmp 文件");
        assert_eq!(lines(&path).len(), 1);
    }
}
