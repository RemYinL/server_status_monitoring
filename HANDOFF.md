# HANDOFF — 交接文档

> 给「下一个接手这个项目的人 / Agent」看的文档。记录**当前真实状态**、环境怎么复现、怎么构建运行、踩过哪些坑，以及下一步该干什么。
> 与 `DEVPLAN.md`（进度跟踪）和 `SPEC.md`（实现契约）配套。

最后更新：M0–M4 实现完成并通过验收（详见 §7）

---

## 1. 项目一句话

`server_status_monitoring`（简称 **ssm**）是一套自建的 Linux 服务器状态监控与告警系统：
**agent（Rust，跑在被监控服务器）→ relay（Rust，中转判定器，跑在独立的 N100 软路由）→ Bark/APNs → iPhone + 小米手环**。

核心命题：**服务器离线时，服务器自己没法告诉你它离线了** —— 所以离线判定必须由一个独立于被监控者的观察者来做。

设计文档见 `docs/01`–`docs/07`（**已完整，是行为权威**）；代码契约见 `SPEC.md`。

---

## 2. ⚠️ 最重要的四条环境事实

这几条不知道会浪费你半小时以上：

1. **Windows 侧没有可用的 Rust 工具链。** 存在 `~/.cargo/bin/cargo.exe`，但默认是 `stable-x86_64-pc-windows-msvc` 而本机**没有 `link.exe`**，直接编译必然失败。**所有编译/测试/运行都必须在 WSL Ubuntu 内做。**
2. **仓库在 `/mnt/c/...`（drvfs），cargo 直接在这里编译会慢 5–10 倍。** 必须把 `CARGO_TARGET_DIR` 指到 ext4：
   ```bash
   export CARGO_TARGET_DIR=$HOME/ssm-target
   ```
3. **本机 `sudo` 需要密码。** root 操作用 `wsl -d Ubuntu -u root -- <cmd>` 绕过，不需要密码。
4. **系统故意没有装 `libssl-dev`。** 如果某个依赖引入 OpenSSL，编译会失败 —— **这是有意的失败信号**，正确反应是改回 rustls，**不是** `apt install libssl-dev`（装了会让 glibc 动态链接偷偷混进来，M5 的 musl 静态构建就废了）。

---

## 3. 环境复现 / 日常使用

### 3.1 进入项目

```powershell
# 从 Windows PowerShell
wsl -d Ubuntu -- bash -lc 'cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring && bash'
```

一次性命令模板（推荐，脚本/编排用这个）：

```powershell
wsl -d Ubuntu -- bash -lc 'export PATH="$HOME/.cargo/bin:$PATH"; export CARGO_TARGET_DIR=$HOME/ssm-target; cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring && cargo test --workspace 2>&1 | tail -30'
```

### 3.2 已安装的工具

| 组件 | 版本 | 位置 / 备注 |
|---|---|---|
| Rust / cargo | 1.99.0 stable | `$HOME/.cargo/bin`（**不在默认 PATH**，要显式 export） |
| musl target | `x86_64-unknown-linux-musl` | 供 M5 静态构建 |
| gcc / make / pkg-config | 13.3.0 | 系统（build-essential）；`rusqlite bundled` 与 `aws-lc-rs` 都需要 C 编译器 |
| sqlite3 CLI | 3.45.1 | 验收时直接查 relay 台账用 |
| python3 | 3.12.3 | **WSL 内可用**；Bark mock、验收脚本用 |
| node | v24.11.0 | **只在 Windows 侧**（`C:\Program Files\nodejs\node.exe`）。WSL 里没有 node，只有互操作能调到的 Windows 节点，且路径要经 `wslpath -w` 转换 |
| crates.io 镜像 | rsproxy 稀疏镜像 | `~/.cargo/config.toml` |

**WSL 发行版**：Ubuntu 24.04.4 LTS，x86_64，**systemd 已启用**（`/etc/wsl.conf` → `systemd=true`），默认用户 `remyinl0713`。

### 3.3 从零重建环境

```powershell
# 1) 系统构建依赖（故意不装 libssl-dev，见 §2 第 4 条）
wsl -d Ubuntu -u root -- bash -c "apt-get update && apt-get install -y build-essential pkg-config musl-tools sqlite3 ca-certificates"

# 2) Rust（走 rsproxy 镜像，国内快）
wsl -d Ubuntu -- bash -lc 'export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup; curl -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --default-toolchain stable; $HOME/.cargo/bin/rustup target add x86_64-unknown-linux-musl'
```

---

## 4. 构建 / 测试 / 运行

> 下面命令假定已 `export PATH="$HOME/.cargo/bin:$PATH"`、`export CARGO_TARGET_DIR=$HOME/ssm-target` 且 `cd` 到仓库根。

```bash
# 质量门（提交前必过三条）
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 构建
cargo build --release
# 产物：$CARGO_TARGET_DIR/release/ssm-agent、ssm-relay

# M5：静态单二进制（部署到 N100 软路由用）
cargo build --release --target x86_64-unknown-linux-musl
```

### 运行

