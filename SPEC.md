# SPEC — server_status_monitoring 实现契约

> **本文件是代码结构、命名、协议字段的唯一权威**。`docs/01`–`docs/07` 是行为与设计权威。
> 两者冲突时：**行为语义以 docs 为准，代码路径/类型名/字段名以本文件为准**。
> 派发子 Agent 时，prompt 里只写「读本文件 + 你的边界」，不要把契约复制进 prompt。

最后更新：M0 派发前

---

## 0. 铁律（子 Agent 必须遵守）

1. **只改你被指派目录下的文件**，绝不碰其他 crate。边界见第 8 节任务表。
2. 每完成一个步骤**立即 `git add` + `git commit`**，Conventional Commits（`feat(agent): ...`）。
3. 完工前必须跑通：`cargo fmt --all` → `cargo clippy --all-targets -- -D warnings` → `cargo test --workspace`。**三者全绿才算完成**，报 completion 时要贴出实际命令输出。
4. **禁止编造 API**。第三方 crate 的 API 必须靠**编译循环**验证（`cargo build` 报错就改），不许凭记忆写。特别提醒 `sysinfo` 各版本 API 差异极大。
5. **禁止使用 OpenSSL / native-tls**。`reqwest` 必须 `default-features = false, features = ["rustls", "json"]`（**注意：0.13 起特性名是 `rustls`，不是 `rustls-tls`**，写旧名 cargo 直接报错）。系统里故意不装 `libssl-dev`，一旦有人引入 OpenSSL 会编译失败——这是**有意的失败**，不要去装 libssl-dev，改回 rustls。
6. 不许删除/改写 `docs/` 下的文档。不许删除既有测试。
7. 不确定时**停下来在返回结果里说明**，不要猜着写一堆代码。

---

## 1. 构建环境（Windows + WSL2）

**开发/编译全部在 WSL Ubuntu 内进行**（Windows 侧无 Rust 工具链，且目标平台是 Linux）。

```bash
# 从 Windows PowerShell 进入
wsl -d Ubuntu -- bash -lc 'cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring && <cmd>'

# 如果 cargo 不在 PATH
export PATH="$HOME/.cargo/bin:$PATH"
```

- WSL 发行版：Ubuntu 24.04.4 LTS，x86_64，systemd 已启用（`/etc/wsl.conf` → `systemd=true`）
- root 操作：`wsl -d Ubuntu -u root -- <cmd>`（此机器 sudo 需要密码，用 `-u root` 绕过）
- crates.io 已配置 rsproxy 稀疏镜像（`~/.cargo/config.toml`）

### ⚠️ 构建目录必须放在 ext4，不要放 /mnt/c

仓库在 `/mnt/c/...`（drvfs，跨文件系统 IO 极慢，cargo 会慢 5–10 倍）。**每个子 Agent 用自己独立的 `CARGO_TARGET_DIR`**，避免并发抢锁：

```bash
export CARGO_TARGET_DIR=$HOME/ssm-target-<你的任务名>
```

报告完成时请注明你用的 target dir。

---

## 2. 仓库布局（精确路径）

```
server_status_monitoring/
├── Cargo.toml                      # workspace 根
├── rust-toolchain.toml             # stable + rustfmt/clippy
├── .gitignore                      # 注意：Cargo.lock 必须提交（本项目是二进制 crate）
├── .github/workflows/ci.yml        # fmt + clippy + test
├── README.md
├── DEVPLAN.md                      # 进度跟踪单（编排会话维护）
├── HANDOFF.md                      # 交接文档（编排会话维护）
├── SPEC.md                         # 本文件
├── crates/
│   ├── common/                     # 协议 + 配置 + 签名（唯一来源）
│   │   ├── Cargo.toml
│   │   └── src/{lib.rs,protocol.rs,config.rs,sign.rs,error.rs}
│   ├── agent/                      # ssm-agent：被监控服务器上的采集/上报端
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── buffer.rs           # 本地环形缓冲（断点续传）
│   │       ├── reporter.rs         # reqwest POST + HMAC 签名
│   │       ├── probe_server.rs     # axum GET /probe
│   │       └── collectors/
│   │           ├── mod.rs
│   │           ├── sys_metrics.rs
│   │           ├── service_probe.rs
│   │           ├── log_watcher.rs
│   │           ├── git_tracker.rs
│   │           └── thresholds.rs
│   └── relay/                      # ssm-relay：中转判定器
│       ├── Cargo.toml
│       └── src/{main.rs,store.rs,ledger.rs,fsm.rs,api.rs,alerter.rs,prober.rs}
├── ios/widget/server_status.js     # Scriptable 小组件
├── deploy/
│   ├── ssm-agent.service
│   ├── ssm-relay.service
│   ├── install.sh
│   ├── agent.toml.example
│   ├── relay.toml.example
│   └── secrets.toml.example
└── docs/                           # 01–07 设计文档（权威，勿改）
```

