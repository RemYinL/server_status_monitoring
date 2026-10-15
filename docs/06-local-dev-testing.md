# 06 本机全栈调试方案（Windows + WSL）

目标：在服务器缺席的情况下，于 Windows 笔记本（WSL2）内完成 agent + relay + Bark + 小组件的全链路调试。

## 1. 环境拓扑（开发期）

```
WSL2 (Ubuntu)
├── ssm-agent    模拟"被监控服务器"：采集 WSL 指标，上报至 relay
│                 监听 127.0.0.1:9101 (/probe)
└── ssm-relay    监听 127.0.0.1:9100 (/report, /status)

Windows 宿主
└── portproxy    将局域网:9100 → WSL:9100（供 iPhone 小组件访问）

iPhone（同一局域网）
├── Bark         接收 relay 推送
└── Scriptable   GET http://<windows-lan-ip>:9100/status
```

## 2. 前置准备

1. WSL2 内安装 Rust：`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
2. iPhone 安装 Bark，获取 bark_key（App 内「服务器」页可见推送 URL）。
3. iPhone 安装 Scriptable。
4. Windows 与 iPhone 处于同一局域网。

## 3. 端口转发（iPhone → WSL）

WSL2 为 NAT 网络，iPhone 无法直连。在 Windows PowerShell（管理员）执行：

```powershell
# 查询 WSL IP
wsl hostname -I

# 建立转发（示例 WSL IP 为 172.x.x.x）
netsh interface portproxy add v4tov4 listenport=9100 listenaddress=0.0.0.0 connectport=9100 connectaddress=<WSL-IP>

# 放行防火墙
New-NetFirewallRule -DisplayName "ssm-relay" -Direction Inbound -LocalPort 9100 -Protocol TCP -Action Allow
```

注意：WSL 重启后 IP 可能变化，需更新 portproxy（或改用 Windows 版 tailscaled 走 100.x 网段，更稳定）。

## 4. 联调用例清单

| # | 用例 | 操作 | 预期 |
|---|---|---|---|
| T1 | 正常心跳 | 同时启动 relay、agent | relay 日志每 30s 一条心跳；`/status` 返回 `online` |
| T2 | 离线判定 | `kill <agent-pid>` | 90s 后 relay 进入 SUSPECT，3 次握手失败 → Bark critical 告警（手机弹窗 + 手环震动） |
| T3 | 抖动不误报 | agent 暂停 100s 后恢复（`kill -STOP/-CONT`） | 握手第 1~2 次成功则静默恢复，无推送 |
| T4 | 恢复通知 | T2 后重启 agent | Bark info「已上线，离线时长 X」 |
| T5 | 服务探针 | agent.toml 配置探针指向 WSL 内一个测试 HTTP 服务，停掉它 | 连续 2 周期 → warning 告警；恢复 → info |
| T6 | 日志规则 | 向监控的测试日志文件写入 `ERROR test` | 随下一心跳上报告警 → warning 推送；5 分钟内重复写入不再推送 |
| T7 | 代码变动 | 在监控的 git 仓库提交一个 commit | 下一心跳后收到 info「代码变动」 |
| T8 | 磁盘阈值 | agent.toml 将阈值临时调低触发 | warning 推送，6h 内不重复 |
| T9 | 签名防伪造 | curl 无签名 POST /report | relay 返回 401，不入库 |
| T10 | 小组件 | Scriptable 脚本运行 | 显示 online + 指标；杀 agent 后手动刷新显示 offline（红色） |
| T11 | relay 重启持久化 | 重启 relay 进程 | 心跳台账从 SQLite 恢复，不误报离线 |

## 5. Scriptable 小组件脚本要点

- 配置区：`RELAY_URL`（`http://<windows-lan-ip>:9100`）、`TOKEN`。
- 每 15 分钟由系统调度；渲染：`state` 色块（绿/黄/红）+ CPU/内存/磁盘百分比 + 服务灯 + `last_heartbeat` 相对时间。
- 离线时背景红色并显示「最后心跳 x 分钟前」。
- 脚本源码随仓库 `ios/widget/` 维护。

## 6. 与生产的差异记录

| 项 | 开发期 | 生产期 |
|---|---|---|
| agent 宿主 | WSL2 | 真实 Linux 服务器 |
| 传输通道 | WSL 内 loopback / 局域网 | Tailscale 内网（或 HTTPS 公网域名） |
| relay 宿主 | WSL2 | 常开设备（笔记本/VPS，优先独立于被监控机） |
| journald | WSL 需启用 systemd（`/etc/wsl.conf` 设 `systemd=true`） | 原生支持 |
| 小组件访问 relay | portproxy / tailscaled on Windows | Tailscale 100.x 地址或公网域名 + HTTPS |