```bash
ssm-agent --config /etc/ssm-agent/agent.toml [--log-level debug]
ssm-relay --config /etc/ssm-relay/relay.toml [--log-level debug]
```

| 端口 | 用途 |
|---|---|
| `:9100 /report` | agent 上报（HMAC-SHA256 验签，无签名/坏签名/超 300s 一律 401） |
| `:9100 /status` | 状态快照（`Authorization: Bearer <status_token>`） |
| `:9100 /healthz` | 健康检查（无鉴权，供外部兜底监控） |
| `:9101 /probe` | agent 握手应答（供 relay 探测存活） |

### 小组件测试（不需要 Rust）

```bash
bash tests/widget/run.sh     # 106 条渲染断言 + node --check，退出码反映结果
```

它 mock 了 Scriptable API 并**加载真实的 `ios/widget/server_status.js`**（不复制源码）。防的是「离线 / 连不上 relay 这两条路径静默崩溃」——`node --check` 覆盖不到（历史上真漏过一次 `return widget;`，导致最需要工作的两条路径白屏）。

### 端到端验收（编排会话资产，未入库）

`%TEMP%\ssm-e2e.sh`（Windows 侧）—— 独立从 SPEC 反推断言的 e2e 脚本，用**本地 Bark mock**（`:9200`）避免打扰真机。覆盖 T1/T2/T3/T4/T5/T6/T7/T9/T11，包括：

- 签名防伪造（无签名 / 坏签名 / `/status` 无 Token → 全部 401）
- 抖动容忍（`SIGSTOP` 5s 不误报离线）
- 离线判定 → `level=critical&volume=8&call=1`
- 恢复通知（含离线时长）
- 探针 / 日志规则 / 代码变动三类告警，以及**事件型不得推「已恢复」**
- relay 重启：**先停 agent 再重启并删掉 tmpfs 上的 `ledger.db`**，若还能报出 `last_heartbeat` 就只可能来自快照恢复

运行：`wsl -d Ubuntu -- bash /mnt/c/Users/22602/AppData/Local/Temp/ssm-e2e.sh`

---

## 5. 已定案的关键决策（别再重新讨论）

| 决策 | 内容 | 理由 |
|---|---|---|
| 中转端必须独立 | relay 不与被监控服务器同机 | agent 无法报告自己离线；观察者必须独立。生产期放**新购 N100 软路由**（旁路网关模式） |
| 双路探测 | relay 同局域网时同时走 `lan` + `tailscale` | 区分「服务器离线」与「仅隧道断开」（docs/03 §2 矩阵） |
| 离线判定双确认 | 心跳超时 90s + 3 次握手各间隔 10s 全失败才判离线 | 防网络抖动误报 |
| 不用 Prometheus/Grafana | 自建轻量链路 | 单服务器场景过重（常驻 > 500MB vs 本方案 < 100MB） |
| SQLite 用 `rusqlite` + `bundled` | 不依赖系统 sqlite | 保证 musl 静态构建可用 |
| HTTP 用 rustls，禁用 OpenSSL | `reqwest` `default-features=false, features=["rustls","json"]` | 静态构建友好；**注意 0.13 起特性名是 `rustls` 不是 `rustls-tls`** |
| 小组件用 Scriptable | 免上架、免开发者账号 | iOS 限制下最轻方案 |
| 开发期全在 WSL | Windows 无 Rust 且目标是 Linux | 避免交叉编译调试成本 |
| **告警生命周期契约** | **条件型**（`service_probe`/`thresholds`/`relay`）条件存续期间**每轮上报**，缺席 = 恢复；**事件型**（`log_watcher`/`git_tracker`）只报一轮，缺席**静默**移出 active、**不推恢复通知** | 见下方 §5.1，这是 e2e 抓出的真 bug 的修复结论 |
| 告警 `title` 必须稳定 | 绝不能含逐轮变化的值（实测值放 `summary`） | relay 用 `(source,title)` 做去重**和**恢复判定；动态 title 会同时造成推送风暴与假恢复 |
| `lag_threshold_sec` 按**包新鲜度**判 | `now - packet.sent_at > lag`，**不是** `now - alert.first_seen` | 后者会让「relay 重启 + 故障已持续 > 5min」的告警只入库永不推送（永久失声） |

### 5.1 ⚠️ 一次真实的跨 crate bug（务必理解，别再犯）

首次跑通全链路 e2e 时，`relay.log` 长这样（探针目标**始终不可达**）：
```
seq=2  alerts=1   ← 服务探针告警出现
seq=3  alerts=1
seq=4  alerts=0   → REC「【服务不可用】web-frontend 探测失败」已恢复   ← 服务根本没恢复！
seq=5..11 alerts=0   ← 此后永久沉默
```
**根因**：agent 侧只在条件成立的**那一轮**（前沿）产出告警，而 relay 把「本轮 `alerts[]` 里没有它」当作「条件已恢复」。**双方各自的单元测试都通过，只有跑真链路才暴露。**

**教训**：跨组件的语义契约（谁在什么时候产出什么）必须**显式写进 SPEC 并配 e2e**，不能靠两侧各自「看起来合理」的实现去对齐。本项目的契约现在是 `SPEC.md` §13（含 §13.4 的三条配套硬规则）。