---

## 3. 端口与二进制

| 项 | 值 |
|---|---|
| 二进制名 | `ssm-agent`、`ssm-relay`（由 crate 名决定） |
| agent `/probe` | `GET http://<agent>:9101/probe` |
| relay `/report` | `POST http://<relay>:9100/report` |
| relay `/status` | `GET http://<relay>:9100/status`（`Authorization: Bearer <status_token>`） |
| relay `/healthz` | `GET http://<relay>:9100/healthz`（无鉴权，供 UptimeRobot 兜底） |

### CLI 契约（T1/T2/T4 必须一致，`clap` derive）

```
ssm-agent --config /etc/ssm-agent/agent.toml [--log-level info]
ssm-relay --config /etc/ssm-relay/relay.toml [--log-level info]
```

- `--config` 必填，`-c` 短选项；`--log-level` 可选，默认 `info`，取值 `trace|debug|info|warn|error`。
- `--version` 输出版本；`-h/--help` 可用。
- 两者都把 `tracing` 输出到 **stderr**，日志格式含时间戳/级别/模块名。
- 收到 `SIGTERM`/`SIGINT` 时优雅退出（flush 缓冲、关连接），退出码 0。

---

## 4. 协议契约（`crates/common/src/protocol.rs`）

时间统一 `chrono::DateTime<Utc>`，JSON 为 RFC3339（`2026-10-15T01:30:00Z`）。
枚举一律 `#[serde(rename_all = "lowercase")]`。可选集合字段一律 `#[serde(default)]`。

```rust
pub const PROTOCOL_VERSION: u32 = 1;

pub struct ReportPacket {
    pub version: u32,            // PROTOCOL_VERSION
    pub host_id: String,
    pub sent_at: DateTime<Utc>,
    pub seq: u64,                // 单调递增
    pub metrics: Metrics,
    #[serde(default)] pub services: Vec<ServiceStatus>,
    #[serde(default)] pub git: Vec<GitStatus>,
    #[serde(default)] pub alerts: Vec<Alert>,
}

pub struct Metrics {
    pub cpu_percent: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub disks: Vec<DiskUsage>,
    pub load_avg: [f64; 3],      // 1/5/15 分钟
    pub uptime_sec: u64,
    pub net: Option<NetThroughput>,
}
pub struct DiskUsage { pub mount: String, pub used_gb: f64, pub total_gb: f64 }
pub struct NetThroughput { pub rx_kbps: f64, pub tx_kbps: f64 }

pub enum ServiceKind { Http, Tcp, Process, Tailscale }   // serde lowercase
pub struct ServiceStatus {
    pub name: String,
    pub kind: ServiceKind,
    pub target: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub detail: Option<String>,
}

pub struct GitStatus {
    pub repo: String, pub branch: String, pub head: String,
    pub dirty: bool, pub changed: bool,     // changed = 与上次采集相比 HEAD 变了
}

pub enum AlertLevel { Info, Warning, Critical }   // serde lowercase
pub struct Alert {
    pub id: String,               // 稳定去重键，如 "log:nginx:error"
    pub level: AlertLevel,
    pub source: String,           // "log_watcher" | "service_probe" | "git_tracker" | "thresholds"
    pub title: String,
    pub summary: String,
    pub first_seen: DateTime<Utc>,
}

/// agent GET /probe 响应
pub struct ProbeResponse { pub ok: bool, pub host_id: String, pub uptime_sec: u64 }

pub enum HostState { Online, Suspect, Offline }   // serde lowercase
pub struct SnapshotMetrics {
    pub cpu_percent: f32, pub mem_percent: f32,
    pub disk_percent: f32, pub uptime_sec: u64,
}
/// relay GET /status 响应（字段严格按 docs/03 §4，勿增删）
pub struct StatusSnapshot {
    pub host_id: String,
    pub state: HostState,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub metrics: Option<SnapshotMetrics>,   // 从未收到心跳时为 None
    pub services: Vec<ServiceBrief>,
    pub active_alerts: usize,
    pub generated_at: DateTime<Utc>,
}
pub struct ServiceBrief { pub name: String, pub ok: bool }
```

