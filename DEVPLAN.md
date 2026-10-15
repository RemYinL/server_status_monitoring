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
| **T0** | M0 骨架：workspace + `common`（协议/配置/签名）+ 三 crate stub + CI | 子 Agent A | — | ✅ **完成**（`d44f439`） |
| **T0b** | 仓库根 `.gitattributes`（修 CRLF，A/B 克隆实证）+ 合集 secrets 解析复核 | 子 Agent A | T0 | ✅ **完成**（`7aa01cf`） |
| **T0c** | `AgentConfig`/`RelayConfig` 严格未知键 + `snapshot_path` 补齐 | 子 Agent A | T0 | ✅ **完成**（`b80f16a`） |
| **T1** | relay：store / ledger / fsm / prober / alerter / api / settings | 子 Agent B | T0 | ✅ **完成**（`48ca220`，152 tests） |
| **T1b** | relay 告警生命周期分流 + `lag_threshold` 口径修正 | 子 Agent B | T1 | ✅ **完成**（并入 `48ca220`） |
| **T2** | agent 核心：collectors 框架 / reporter / buffer / probe_server / main | 子 Agent C | T0 | ✅ **完成**（`eff1d24`） |
| **T2b** | agent 启动期 fail-fast 配置校验 | 子 Agent C | T2 | ✅ **完成**（并入 `eff1d24`） |
| **T3** | agent 高级采集：service_probe / log_watcher / git_tracker / thresholds | 子 Agent D | T2 | ✅ **完成**（`eff1d24`，121 tests） |
| **T3b** | 条件型告警改为「存续期间每轮上报」+ 稳定 title | 子 Agent D | T3 | ✅ **完成**（并入 `eff1d24`） |
| **T4** | M4 交付物：Scriptable 小组件 + deploy/（systemd/install/示例配置） | 子 Agent E | T0 | ✅ **完成**（`ca25f45`） |
| **T4b** | Scriptable mock 回归测试固化进 `tests/widget/` | 子 Agent E | T4 | ✅ **完成**（`ac0170b`，106 断言） |
| **T5** | 端到端集成验证（WSL 全链路 + Bark mock）+ 契约 bug 修复 | 编排会话 | T1–T4 | ✅ **完成**（e2e 30 PASS / 0 FAIL） |
| **T6** | 文档收尾：README / HANDOFF / DEVPLAN 终态 | 编排会话 | T5 | ✅ **完成** |

**最终验收（编排会话实跑，非子 Agent 自述）**
```
cargo fmt --all --check                                exit 0
cargo clippy --workspace --all-targets -- -D warnings   exit 0（零警告）
cargo test --workspace                                  329 passed; 0 failed
    └── ssm-agent 121 / ssm-common 56 / ssm-relay 152
cargo build --release                                   两二进制均产出并 --help 正常
端到端 e2e（T1–T11）                                    PASS = 30   FAIL = 0
tests/widget/run.sh                                     106 断言全过
```

**文件边界**：T1/T2/T4 在 T0 后可**并行**（三者的文件集不相交）。
**为避免 cargo 抢锁**，每个子 Agent 用**已预热且互相独立**的 `CARGO_TARGET_DIR`：
- 子 Agent A / T1 → `$HOME/ssm-target-t0`
- 子 Agent C / T2 / T3 / 编排验收 → `$HOME/ssm-target-verify`

**为避免 git index 串味**，并行阶段**一律由编排会话统一提交**，子 Agent 明确被要求**不执行 `git add`/`git commit`**。
（首次并行时未加此约束，导致 `d44f439` 的 message 只写了 M0 却混入了 T4 的 `deploy/**` 与 `ios/**`；后又因一条 commit message 里的转义引号导致 `git commit` 失败、已暂存的 `common/config.rs` 被卷进 relay 提交。两处都已在 push 前用 `git reset --mixed` + 重做提交修正，工作区经 SHA256 校验未受影响。）


