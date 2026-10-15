//! SQLite 持久化层（SPEC §7 `store.rs`）。
//!
//! 两条职责：
//!
//! 1. **落库**：`heartbeats` / `alerts` / `state` 三张表，`(host_id, seq)` 唯一去重。
//! 2. **tmpfs → SSD 快照**：`db_path` 通常位于 tmpfs（掉电即失），每
//!    `snapshot_interval_sec` 用 SQLite `VACUUM INTO` 生成一致性快照，再原子替换到
//!    `snapshot_path`；**启动时若 `db_path` 不存在而 `snapshot_path` 存在则先恢复**
//!    （docs/03 §5「relay 重启不误报」，e2e T11 的判定依据）。
//!
//! `state` 表存两类记录（e2e 只校验表存在且可查询，字段含义见下方注释）：
//!
//! - `kind = "host"`：每台主机的 FSM 状态快照（`state` / `last_heartbeat` /
//!   探测结论 `probe_verdict` / `offline_since`），供 relay 重启后恢复判定上下文。
//! - `kind = "active_alert"`：`(host_id, source, title)` 活跃告警集合——docs/04 §5
//!   要求的「恢复前不重复推送」去重表。
//! - `kind = "meta"`：键值杂项，存 `startup_at`（启动宽限期基准必须跨重启稳定）
//!   与 `fsm_period_sec`（= `heartbeat_timeout_sec / 3`，即一个上报周期）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, OptionalExtension};
use ssm_common::prelude::*;
use thiserror::Error;

/// 存储层错误。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite 错误: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO 错误（{path}）: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("时间戳无法解析: {0}")]
    Time(String),
    #[error("快照校验失败: {0}")]
    Snapshot(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// 时间入库/出库统一用 RFC3339（纳秒精度，始终带 `Z`）。
pub fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// 解析 [`fmt_time`] 写出的时间戳。
pub fn parse_time(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| StoreError::Time(s.to_string()))
}

/// `heartbeats` 表的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatRow {
    pub host_id: String,
    pub seq: u64,
    pub sent_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    /// `ReportPacket` 的原始 JSON——台账启动恢复靠它精确还原包内容。
    pub payload: String,
}

/// `alerts` 表的一行（测试/诊断用）。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct AlertRow {
    pub host_id: String,
    pub alert_id: String,
    pub level: AlertLevel,
    pub source: String,
    pub title: String,
    pub summary: String,
    pub first_seen: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub pushed: bool,
}

/// 待落库的一条告警。
#[derive(Debug, Clone, PartialEq)]
pub struct AlertInsert {
    pub host_id: String,
    pub alert_id: String,
    pub level: AlertLevel,
    pub source: String,
    pub title: String,
    pub summary: String,
    pub first_seen: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    /// 是否真的推送出去了（过期告警 / 去重命中为 `false`）。
    pub pushed: bool,
}

/// `state` 表中 `kind = "host"` 的行。
#[derive(Debug, Clone, PartialEq)]
pub struct HostStateRow {
    pub host_id: String,
    pub state: String,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub probe_verdict: Option<String>,
    pub offline_since: Option<DateTime<Utc>>,
}

/// 写入 `kind = "host"` 行的入参。
#[derive(Debug, Clone, PartialEq)]
pub struct HostStateUpsert {
    pub host_id: String,
    pub state: String,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub probe_verdict: Option<String>,
    pub offline_since: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// `state` 表中 `kind = "active_alert"` 的行。
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAlertRow {
    pub host_id: String,
    pub alert_id: String,
    pub source: String,
    pub title: String,
    pub level: AlertLevel,
    pub first_seen: DateTime<Utc>,
}

fn level_str(level: AlertLevel) -> &'static str {
    match level {
        AlertLevel::Info => "info",
        AlertLevel::Warning => "warning",
        AlertLevel::Critical => "critical",
    }
}

fn parse_level(s: &str) -> AlertLevel {
    match s {
        "critical" => AlertLevel::Critical,
        "info" => AlertLevel::Info,
        _ => AlertLevel::Warning,
    }
}