> **双路探测结论（ONLINE_TUNNEL_DOWN / ONLINE_LAN_ANOMALY）不进 `/status.state`**——`state` 只有三值。
> 这两类以 `warning` 级 Alert 形式呈现并计入 `active_alerts`，relay 侧同时打日志。见 docs/03 §2 矩阵。

测试要求：`protocol.rs` 必须带 serde 往返单测 + **对 docs/03 §1 与 §4 的示例 JSON 做逐字段反序列化断言**（用文档里的字面 JSON）。

---

## 5. 签名契约（`crates/common/src/sign.rs`）

```
message   = <timestamp 十进制 ASCII 秒> || <raw body bytes>
signature = hex_lower(HMAC_SHA256(shared_secret, message))
```

- 请求头：`X-SSM-Timestamp: <unix 秒>`、`X-SSM-Signature: <hex>`
- 校验：`|now - timestamp| <= 300s` 否则拒绝（防重放）；签名用**常量时间比较**
- 空 body 也参与签名（`GET /probe` 不签名，只有 relay→agent 方向且仅用于存活判定）

公开 API：
```rust
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String;
pub fn verify(secret: &[u8], timestamp: i64, body: &[u8], sig_hex: &str, now: i64) -> Result<(), SignError>;
```
单测：正确签名通过；篡改 body 失败；时间戳偏差 301s 失败。

---

## 6. 配置文件契约（`crates/common/src/config.rs`）

两层：`agent.toml`/`relay.toml`（可入库，`.example` 进仓库）+ `secrets.toml`（600 权限，**.gitignore 已排除**）。

### `secrets.toml`
```toml
shared_secret = "..."      # agent ↔ relay HMAC 共享密钥
status_token  = "..."      # /status Bearer Token
[bark]
endpoint = "https://api.day.app"
key      = "..."
group    = "server_status"
```

### `agent.toml`
```toml
host_id             = "kebei-prod-01"
relay_url           = "http://127.0.0.1:9100/report"
report_interval_sec = 30
probe_listen        = "0.0.0.0:9101"
seq_state_path      = "/var/lib/ssm-agent/seq"
buffer_path         = "/var/lib/ssm-agent/buffer.jsonl"
buffer_max          = 100
secrets_path        = "/etc/ssm-agent/secrets.toml"
disks               = ["/"]

[[probes]]
name = "web-frontend"
kind = "http"              # http | tcp | process | tailscale
target = "https://example.com"
fail_threshold = 2         # 连续失败 N 次才告警（docs/01 F3）
timeout_sec = 5

[[log_rules]]
name = "nginx-error"
source = "journald:nginx.service"   # journald:<unit> 或 file:<绝对路径>
pattern = "ERROR|upstream timed out"
level = "warning"
throttle_sec = 300
max_lines = 3
include_raw = false        # 默认 false（脱敏，docs/04 §2.3）

[[git_repos]]
name = "kebei-web"
path = "/srv/kebei-web"
timeout_sec = 5            # 单次 git 命令超时（T0 补充，SPEC 原文漏写）

[thresholds]
disk_percent = 85.0
disk_throttle_sec = 21600
cpu_percent = 95.0
mem_percent = 90.0
sustained_sec = 300
```

### `relay.toml`
```toml
listen               = "0.0.0.0:9100"
db_path              = "/run/ssm-relay/ledger.db"
snapshot_path        = "/var/lib/ssm-relay/ledger.snapshot.db"
secrets_path         = "/etc/ssm-relay/secrets.toml"
heartbeat_timeout_sec = 90
handshake_retries     = 3
handshake_interval_sec = 10
handshake_timeout_sec  = 5
repeat_alert_min       = 30
startup_grace_periods  = 2
rate_limit_per_min     = 5
snapshot_interval_sec  = 300     # SQLite 快照落盘间隔（tmpfs 场景）
lag_threshold_sec      = 300     # 补报过期告警只入库不推送

[[hosts]]
host_id = "kebei-prod-01"
[[hosts.paths]]
kind = "lan"                     # lan | tailscale
url  = "http://192.168.1.10:9101/probe"
[[hosts.paths]]
kind = "tailscale"
url  = "http://100.x.x.x:9101/probe"
```

