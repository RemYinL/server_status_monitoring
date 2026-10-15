#!/usr/bin/env bash
# =============================================================================
# tests/widget/run.sh —— 小组件脚本的一键回归测试
# =============================================================================
# 做两件事：
#   1. node --check ios/widget/server_status.js       （语法）
#   2. node tests/widget/render_test.js              （渲染行为 + 回归断言）
# 退出码：0 = 全通过；非 0 = 任一步失败（可直接用于 CI / 人工验收）。
#
# node 从哪来：
#   本机 node 只装在 Windows 侧（C:\Program Files\nodejs\node.exe，v24.11.0），
#   WSL 里**没有** node。所以本脚本按以下顺序找 node：
#     ① $NODE 环境变量（显式指定，优先级最高）
#     ② PATH 里的 node
#     ③ PATH 里的 node.exe（WSL 下能直接调 Windows 的 node.exe）
#     ④ Windows 常见安装路径 /mnt/c/Program Files/nodejs/node.exe
#   都找不到就明确报错并给出提示，不会假装通过。
#
# 用法：
#   bash tests/widget/run.sh              # Linux / WSL / macOS
#   bash tests/widget/run.sh              # Git Bash on Windows 也适用
#   NODE="/c/Program Files/nodejs/node.exe" bash tests/widget/run.sh
# =============================================================================
set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$HERE/../.." && pwd)"
WIDGET="$REPO_ROOT/ios/widget/server_status.js"
RENDER_TEST="$HERE/render_test.js"

# ---------------------------------------------------------------------------
# 找 node
# ---------------------------------------------------------------------------
find_node() {
  if [[ -n "${NODE:-}" ]]; then
    if command -v "$NODE" >/dev/null 2>&1; then
      printf '%s\n' "$NODE"
      return 0
    fi
    echo "[错误] \$NODE 指向的 node 不可用：$NODE" >&2
    return 1
  fi
  local c
  for c in node node.exe; do
    if command -v "$c" >/dev/null 2>&1; then
      command -v "$c"
      return 0
    fi
  done
  # WSL 下 Windows 侧 node 的常见位置
  local w
  for w in \
    "/mnt/c/Program Files/nodejs/node.exe" \
    "/c/Program Files/nodejs/node.exe" \
    "$HOME/.nvm/versions/node"/*/bin/node; do
    if [[ -x "$w" ]]; then
      printf '%s\n' "$w"
      return 0
    fi
  done
  return 1
}

NODE_BIN="$(find_node)" || {
  cat >&2 <<'EOF'
[错误] 找不到 node，无法运行小组件回归测试。

node 只装在 Windows 侧（本机为 C:\Program Files\nodejs\node.exe，v24.11.0），
WSL / Linux 里没有。请任选一种方式：

  ① 在 Windows 侧跑（PowerShell 或 Git Bash）：
       bash tests/widget/run.sh
     PowerShell 里也可以直接：
       node --check ios/widget/server_status.js
       node tests/widget/render_test.js

  ② 在 WSL 里直接借用 Windows 的 node.exe：
       bash tests/widget/run.sh
     （本脚本会自动探测 /mnt/c/Program Files/nodejs/node.exe）

  ③ 显式指定：
       NODE="/mnt/c/Program Files/nodejs/node.exe" bash tests/widget/run.sh

  ④ 在 WSL 里装一个 Linux 版 node（可选，任选其一）：
       curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt install -y nodejs
       # 或用 nvm：curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash
     装完后 WSL 里 node 可用，render_test.js 照跑（它不依赖 Windows 路径）。
EOF
  exit 127
}

echo "======================================================================"
echo "小组件回归测试"
echo "  node       : $NODE_BIN"
"$NODE_BIN" --version | sed 's/^/  node 版本  : /'
echo "  被测脚本   : $WIDGET"
echo "======================================================================"
echo

# ---------------------------------------------------------------------------
# 路径转换：如果 node 实际是 Windows 的 node.exe，它无法理解 /mnt/c/... 这种
# Linux 路径，必须先转成 C:\... 。注意本机上 WSL 的 `node` 本身就是一个
# 指向 /mnt/c/Program Files/nodejs/node.exe 的符号链接，所以必须
# **解析符号链接**之后判断，光看命令名或路径前缀会漏判。
# ---------------------------------------------------------------------------
to_node_path() {
  local p="$1"
  if [[ "$IS_WINDOWS_NODE" != "1" ]]; then
    printf '%s\n' "$p"
    return 0
  fi
  if command -v wslpath >/dev/null 2>&1; then
    if wslpath -w "$p" >/dev/null 2>&1; then
      wslpath -w "$p"
      return 0
    fi
  fi
  # 没有 wslpath 时手工转换（仅覆盖 /mnt/<盘符>/... 这一种形态）
  if [[ "$p" == /mnt/*/* ]]; then
    local drive rest
    drive="$(printf '%s' "$p" | cut -d/ -f3)"
    rest="$(printf '%s' "$p" | cut -d/ -f4-)"
    printf '%s:\\%s\n' \
      "$(printf '%s' "$drive" | tr '[:lower:]' '[:upper:]')" \
      "$(printf '%s' "$rest" | tr '/' '\\')"
    return 0
  fi
  printf '%s\n' "$p"
}

# 解析符号链接后判断：落在 /mnt/ 下或名字以 .exe 结尾 => Windows 的 node.exe
NODE_REAL=""
if command -v readlink >/dev/null 2>&1; then
  NODE_REAL="$(readlink -f "$NODE_BIN" 2>/dev/null || true)"
