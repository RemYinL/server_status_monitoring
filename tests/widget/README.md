# tests/widget —— Scriptable 小组件脚本的回归测试

针对 [`ios/widget/server_status.js`](../../ios/widget/server_status.js) 的自动化测试。

## 怎么跑

```bash
# 一键（语法 + 渲染行为），退出码 0 = 全通过
bash tests/widget/run.sh

# 只跑渲染行为测试
node tests/widget/render_test.js

# 只做语法检查
node --check ios/widget/server_status.js
```

**`node` 从哪来（重要）**：本项目的 `node` 只装在 **Windows 侧**
（`C:\Program Files\nodejs\node.exe`，v24.11.0），**WSL 里没有 node**。
`run.sh` 会按 `$NODE` → `PATH` 里的 `node`/`node.exe` → `/mnt/c/Program Files/nodejs/node.exe`
的顺序自动探测：

- **在 WSL 里跑**：脚本会自动借用 Windows 的 `node.exe`（实测可用）；
- **在 Windows 侧跑**（PowerShell / Git Bash）：直接用 `node`；
- 也可显式指定：`NODE="/mnt/c/Program Files/nodejs/node.exe" bash tests/widget/run.sh`；
- 想装 Linux 版 node，见 `run.sh` 报错时打印的 `apt` / `nvm` 提示（脚本不依赖任何 npm 包，
  只用 node 内置模块，装个裸 node 就够）。

## 文件

| 文件 | 作用 |
|---|---|
| `scriptable_mock.js` | Scriptable API 的 Node mock：`Request` / `Color` / `Font` / `ListWidget` / `Script` / `config` / `args`。可复用，不含测试用例 |
| `render_test.js` | 13+ 个场景的渲染断言本体，含两条**回归断言**（见下） |
| `run.sh` | 一键入口：先 `node --check`，再跑 `render_test.js`，退出码反映结果 |

## 为什么需要它（`node --check` 不够）

`node --check` 只做**语法解析**，它看不到运行期的链路错误。本项目的真实事故：

> `renderOffline()` 与 `renderUnreachable()` 曾漏写 `return widget;`，
> 于是 `main()` 里的
> ```js
> widget.refreshAfterDate = new Date(Date.now() + 15 * 60 * 1000);
> ```
> 抛 `TypeError: Cannot set properties of undefined`。
>
> 后果非常恶劣：**恰好是「服务器离线」和「连不上 relay」这两条最关键的路径白屏**。
> 平时一切正常，只有真出事时才失效 —— 监控工具最不能犯的错。

`node --check` 对这个 bug 完全无感（两个函数语法都合法）。所以必须把脚本真正跑起来、
对**产出物**做断言。`render_test.js` 的「回归 #1a–#1i」就是为钉死它而存在的：

- 离线路径 / 请求异常路径 / 401 路径：`main()` 不得抛异常；
- 上述三条路径都必须返回**非空 widget 对象**；
- 上述三条路径的 `refreshAfterDate` 必须被成功赋值（这就是原 bug 的触发点）；
- 上述三条路径都必须把 widget 交给 `Script.setWidget`。

删掉任何一个 `return widget;`，这些断言立刻变红（已实测验证，见下）。

## 设计要点

1. **测试加载真实脚本，绝不复制内容。**
   `scriptable_mock.js` 的 `loadWidget()` 读取 `ios/widget/server_status.js`，
   只在**末尾追加一行 `export { main }`**，写到 `tests/widget/.tmp/` 后动态 `import`。
   源文件内容一字不改，所以脚本一改，测试必然跟着改 —— 不存在「测试测的是一份过期拷贝」。
   （为什么不能在脚本里直接 `export`：Scriptable 不支持 ESM 导出语法，加了会粘贴运行失败。）
2. **没有 hardcode 的绝对路径。** 全部用 `import.meta.url` → `__dirname` 等价物 → `path.resolve`，
   换机器、换 clone 目录都能跑。
3. **零依赖。** 只用 `node:fs` / `node:path` / `node:url`，不需要 `npm install`。
4. **断言看「真正交付出去的对象」，不看「new 出来的对象」。**
   这是个真实的坑：一开始回归断言用的是 mock 记录的 `env.lastWidget`，
   而它在 `new ListWidget()` **构造时**就被赋值 —— 哪怕渲染函数忘了 `return`，
   它也照样是非空对象，断言恒真、测了个寂寞（假阳性，本项目踩过）。
   现在断言的是传给 `Script.setWidget()` / `presentSmall()/presentMedium()` 的实参：
   这两个调用点都在 `main()` 的 `refreshAfterDate` 赋值**之后**，
   它能拿到对象 ⟺ 整条链路（渲染函数 return → main 取到 → 赋值成功 → 交付）都走通了。
5. **退出码不靠 `process.exit()` 单打独斗。**
   `render_test.js` 除了设置退出码，还会打印一行机器可读摘要
   `WIDGET_TEST_RESULT {"pass":..,"fail":..,"ok":true/false,..}` 并写一份
   `.tmp/result.json`；`run.sh` 以摘要行为准判定成败。
   这样在原生 Linux node 和 WSL 借用 Windows node.exe 两种环境下都靠得住。

## 运行期产物

`tests/widget/.tmp/` 是运行期目录（临时 `.mjs`、`run.sh` 的日志与 `result.json`），
测试结束会自动清理临时 `.mjs`；日志与结果文件保留下来便于排障。
`scriptable_mock.js` 会尽力把 `tests/widget/.tmp/` 追加到 `.git/info/exclude`（本机私有忽略），
**避免有人 `git add -A` 时把运行期垃圾提交进仓库**。它不改受版本控制的 `.gitignore`，
所以不影响其他人。