配置加载须有单测（至少：完整 TOML 解析成功、缺字段报清晰错误）。

---

## 7. 各 crate 模块职责

### `crates/agent`
| 模块 | 职责 | 关键点 |
|---|---|---|
| `collectors/sys_metrics.rs` | CPU/内存/磁盘/负载/uptime/网络 | `sysinfo`；CPU 首次采样需间隔重采；磁盘按 `disks` 挂载点过滤 |
| `collectors/service_probe.rs` | http/tcp/process/tailscale 四类探针 | http 用 reqwest 带 `timeout_sec`；process 查 `/proc` 或 sysinfo 进程表；tailscale 解析 `tailscale status --json`；维护每探针连续失败计数 |
| `collectors/log_watcher.rs` | journald + file tail，正则匹配 | journald 用 `journalctl -u <unit> -f -o cat` 子进程（**不要**依赖 systemd crate）；文件 tail 记录 offset 到内存，重启从头尾随；`throttle_sec` 限频；`include_raw=false` 时 summary 只含规则名+匹配行数 |
| `collectors/git_tracker.rs` | HEAD/分支/脏状态 | `git -C <path> rev-parse HEAD`、`rev-parse --abbrev-ref HEAD`、`status --porcelain`；`changed` 与上次比对（首次不告警） |
| `collectors/thresholds.rs` | 磁盘/CPU/内存阈值 | CPU/内存需 `sustained_sec` 持续超阈才告警；各规则独立节流 |
| `buffer.rs` | 上报失败本地缓冲 | JSONL 追加，最多 `buffer_max` 条环形淘汰，成功后清空；持久化到 `buffer_path` |
| `reporter.rs` | 组包 + 签名 + POST | 失败入 buffer，下轮连 buffered 一起按 `sent_at` 原时间补发；`seq` 持久化到 `seq_state_path`，重启续接 |
| `probe_server.rs` | `GET /probe` 应答 | axum，返回 `ProbeResponse` |
| `main.rs` | 装配 + 定时循环 | tracing 初始化；每 `report_interval_sec` 一轮；优雅退出（SIGTERM） |

### `crates/relay`
| 模块 | 职责 | 关键点 |
|---|---|---|
| `store.rs` | SQLite 落库 | **rusqlite + `bundled` feature**（禁止依赖系统 sqlite，保证 musl 静态）；表 `heartbeats(host_id, seq, sent_at, received_at, payload)`、`alerts(...)`、`state(...)`；`(host_id, seq)` 唯一去重 |
| `store.rs`（快照） | tmpfs → SSD 持久化 | `db_path` 在 tmpfs（掉电即失）；每 `snapshot_interval_sec` 用 SQLite `VACUUM INTO` 或 checkpoint 后原子拷贝到 `snapshot_path`；**启动时若 `db_path` 不存在但 `snapshot_path` 存在则先恢复**（docs/03 §5「relay 重启不误报」）。此恢复路径必须有单测 |
| `ledger.rs` | 内存心跳台账 | 每 host 最新 `ReportPacket` + `last_heartbeat`；启动时从 SQLite 恢复 |
| `fsm.rs` | 离线判定状态机 | 严格按 docs/03 §3：`ONLINE --(>heartbeat_timeout)--> SUSPECT --(3×握手, 间隔10s)--> OFFLINE`；任一成功静默恢复；`repeat_alert_min` 重复提醒；启动宽限 `startup_grace_periods` |
| `prober.rs` | 双路握手探测 | 按 `hosts[].paths` 并发探测；`handshake_timeout_sec`；产出 docs/03 §2 判定矩阵结论 |
| `alerter.rs` | Bark 推送 | `POST <endpoint>/<key>/<title>/<body>?level=&group=`；级别映射见 docs/04 §1；指数退避 3 次（1s/4s/16s）；全局限流 `rate_limit_per_min`；`(source,title)` active 集合去重 |
| `api.rs` | axum 路由 | `POST /report`（验签→落库→更新台账→即时告警通道）、`GET /status`（Bearer）、`GET /healthz` |
| `main.rs` | 装配 | 后台任务：判定 tick、快照落盘 tick |