**M0 独立验收证据**（编排会话实跑，非子 Agent 自述）：
```
cargo fmt --all --check                → exit 0
cargo clippy --workspace --all-targets -- -D warnings → exit 0，零警告
cargo test --workspace                 → 44 passed; 0 failed
cargo build --release                  → 产出 ssm-agent / ssm-relay（各 444736 bytes）
ldd ssm-agent                          → 仅 libc / libgcc_s / ld-linux（无 libssl）
```


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
| M0 | 三 crate 编译；协议 serde 往返 + docs 示例 JSON 反序列化测试全绿；release 产出两二进制 | ✅ **已验收**（56 tests，clippy 零警告） |
| M1 | WSL 内 agent→relay 连续心跳（每 30s）；`/status` 返回 `online`；杀 agent 后台账停更 | ✅ **已验收**（e2e T1） |
| M2 | docs/06 §4 的 T1/T2/T3/T4/T9/T11 通过（Bark 用**本地 mock**，不推真机） | ✅ **已验收**（T1/T2/T3/T4/T9/T11 全过） |
| M3 | docs/06 §4 的 T5/T6/T7/T8 通过 | ✅ **已验收**（T5/T6/T7 过；T8 磁盘阈值由单测覆盖，e2e 未强制造盘满） |
| M4 | Scriptable 脚本渲染逻辑走查；systemd unit 过 `systemd-analyze verify` | ✅ **已验收**（mock 106 断言 + unit 校验）；静态二进制部署验证留待 M5 |

**端到端验收脚本**：编排会话独立编写于 `%TEMP%\ssm-e2e.sh`（不入库，属验收工具）。
它从 **SPEC** 而非从实现反推断言，覆盖 T1/T2/T3/T4/T6/T7/T9/T11，用**本地 Bark mock**（`:9200`）避免打扰真机。
其中 **T11 特意设计为「先停 agent 再重启 relay、并删掉 tmpfs 上的 ledger.db」** —— 这样重启后若还能报出
`last_heartbeat`，就只可能来自快照恢复；否则（agent 仍存活）该测试无法区分「快照恢复」与「新心跳」。这是
唯一能真正验证 docs/03 §5「relay 重启不误报」的构造方式。

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
| 起 | 写入 `SPEC.md`（实现契约，含 CLI 契约与 `snapshot_path` 补充）；派发 T0 |
| + | 派发 T4（与 T0 并行，文件集不相交） |
| + | **T0 完成** `d44f439`；**T4 完成** `ca25f45` |
| + | M0 独立验收通过（fmt/clippy/test/build 全绿，`ldd` 无 libssl）；提交并推送 `6c05b97` |
| + | 回填 SPEC §11（真实解析版本 + reqwest 0.13 API 坑 + sysinfo 0.36 提醒）；修正 `Collector` trait 设计缺陷（原设计只有 `Vec<Alert>`，装不下 `services[]`/`git[]`） |
| + | 派发 T1（relay）/ T2（agent 核心）+ T0b（`.gitattributes`）/ T4b（widget 回归测试入库） |
| + | **T1 完成**（140 tests）/ **T2 完成**（49 tests）/ **T3 完成**（117 tests）/ T0b 完成 / T4b 完成 |
| + | 全 workspace 验收通过：**313 tests 全绿**、clippy 零警告、两二进制可构建、CLI 符合 §3 契约 |
| + | **端到端 e2e 首跑**：26 PASS / 1 FAIL。经查明 3 个"失败"是**我自己的验收脚本 bug**（`grep -c \|\| echo 0` 输出 `0\n0` 导致算术语法错误；Bark title 在 URL path 里是 percent-encoded，直接 grep 中文永远匹配不到）——已修脚本并复跑 |
| + | 🔴 **e2e 抓出一个真实的跨 crate 契约 bug**：条件型告警只在"前沿"产出一次，而 relay 把"本轮缺席"当"已恢复"→ 产生**假恢复通知**且此后**永久失声**。已写入 `SPEC.md` §13 并派发 agent 侧 + relay 侧双向修复 |

