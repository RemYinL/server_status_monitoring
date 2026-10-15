# 02 总体架构设计

## 1. 架构总览

```
┌────────────────────────┐         ┌─────────────────────────┐
│   Linux 服务器          │         │   中转端 (relay)          │
│  ┌──────────────────┐  │  HTTPS  │  开发期: WSL on 笔记本    │
│  │  ssm-agent (Rust)│──┼────────►│  生产期: 可迁回服务器     │
│  │  - 指标采集       │  │ POST    │  ┌─────────────────────┐ │
│  │  - 服务探针       │  │ /report │  │ ssm-relay (Rust)    │ │
│  │  - 日志规则匹配   │  │         │  │ - 心跳台账(内存+SQLite)│ │
│  │  - git 变动跟踪   │  │         │  │ - 离线判定状态机      │ │
│  └──────────────────┘  │         │  │ - 握手探测(出站)     │ │
│           ▲            │◄────────┼──│   /probe 3次重试     │ │
│           │ handshake  │  probe  │  └─────────┬───────────┘ │
└────────────────────────┘         └────────────┼─────────────┘
                                                │ Bark API (APNs)
                                                ▼
                                     ┌─────────────────────────┐
                                     │  iPhone 16 Pro           │
                                     │  - Bark 推送告警          │
                                     │  - Scriptable 小组件      │
                                     │    (GET relay /status)   │
                                     └───────────┬─────────────┘
                                                 │ 通知同步
                                                 ▼
                                          小米手环 11 NFC
```

## 2. 组件划分

### 2.1 ssm-agent（部署于 Linux 服务器）
Rust 二进制，systemd 托管。职责：
- **采集器（collectors）**
  - `sys_metrics`：CPU/内存/磁盘/负载/运行时长（`sysinfo` crate）
  - `service_probe`：HTTP 探针（生产前端 URL）、进程探针、端口探针、Tailscale 探针（`tailscale status` 解析或 ping 对端）
  - `log_watcher`：journald（`systemd` crate 或 `journalctl -f` 子进程）+ 文件 tail，正则规则匹配、去重限频
  - `git_tracker`：指定仓库 `git rev-parse HEAD` + `git status --porcelain` 轮询比对
- **上报器（reporter）**：将 `ReportPacket` 序列化为 JSON，HTTP POST 至 relay，带 HMAC-SHA256 签名头；失败本地缓冲（环形队列，最多 100 条）断点续传
- **握手应答端**：暴露轻量 HTTP `/probe`（仅监听 Tailscale 接口），供 relay 握手探测
- **配置**：`agent.toml`（上报周期、relay 地址、探针列表、日志规则、git 仓库列表、共享密钥路径）

### 2.2 ssm-relay（中转判定器）
Rust 二进制，开发期跑在 WSL，生产期可迁移。职责：
- **接收端**：`POST /report` 验签、落库（SQLite）、更新内存心跳台账
- **状态接口**：`GET /status`（供 iPhone 小组件拉取，返回最新快照 JSON，Token 鉴权）
- **离线判定状态机**：
  ```
  ONLINE --(心跳超时 90s)--> SUSPECT --(发起握手, 间隔10s, 3次)-->
    ├─ 任一成功 --> ONLINE（静默恢复）
    └─ 全部失败 --> OFFLINE --> 触发 Bark 离线告警
  OFFLINE --(收到心跳/握手成功)--> ONLINE --> 触发 Bark 恢复通知
  ```
- **即时告警通道**：上报包中 `alerts[]` 非空（服务失败/日志命中/代码变动）→ 立即按告警级别推送 Bark
- **Bark 推送器**：调用 Bark server API（自建或官方 `https://api.day.app`），支持级别映射（紧急=timeSensitive/critical，普通=active）

### 2.3 iPhone 侧
- **Bark App**：接收告警。`level=critical` 的离线告警会穿透静音并同步至小米手环震动。
- **Scriptable 小组件**：JS 脚本每 15 分钟（系统调度）`GET https://<relay>/status`，渲染在线状态、CPU/内存/磁盘、关键服务灯、最后心跳时间。配置项：relay URL + Token。

### 2.4 小米手环
无需独立开发：iPhone 通知通过小米运动健康 App 同步至手环震动。需在 iOS 与小米运动健康中开启 Bark/通知同步权限（部署文档中列出步骤）。

