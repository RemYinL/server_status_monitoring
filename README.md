# server_status_monitoring

基于 Rust 的 Linux 服务器状态监控组件：服务器端 agent 定时上报心跳与遥测数据，中转判定器检测离线并触发告警，iPhone 通过 Bark 接收告警、通过小组件实时查看服务器状态。

## 核心能力

- **心跳与离线判定**：agent 定时上报；中转端检测心跳超时后主动发起最多 3 次握手重试，全部失败判定离线。
- **遥测采集**：CPU / 内存 / 磁盘 / 负载 / 网络 / 运行时长等基础指标。
- **关键服务存活监控**：网站生产前端、Tailscale 隧道、自定义进程/端口/HTTP 探针。
- **日志告警**：对关键日志（journald / 文件）做规则匹配，命中即上报告警事件。
- **代码变动监控**：跟踪关键项目 git 仓库的 HEAD 变动与工作区状态。
- **多端告警与展示**：Bark（iOS APNs 原生推送，可同步小米手环震动）+ iPhone 小组件实时状态。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/01-requirements.md](docs/01-requirements.md) | 需求分析与非功能需求 |
| [docs/02-architecture.md](docs/02-architecture.md) | 总体架构、组件划分与技术选型 |
| [docs/03-protocol.md](docs/03-protocol.md) | 数据包协议与握手/离线判定流程 |
| [docs/04-alerting.md](docs/04-alerting.md) | 告警规则、分级与推送渠道 |
| [docs/05-roadmap.md](docs/05-roadmap.md) | 项目规划、里程碑与任务分解 |
| [docs/06-local-dev-testing.md](docs/06-local-dev-testing.md) | 本机（Windows + WSL）全栈调试方案 |
| [docs/07-relay-hardware.md](docs/07-relay-hardware.md) | relay 主机硬件调研与采购决策（N100 软路由） |

## 仓库结构（规划）

```
server_status_monitoring/
├── Cargo.toml              # workspace 根
├── crates/
│   ├── common/             # 协议结构体、序列化、共享类型
│   ├── agent/              # 部署于被监控 Linux 服务器的采集/上报端
│   └── relay/              # 中转判定器：接收心跳、离线判定、触发 Bark
├── ios/
│   └── widget/             # Scriptable 小组件脚本（iPhone 状态展示）
├── deploy/                 # systemd unit、部署脚本
└── docs/
```

## 状态

当前处于工程设计与文档阶段。目标服务器暂离线，全部开发与调试在本机（Windows + WSL）完成，待服务器抵达后部署。