**即时告警通道**：`alerts[]` 非空 → 按 level 立即推送 Bark（`sent_at` 超过 `lag_threshold_sec` 的过期告警只入库不推送）。

---

## 8. 任务分解与文件边界

| 任务 | 负责 | 独占文件边界 | 依赖 |
|---|---|---|---|
| **T0 骨架** | 子 Agent A | `Cargo.toml`、`rust-toolchain.toml`、`.gitignore`、`.github/**`、`crates/common/**`、三个 crate 的 stub `main.rs` 与 `Cargo.toml` | — |
| **T1 relay** | 子 Agent B | `crates/relay/**` | T0 |
| **T2 agent 核心** | 子 Agent C | `crates/agent/**` 除 `collectors/{service_probe,log_watcher,git_tracker,thresholds}.rs` | T0 |
| **T3 agent 高级采集** | 子 Agent D | `crates/agent/src/collectors/{service_probe,log_watcher,git_tracker,thresholds}.rs` | T2 |
| **T4 交付物** | 子 Agent E | `ios/**`、`deploy/**` | T0（只需协议契约） |

> T0 必须**先单独完成**——它创建的 workspace 与 stub 是并发编译的前提。
> T1/T2/T4 在 T0 后**可并行**。T3 在 T2 后。

---

## 9. 验收标准（编排会话逐条实跑，不看子 Agent 的自述）

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/ssm-target-verify
cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
ls -la target/release/ssm-agent target/release/ssm-relay
```

**M0**：三 crate 编译通过；`common` 协议 serde 往返 + docs 示例 JSON 反序列化测试全绿；`cargo build --release` 产出两个二进制（stub 即可）。
**M1**：WSL 内 agent→relay 连续心跳，relay 日志每 30s 一条；`/status` 返回 `online`；杀掉 agent 后台账停止更新。
**M2**：docs/06 §4 的 T1/T2/T3/T4/T9/T11 通过（Bark 用本地 mock server 校验请求，不依赖真机）。
**M3**：docs/06 §4 的 T5/T6/T7/T8 通过。
**M4**：Scriptable 脚本语法自检 + 对 mock `/status` 的字段渲染逻辑走查；`deploy/` 下 systemd unit 通过 `systemd-analyze verify`。

**Bark 测试方案**：起一个本地 HTTP mock（Python `http.server` 或 `nc`）记录 relay 发出的请求，断言 URL/level/body 符合 docs/04 §3。**不要**向真实 Bark 发推送。

---

## 10. 与 T2/T3 的衔接

T2 必须在 `collectors/mod.rs` 中为 T3 留出稳定的 trait 接口。

⚠️ **注意：SPEC 早先版本的 `Collector::collect` 只返回 `Vec<Alert>`，这是不够的** —— `service_probe` 要产出
`services[]`、`git_tracker` 要产出 `git[]`，都塞不进 `Vec<Alert>`。因此**以本版为准**：

```rust
/// 一轮采集的产出。三个通道分别对应 ReportPacket 的三个可选数组。
#[derive(Debug, Default, Clone)]
pub struct CollectOutcome {
    pub alerts: Vec<Alert>,
    pub services: Vec<ServiceStatus>,
    pub git: Vec<GitStatus>,
}
impl CollectOutcome {
    pub fn empty() -> Self { Self::default() }
    pub fn merge(&mut self, other: CollectOutcome) { /* 追加三个 vec */ }
}