## 环境相关的坑（都实测过，省得再踩）

| 坑 | 现象 | 本项目的处理 |
|---|---|---|
| WSL 的 `node` 是 Windows 的 `node.exe` 符号链接 | 传 `/mnt/c/...` 路径会被当成相对路径，报 `Cannot find module 'C:\mnt\c\...'` | `run.sh` 用 `readlink -f` 解析符号链接识别出 Windows node，再 `wslpath -w` 转成 `C:\...` |
| WSL 互操作**不传自定义环境变量** | `FOO=bar node -e 'console.log(process.env.FOO)'` 打印 `undefined` | 调试开关用命令行参数 `DEBUG`，不用环境变量 |
| 同上，也不适合用环境变量传结果文件路径 | 结果文件路径传不进去 | 判据以 stdout 的 `WIDGET_TEST_RESULT` 行为准，结果文件只作冗余校验 |
| 管道输出可能丢 | `node ... \| tee log` 时部分输出缺失 | `run.sh` 改成重定向到文件再 `cat` |
| `.tmp` 目录整体删除会带走日志 | `run.sh` 写完日志后被 mock 的清理逻辑删掉 | mock 只删自己建的临时 `.mjs`，不删目录 |
| PowerShell 会吞 `$?` / `$var` | 在 PowerShell 里用双引号包 `wsl ... 'echo $?'`，`$?` 会被 PowerShell 先展开成 0，导致**误判脚本没在报错** | 从 PowerShell 验证退出码时用反引号转义 `` `$? ``，或干脆看 `run.sh` 自己打印的结论 |

## 断言覆盖的场景

| # | 场景 | 关键断言 |
|---|---|---|
| 1 | `online`（medium） | 三指标百分比、服务红/绿灯、相对时间「最后心跳 2 分钟前」、告警计数、绿底 `#34C759`、`setWidget` 被调 |
| 2 | `suspect` | 黄底 `#FFCC00`（颜色只由 state 决定）、告警为 0 时不显示计数 |
| 3 | `offline` | 红底 `#FF3B30`、大字「离线」（字号 ≥ 24）、「最后心跳 7 分钟前」 |
| 4 | `offline` + `last_heartbeat=null` | 显示「最后心跳 未知」、仍是红底 |
| 5 | `metrics=null` | 显示「尚未收到心跳，无指标」、不渲染百分比、服务灯照常 |
| 6 | `metrics`/`services` 字段整个缺失 | 降级文案、不崩 |
| 7 | HTTP 401 / 403 | 「无法连接 relay」+ TOKEN 无效原因 |
| 8 | HTTP 502 | 「无法连接 relay」+ 状态码 |
| 9 | 请求超时（`loadJSON` 抛异常） | 附超时原因，不崩 |
| 10 | 响应不是合法 JSON | 提示「响应不是合法 JSON」 |
| 11 | `state` 字段缺失 | 判为畸形响应并说明，**不猜成离线** |
| 12 | `state` 是未知值 | 灰色回落，原样展示，不崩 |
| 13 | `runsInWidget=false` | `presentSmall()` / `presentMedium()` 被调，不调 `setWidget` |
| 14 | `widgetFamily` 异常值 | 回落到 `medium` |
| 15 | 认证 | Parameter 优先于 `TOKEN` 常量、两端空格 trim、`Authorization: Bearer <token>` |
| 16 | URL 拼接 | `RELAY_URL + /status`，不出现重复 `/status` |
| 17 | ★**回归 #1** | 离线 / 异常 / 401 / 预览四条路径：不抛异常 + **确实交付了非空 widget** + `refreshAfterDate` 赋值成功 + 内容正确（红底大字「离线」） |
| 18 | 契约核对 | `StatusSnapshot` 七个字段都被渲染消费（docs/03 §4） |

当前基线：**PASS=106  FAIL=0**。

## 有效性怎么证明的（别只信「测试存在」）

把 `ios/widget/server_status.js` 里 `renderOffline()` 的 `return widget;` 注释掉（模拟历史 bug），
跑 `run.sh`：

```
汇总：PASS=91  FAIL=9   ❌ 有失败
  FAIL  交付了非空 widget（offline）   -> 交付对象=null
  FAIL  回归 #1a：离线路径 main() 不抛异常（曾抛 TypeError）   -> Cannot set properties of undefined (setting 'refreshAfterDate')
  FAIL  回归 #1b：离线路径确实交付了非空 widget 给 Script.setWidget   -> setWidget 调用次数=0 实参=undefined
  FAIL  回归 #1c：交付出去的那个 widget 上 refreshAfterDate 已被 main() 成功赋值   -> 无交付对象
  FAIL  回归 #1d：离线内容正确（大字『离线』+ 红底）   -> []
  FAIL  回归 #1j：预览模式下离线路径也交付了非空 widget 给 presentMedium   -> present 调用次数=0
run.sh 退出码 = 1
```

改回来（字节级还原）后：`汇总：PASS=106  FAIL=0  ✅ 全部通过`，`run.sh 退出码 = 0`。

注意其中 **`node --check` 在破坏态依然是「通过」** —— 这正是要写这套测试的理由。

## 改脚本时

**改了 `ios/widget/server_status.js` 就一定要跑一次**：

```bash
bash tests/widget/run.sh
```

涉及 `main()` 入口结构、渲染函数返回值、`config`/`args` 字段名时尤其重要 ——
这些正是语法检查覆盖不到、而这套测试专门盯的地方。
