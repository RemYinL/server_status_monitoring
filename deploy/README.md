# deploy —— 部署手册（server_status_monitoring）

面向**真实服务器**的部署步骤。开发期本机联调（WSL 里同时跑 agent + relay + portproxy）见
[`docs/06-local-dev-testing.md`](../docs/06-local-dev-testing.md)。

本目录文件一览：

| 文件 | 作用 |
|---|---|
| `install.sh` | 幂等安装脚本（Debian/Ubuntu）：建用户/目录、装二进制、装配置与 unit |
| `ssm-agent.service` | 被监控服务器上的采集/上报端 unit |
| `ssm-relay.service` | 中转判定器 unit（含 tmpfs 台账 + SSD 快照的目录机制） |
| `agent.toml.example` | `ssm-agent` 配置模板（含逐字段中文注释） |
| `relay.toml.example` | `ssm-relay` 配置模板（含逐字段中文注释） |
| `secrets.toml.example` | 密钥模板（**只有占位符**，真密钥必须手工填） |

---

## 0. 部署拓扑与端口

```
被监控服务器 (agent)                      relay 主机 (N100 软路由, 常开)
┌───────────────────────────┐            ┌────────────────────────────────┐
│ ssm-agent                 │            │ ssm-relay                      │
│  :9101 GET /probe  ◄──────┼──握手探测──┤  :9100 POST /report  (HMAC 验签)│
│  每 30s POST /report ─────┼───上报────►│  :9100 GET  /status  (Bearer)  │
└───────────────────────────┘            │  :9100 GET  /healthz (无鉴权)  │
                                         └───────────┬────────────────────┘
                                                     │ 出站 HTTPS（无需入站映射）
                                                     ▼
                                        Bark → iPhone 通知 → 小米手环
iPhone 小组件 ──GET /status (Bearer)──► relay:9100
```

| 端口 | 方向 | 用途 | 是否需要对外开放 |
|---|---|---|---|
| `9100` | 入站 relay | `/report`、`/status`、`/healthz` | 只在 tailnet / 内网；**不要**明文暴露公网 |
| `9101` | 入站 agent | `/probe` 握手应答（无鉴权） | 只需 relay 可达 |
| `443` | **出站** relay | Bark 推送（`api.day.app` 或自建） | 无需入站映射（[docs/07 §3](../docs/07-relay-hardware.md)） |

---

## 1. 交叉编译 musl 静态二进制

在生产机上直接 `cargo build` 也可以，但**推荐在开发机交叉编译**，产物是单文件静态二进制，scp 过去即可用，
不受目标机 glibc 版本影响（软路由上的 Debian 往往比开发机旧）。

在 WSL Ubuntu 内（[SPEC.md §1](../SPEC.md) 要求在 ext4 上编译，不要放 `/mnt/c`）：

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/ssm-target-release       # 独立 target dir，避免抢锁
cd ~/ssm-src/server_status_monitoring                   # 仓库的 ext4 副本

# 一次性安装 musl 目标
rustup target add x86_64-unknown-linux-musl             # N100 软路由（定案，docs/07 §4）
# rustup target add aarch64-unknown-linux-musl          # ARM 存档方案（N1 / NanoPi / 树莓派）

# 构建（本项目零 OpenSSL 依赖，musl 静态构建不需要额外 C 工具链）
cargo build --release --target x86_64-unknown-linux-musl

ls -la target/x86_64-unknown-linux-musl/release/ssm-agent \
       target/x86_64-unknown-linux-musl/release/ssm-relay
file target/x86_64-unknown-linux-musl/release/ssm-relay   # 期望：statically linked
```

> **musl 构建失败排查**：若报 `linker 'cc' not found` / `cannot find -lc`，装 musl 工具链
> `sudo apt install musl-tools`；若报找不到 `x86_64-linux-musl-gcc`，`rustup target add` 那一步没做成功。

---

## 2. scp 二进制到目标机

```bash
# relay 主机（N100 软路由）
scp target/x86_64-unknown-linux-musl/release/ssm-relay  root@192.168.1.2:/tmp/
# 被监控服务器
scp target/x86_64-unknown-linux-musl/release/ssm-agent  root@192.168.1.10:/tmp/

