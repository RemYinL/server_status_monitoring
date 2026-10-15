# server_status_monitoring

基于 Rust 的 Linux 服务器状态监控组件：**服务器端 agent 定时上报心跳与遥测 → 中转判定器（relay）独立判定离线并触发告警 → iPhone 通过 Bark 接收告警、通过 Scriptable 小组件实时查看状态。**

解决的核心问题：**服务器离线时，服务器自己没法告诉你它离线了** —— 所以离线判定必须由一个独立于被监控者的观察者来做。

---

## 核心能力

| 编号 | 能力 | 说明 |
|---|---|---|
| F1 | **在线状态监控** | agent 定时上报；relay 心跳超时后主动握手探测，连续 3 次失败才判定离线（双重确认，防抖动误报） |
| F2 | **基础资源指标** | CPU / 内存 / 磁盘（按挂载点）/ 负载 / 运行时长 / 网络吞吐 |
| F3 | **关键服务存活** | HTTP / TCP / 进程 / Tailscale 四类探针，连续 N 次失败才告警 |
| F4 | **日志告警** | journald 指定 unit 与任意日志文件，正则匹配、限频、**默认脱敏不上送原文** |
| F5 | **代码变动监控** | 跟踪关键 git 仓库的 HEAD 变动与工作区脏状态 |
| F6 | **多端告警与展示** | Bark（iOS APNs 原生推送，可同步小米手环震动）+ Scriptable 小组件 |

---

## 架构

```
┌──────────────────────┐          ┌──────────────────────────┐
│  Linux 服务器         │          │  relay（独立常开设备）      │
│  ┌────────────────┐  │  HTTPS   │  ┌────────────────────┐  │
│  │ ssm-agent      │──┼─────────►│  │ ssm-relay          │  │
│  │ · 指标采集      │  │ POST     │  │ · 心跳台账(内存+SQLite)│  │
│  │ · 服务探针      │  │ /report  │  │ · 离线判定状态机     │  │
│  │ · 日志规则      │  │  (HMAC)  │  │ · 双路握手探测(出站) │  │
│  │ · git 变动      │  │          │  │ · Bark 推送器       │  │
│  └────────────────┘  │◄─────────┼──┤   GET /probe        │  │
│        :9101/probe   │  握手探测 │  └─────────┬──────────┘  │
└──────────────────────┘          └────────────┼─────────────┘
                                               │ Bark API (APNs)
                                               ▼
                                  ┌──────────────────────────┐
                                  │  iPhone                  │
                                  │  · Bark 告警推送          │
                                  │  · Scriptable 小组件      │
                                  │    GET /status           │
                                  └───────────┬──────────────┘
                                              │ 通知同步
                                              ▼
                                      小米手环（震动提醒）
```

### 三个关键设计

1. **为什么必须有 relay，而不是 agent 直接推 Bark**：agent 离线时无法报告自己离线。relay 集中持有 Bark key 与告警状态机，agent 保持无状态、可重建。
2. **离线判定双重确认**：`心跳超时 90s → SUSPECT → 3 次握手（间隔 10s，超时 5s）→ 全失败才判 OFFLINE`。任一环节恢复即静默取消，不产生告警。
3. **双路探测**：relay 与被监控机同局域网时，同时探测 `lan` 与 `tailscale` 两条路径，用判定矩阵区分「服务器离线」与「服务器在线但隧道断开」（见 `docs/03-protocol.md` §2）。

---

## 仓库结构

```
server_status_monitoring/
├── Cargo.toml                  # workspace 根
├── crates/
│   ├── common/                 # 协议结构体 / 配置 / HMAC 签名（唯一来源）
│   ├── agent/                  # ssm-agent：采集 + 签名上报 + /probe 握手应答
│   │   └── src/collectors/     #   sys_metrics / service_probe / log_watcher
│   │                           #   git_tracker / thresholds
│   └── relay/                  # ssm-relay：台账 / 离线状态机 / 双路探测 / Bark / API
├── ios/widget/server_status.js # Scriptable 小组件
├── tests/widget/               # 小组件的 mock 回归测试（106 断言）
├── deploy/                     # systemd unit、安装脚本、配置模板
├── docs/01..07                 # 需求 / 架构 / 协议 / 告警 / 规划 / 调试 / 硬件
├── SPEC.md                     # 实现契约（代码结构、字段、跨 crate 约定）
├── DEVPLAN.md                  # 里程碑任务表与进度
└── HANDOFF.md                  # 交接文档（环境、构建、坑位）
```

---

## 快速开始

### 构建与测试

