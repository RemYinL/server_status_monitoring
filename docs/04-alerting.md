# 04 告警规则与推送设计

## 1. 告警分级

| 级别 | 触发条件 | Bark 参数 | 触达效果 |
|---|---|---|---|
| critical | 服务器离线（3 次握手失败）；关键服务（web-frontend / tailscale）连续失败 | `level=critical` + `volume=8` + `call=1` | 穿透静音/专注模式，手机强提醒，手环震动 |
| warning | 日志规则命中（ERROR/OOM）；磁盘 > 85%；非关键服务失败 | `level=timeSensitive` | 正常推送，可穿透专注模式 |
| info | 代码 HEAD 变动；服务恢复；服务器恢复上线 | `level=active` | 普通通知 |

## 2. 告警源与规则

### 2.1 离线/恢复（relay 生成）
- 离线：`【离线】<host_id> 最后心跳 HH:MM:SS，握手 3/3 失败`
- 恢复：`【恢复】<host_id> 已上线，离线时长 Xm Xs`
- 持续离线重复提醒：`repeat_alert_min`（默认 30 分钟，0 关闭）。

### 2.2 服务存活（agent 探测，relay 推送）
- 单服务失败即时上报，relay 侧要求**连续 2 个上报周期失败**才推送，单周期抖动仅记录。
- 恢复时推送 info 级恢复通知。

### 2.3 日志规则（agent 匹配，随包上报）
`agent.toml` 示例：

```toml
[[log_rules]]
name = "nginx-error"
source = "journald:nginx.service"
pattern = "ERROR|upstream timed out|connect\\(\\) failed"
level = "warning"
throttle_sec = 300        # 同规则 5 分钟内只报一次
max_lines = 3             # summary 中最多附 3 行摘要

[[log_rules]]
name = "oom"
source = "journald:kernel"
pattern = "Out of memory|oom-kill"
level = "critical"
throttle_sec = 60
```

脱敏：summary 仅含匹配行号、计数与规则名；原始日志行默认不上送（`include_raw = false`）。

### 2.4 资源阈值（agent 评估）
- 磁盘使用率 > 85%（每挂载点，可配）→ warning，6 小时节流。
- CPU > 95% 持续 5 分钟 → warning（避免瞬时峰值误报）。
- 内存 > 90% 持续 5 分钟 → warning。

### 2.5 代码变动（agent 探测）
- HEAD hash 变化 → info 推送：`【代码变动】kebei-web main: a1b2c3d → e4f5g6h "fix: ..." (by author)`。
- 工作区出现脏文件超过 24h → info（提示有未提交改动）。

## 3. Bark 推送实现

- 端点可配：官方 `https://api.day.app` 或自建 Bark server。
- 请求：`POST <endpoint>/<bark_key>/<title>/<body>?level=<level>&group=server_status&icon=<url>`
- 分组 `group=server_status` 便于 iOS 通知聚合。
- 失败重试：指数退避 3 次（1s/4s/16s），仍失败写入 relay 日志并计入 `push_failures_total`。
- Bark key 存于 `relay.toml` 外的 `secrets.toml`（权限 600，.gitignore 排除）。

## 4. 手环链路

无需手环侧开发。检查清单（部署文档同步列出）：
1. iOS 设置 → 通知 → Bark：允许通知、开启「即时提醒」。
2. Bark App 内：critical 级别通知开启。
3. 小米运动健康 → 通知与提醒 → 应用通知 → 开启 Bark 同步。
4. 手环勿扰时段设置确认（避免夜间 critical 告警被手环过滤——服务器告警通常应例外放行）。

## 5. 防告警风暴

- relay 侧全局限流：同一 host 每分钟最多 5 条推送，超出合并为一条摘要。
- 同一 `(source, title)` 告警在恢复前不重复推送（告警台账记录 active 集合）。
- agent 重启/网络恢复后的补报包按 `sent_at` 去重，过期告警（> 10 分钟）只入库不推送。