# 部署文件（unit / example / install.sh）也一起过去
scp -r deploy root@192.168.1.2:/tmp/ssm-deploy
```

---

## 3. 在目标机上执行 install.sh

```bash
ssh root@192.168.1.2
cd /tmp/ssm-deploy

# 先看帮助，确认参数符合你的场景
./install.sh --help

# relay 主机：只装 relay，装完立即启动
sudo ./install.sh --relay-only --src-dir /tmp --start

# 被监控服务器：只装 agent
sudo ./install.sh --agent-only --src-dir /tmp --start
```

脚本是**幂等**的，重复跑安全：

- 已有配置 / secrets **一律不覆盖**，只提示；
- 已存在的用户、组、目录只校正属主与权限；
- secrets 若已存在但权限不是 600，会被 `chmod 600` 修正，属主若不对会被 `chown` 到服务用户。

脚本**不会做的事**（有意为之）：

- 不生成任何密钥 —— 弱密钥比没有密钥更危险；
- 不改你的 `host_id` / `relay_url` —— 那是你的业务信息；
- 默认不 `start` —— 因为 secrets 还没就绪时启动必然失败（见下一步顺序）。

一次典型的首次安装输出（节选）：

```
[信息] ===== 1/6 用户与组 =====
[完成] 创建系统用户 ssm-relay
[信息] ===== 3/6 二进制 =====
[信息]   /tmp/ssm-relay 是静态二进制（musl），可跨发行版直接运行
[完成] 安装 /usr/local/bin/ssm-relay  (来源：/tmp/ssm-relay)
[注意] /etc/ssm-relay/secrets.toml 尚未创建，需手工执行：...
```

---

## 4. 创建 secrets（脚本不代劳）

顺序很重要：**先 secrets，后 config，最后 start**。

```bash
# 生成密钥（32 字节熵，十六进制）
openssl rand -hex 32     # → shared_secret（agent 与 relay 两端必须完全一样）
openssl rand -hex 32     # → status_token（只要 relay 有，iPhone 小组件用；不要与上面同值）

# --- relay 主机 ---
sudo install -m 0600 -o ssm-relay -g ssm-relay \
     /tmp/ssm-deploy/secrets.toml.example /etc/ssm-relay/secrets.toml
sudo -e /etc/ssm-relay/secrets.toml
#   填：shared_secret、status_token、[bark] endpoint / key / group

# --- 被监控服务器 ---
sudo install -m 0600 -o ssm-agent -g ssm-agent \
     /tmp/ssm-deploy/secrets.toml.example /etc/ssm-agent/secrets.toml
sudo -e /etc/ssm-agent/secrets.toml
#   填：shared_secret（与 relay 端同一个值）
```

> **600 权限的坑**：`chmod 600` 之后只有**属主**能读。所以属主必须是服务运行用户
> （`ssm-agent` / `ssm-relay`），不能是 `root`，否则服务读不到密钥会启动失败。
> 上面的 `install -m 0600 -o <user> -g <user>` 已经处理好了。

`secrets.toml` 已在仓库 `.gitignore` 中排除（`secrets.toml` 一行），永远不会被 commit。

---

## 5. 改配置

```bash
# relay 主机
sudo -e /etc/ssm-relay/relay.toml
#   必改：[[hosts]] host_id（与 agent 完全一致）、paths 里的探测 URL
#   核对：db_path = /run/ssm-relay/ledger.db（tmpfs）
#         snapshot_path = /var/lib/ssm-relay/ledger.snapshot.db（持久）

