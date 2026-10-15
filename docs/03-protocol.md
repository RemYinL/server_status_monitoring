# 03 数据协议与离线判定流程

## 1. 上报数据包 ReportPacket

agent → relay，`POST /report`，`Content-Type: application/json`，签名头 `X-SSM-Timestamp` + `X-SSM-Signature`。

```json
{
  "version": 1,
  "host_id": "kebei-prod-01",
  "sent_at": "2026-10-15T01:30:00Z",
  "seq": 10234,
  "metrics": {
    "cpu_percent": 12.4,
    "mem_used_mb": 3120,
    "mem_total_mb": 8192,
    "disks": [
      {"mount": "/", "used_gb": 42.1, "total_gb": 100.0}
    ],
    "load_avg": [0.32, 0.41, 0.38],
    "uptime_sec": 1234567,
    "net": {"rx_kbps": 120.5, "tx_kbps": 88.2}
  },
  "services": [
    {"name": "web-frontend", "kind": "http", "target": "https://example.com", "ok": true, "latency_ms": 145, "detail": "200 OK"},
    {"name": "tailscale", "kind": "tailscale", "target": "100.x.x.x", "ok": true, "latency_ms": 23, "detail": "direct"}
  ],
  "git": [
    {"repo": "kebei-web", "branch": "main", "head": "a1b2c3d", "dirty": false, "changed": false}
  ],
  "alerts": [
    {
      "id": "log:nginx:error:20261015-0130",
      "level": "warning",
      "source": "log_watcher",
      "title": "nginx 出现 ERROR 日志",
      "summary": "upstream timed out (matched 3 lines)",
      "first_seen": "2026-10-15T01:29:40Z"
    }
  ]
}
```

字段约定：
- `version`：协议版本，relay 按版本解析，向前兼容。
- `seq`：单调递增序号，relay 用于检测丢包/乱序（仅记录，不告警）。
- `services[].kind`：`http | tcp | process | tailscale`。
- `alerts[].level`：`info | warning | critical`。
- agent 本地缓冲的重传包 `sent_at` 保留原始时间，relay 按 `sent_at` 去重（`(host_id, seq)` 唯一）。

## 2. 握手探测 ProbePacket

relay → agent，`GET http://<agent-ip>:9101/probe`，超时 5s。

响应：
```json
{"ok": true, "host_id": "kebei-prod-01", "uptime_sec": 1234567}
```

判定规则：HTTP 非 2xx、超时、TLS/连接失败均计为一次握手失败。

### 双路探测（relay 部署于软路由/同局域网时）

relay 配置中可同时声明两条探测路径，`lan`（内网直连 IP）与 `tailscale`（100.x 地址）。判定矩阵：

| lan | tailscale | 结论 | 告警文案 |
|---|---|---|---|
| 通 | 通 | ONLINE | — |
| 通 | 不通 | ONLINE_TUNNEL_DOWN | warning：「服务器在线，Tailscale 隧道断开」 |
| 不通 | 不通 | OFFLINE | critical：「服务器离线」 |
| 不通 | 通 | ONLINE_LAN_ANOMALY | warning：「内网直连异常（Tailscale 可达）」，通常表示网口/交换机问题 |

单路部署时（relay 不在同局域网），退化为仅 tailscale 路径，矩阵第一、三行适用。

## 3. 离线判定状态机（时序）

```
正常周期: agent 每 30s POST /report
        │
        ▼ relay 距上次心跳 > 90s (heartbeat_timeout)
   [SUSPECT] relay 发起第 1 次握手 ──成功──► [ONLINE] 静默恢复，不计事件
        │ 失败，等 10s
        ▼ 第 2 次握手 ──成功──► [ONLINE] 静默恢复
        │ 失败，等 10s
        ▼ 第 3 次握手 ──成功──► [ONLINE] 静默恢复
        │ 失败
        ▼
   [OFFLINE] Bark 推送 critical 级告警：
             「服务器离线」kebei-prod-01 最后心跳 01:30:22，3 次握手均失败
        │
        ▼ 任一：收到 /report 或握手成功
   [RECOVERED] Bark 推送恢复通知，附离线时长
```

参数（relay.toml 可配）：

| 参数 | 默认 | 说明 |
|---|---|---|
| `heartbeat_timeout_sec` | 90 | 心跳超时阈值（= 3 个上报周期） |
| `handshake_retries` | 3 | 握手重试次数 |
| `handshake_interval_sec` | 10 | 重试间隔 |
| `handshake_timeout_sec` | 5 | 单次握手超时 |
| `repeat_alert_min` | 30 | 持续离线期间的重复提醒间隔（0 = 不重复） |

## 4. 状态快照 StatusSnapshot

iPhone 小组件 `GET /status`（Bearer Token）响应：

```json
{
  "host_id": "kebei-prod-01",
  "state": "online",
  "last_heartbeat": "2026-10-15T01:30:22Z",
  "metrics": { "cpu_percent": 12.4, "mem_percent": 38.1, "disk_percent": 42.1, "uptime_sec": 1234567 },
  "services": [{"name": "web-frontend", "ok": true}, {"name": "tailscale", "ok": true}],
  "active_alerts": 0,
  "generated_at": "2026-10-15T01:30:25Z"
}
```

`state ∈ {online, suspect, offline}`。小组件以颜色区分：绿 / 黄 / 红。

## 5. 错误与边界

- relay 重启：心跳台账从 SQLite 恢复，避免误报；启动后给予 2 个上报周期的宽限。
- agent 重启：`seq` 从 SQLite/文件持久化恢复，断裂仅记录。
- 时钟偏差：签名时间戳容忍 300s；agent 与 relay 均要求 NTP 同步（部署清单检查项）。
- relay 自身宕机：属「观察者失效」，由外部兜底（UptimeRobot 监控 `/status`，见 02 文档 4.2）。