/// 加入活跃告警集合的入参。
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAlertUpsert {
    pub host_id: String,
    pub alert_id: String,
    pub source: String,
    pub title: String,
    pub level: AlertLevel,
    pub first_seen: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// SQLite 存储句柄（`Arc<Mutex<Connection>>`，`Clone` 廉价、可跨线程共享）。
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<rusqlite::Connection>>,
    db_path: PathBuf,
    snapshot_path: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("db_path", &self.db_path)
            .field("snapshot_path", &self.snapshot_path)
            .finish()
    }
}

impl Store {
    /// 打开（必要时先恢复）台账数据库。
    ///
    /// 恢复规则（SPEC §7）：`db_path` 不存在且 `snapshot_path` 存在 → 先把快照拷成
    /// `db_path`，再正常打开。`db_path` 已存在时**不**用快照覆盖（热台账优先）。
    pub fn open(
        db_path: impl AsRef<Path>,
        snapshot_path: impl AsRef<Path>,
    ) -> Result<Self, StoreError> {
        let db_path = db_path.as_ref().to_path_buf();
        let snapshot_path = snapshot_path.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(io_err(parent))?;
            }
        }

        if !db_path.exists() && snapshot_path.exists() {
            if let Some(parent) = snapshot_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(io_err(parent))?;
                }
            }
            Self::restore_from_snapshot(&db_path, &snapshot_path)?;
            tracing::info!(
                db = %db_path.display(),
                snapshot = %snapshot_path.display(),
                "台账缺失，已从快照恢复（docs/03 §5）"
            );
        }

        let conn = rusqlite::Connection::open(&db_path)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path,
            snapshot_path,
        };
        store.init_schema()?;
        Ok(store)
    }

    /// 打开一个纯内存台账（单测用；`db_path` 与快照都不会落盘）。
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::from(":memory:"),
            snapshot_path: PathBuf::from(":memory:"),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// 把 `snapshot` 拷到 `db_path`，并做一次完整性校验。
    fn restore_from_snapshot(db_path: &Path, snapshot: &Path) -> Result<(), StoreError> {
        std::fs::copy(snapshot, db_path).map_err(io_err(snapshot))?;
        // 校验：快照必须是可读的 SQLite 库，否则宁可从头开始也不要带着坏库跑。
        let conn = rusqlite::Connection::open(db_path)?;
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if integrity != "ok" {
            drop(conn);
            let _ = std::fs::remove_file(db_path);
            return Err(StoreError::Snapshot(format!(
                "快照 {} 完整性校验失败: {integrity}",
                snapshot.display()
            )));
        }
        Ok(())
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS heartbeats (
                host_id     TEXT    NOT NULL,
                seq         INTEGER NOT NULL,
                sent_at     TEXT    NOT NULL,
                received_at TEXT    NOT NULL,
                payload     TEXT    NOT NULL,
                PRIMARY KEY (host_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_heartbeats_host_seq
                ON heartbeats (host_id, seq DESC);

            CREATE TABLE IF NOT EXISTS alerts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                host_id     TEXT    NOT NULL,
                alert_id    TEXT    NOT NULL,
                level       TEXT    NOT NULL,
                source      TEXT    NOT NULL,
                title       TEXT    NOT NULL,
                summary     TEXT    NOT NULL,
                first_seen  TEXT    NOT NULL,
                received_at TEXT    NOT NULL,
                pushed      INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_alerts_host ON alerts (host_id, id DESC);

            CREATE TABLE IF NOT EXISTS state (
                kind       TEXT NOT NULL,
                host_id    TEXT NOT NULL,
                key        TEXT NOT NULL,
                state      TEXT,
                last_heartbeat TEXT,
                probe_verdict  TEXT,
                offline_since  TEXT,
                alert_id   TEXT,
                source     TEXT,
                title      TEXT,
                level      TEXT,
                first_seen TEXT,
                value      TEXT,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (kind, host_id, key)
            );
            "#,
        )?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        // 任何一次 panic 都不会让连接处于「逻辑不一致」状态（SQLite 语句是原子的），
        // 因此 poisoned 锁直接恢复使用，避免级联 panic。
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------
    // heartbeats
    // -----------------------------------------------------------------------

    /// 写入一条心跳；`(host_id, seq)` 已存在时返回 `false`（重复包，调用方跳过告警）。
    pub fn insert_heartbeat(
        &self,
        host_id: &str,
        seq: u64,
        sent_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        payload: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO heartbeats (host_id, seq, sent_at, received_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                host_id,
                seq as i64,
                fmt_time(sent_at),
                fmt_time(received_at),
                payload
            ],
        )?;
        Ok(changed > 0)
    }

    /// 该 `(host_id, seq)` 是否已入库（测试/诊断用）。
    #[cfg(test)]
    pub fn has_heartbeat(&self, host_id: &str, seq: u64) -> Result<bool, StoreError> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM heartbeats WHERE host_id = ?1 AND seq = ?2",
            params![host_id, seq as i64],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// 每台主机最新一条心跳。
    pub fn latest_heartbeats(&self) -> Result<Vec<HeartbeatRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT h.host_id, h.seq, h.sent_at, h.received_at, h.payload
             FROM heartbeats h
             JOIN (SELECT host_id, MAX(seq) AS seq FROM heartbeats GROUP BY host_id) m
               ON m.host_id = h.host_id AND m.seq = h.seq",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (host_id, seq, sent_at, received_at, payload) = row?;
            out.push(HeartbeatRow {
                host_id,
                seq: seq as u64,
                sent_at: parse_time(&sent_at)?,
                received_at: parse_time(&received_at)?,
                payload,
            });
        }
        out.sort_by(|a, b| a.host_id.cmp(&b.host_id));
        Ok(out)
    }

    /// 心跳总条数（测试/诊断用）。
    #[cfg(test)]
    pub fn heartbeat_count(&self) -> Result<usize, StoreError> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))?;
        Ok(n as usize)
    } // -----------------------------------------------------------------------
      // alerts
      // -----------------------------------------------------------------------

    /// 落库一条告警。
    pub fn insert_alert(&self, row: &AlertInsert) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO alerts
               (host_id, alert_id, level, source, title, summary, first_seen, received_at, pushed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                row.host_id,
                row.alert_id,
                level_str(row.level),
                row.source,
                row.title,
                row.summary,
                fmt_time(row.first_seen),
                fmt_time(row.received_at),
                i64::from(row.pushed)
            ],
        )?;
        Ok(())
    }

    /// 某主机最近的告警（测试/诊断用）。
    #[cfg(test)]
    pub fn recent_alerts(&self, host_id: &str, limit: usize) -> Result<Vec<AlertRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT host_id, alert_id, level, source, title, summary, first_seen, received_at, pushed
             FROM alerts WHERE host_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![host_id, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (host_id, alert_id, level, source, title, summary, first_seen, received_at, pushed) =
                row?;
            out.push(AlertRow {
                host_id,
                alert_id,
                level: parse_level(&level),
                source,
                title,
                summary,
                first_seen: parse_time(&first_seen)?,
                received_at: parse_time(&received_at)?,
                pushed: pushed != 0,
            });
        }
        Ok(out)
    }

    /// `alerts` 表总行数（测试/诊断用）。
    #[cfg(test)]
    pub fn alert_count(&self) -> Result<usize, StoreError> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM alerts", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    // -----------------------------------------------------------------------
    // state: active_alert 集合
    // -----------------------------------------------------------------------

    /// 把 `(host_id, source, title)` 加入活跃告警集合；已存在时返回 `false`（**不重复推送**）。
    pub fn activate_alert(&self, row: &ActiveAlertUpsert) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO state
               (kind, host_id, key, alert_id, source, title, level, first_seen, updated_at)
             VALUES ('active_alert', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.host_id,
                format!("{}\u{1}{}", row.source, row.title),
                row.alert_id,
                row.source,
                row.title,
                level_str(row.level),
                fmt_time(row.first_seen),
                fmt_time(row.updated_at)
            ],
        )?;
        Ok(changed > 0)
    }

    /// 告警恢复：从活跃集合移除。
    pub fn resolve_alert(
        &self,
        host_id: &str,
        source: &str,
        title: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "DELETE FROM state WHERE kind = 'active_alert' AND host_id = ?1 AND key = ?2",
            params![host_id, format!("{source}\u{1}{title}")],
        )?;
        Ok(changed > 0)
    }

    /// 活跃告警条数（`/status.active_alerts`）。
    pub fn active_alert_count(&self, host_id: &str) -> Result<usize, StoreError> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM state WHERE kind = 'active_alert' AND host_id = ?1",
            params![host_id],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// 全部活跃告警（重启后恢复去重集合用）。
    pub fn active_alerts(&self) -> Result<Vec<ActiveAlertRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT host_id, alert_id, source, title, level, first_seen
             FROM state WHERE kind = 'active_alert'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (host_id, alert_id, source, title, level, first_seen) = row?;
            out.push(ActiveAlertRow {
                host_id,
                alert_id: alert_id.unwrap_or_default(),
                source,
                title,
                level: parse_level(level.as_deref().unwrap_or("warning")),
                first_seen: match first_seen {
                    Some(s) => parse_time(&s)?,
                    None => DateTime::<Utc>::UNIX_EPOCH,
                },
            });
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // state: host FSM 上下文
    // -----------------------------------------------------------------------

    /// 读取某主机的 FSM 上下文。
    pub fn host_state(&self, host_id: &str) -> Result<Option<HostStateRow>, StoreError> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT host_id, state, last_heartbeat, probe_verdict, offline_since
                 FROM state WHERE kind = 'host' AND host_id = ?1 AND key = 'fsm'",
                params![host_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((host_id, state, last_heartbeat, probe_verdict, offline_since)) = row else {
            return Ok(None);
        };
        Ok(Some(HostStateRow {
            host_id,
            state: state.unwrap_or_else(|| "online".to_string()),
            last_heartbeat: match last_heartbeat {
                Some(s) => Some(parse_time(&s)?),
                None => None,
            },
            probe_verdict,
            offline_since: match offline_since {
                Some(s) => Some(parse_time(&s)?),
                None => None,
            },
        }))
    }

    /// 写入某主机的 FSM 上下文。
    pub fn upsert_host_state(&self, row: &HostStateUpsert) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO state
               (kind, host_id, key, state, last_heartbeat, probe_verdict, offline_since, updated_at)
             VALUES ('host', ?1, 'fsm', ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (kind, host_id, key) DO UPDATE SET
               state = excluded.state,
               last_heartbeat = excluded.last_heartbeat,
               probe_verdict = excluded.probe_verdict,
               offline_since = excluded.offline_since,
               updated_at = excluded.updated_at",
            params![
                row.host_id,
                row.state,
                row.last_heartbeat.map(fmt_time),
                row.probe_verdict,
                row.offline_since.map(fmt_time),
                fmt_time(row.updated_at)
            ],
        )?;
        Ok(())
    }

    /// 所有主机的 FSM 上下文。
    pub fn all_host_states(&self) -> Result<Vec<HostStateRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT host_id, state, last_heartbeat, probe_verdict, offline_since
             FROM state WHERE kind = 'host'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (host_id, state, last_heartbeat, probe_verdict, offline_since) = row?;
            out.push(HostStateRow {
                host_id,
                state: state.unwrap_or_else(|| "online".to_string()),
                last_heartbeat: match last_heartbeat {
                    Some(s) => Some(parse_time(&s)?),
                    None => None,
                },
                probe_verdict,
                offline_since: match offline_since {
                    Some(s) => Some(parse_time(&s)?),
                    None => None,
                },
            });
        }
        Ok(out)
    }

    /// 读取 `state` 里的 meta 键值。
    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT value FROM state WHERE kind = 'meta' AND host_id = '' AND key = ?1",
                params![key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// 写入 `state` 里的 meta 键值。
    pub fn set_meta(&self, key: &str, value: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO state (kind, host_id, key, value, updated_at)
             VALUES ('meta', '', ?1, ?2, ?3)
             ON CONFLICT (kind, host_id, key) DO UPDATE SET
               value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, fmt_time(now)],
        )?;
        Ok(())
    }

    /// `state` 表总行数（测试/诊断用）。
    #[cfg(test)]
    pub fn state_count(&self) -> Result<usize, StoreError> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM state", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    // -----------------------------------------------------------------------
    // 快照
    // -----------------------------------------------------------------------

    /// 生成一致性快照并原子替换到 `snapshot_path`。
    ///
    /// 流程：`VACUUM INTO <tmp>` → 删除旧快照 → `rename(tmp, snapshot_path)`。
    /// 同目录 rename 是原子的，因此 `snapshot_path` 永远指向一个完整可用的库。
    pub fn snapshot(&self) -> Result<PathBuf, StoreError> {
        if self.snapshot_path.as_os_str() == ":memory:" {
            return Err(StoreError::Snapshot("内存库不支持快照".to_string()));
        }
        let Some(dir) = self.snapshot_path.parent() else {
            return Err(StoreError::Snapshot("snapshot_path 无父目录".to_string()));
        };
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(io_err(dir))?;
        }

        let tmp = snapshot_tmp_path(&self.snapshot_path);
        let _ = std::fs::remove_file(&tmp);

        {
            let conn = self.lock();
            // VACUUM INTO 必须在无活动事务的连接上执行，且目标文件必须不存在。
            conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])?;
        }

        if !tmp.exists() {
            return Err(StoreError::Snapshot(format!(
                "VACUUM INTO 未产出 {}",
                tmp.display()
            )));
        }
        // WAL 模式下 VACUUM INTO 的产物是自洽的，但保险起见校验一次。
        {
            let check = rusqlite::Connection::open(&tmp)?;
            let integrity: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if integrity != "ok" {
                drop(check);
                let _ = std::fs::remove_file(&tmp);
                return Err(StoreError::Snapshot(format!(
                    "快照完整性校验失败: {integrity}"
                )));
            }
        }

        let _ = std::fs::remove_file(&self.snapshot_path);
        std::fs::rename(&tmp, &self.snapshot_path).map_err(io_err(&self.snapshot_path))?;
        Ok(self.snapshot_path.clone())
    }
}