# 被监控服务器
sudo -e /etc/ssm-agent/agent.toml
#   必改：host_id（与 relay 的 [[hosts]] 一致）、relay_url
#   核对：probe_listen = "0.0.0.0:9101"（只写 127.0.0.1 会让 relay 永远握手失败 → 误判离线）
```

每个字段的含义与建议值都写在 `.example` 的中文注释里，直接对着看即可。

---

## 6. 起服务并验收

```bash
# relay 主机
sudo systemctl restart ssm-relay
systemctl status ssm-relay --no-pager
journalctl -u ssm-relay -f            # 期望：agent 每 30s 一条心跳

curl -s http://127.0.0.1:9100/healthz
curl -s -H "Authorization: Bearer <status_token>" http://127.0.0.1:9100/status | python3 -m json.tool

# 被监控服务器
sudo systemctl restart ssm-agent
systemctl status ssm-agent --no-pager
journalctl -u ssm-agent -f
curl -s http://127.0.0.1:9101/probe   # 期望：{"ok":true,"host_id":"...","uptime_sec":...}
```

`/status` 期望长这样（字段契约见 [docs/03 §4](../docs/03-protocol.md)）：

```json
{
  "host_id": "kebei-prod-01",
  "state": "online",
  "last_heartbeat": "2026-10-15T01:30:22Z",
  "metrics": { "cpu_percent": 12.4, "mem_percent": 38.1, "disk_percent": 42.1, "uptime_sec": 1234567 },
  "services": [{"name": "web-frontend", "ok": true}],
  "active_alerts": 0,
  "generated_at": "2026-10-15T01:30:25Z"
}
```

### 6.1 relay 存储分层的自洽性检查（重要）

[docs/07 §3](../docs/07-relay-hardware.md) 要求「台账放 tmpfs、快照落 SSD」，避免 TF 卡/eMMC 被频繁写坏。
本目录用 systemd 的目录机制实现，**unit 与 `relay.toml.example` 必须自洽**：

| 配置项 | 路径 | 由谁提供 | 介质 | 内容 |
|---|---|---|---|---|
| `db_path` | `/run/ssm-relay/ledger.db` | `ssm-relay.service` 的 `RuntimeDirectory=ssm-relay` | tmpfs（重启即失） | 热台账 |
| `snapshot_path` | `/var/lib/ssm-relay/ledger.snapshot.db` | `ssm-relay.service` 的 `StateDirectory=ssm-relay` | 持久分区 | 周期快照 |

启动后自检：

```bash
# 目录存在、属主正确、/run 那个是 tmpfs
ls -ld /run/ssm-relay /var/lib/ssm-relay
systemctl show ssm-relay -p RuntimeDirectory -p StateDirectory -p User -p Group
findmnt /run/ssm-relay      # 期望：tmpfs

# 跑满一个 snapshot_interval_sec（默认 300s）后应看到快照文件
ls -l /var/lib/ssm-relay/

