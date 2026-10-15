# HANDOFF — 交接文档

> 给「下一个接手这个项目的人 / Agent」看的文档。记录**当前真实状态**、环境怎么复现、怎么构建运行、踩过哪些坑，以及下一步该干什么。
> 与 `DEVPLAN.md`（进度跟踪）和 `SPEC.md`（实现契约）配套。

最后更新：M0 派发中（见 §6 时间线）

---

## 1. 项目一句话

`server_status_monitoring`（简称 **ssm**）是一套自建的 Linux 服务器状态监控与告警系统：
**agent（Rust，跑在被监控服务器）→ relay（Rust，中转判定器，跑在独立的 N100 软路由）→ Bark/APNs → iPhone + 小米手环**。

设计文档见 `docs/01`–`docs/07`（**已完整，是行为权威**）。

---

## 2. ⚠️ 最重要的三条环境事实

这三条不知道会浪费你半小时以上：

1. **Windows 侧没有 Rust 工具链，也没有 MSVC linker。** 所有编译/测试/运行**必须在 WSL Ubuntu 内**做。
   （WSL 里的 Rust 是 2026-10-15 由本次会话安装的。）
2. **仓库在 `/mnt/c/...`，是 drvfs 跨文件系统挂载，cargo 直接在这里编译会慢 5–10 倍。**
   必须把 `CARGO_TARGET_DIR` 指到 ext4 上：
   ```bash
   export CARGO_TARGET_DIR=$HOME/ssm-target   # 或任何 $HOME 下的路径
   ```
3. **本机 `sudo` 需要密码。** root 操作用 `wsl -d Ubuntu -u root -- <cmd>` 绕过，不需要密码。

---

## 3. 环境复现 / 日常使用

### 3.1 进入项目

```powershell
# 从 Windows PowerShell
wsl -d Ubuntu -- bash -lc 'cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring && bash'
```

一次性命令模板（推荐，编排/脚本用这个）：

```powershell
wsl -d Ubuntu -- bash -lc 'export PATH="$HOME/.cargo/bin:$PATH"; export CARGO_TARGET_DIR=$HOME/ssm-target; cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring && cargo test --workspace 2>&1 | tail -30'
```

### 3.2 已安装的工具

| 组件 | 版本 | 位置 |
|---|---|---|
| Rust / cargo | 1.99.0 stable | `$HOME/.cargo/bin`（**不在默认 PATH**，要显式 export） |
| musl target | `x86_64-unknown-linux-musl` | 供 M5 静态构建 |
| gcc / make / pkg-config | 13.3.0 | 系统（build-essential） |
| sqlite3 CLI | 3.45.1 | 系统，用于验收时直接查 relay 台账 |
| node | v24.11.0 | 系统，用于 Scriptable 小组件语法检查 |
| python3 | 3.12.3 | 系统，用于 Bark mock server |
| crates.io 镜像 | rsproxy 稀疏镜像 | `~/.cargo/config.toml` |

**WSL 发行版**：Ubuntu 24.04.4 LTS，x86_64，**systemd 已启用**（`/etc/wsl.conf` → `systemd=true`），默认用户 `remyinl0713`。

### 3.3 从零重建环境

```powershell
# 1) 系统构建依赖（故意不装 libssl-dev，见 §5 坑位 3）
wsl -d Ubuntu -u root -- bash -c "apt-get update && apt-get install -y build-essential pkg-config musl-tools sqlite3 ca-certificates"

# 2) Rust（走 rsproxy 镜像，国内快）
wsl -d Ubuntu -- bash -lc 'export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup; curl -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --default-toolchain stable; $HOME/.cargo/bin/rustup target add x86_64-unknown-linux-musl'
```

---

## 4. 构建 / 测试 / 运行

> 下面命令假定已 `export PATH="$HOME/.cargo/bin:$PATH"` 且 `cd` 到仓库根。