fn snapshot_tmp_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "ledger.snapshot.db".to_string());
    name.push_str(&format!(".tmp{}", std::process::id()));
    match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse::<DateTime<Utc>>().unwrap()
    }

    #[test]
    fn schema_creates_all_three_tables() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.heartbeat_count().unwrap(), 0);
        assert_eq!(store.alert_count().unwrap(), 0);
        assert_eq!(store.state_count().unwrap(), 0);
    }

    #[test]
    fn duplicate_host_seq_is_ignored() {
        let store = Store::open_in_memory().unwrap();
        let sent = t("2026-10-15T01:30:00Z");
        let recv = t("2026-10-15T01:30:01Z");

        assert!(store
            .insert_heartbeat("h1", 7, sent, recv, r#"{"seq":7}"#)
            .unwrap());
        assert!(
            !store
                .insert_heartbeat("h1", 7, sent, recv, r#"{"seq":7}"#)
                .unwrap(),
            "同 (host_id, seq) 第二次插入必须被去重"
        );
        assert_eq!(store.heartbeat_count().unwrap(), 1);
        assert!(store.has_heartbeat("h1", 7).unwrap());
        assert!(!store.has_heartbeat("h1", 8).unwrap());
        // 不同 host 的同 seq 不受影响
        assert!(store
            .insert_heartbeat("h2", 7, sent, recv, r#"{"seq":7}"#)
            .unwrap());
        assert_eq!(store.heartbeat_count().unwrap(), 2);
    }

    #[test]
    fn latest_heartbeat_per_host() {
        let store = Store::open_in_memory().unwrap();
        let sent = t("2026-10-15T01:30:00Z");
        let recv = t("2026-10-15T01:30:01Z");
        for seq in [1_u64, 5, 3] {
            store
                .insert_heartbeat("h1", seq, sent, recv, &format!(r#"{{"seq":{seq}}}"#))
                .unwrap();
        }
        store
            .insert_heartbeat("h2", 1, sent, recv, r#"{"seq":1}"#)
            .unwrap();

        let latest = store.latest_heartbeats().unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].host_id, "h1");
        assert_eq!(latest[0].seq, 5, "必须取 MAX(seq)");
        assert_eq!(latest[1].host_id, "h2");
    }

    #[test]
    fn active_alert_set_dedupes_and_resolves() {
        let store = Store::open_in_memory().unwrap();
        let now = t("2026-10-15T01:30:00Z");
        let row = || ActiveAlertUpsert {
            host_id: "h1".into(),
            alert_id: "a1".into(),
            source: "relay".into(),
            title: "服务器离线".into(),
            level: AlertLevel::Critical,
            first_seen: now,
            updated_at: now,
        };
        assert!(store.activate_alert(&row()).unwrap());
        assert!(
            !store.activate_alert(&row()).unwrap(),
            "同 (source,title) 恢复前不得重复激活"
        );
        assert_eq!(store.active_alert_count("h1").unwrap(), 1);
        assert_eq!(store.active_alert_count("h2").unwrap(), 0);

        assert!(store.resolve_alert("h1", "relay", "服务器离线").unwrap());
        assert!(!store.resolve_alert("h1", "relay", "服务器离线").unwrap());
        assert_eq!(store.active_alert_count("h1").unwrap(), 0);
    }

    #[test]
    fn host_state_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let hb = t("2026-10-15T01:30:00Z");
        let off = t("2026-10-15T01:31:00Z");
        assert!(store.host_state("h1").unwrap().is_none());

        store
            .upsert_host_state(&HostStateUpsert {
                host_id: "h1".into(),
                state: "offline".into(),
                last_heartbeat: Some(hb),
                probe_verdict: Some("offline".into()),
                offline_since: Some(off),
                updated_at: t("2026-10-15T01:31:05Z"),
            })
            .unwrap();
        let row = store.host_state("h1").unwrap().unwrap();
        assert_eq!(row.state, "offline");
        assert_eq!(row.last_heartbeat, Some(hb));
        assert_eq!(row.probe_verdict.as_deref(), Some("offline"));
        assert_eq!(row.offline_since, Some(off));

        // 二次写入走 UPDATE（不新增行）
        store
            .upsert_host_state(&HostStateUpsert {
                host_id: "h1".into(),
                state: "online".into(),
                last_heartbeat: Some(hb),
                probe_verdict: None,
                offline_since: None,
                updated_at: t("2026-10-15T01:32:00Z"),
            })
            .unwrap();
        let row = store.host_state("h1").unwrap().unwrap();
        assert_eq!(row.state, "online");
        assert_eq!(row.offline_since, None);
        assert_eq!(store.all_host_states().unwrap().len(), 1);
    }

    #[test]
    fn meta_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let now = t("2026-10-15T01:30:00Z");
        assert_eq!(store.meta("fsm_period_sec").unwrap(), None);
        store.set_meta("fsm_period_sec", "30", now).unwrap();
        assert_eq!(store.meta("fsm_period_sec").unwrap().as_deref(), Some("30"));
        store.set_meta("fsm_period_sec", "31", now).unwrap();
        assert_eq!(store.meta("fsm_period_sec").unwrap().as_deref(), Some("31"));
    }

    #[test]
    fn alert_rows_are_persisted() {
        let store = Store::open_in_memory().unwrap();
        let now = t("2026-10-15T01:30:00Z");
        store
            .insert_alert(&AlertInsert {
                host_id: "h1".into(),
                alert_id: "log:nginx:error".into(),
                level: AlertLevel::Warning,
                source: "log_watcher".into(),
                title: "nginx 出现 ERROR 日志".into(),
                summary: "matched 3 lines".into(),
                first_seen: now,
                received_at: now,
                pushed: true,
            })
            .unwrap();
        let rows = store.recent_alerts("h1", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, AlertLevel::Warning);
        assert!(rows[0].pushed);
        assert_eq!(store.alert_count().unwrap(), 1);
    }

    /// e2e T11 的单元级对应物：写数据 → 快照 → 删掉 db_path → 重开 → 数据回来了。
    #[test]
    fn snapshot_restores_ledger_after_db_is_lost() {
        let dir = TempDir::new("store-restore");
        let db_path = dir.path().join("ledger.db");
        let snap_path = dir.path().join("ledger.snapshot.db");
        let sent = t("2026-10-15T01:30:00Z");

        {
            let store = Store::open(&db_path, &snap_path).unwrap();
            store
                .insert_heartbeat("h1", 42, sent, sent, r#"{"seq":42}"#)
                .unwrap();
            store
                .activate_alert(&ActiveAlertUpsert {
                    host_id: "h1".into(),
                    alert_id: "a1".into(),
                    source: "relay".into(),
                    title: "服务器离线".into(),
                    level: AlertLevel::Critical,
                    first_seen: sent,
                    updated_at: sent,
                })
                .unwrap();
            store
                .upsert_host_state(&HostStateUpsert {
                    host_id: "h1".into(),
                    state: "offline".into(),
                    last_heartbeat: Some(sent),
                    probe_verdict: Some("offline".into()),
                    offline_since: Some(sent),
                    updated_at: sent,
                })
                .unwrap();
            let written = store.snapshot().unwrap();
            assert_eq!(written, snap_path);
            assert!(snap_path.exists(), "快照文件必须生成");
        }

        // 模拟 tmpfs 掉电：热台账消失，只剩 SSD 上的快照
        std::fs::remove_file(&db_path).unwrap();
        assert!(!db_path.exists());

        let store = Store::open(&db_path, &snap_path).unwrap();
        assert!(db_path.exists(), "启动必须先把快照恢复成热台账");
        assert_eq!(store.heartbeat_count().unwrap(), 1);
        let latest = store.latest_heartbeats().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].seq, 42);
        assert_eq!(latest[0].sent_at, sent);
        assert_eq!(latest[0].payload, r#"{"seq":42}"#);

        let state = store.host_state("h1").unwrap().unwrap();
        assert_eq!(state.state, "offline");
        assert_eq!(
            state.last_heartbeat,
            Some(sent),
            "last_heartbeat 必须被恢复"
        );
        assert_eq!(store.active_alert_count("h1").unwrap(), 1);

        // 恢复后写入仍然可去重（等价于「重复包」判定跨重启有效）
        assert!(!store
            .insert_heartbeat("h1", 42, sent, sent, r#"{"seq":42}"#)
            .unwrap());
    }

    #[test]
    fn snapshot_does_not_overwrite_existing_hot_db() {
        let dir = TempDir::new("store-hot-wins");
        let db_path = dir.path().join("ledger.db");
        let snap_path = dir.path().join("ledger.snapshot.db");
        let sent = t("2026-10-15T01:30:00Z");

        {
            let store = Store::open(&db_path, &snap_path).unwrap();
            store
                .insert_heartbeat("h1", 1, sent, sent, r#"{"seq":1}"#)
                .unwrap();
            store.snapshot().unwrap();
            // 快照之后再写一条「更新」的数据
            store
                .insert_heartbeat("h1", 2, sent, sent, r#"{"seq":2}"#)
                .unwrap();
        }
        // db_path 还在 → 必须用热台账（含 seq=2），不能被旧快照覆盖
        let store = Store::open(&db_path, &snap_path).unwrap();
        assert_eq!(store.heartbeat_count().unwrap(), 2);
        assert_eq!(store.latest_heartbeats().unwrap()[0].seq, 2);
    }

    #[test]
    fn snapshot_from_memory_store_is_rejected() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(store.snapshot(), Err(StoreError::Snapshot(_))));
    }

    #[test]
    fn open_creates_missing_parent_directories() {
        let dir = TempDir::new("store-mkdir");
        let db_path = dir.path().join("nested/deeper/ledger.db");
        let snap_path = dir.path().join("nested/snap/ledger.snapshot.db");
        let store = Store::open(&db_path, &snap_path).unwrap();
        assert!(db_path.exists());
        store.snapshot().unwrap();
        assert!(snap_path.exists());
    }

    #[test]
    fn time_format_roundtrip_keeps_utc_and_nanos() {
        let now = t("2026-10-15T01:30:00.123456789Z");
        let s = fmt_time(now);
        assert!(s.ends_with('Z'), "必须是 UTC 字面量: {s}");
        assert_eq!(parse_time(&s).unwrap(), now);
        assert!(parse_time("not-a-time").is_err());
    }
}