> ⚠️ **本项目的编译在 WSL（Ubuntu）内进行** —— 目标平台是 Linux，且 Windows 侧未装 Rust 工具链。
> 仓库位于 `/mnt/c`（drvfs），**务必把 `CARGO_TARGET_DIR` 指到 ext4**，否则 cargo 会慢 5–10 倍。

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/ssm-target
cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
# 产物：$CARGO_TARGET_DIR/release/ssm-agent、ssm-relay
```

环境搭建与坑位清单见 **[HANDOFF.md](HANDOFF.md)**。

### 运行

```bash
ssm-relay --config /etc/ssm-relay/relay.toml
ssm-agent --config /etc/ssm-agent/agent.toml
```

| 端口 | 用途 |
|---|---|
| `:9100 /report` | agent 上报（HMAC-SHA256 签名验签） |
| `:9100 /status` | 状态快照（`Authorization: Bearer <token>`，供小组件） |
| `:9100 /healthz` | 健康检查（无鉴权，供 UptimeRobot 外部兜底） |
| `:9101 /probe` | agent 握手应答（供 relay 探测存活） |

### 本机全栈联调

见 `docs/06-local-dev-testing.md`（含 T1–T11 用例清单与 iPhone 访问 WSL 的 portproxy 方案）。

### 小组件测试

```bash
bash tests/widget/run.sh     # 106 条渲染断言 + 语法检查，退出码可靠
```

要点：它 mock 了 Scriptable 的 `Request/Color/Font/ListWidget/Script/config/args`，并**加载真实的 `ios/widget/server_status.js`**（不复制源码）。它防的是「离线 / 连不上 relay 这两条路径静默崩溃」——`node --check` 覆盖不到这类问题（历史上真漏过一次 `return widget;`，导致最需要工作的两条路径白屏）。

---

## 配置

三份文件，模板见 `deploy/`：

| 文件 | 入库 | 权限 | 说明 |
|---|---|---|---|
| `agent.toml` | 是（`.example`） | 644 | 上报周期、探针、日志规则、git 仓库、阈值 |
| `relay.toml` | 是（`.example`） | 644 | 离线判定参数、被监控主机与双路探测地址 |
| `secrets.toml` | **否**（已 gitignore） | **600** | `shared_secret` / `status_token` / Bark key |

字段全表与默认值见 **[SPEC.md](SPEC.md)** §6。`agent.toml` / `relay.toml` 启用了 `deny_unknown_fields`（键名拼错会**报错**而不是静默取默认值）；`secrets.toml` 保持宽松（agent 与 relay 共用同一份文件），relay 启动时会**警告**其中的未知键。

---

## 部署

见 **[deploy/README.md](deploy/README.md)**。要点：

- 交叉编译 `x86_64-unknown-linux-musl` 静态单二进制 → `scp` → `deploy/install.sh`（幂等）。
- relay 用 systemd 的 `RuntimeDirectory` 把热台账放 **tmpfs**（`/run/ssm-relay`），`StateDirectory` 把周期快照落 **SSD**（`/var/lib/ssm-relay`）——避免频繁小写磨损，同时保证重启后台账可恢复、不误报离线。
- relay 应部署在**独立于被监控机的常开设备**上（现定案：新购 N100 软路由，旁路网关模式，见 `docs/07-relay-hardware.md`）。
- relay 自身宕机属「观察者失效」，用 UptimeRobot 监控 `/healthz` 兜底。

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/01-requirements.md](docs/01-requirements.md) | 需求分析与非功能需求 |
| [docs/02-architecture.md](docs/02-architecture.md) | 总体架构、组件划分与技术选型 |
| [docs/03-protocol.md](docs/03-protocol.md) | 数据包协议、握手/离线判定流程、双路探测矩阵 |
| [docs/04-alerting.md](docs/04-alerting.md) | 告警规则、分级与推送渠道 |
| [docs/05-roadmap.md](docs/05-roadmap.md) | 里程碑与任务分解 |
| [docs/06-local-dev-testing.md](docs/06-local-dev-testing.md) | 本机（Windows + WSL）全栈调试方案 |
| [docs/07-relay-hardware.md](docs/07-relay-hardware.md) | relay 主机硬件调研与采购决策 |
| [SPEC.md](SPEC.md) | **实现契约**：协议字段、配置、模块边界、告警生命周期契约 |
| [DEVPLAN.md](DEVPLAN.md) | 进度跟踪：任务表、验收标准、进度日志 |
| [HANDOFF.md](HANDOFF.md) | **交接文档**：环境复现、构建运行、已定案决策、坑位 |

---

## 项目状态

M0–M4 已在 Windows + WSL 内实现并验收（详见 [DEVPLAN.md](DEVPLAN.md)）：

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 | Cargo workspace + `common`（协议/配置/签名）+ CI | ✅ |
| M1 | agent 采集上报闭环 + relay 接收落库 | ✅ |
| M2 | 离线判定状态机 + Bark 告警 + `/status` | ✅ |
| M3 | 高级采集：服务探针 / 日志规则 / git 变动 / 资源阈值 | ✅ |
| M4 | Scriptable 小组件 + `deploy/` 部署脚本 | ✅ |
| M5 | 部署到真实服务器 + N100 软路由 | ⏳ 待服务器与硬件到位 |

**质量门**：`cargo fmt --check` / `cargo clippy --all-targets -D warnings` / `cargo test --workspace` 全绿（详见 DEVPLAN 的验收记录），另有端到端 e2e 用例（T1–T11）与小组件 mock 回归测试。

端到端验收覆盖：正常心跳、抖动不误报、离线判定与 critical 告警、恢复通知、服务探针、日志规则与节流、代码变动、签名防伪造（无签名/坏签名/无 Token 一律 401）、relay 重启后从快照恢复台账不误报。