```bash
# 质量门（提交前必过三条）
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 构建
cargo build --release
# 产物：target/release/ssm-agent、target/release/ssm-relay

# M5：静态单二进制（部署到 N100 软路由用）
cargo build --release --target x86_64-unknown-linux-musl
```

### 本机全栈联调（WSL 内自测）

链路与用例见 `docs/06-local-dev-testing.md`（T1–T11）。要点：

- agent 监听 `:9101/probe`，relay 监听 `:9100`（`/report`、`/status`、`/healthz`）。
- iPhone 小组件要访问 relay 时，WSL2 是 NAT，需要 Windows 侧 `netsh interface portproxy` 转发（docs/06 §3）。
- **Bark 测试一律用本地 mock server，不要向真实 Bark 推消息**（避免打扰手机 + 避免泄露 key）。

---

## 5. 已定案的关键决策（别再重新讨论）

| 决策 | 内容 | 理由 |
|---|---|---|
| 中转端必须独立 | relay 不与被监控服务器同机 | agent 无法报告自己离线；观察者必须独立。生产期放**新购 N100 软路由**（旁路网关模式） |
| 双路探测 | relay 同局域网时同时走 `lan` + `tailscale` | 区分「服务器离线」与「仅隧道断开」（docs/03 §2 矩阵） |
| 离线判定双确认 | 心跳超时 90s + 3 次握手各间隔 10s 全失败才判离线 | 防网络抖动误报 |
| 不用 Prometheus/Grafana | 自建轻量链路，常驻 < 100MB | 单服务器场景过重 |
| SQLite 用 `rusqlite` + `bundled` | 不依赖系统 sqlite | 保证 musl 静态构建可用 |
| HTTP 用 rustls，禁用 OpenSSL | reqwest `default-features=false, features=["rustls-tls","json"]` | 静态构建友好（见坑位 3） |
| 小组件用 Scriptable | 免上架、免开发者账号 | iOS 限制下最轻方案 |
| 开发期全在 WSL | Windows 无 Rust 且目标是 Linux | 避免交叉编译调试成本 |

---

## 6. 已知坑位（踩过的，别重复踩）

1. **`cargo` 不在默认 PATH** —— `bash -lc` 下要 `export PATH="$HOME/.cargo/bin:$PATH"`，否则报 command not found。
2. **在 `/mnt/c` 直接编译极慢** —— 必须设 `CARGO_TARGET_DIR` 到 `$HOME` 下。
3. **系统故意没装 `libssl-dev`** —— 如果某个依赖引入 OpenSSL，编译会失败。**这是有意的失败信号**，正确反应是改回 rustls，不是 `apt install libssl-dev`。（装了会让 glibc 动态链接偷偷混进来，M5 的 musl 静态构建就废了。）
4. **PowerShell 会吞掉传给 WSL 的 `$变量`** —— 在 PowerShell 里写 `wsl ... bash -c "... $c ..."` 时 `$c` 会被 PowerShell 先展开成空。**解法**：把 bash 脚本写成 `.sh` 文件再 `wsl -d Ubuntu -- bash /mnt/c/.../x.sh`，或用单引号 PowerShell 字符串。
5. **`wsl` 命令会往 stderr 打一行中文乱码警告**（关于 localhost 代理）—— 无害，用 `2>$null` 屏蔽。
6. **`.gitignore` 原本忽略了 `Cargo.lock`** —— 本项目产出二进制，lock **必须提交**；已修正。
7. **子 Agent 常报「完成」但没 commit** —— 每次子 Agent 返回后必须 `git status -s && git log --oneline -3` 核验，别信自述。

---

## 7. 下一步（当前状态）

见 `DEVPLAN.md` §2 的任务表和 §5 进度日志。

当前：**M0 派发中**。后续顺序：T1(relay) / T2(agent 核心) / T4(小组件+部署) 在 M0 完成后并行 → T3(agent 高级采集) → T5 端到端集成验证 → T6 文档收尾。

M5（部署到真实服务器 + N100 软路由）需要等硬件与服务器到位，见 `docs/05-roadmap.md` 与 `docs/07-relay-hardware.md`。
