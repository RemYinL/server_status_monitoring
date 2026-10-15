# 05 项目规划与里程碑

总周期预估 5 个里程碑，全部开发与调试在本机（Windows + WSL）完成，最后里程碑待服务器抵达执行。

## M0 工程骨架（0.5 天）
- [ ] Cargo workspace：`common` / `agent` / `relay` 三 crate + 基础 CI（fmt、clippy、test）
- [ ] `common`：协议结构体（ReportPacket / StatusSnapshot / Alert）+ serde 单测
- [ ] TOML 配置加载骨架、tracing 日志初始化
- 验收：`cargo test` 全绿，`cargo build --release` 产出两个二进制

## M1 采集与上报闭环（1.5 天）
- [ ] agent：sys_metrics 采集器（sysinfo）
- [ ] agent：reporter（reqwest + HMAC 签名 + 本地环形缓冲重传）
- [ ] agent：`/probe` 握手应答端点
- [ ] relay：`POST /report` 验签接收 + SQLite 落库 + 内存心跳台账
- [ ] WSL 内联调：agent(WSL) → relay(WSL) 全链路心跳
- 验收：relay 日志可见连续心跳；杀掉 agent 进程后台账停止更新

## M2 离线判定与告警（1.5 天）
- [ ] relay：离线判定状态机（超时 → 3 次握手 → OFFLINE）
- [ ] relay：Bark 推送器 + 重试 + 全局限流
- [ ] relay：`GET /status` 快照接口（Bearer Token）
- [ ] 告警台账（active 集合去重）
- 验收（本机实测）：启动 agent 正常 → `kill` agent → 90s 内收到 Bark critical 告警（手机弹窗 + 手环震动）→ 重启 agent → 收到恢复通知

## M3 高级采集（2 天）
- [ ] service_probe：HTTP / TCP / process / tailscale 四类探针
- [ ] log_watcher：journald + 文件 tail + 正则规则 + 节流
- [ ] git_tracker：HEAD 变动与工作区脏状态
- [ ] 资源阈值评估（磁盘/CPU/内存）
- 验收：对 WSL 内测试服务/日志源注入故障，各类告警按级别正确推送

## M4 iPhone 小组件与交付（1 天）
- [ ] Scriptable 小组件脚本（状态快照渲染、绿/黄/红状态色）
- [ ] `deploy/`：systemd unit（agent + relay）、安装脚本、secrets 模板
- [ ] 部署文档：Tailscale 网络要求、Bark 开通、手环通知同步清单、portproxy 说明
- [ ] README 完善 + 架构图
- 验收：iPhone 桌面小组件每 15 分钟刷新，显示在线状态与核心指标；文档可由他人按步骤复现部署

## 待服务器抵达后（M5 部署日清单）
1. 服务器开机，确认 Tailscale 在线、SSH 可达
2. 交叉编译（`x86_64-unknown-linux-musl` 静态）或服务器端 `cargo build --release`
3. 部署 agent（systemd），relay 按 02 文档 4.2 决策部署位置（优先笔记本/VPS 常开设备）
4. 生产前端与隧道探针纳入 `agent.toml`
5. 全链路演练：断网模拟离线 → 验证 critical 告警 → 恢复验证
6. （可选）配置 UptimeRobot 兜底监控 relay `/status`

## 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| relay 与被监控服务器同机宕机 | 离线告警失效 | relay 放第二台常开设备；外部 UptimeRobot 兜底 |
| WSL 开发期与生产 Linux 行为差异 | 部署日返工 | 采集器统一走 sysinfo/journalctl 抽象；M3 起在 WSL 内跑 systemd 模式验证 |
| Bark 官方服务不可用 | 告警延迟 | 端点可切换自建 Bark server；推送失败重试 + 日志 |
| 小组件刷新受 iOS 调度限制 | 状态显示延迟 ≤ 15~30 分钟 | 可接受（实时性由 Bark 告警保证）；后续可评估原生 App |
| 手环同步被勿扰过滤 | 夜间漏告警 | 部署清单中明确勿扰例外配置步骤 |