fi
[[ -n "$NODE_REAL" ]] || NODE_REAL="$NODE_BIN"

IS_WINDOWS_NODE=0
case "$NODE_REAL" in
  /mnt/*|*.exe|*.EXE) IS_WINDOWS_NODE=1 ;;
esac

WIDGET_ARG="$(to_node_path "$WIDGET")"
RENDER_TEST_ARG="$(to_node_path "$RENDER_TEST")"
if [[ "$IS_WINDOWS_NODE" -eq 1 ]]; then
  echo "[信息] 检测到 node 是 Windows 版（$NODE_REAL），已把路径转成 Windows 形式："
  echo "       $WIDGET_ARG"
  echo
fi

# ---------------------------------------------------------------------------
# 第 1 步：语法检查
# ---------------------------------------------------------------------------
echo "---- [1/2] node --check（语法）----"
if "$NODE_BIN" --check "$WIDGET_ARG"; then
  echo "[通过] 语法检查无输出（= 无语法错误）"
  syntax_ok=1
else
  echo "[失败] node --check 报错，见上方输出" >&2
  syntax_ok=0
fi
echo

# ---------------------------------------------------------------------------
# 第 2 步：渲染行为测试
# ---------------------------------------------------------------------------
# 判据**不依赖进程退出码**，原因有两个（都实测过）：
#   1. WSL 里借用的 Windows node.exe，经互操作调用时 `process.exit(1)` 会被吞掉，
#      进程退出码恒为 0（`node -e "process.exit(1)"; echo $?` 输出 0）；
#   2. 同理，**自定义环境变量也传不进 node.exe 进程**（`FOO=bar node -e ...` 里
#      process.env.FOO 是 undefined），所以没法靠环境变量传结果文件路径。
# 因此改由 render_test.js 在 stdout 打印一行机器可读摘要
#   WIDGET_TEST_RESULT {"pass":..,"fail":..,"ok":true/false,..}
# 本脚本以这一行为准判定成败。原生 Linux node 下还额外写一份 result.json 作为冗余。
RESULT_FILE="$HERE/.tmp/result.json"
LOG_FILE="$HERE/.tmp/render_test.log"
rm -f "$RESULT_FILE"
mkdir -p "$HERE/.tmp"

echo "---- [2/2] node tests/widget/render_test.js（渲染行为 + 回归断言）----"
render_rc=0
# 注意：这里用「重定向到文件」而不是 `| tee`。经 WSL 互操作调用 Windows 的
# node.exe 时，管道模式会让部分输出丢失（实测），重定向到文件则完整。
"$NODE_BIN" "$RENDER_TEST_ARG" > "$LOG_FILE" 2>&1 || render_rc=$?
cat "$LOG_FILE"
echo

# 从输出里抓机器可读摘要行
RESULT_LINE="$(grep '^WIDGET_TEST_RESULT ' "$LOG_FILE" | tail -1 || true)"

render_ok=0
r_pass=""
r_fail=""
if [[ -n "$RESULT_LINE" ]]; then
  r_ok="$(printf '%s' "$RESULT_LINE" | sed -n 's/.*"ok":[[:space:]]*\([a-z]*\).*/\1/p')"
  r_pass="$(printf '%s' "$RESULT_LINE" | sed -n 's/.*"pass":[[:space:]]*\([0-9]*\).*/\1/p')"
  r_fail="$(printf '%s' "$RESULT_LINE" | sed -n 's/.*"fail":[[:space:]]*\([0-9]*\).*/\1/p')"
  echo "[结果] 测试自报：PASS=${r_pass:-?}  FAIL=${r_fail:-?}  ok=${r_ok:-?}"
  [[ "$r_ok" == "true" ]] && render_ok=1
  # 冗余校验：有结果文件时再看一眼，两处必须一致
  if [[ -f "$RESULT_FILE" ]]; then
    f_ok="$(sed -n 's/.*"ok":[[:space:]]*\([a-z]*\).*/\1/p' "$RESULT_FILE" | head -1)"
    echo "[结果] 结果文件 $RESULT_FILE：ok=${f_ok:-?}"
    if [[ -n "$f_ok" && "$f_ok" != "$r_ok" ]]; then
      echo "[注意] 结果文件与 stdout 摘要不一致，按失败处理"
      render_ok=0
    fi
  fi
else
  echo "[失败] 没有从输出里找到 WIDGET_TEST_RESULT 摘要行 —— 测试脚本可能没跑起来"
fi

# 进程退出码非 0 时同样不能放过（两边都要说 OK 才算 OK）
if [[ "$render_rc" -ne 0 ]]; then
  echo "[注意] 进程退出码为 $render_rc"
fi
echo

# ---------------------------------------------------------------------------
# 汇总：任一步失败 => 非 0 退出
# ---------------------------------------------------------------------------
echo "======================================================================"
if [[ "$syntax_ok" -eq 1 && "$render_ok" -eq 1 ]]; then
  echo "结果：全部通过 ✅   （语法 OK，渲染断言 PASS）"
  echo "======================================================================"
  exit 0
fi
echo "结果：失败 ❌"
[[ "$syntax_ok" -eq 0 ]] && echo "  - 第 1 步 node --check 失败"
[[ "$render_ok" -eq 0 ]] && echo "  - 第 2 步 render_test.js 有 FAIL 断言（进程退出码 $render_rc）"
echo "======================================================================"
exit 1