# 重启不误报离线（docs/06 用例 T11）
sudo systemctl restart ssm-relay && sleep 5
curl -s -H "Authorization: Bearer <status_token>" http://127.0.0.1:9100/status   # 仍应是 online
```

> **改了路径必须两处同步**：只改 `relay.toml` 的 `db_path` 而不同步 unit 的 `RuntimeDirectory`，
> 或反过来，会导致数据库落在没有写权限的位置（`ProtectSystem=strict` 下直接启动失败）。

---

## 7. iPhone 小组件（Scriptable）

1. iPhone 装 [Scriptable](https://apps.apple.com/app/scriptable/id1405459188) 与 Bark。
2. 把 [`ios/widget/server_status.js`](../ios/widget/server_status.js) 全文粘进 Scriptable 新建脚本，命名 `server_status`。
3. 改脚本顶部配置区：
   - `RELAY_URL`：relay 地址，**不要**带结尾斜杠、不要带 `/status`。
     生产填 relay 的 Tailscale `100.x.y.z:9100` 或内网固定 IP。
   - `TOKEN`：建议**留空占位符**，把真 token 填到小组件的 Parameter 里（脚本优先读 Parameter），
     这样真 token 不会被 git 提交。
4. 主屏长按 → 加 Scriptable 小组件（small / medium 都支持）→ 长按 → 编辑小组件 → Script 选 `server_status`。
5. 自检：小组件应显示绿色 `ONLINE`；`sudo systemctl stop ssm-agent` 后等 90s + 3 次握手，
   小组件刷新为红底大字「离线」。

渲染规则（对应 [docs/06 §5](../docs/06-local-dev-testing.md)）：

- `state` 色块：`online`=绿 / `suspect`=黄 / `offline`=红，**颜色只由 state 决定**；
- CPU / 内存 / 磁盘百分比（`metrics.{cpu_percent,mem_percent,disk_percent}`）；
- `services[]` 服务灯：`ok=true` 绿点、`false` 红点；
- `last_heartbeat` 相对时间（「最后心跳 2 分钟前」）；
- `active_alerts > 0` 显示告警计数；
- `metrics` 为 `null`（relay 从未收到心跳）时显示「无指标」，不崩；
- **离线态**：整块红底 + 大字「离线」+「最后心跳 x 分钟前」；
- **异常态**（请求失败 / 超时 / 401）：显示「无法连接 relay」+ 简短原因，不崩。

---

## 8. 小米手环通知同步检查清单

服务器告警要真正震到手腕上，这 4 条必须全过（[docs/04 §4](../docs/04-alerting.md)）：

- [ ] **1. iOS 设置 → 通知 → Bark**：允许通知，并开启「即时提醒」（Immediate Delivery）。
      关掉「定时摘要」——摘要会把 critical 告警延迟到下一次汇总，等于失效。
- [ ] **2. Bark App 内**：critical 级别通知开关已打开（relay 对 critical 用 `level=critical`
      + `volume=8` + `call=1`，需要 App 侧允许）。
- [ ] **3. 小米运动健康 → 通知与提醒 → 应用通知**：开启 **Bark** 同步。
      这一步最容易漏 —— iOS 弹了通知但手环不动，基本都是这里没开。
- [ ] **4. 手环勿扰时段**：确认夜间勿扰**不会**过滤 critical 告警。
      服务器宕机常在半夜，若被静音，整套监控的意义就没了；建议把勿扰设为「仅闹钟除外」
      或直接为 Bark 放行。

**端到端验收**（不要只看着 App 里那条，要确认手环震了）：

```bash
# 手工推一条 critical 测试通知（替换 <key>），确认手机强提醒 + 手环震动
curl -X POST "https://api.day.app/<key>/【测试】server_status/手环链路自检?level=critical&volume=8&call=1&group=server_status"
```

然后跑真实链路：`sudo systemctl stop ssm-agent` → 等 90s + 3×10s → 应收到 critical「服务器离线」。

---

## 9. 本机调试专用的 portproxy 说明

开发期 relay 跑在 WSL2 里，而 WSL2 是 NAT 网络，iPhone 连不进去（[docs/06 §3](../docs/06-local-dev-testing.md)）。
需要在 Windows PowerShell（**管理员**）做端口转发：

```powershell
# 1. 查 WSL 的 IP
wsl hostname -I

# 2. 建立转发：Windows 局域网:9100 → WSL:9100（把 <WSL-IP> 换成上一步的输出）
netsh interface portproxy add v4tov4 listenport=9100 listenaddress=0.0.0.0 `
      connectport=9100 connectaddress=<WSL-IP>

# 3. 放行防火墙
New-NetFirewallRule -DisplayName "ssm-relay" -Direction Inbound `
      -LocalPort 9100 -Protocol TCP -Action Allow

# 4. 查 Windows 局域网 IP，填进小组件的 RELAY_URL
ipconfig | findstr /i "IPv4"