---

## 6. 已知坑位（踩过的，别重复踩）

1. **`cargo` 不在默认 PATH** —— `bash -lc` 下要 `export PATH="$HOME/.cargo/bin:$PATH"`。
2. **在 `/mnt/c` 直接编译极慢** —— 必须设 `CARGO_TARGET_DIR` 到 `$HOME` 下。
3. **系统故意没装 `libssl-dev`** —— 见 §2 第 4 条。依赖树里的 `openssl-probe` **不是 OpenSSL**（它来自 `rustls-platform-verifier`，只用来查系统 CA 路径；`ldd` 可确认二进制只链 `libc`/`libgcc_s`）。
4. **⚠️ PowerShell 会吞掉传给 WSL 的 `$变量`** —— 在 PowerShell 里写 `wsl ... bash -lc "... $c ..."` 时 `$c` 会被 PowerShell 先展开成空，症状是循环变量凭空变空。**解法**：把 bash 脚本写成 `.sh` 文件再 `wsl -d Ubuntu -- bash /mnt/c/.../x.sh`。本会话为此浪费过多次往返。
5. **PowerShell 双引号里的 `$?` 也会被展开成 0** —— 想验退出码要用反引号转义 `` `$? ``，否则会误判「脚本没报错」。
6. **`wsl` 命令会往 stderr 打一行中文乱码警告**（关于 localhost 代理）—— 无害，用 `2>$null` 屏蔽。
7. **`.gitignore` 原本忽略了 `Cargo.lock`** —— 本项目产出二进制，lock **必须提交**；已修正。
8. **CRLF 会打断 shell 脚本与 systemd unit** —— 已加 `.gitattributes` 强制 LF。Windows 上 `core.autocrlf=true` 时，没有它克隆出来的 `deploy/*.sh` 到 Linux 会报 `bash^M: bad interpreter`。
9. **子 Agent 常报「完成」但没 commit** —— 每次子 Agent 返回后必须 `git status -s && git log --oneline -3` 核验，别信自述。
10. **⚠️ 多个 Agent 共享同一个 git index 会串味** —— 本会话发生过两次：一个 Agent 已 `git add` 但还没 commit 时，另一个 Agent 抢先 `git commit`，把前者的文件卷进了自己的 commit；另有一次有 Agent 为了「验证」搭临时 commit，把 HEAD 移到了临时提交（已还原）。**纪律：并行阶段一律由编排会话统一提交，子 Agent 禁止 `git add`/`commit`/`reset`。**
11. **写验收脚本时注意 `grep -c` 的 `|| echo 0` 陷阱** —— `grep -c` 无匹配时**已经输出 `0`** 且返回非 0，再 `|| echo 0` 会得到 `"0\n0"`，后续 `[[ ... -gt ... ]]` 直接语法错误。本会话的 e2e 首跑因此误报了 3 个"失败"。
12. **Bark 的 title 在 URL path 里是 percent-encoded 的** —— 直接 grep 中文通知标题永远匹配不到，要先 `unquote`（这是正确行为，不是 bug）。
13. **测试要断言「真产出」** —— 本会话出现过两次假阳性：小组件测试断言了 `ListWidget` 构造期的对象（漏 `return` 时依然恒真），以及「绑端口再放掉」造失败目标时并行测试偶尔重新绑上同一端口。**断言恒真 = 没有测试。** 验证测试有效性的唯一方式是**故意把 bug 放回去看它是否变红**。

---

## 7. 当前状态与下一步

**M0–M4 已实现并通过验收**（详见 `DEVPLAN.md`）：

```
cargo fmt --all --check                                  exit 0
cargo clippy --workspace --all-targets -- -D warnings     exit 0（零警告）
cargo test --workspace                                    329 passed; 0 failed
    └── ssm-agent 121 / ssm-common 56 / ssm-relay 152
cargo build --release                                     两个二进制均产出
端到端 e2e                                                PASS = 30  FAIL = 0
tests/widget/run.sh                                       106 断言全过
```

**下一步（M5，需硬件与服务器到位）**，见 `docs/05-roadmap.md` 与 `docs/07-relay-hardware.md`：

1. **先解决一个已知的 M5 风险**：TLS 信任根 vs musl 静态二进制。relay 必须能出站 HTTPS 到 Bark，而精简系统上静态 musl 二进制可能找不到系统 CA。处置建议见 `SPEC.md` §11「M5 风险」。
2. 交叉编译 `x86_64-unknown-linux-musl` → 部署 relay 到新购 N100 软路由（PVE + Debian LXC 或裸 Debian），配置双路探测。
3. 部署 agent 到真实服务器，把生产前端与 Tailscale 探针纳入 `agent.toml`。
4. 全链路演练：断网 → critical 告警；只断 Tailscale → 「隧道断开」warning；恢复验证。
5. （可选）配 UptimeRobot 监控 relay `/healthz` 作外部兜底。
6. 采购验收清单见 `docs/07-relay-hardware.md` §4.3。