## 3. 技术选型

| 项 | 选择 | 理由 |
|---|---|---|
| 语言 | Rust 1.7x stable | 单二进制、低资源占用、强类型协议 |
| 异步运行时 | tokio | 生态标准 |
| HTTP | axum（relay/server 侧）+ reqwest（client 侧） | 轻量、类型安全 |
| 系统指标 | sysinfo | 跨平台，WSL 亦可调试 |
| 序列化 | serde + serde_json | 协议结构体定义于 common crate |
| 存储（relay） | SQLite（sqlx 或 rusqlite） | 零运维，单文件，够支撑单机监控 |
| 签名 | hmac + sha2 | 共享密钥，简单可靠 |
| 日志 | tracing + tracing-subscriber | 结构化日志 |
| 配置 | serde + toml | 人类可读 |
| 告警推送 | Bark HTTP API | iOS APNs 直达，免费，可自建 |
| 小组件 | Scriptable | 免上架、免开发者账号，原生 Widget 体验 |

## 4. 关键设计取舍

1. **为何需要 relay 而不是 agent 直推 Bark？**
   - agent 离线时无法自己报告「我离线了」——离线判定必须有独立于被监控者的观察者。
   - relay 集中持有 Bark key 与告警状态机，agent 保持无状态、可重建。
2. **relay 与 agent 同机部署的风险**：服务器宕机时 relay 同时死亡，无法发出离线告警。缓解：relay 必须放在独立于被监控机的常开设备，优先级为**软路由 > 常开笔记本 > 廉价 VPS**；若必须同机，则额外配置外部兜底（如 UptimeRobot 免费计划监控 relay 的 `/status`，文档给出配置方法）。

### 4.2a relay 部署于软路由（生产期首选形态）

软路由（OpenWrt / PVE LXC，x86 或 ARM64）是 relay 的最优部署位置：

- **常开且独立**：7×24 运行，与被监控服务器物理独立，满足观察者独立性要求。
- **双路探测**：relay 通常与被监控服务器同局域网，握手探测可同时走**内网直连**与 **Tailscale** 两条路径。可区分两类故障并推送精确告警：
  - 双路均失败 → 「服务器离线」
  - 内网通、Tailscale 不通 → 「服务器在线，Tailscale 隧道断开」（对依赖隧道访问的场景价值高）
- **资源占用可忽略**：relay 常驻内存 < 30MB、CPU ≈ 0，任何软路由均可承载。

部署约束与对策：

| 约束 | 对策 |
|---|---|
| OpenWrt 无 systemd（procd 托管）、musl libc | 交叉编译 `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` 静态单二进制；deploy/ 提供 procd init 脚本 `ssm-relay.init` |
| SQLite 频繁写闪存（eMMC/TF 磨损） | 心跳台账放 tmpfs（`/tmp/ssm-relay/`），每 5 分钟异步快照至持久分区；或 extroot/U 盘 |
| iKuai 等封闭固件 | 不支持，选用 OpenWrt 或 PVE 内 Debian LXC |
| 软路由宕机 = 全家断网，告警无人送达 | 属观察者失效固有边界，保留外部 UptimeRobot 兜底 |

3. **为何不用 Prometheus/Grafana**：单服务器场景过重（常驻内存 > 500MB），且推送与小组件链路仍需自建。本方案常驻总占用 < 100MB，闭环完整。后续若扩展多服务器，协议层兼容，可在 relay 侧加 Prometheus exporter。
4. **WSL 作为开发期 relay 的注意事项**：WSL2 网络为 NAT，iPhone 小组件无法直连 WSL 内服务。调试期小组件数据用局域网端口转发（`netsh interface portproxy`）或 tailscaled on Windows 解决，详见 06 文档。

## 5. 安全设计

- 上报通道：生产走 Tailscale 内网（WireGuard 加密）+ HMAC 签名双保险；若经公网域名则强制 HTTPS。
- HMAC 签名：`X-SSM-Signature = HMAC_SHA256(secret, timestamp || body)`，relay 校验时间戳偏差 < 300s 防重放。
- `/status` 接口：静态 Token（`Authorization: Bearer <token>`），Token 与密钥文件权限 600。
- 告警文本脱敏：仅含服务名与状态摘要，不含日志原文中可能的敏感信息（可配置白名单字段）。