# 5. 从 iPhone Safari 直接访问验证（应返回 JSON）
#    http://<Windows-IP>:9100/healthz
```

维护命令：

```powershell
netsh interface portproxy show v4tov4                              # 查看现有转发
netsh interface portproxy delete v4tov4 listenport=9100 listenaddress=0.0.0.0   # 删除
Remove-NetFirewallRule -DisplayName "ssm-relay"                    # 撤销防火墙规则
```

**注意**：WSL 重启后 IP 会变，portproxy 的 `connectaddress` 就失效了，需要重新执行第 1、2 步。
生产环境不要用 portproxy —— 直接给 relay 主机固定内网 IP + Tailscale（[docs/07 §3](../docs/07-relay-hardware.md)）。
`portproxy` 是 `netsh` 的子命令，不是独立工具，别在 WSL/Linux 里找它。

---

## 10. 升级与回滚

```bash
# 升级：把新二进制放到 /tmp，然后重跑 install.sh（配置与 secrets 不会被覆盖）
sudo ./install.sh --relay-only --src-dir /tmp --restart
sudo ./install.sh --agent-only --src-dir /tmp --restart

# 回滚：把旧二进制放回 /usr/local/bin 再 restart
sudo systemctl stop ssm-relay
sudo install -m 0755 /root/ssm-relay.bak /usr/local/bin/ssm-relay
sudo systemctl start ssm-relay
```

**升级前先留一份旧二进制**（上面的 `/root/ssm-relay.bak`），本脚本不做备份。

---

## 11. 常见故障

| 现象 | 原因 | 处理 |
|---|---|---|
| 服务起不来，`journalctl` 报 `Permission denied` 读 secrets | secrets 属主不是服务用户（600 只有属主可读） | `sudo chown ssm-relay:ssm-relay /etc/ssm-relay/secrets.toml` |
| relay 一直把服务器判为 offline，但 agent 日志正常 | 握手探测不通：`probe_listen` 写成 `127.0.0.1`，或防火墙挡了 9101 | 改 `0.0.0.0:9101`，确认 relay 侧 `curl http://<agent-ip>:9101/probe` 通 |
| `/report` 返回 401 | 两端 `shared_secret` 不一致，或两端时钟偏差 > 300s | 对齐密钥；`timedatectl status` 确认 NTP 已同步 |
| 小组件显示「无法连接 relay」 | 地址/端口错、token 错、relay 未启动、iPhone 不在同一 tailnet | 先 iPhone Safari 打开 `/healthz` 定位是网络还是 token |
| 小组件显示「无法连接 relay（TOKEN 无效）」 | `status_token` 与小组件不一致 | 重新复制 token（注意前后空格） |
| agent 启动即失败，报 db/seq 不可写 | `ProtectSystem=strict` 下写入未白名单的路径 | 用 `StateDirectory` 提供的 `/var/lib/ssm-agent`，别改到别处 |
| relay 重启后误报离线 | 快照没落盘或 `startup_grace_periods` 太小 | 检查 `/var/lib/ssm-relay/ledger.snapshot.db` 是否生成 |
| 收不到日志告警（`log_rules` 全不命中） | agent 用户不在 `systemd-journal` 组 | 已由 unit 的 `SupplementaryGroups=systemd-journal` 处理；确认组存在且有 journal 读取权限 |

---

## 12. 安全清单

- [ ] `secrets.toml` 权限 600、属主为服务用户，且**未**进入 git（`.gitignore` 已排除 `secrets.toml`）
- [ ] `9100` 不直接暴露公网明文；跨网走 Tailscale 或 HTTPS 反代
- [ ] `shared_secret` / `status_token` 用 `openssl rand -hex 32` 生成，互不相同
- [ ] 两端 `timedatectl` 确认 NTP 同步（签名时间戳容忍 300s）
- [ ] iPhone 小组件的真 token 放 Parameter，不写进 `server_status.js`
- [ ] relay 只需**出站** HTTPS，不要在路由器上为它开入站端口映射