#[async_trait]
pub trait Collector: Send + Sync {
    /// 唯一标识，用于日志与告警 source
    fn name(&self) -> &'static str;
    /// 每轮上报前调用一次
    async fn collect(&mut self, ctx: &CollectCtx) -> CollectOutcome;
}
```

`CollectCtx` 承载跨采集器共享的只读状态，**字段名以此为准**（T2 定义，T3 只读不改）：

```rust
pub struct CollectCtx {
    pub host_id: String,
    pub uptime_sec: u64,
    /// 单调递增的轮次号，T3 的节流/持续性判定可基于它
    pub round: u64,
    /// 本轮时间（便于测试注入）
    pub now: chrono::DateTime<chrono::Utc>,
}
```

T2 在 `main.rs` 里维护一个 `Vec<Box<dyn Collector>>` 并每轮 `merge` 所有 outcome；**T3 只需新增
`collectors/*.rs` 并往这个 Vec 里 push 一行**。T3 **不得修改** trait 签名与 `CollectCtx` 字段（若必须改，
在返回结果里明确说明原因）。

---

## 11. 已解析依赖版本与 API 注意事项（M0 落地后回填，T1/T2 必读）

M0 已 `cargo add` 解析并锁定以下版本（`Cargo.lock` 已入库）。**这些是实际装上的版本，不是你记忆里的版本**——请以编译循环为准。

| crate | 实际版本 | 备注 |
|---|---|---|
| axum | **0.8.9** | `Router::route(get(|| async { Json(..) }))` 可用 |
| reqwest | **0.13.3**（实际解析为 **0.13.5**） | 特性名 `rustls`（非 `rustls-tls`）；默认 TLS 已是 rustls，native-tls 变 opt-in。依赖树含 `openssl-probe`（仅查 CA 路径，**非 OpenSSL**，见下方 M5 风险） |
| rusqlite | **0.40.2** | `bundled` 特性，由 `cc` 编译 SQLite |
| sysinfo | **0.36.1** | ⚠️ **与旧版 API 差异很大，必须靠编译循环，不要凭记忆** |
| toml | **1.0.7** | |
| thiserror | **2.0.21** | |
| tokio | 1.53.3 | `features=["full"]` |
| serde / serde_json | 1.0.229 / 1.0.154 | |
| chrono | 0.4.45 | `features=["serde"]` |
| clap | 4.5.61 | `derive` |
| tracing-subscriber | 0.3.23 | `env-filter` |
| hmac / sha2 / hex | 0.12.1 / 0.10.9 / 0.4.3 | |
| async-trait | 0.1.92 | |

### reqwest 0.13 的坑（T0 用探针实测抓到的，T1/T2 会直接踩）
- **`Request` 上没有 `send()`** —— 必须在 `RequestBuilder` 上 `send()`。先用 `.build()` 拿到 `Request` 再 `send()` 会编译失败。
- `rustls` 特性会自动装好 `aws-lc-rs` provider，`Client::builder().build()` 开箱可用；**不要**手工 `install_default()`（只有 `rustls-no-provider` 才需要，误用会 panic）。
- `ClientBuilder::timeout / connect_timeout`、`RequestBuilder::header / body / json().build()` 均正常。
- `aws-lc-rs` 会编译 C 代码，本机/WSL 实测**不需要** cmake，无需额外 apt 包。

### M0 已定下、T1/T2 必须沿用的约定
- `ssm_common::prelude::*` 一把抓导出（`lib.rs`）。
- 所有 protocol 类型都派生 `PartialEq` → 测试可直接 `assert_eq!` 整个 packet。
- `LogSource` 是**手写 serde**，序列化为纯字符串（`"journald:nginx.service"` / `"file:/path"`）。派生会产生 `{ journald = "..." }` 表，与配置字面不符——**不要改回派生**，已有回归测试。
- `AlertLevel`（告警级别）与 `LogLevel`（日志级别）是**两个独立类型**，别混用。
- 配置默认值：**只有 `host_id` 和 `relay_url` 必填**（agent 侧），其余全部有默认值。默认值见 §6 与本轮 `deploy/*.example`（两者已对齐）。
- `rust-toolchain.toml` 用 `channel = "stable"`（不写死版本号，避免 rustup 联网下载同版本工具链）。

### 已知待办（非 T1/T2 边界）
- **仓库缺 `.gitattributes`**：Windows 全局 `core.autocrlf=true` 会把 `deploy/*.sh` 与 `*.service` 检出成 CRLF，scp 到 Linux 后 `bash^M` / `ExecStart` 带 `^M` 直接炸。需加 `*.sh text eol=lf`、`*.service text eol=lf` 等规则。

### ⚠️ M5 风险：TLS 信任根 vs musl 静态二进制（**部署日会撞上，先记下**）
M0 验收发现依赖树里有 `openssl-probe`。**它不是 OpenSSL**（`ldd` 确认二进制只链 `libc`/`libgcc_s`，无 `libssl`/`libcrypto`）——它来自 `rustls-platform-verifier`，只用于**查找系统 CA 证书路径**。

问题在于：`docs/07` 的部署方案是「交叉编译 `x86_64-unknown-linux-musl` 静态单二进制，scp 拷贝即用」，而 relay 必须能出站 HTTPS 到 `api.day.app`。**静态 musl 二进制在精简系统（OpenWrt / 无 `/etc/ssl/certs` 的容器）上会找不到系统信任根，Bark 推送会静默失败。**

M5 处置建议（三选一，届时验证）：
1. 改用**内置根证书**（`webpki-roots` / reqwest 对应特性），彻底摆脱系统信任库——**最契合「静态单二进制」目标**；
2. 确认 relay 运行环境（PVE+Debian LXC）有 `ca-certificates`，保留平台验证器；
3. 接受 glibc 动态链接，放弃 musl 静态（与 docs/07 目标冲突，不推荐）。

**临时验证命令**（构建 musl 后）：
```bash
cargo build --release --target x86_64-unknown-linux-musl
# 若能构建，再在无 ca-certificates 的环境里实测能否推送成功
```
在 M5 之前不要为此改动 reqwest 特性——**先让 T1/T2 跑通，别引入未验证的变更**。

---

## 12. 变更记录

| 时间 | 变更 | 原因 |
|---|---|---|
| M0 前 | 初版 | 派发 T0 用 |
| M0 后 | 补 CLI 契约（§3）；补 `snapshot_path`（§6）；补 `store.rs` 快照行（§7）；补 §11 | T4 需要 CLI/路径自洽；T0 落地后回填真实版本与 API 坑 |
| M0 后 | `reqwest` 特性名 `rustls-tls` → `rustls`（§0.5）；`GitRepoConfig` 补 `timeout_sec` | 与已解析版本对齐 |
| M3 后 | 新增 §13「告警生命周期契约」 | **e2e 实测抓出 T1/T3 契约不一致**，见 §13 |

---

## 13. 告警生命周期契约（**e2e 实测抓出的跨 crate bug，勿再违反**）

### 13.1 背景：这个 bug 长什么样

首次跑通全链路 e2e 时，`relay.log` 的告警生命周期是这样的（探针目标**始终不可达**）：
```
seq=2  alerts=1   ← 服务探针告警出现
seq=3  alerts=1
seq=4  alerts=0   → REC「【服务不可用】web-frontend 探测失败」已恢复   ← 服务根本没恢复！
seq=5..11 alerts=0   ← 此后永久沉默，服务一直挂着也不再提醒
```
三个用户可见的后果：
1. **假恢复**：告警发出 4 秒后就告诉运维「已恢复」，而服务仍然挂着 —— 主动误导。
2. **永久失声**：此后条件一直成立却再不提醒，服务挂一整天你也不知道。
3. **语义错误**：`log_watcher` / `git_tracker` 也收到「已恢复」通知 —— 但「日志命中一次」「提交了一个 commit」是**点事件**，谈不上"恢复"。

**根因**：T3 的采集器只在**条件成立的那一轮**（前沿）产出告警，之后不再产出；而 T1 的 relay 把「本轮 `alerts[]` 里没有它」判定为「条件已恢复」。双方各自的单测都通过，**只有跑真链路才会暴露**。

### 13.2 契约：按 source 分两类，各自的产出规则

| 类别 | `source` | 语义 | agent 产出规则 | relay 行为 |
|---|---|---|---|---|
| **条件型** | `service_probe`、`thresholds` | 一个**持续存在**的状态 | **条件存续期间，每一轮都必须出现在 `alerts[]` 里**（同一个稳定 `id`）。条件消失即不再出现。 | 出现在 `alerts[]` → 记为 active，**重复出现不重复推送**（去重）；**缺席且此前 active → 推「已恢复」并从 active 移除** |
| **事件型** | `log_watcher`、`git_tracker` | 一个**已经发生**的点事件 | **只在事件发生的那一轮出现一次**（按各自的 `throttle_sec` 限频）。 | 出现 → 记为 active 并推送；**缺席 → 静默从 active 移除，绝不推「已恢复」**（点事件没有恢复语义） |
| **relay 自生成** | `relay`（如「服务器离线」） | 条件型，由 relay 自己的 FSM 管理 | —— | 按 FSM 自行管理 active 与恢复通知 |

**要点**：
- **条件型告警由 agent 每轮重复上报，不靠 relay 去猜**。relay 的 `(source, title)` active 集合负责去重，所以不会造成推送风暴；这正是 docs/04 §5「同一告警在恢复前不重复推送」的实现前提。
- **agent 不需要自己产出「已恢复」通知**（不要再用 `xxx:recovered` 这种 id 上报信息级恢复告警）——恢复通知由 relay 依据「条件型告警从 `alerts[]` 中消失」推导，agent 侧重复上报只会造成重复/混乱。
- **`thresholds` 的持续超阈重复提醒**也属于条件型：每轮都上报即可，**由 relay 的 `rate_limit_per_min` / `repeat_alert_min` 决定是否要再响一次**；不要在 agent 侧用「每 sustained_sec 才报一次」的方式实现（那会产生假恢复窗口）。
- 事件型告警的 `throttle_sec` 仍在 agent 侧生效（同一个日志规则 5 分钟内只报一次），这是**限频**，与"恢复"无关。

### 13.3 必须有的回归测试- **agent 侧**（T3）：探针持续失败 N 轮 → 断言**每一轮** `alerts[]` 都含该告警；探针恢复 → 断言下一轮起不再含。`thresholds` 同理。
- **agent 侧**（T3）：`log_watcher` 命中一次 → 断言**只有那一轮**含告警，后续轮次不含（事件型）。
- **relay 侧**（T1）：条件型告警连续多轮出现 → 断言**只推 1 条**；随后缺席 → 断言推 1 条「已恢复」。
- **relay 侧**（T1）：事件型告警出现后缺席 → 断言**没有任何「已恢复」推送**。
- **e2e**：探针持续不可达 ≥ 6 轮 → 断言 `bark.log` 里**恰好 1 条**该探针的 critical、且**0 条**它的「已恢复」（因为条件一直没消失）。

### 13.4 三条配套硬规则（同一 bug 类的其他实例，别再踩）

**(1) 告警 `title` 必须稳定 —— 绝不能含逐轮变化的值。**
relay 的 active 集合与恢复判定都用 `(source, title)` 做键。若 title 里放了实时数值，键每轮都变 →
**同时产生「推送风暴」和「假恢复」**。
- ❌ 反例：`【磁盘告警】/ 已用 90.0%`（使用率逐轮变 → 每轮都是"新告警"，且旧标题被判"已恢复"）
- ✅ 正例：`【磁盘告警】/ 使用率超过 85.0%`（只含挂载点 + **配置阈值**这种常量），**实测值放 `summary`**

**(2) `lag_threshold_sec`（过期告警只入库不推送）按「上报包的新鲜度」判定，不是按告警持续了多久。**
```
expired = now - packet.sent_at > lag_threshold_sec      // ✅ 正确
expired = now - alert.first_seen > lag_threshold_sec    // ❌ 错误
```
理由：`docs/04` §5 的原意是「**补报包**里的过期告警只入库不推送」，针对的是**包本身是不是补报的**。
而条件型告警的 `first_seen` 在条件存续期间**恒定**（§13.2 要求），拿它判过期会导致：
**relay 重启（台账为空）+ 条件已持续 > `lag_threshold_sec`** → 该告警首轮即被判过期 →
**只入库、永不推送**，此后无论条件持续多久都不再响（比假恢复更严重的「永久失声」）。
- agent 侧行为是配套的：补发时 **`sent_at` 保留原值**、但用**发送当刻的时间戳重新签名**。
  所以 `sent_at` 是判断「这个包是不是补报的」的可靠信号。

**(3) 一个 `source` 可以同时产出条件型与事件型告警 —— 此时靠「产出节奏」而非分类来区分。**
特例：`git_tracker` 有两条语义不同的告警：
| 告警 `id` | 语义 | 产出节奏 | relay（source=git_tracker 事件型） |
|---|---|---|---|
| `git:<repo>:changed` | HEAD 变化 —— **点事件** | **只在该轮出现一次** | 推 1 条；缺席→静默移出 active，不推恢复 |
| `git:<repo>:dirty` | 工作区脏 —— **实为条件型** | **脏的期间每轮都出现** | 推 1 条（active 去重）；提交干净后缺席→静默移出，**不推"已恢复"**；再脏→重新推 1 条 |

这样**无需扩展协议**（不必给 `Alert` 加 lifecycle 字段），仅靠「条件型每轮上报 / 事件型只报一轮」
这一条产出纪律 + source 级分类就自洽。**若将来出现更复杂的情况，再考虑给 `Alert` 加显式的 lifecycle 字段。**
