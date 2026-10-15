# DEVPLAN — server_status_monitoring 执行计划与进度跟踪

> 本文件是**进度跟踪单**，由编排会话实时更新。
> 目标来源于 `docs/05-roadmap.md`（M0–M5）。行为契约见 `docs/01`–`docs/07`，代码契约见 `SPEC.md`。

- **目标**：按项目设计执行 M0 → M4（M5 需真机服务器，暂缓）
- **执行方式**：编排会话（本会话）只做拆解 / 派发 / 验收 / commit&push；**所有源码由子 Agent 编写**
- **启动时间**：M0 派发前
- **约束**：每里程碑验收通过即 commit & push；README 与 HANDOFF 同步更新

---

## 1. 环境基线（已完成，2026-10-15 建立）

| 项 | 状态 | 说明 |
|---|---|---|
| Rust 工具链 | ✅ | WSL Ubuntu 24.04 内 `cargo 1.99.0` / `rustc 1.99.0`（stable） |
| musl target | ✅ | `x86_64-unknown-linux-musl`（M5 静态构建用） |
| C 编译器 | ✅ | gcc 13.3.0 / make / pkg-config（原缺失，已装 build-essential） |
| crates.io 镜像 | ✅ | rsproxy 稀疏镜像写入 `~/.cargo/config.toml` |
| sqlite3 CLI | ✅ | 用于验收时直接查 relay 台账 |
| Windows 侧 Rust | ❌ | 无（且无 MSVC linker）→ **编译全部在 WSL 内进行** |
| 构建目录 | ✅ | `CARGO_TARGET_DIR=$HOME/ssm-target-*`（ext4，避开 /mnt/c 的 drvfs 慢 IO） |

⚠️ **本机 sudo 需要密码**，root 操作走 `wsl -d Ubuntu -u root -- <cmd>`。

---

## 2. 子任务清单

| 任务 | 内容 | 负责 | 依赖 | 状态 |
|---|---|---|---|---|
| **T0** | M0 骨架：workspace + `common`（协议/配置/签名）+ 三 crate stub + CI | 子 Agent A | — | 🔄 进行中 |
| **T1** | M1/M2 relay：store / ledger / fsm / prober / alerter / api | 子 Agent B | T0 | ⏳ 待办 |
| **T2** | M1 agent 核心：sys_metrics / reporter / buffer / probe_server / main | 子 Agent C | T0 | ⏳ 待办 |
| **T3** | M3 agent 高级采集：service_probe / log_watcher / git_tracker / thresholds | 子 Agent D | T2 | ⏳ 待办 |
| **T4** | M4 交付物：Scriptable 小组件 + deploy/（systemd/install/示例配置） | 子 Agent E | T0 | ⏳ 待办 |
| **T5** | 端到端集成验证（WSL 内全链路 + Bark mock）+ 修缺陷 | 编排会话 | T1–T4 | ⏳ 待办 |
| **T6** | 文档收尾：README / HANDOFF / DEVPLAN 终态 | 编排会话 | T5 | ⏳ 待办 |

**文件边界**：T1/T2/T4 在 T0 后可**并行**（三者文件集不相交，且各自用独立 `CARGO_TARGET_DIR` 避免 cargo 抢锁）。
T3 必须等 T2（T2 负责在 `collectors/mod.rs` 定下 `Collector` trait，T3 只增文件不改 trait 签名）。

---

## 3. 验收标准（编排会话逐条实跑，**不信子 Agent 自述**）

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/ssm-target-verify
cd /mnt/c/Users/22602/Projects/kebei/server_status_monitoring
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

| 里程碑 | 验收要点 | 状态 |
|---|---|---|
| M0 | 三 crate 编译；协议 serde 往返 + docs 示例 JSON 反序列化测试全绿；release 产出两二进制 | ⏳ |
| M1 | WSL 内 agent→relay 连续心跳（每 30s）；`/status` 返回 `online`；杀 agent 后台账停更 | ⏳ |
| M2 | docs/06 §4 的 T1/T2/T3/T4/T9/T11 通过（Bark 用**本地 mock**，不推真机） | ⏳ |
| M3 | docs/06 §4 的 T5/T6/T7/T8 通过 | ⏳ |
| M4 | Scriptable 脚本渲染逻辑走查；systemd unit 过 `systemd-analyze verify` | ⏳ |

**关键纪律**：验证一律用「实际命令输出」而非子 Agent 的完成声明。历史上多次出现「子 Agent 报完成但没 commit / 代码没落盘」。
每次子 Agent 返回后立即 `git status -s && git log --oneline -3` 核验。

---

## 4. 本次使用的 Skills

| Skill | 用途 |
|---|---|
| `superpowers` | 开发流水线框架：设计→计划→子 Agent 构建→审查→收尾 |
| `kebei-devmode` | 派发中心模式铁律（主会话不改源码）、SPEC 文件派发、子 Agent 核验清单 |

> 未加载但可能后续需要：`test-driven-development`、`requesting-code-review`、`systematic-debugging`、`writing-plans`。

---

## 5. 进度日志

| 时间 | 事件 |
|---|---|
| 起 | 全库扫描完成，确认设计文档完整、零代码 |
| 起 | WSL 工具链安装完成（build-essential + Rust 1.99.0 + musl target + rsproxy 镜像） |
| 起 | 写入 `SPEC.md`（实现契约）；派发 T0 |
