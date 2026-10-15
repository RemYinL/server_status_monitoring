#!/usr/bin/env bash
# =============================================================================
# install.sh —— server_status_monitoring 部署脚本（Debian / Ubuntu）
# =============================================================================
# 幂等：重复执行安全，已存在的配置与 secrets 一律不动。
#
# 它做什么：
#   1. 创建系统用户/组   ssm-agent、ssm-relay（无登录 shell、无 home）
#   2. 创建目录          /etc/ssm-{agent,relay}/、/var/lib/ssm-{agent,relay}/
#   3. 安装二进制        优先 musl 静态 target/x86_64-unknown-linux-musl/release/，
#                        回退 target/release/；装到 /usr/local/bin/
#   4. 安装配置          从 .example 拷贝（目标已存在则跳过，权限 644）
#   5. 安装 systemd unit /etc/systemd/system/，daemon-reload + enable
#   6. 检查 secrets      **不生成**，缺失时提示手工创建（权限 600）
#
# 它不做什么：不生成任何密钥、不覆盖任何已有配置、默认不启动服务。
#
# 用法：见 --help
# =============================================================================
set -euo pipefail

# ---------------------------------------------------------------------------
# 常量
# ---------------------------------------------------------------------------
readonly BIN_DIR="/usr/local/bin"
readonly ETC_AGENT="/etc/ssm-agent"
readonly ETC_RELAY="/etc/ssm-relay"
readonly LIB_AGENT="/var/lib/ssm-agent"
readonly LIB_RELAY="/var/lib/ssm-relay"
readonly UNIT_DIR="/etc/systemd/system"
readonly AGENT_USER="ssm-agent"
readonly RELAY_USER="ssm-relay"
readonly JOURNAL_GROUP="systemd-journal"

# 脚本自身所在目录（deploy/），用于找 .example 与 .service
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# ---------------------------------------------------------------------------
# 默认选项
# ---------------------------------------------------------------------------
DO_AGENT=1
DO_RELAY=1
DO_START=0
SRC_DIR=""
MUSL_TARGET="x86_64-unknown-linux-musl"

# ---------------------------------------------------------------------------
# 输出助手（中文提示）
# ---------------------------------------------------------------------------
info()  { printf '\033[1;34m[信息]\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m[完成]\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m[注意]\033[0m %s\n' "$*"; }
err()   { printf '\033[1;31m[错误]\033[0m %s\n' "$*" >&2; }
die()   { err "$*"; exit 1; }

usage() {
  cat <<'EOF'
server_status_monitoring 部署脚本（Debian / Ubuntu）

用法：
  sudo ./install.sh [选项]

选项：
  --agent-only         只安装 ssm-agent
  --relay-only         只安装 ssm-relay
  --start              安装后立即 start（默认只 enable，不 start）
  --restart            安装后 restart 已在运行的服务（升级场景更常用）
  --src-dir <路径>     指定二进制所在目录；默认自动探测：
                         <repo>/target/x86_64-unknown-linux-musl/release   （优先，musl 静态）
                         <repo>/target/release
                         当前工作目录
  --musl-target <三元组>  覆盖 musl 目标三元组（默认 x86_64-unknown-linux-musl，
                         ARM 软路由/电视盒子用 aarch64-unknown-linux-musl）
  -h, --help           显示本帮助

示例：
  # 生产（N100 软路由，x86_64 musl 静态二进制）
  sudo ./install.sh --relay-only --start

  # 被监控服务器
  sudo ./install.sh --agent-only --src-dir ./dist --start

  # ARM 方案（斐讯 N1 / NanoPi）
  sudo ./install.sh --relay-only --musl-target aarch64-unknown-linux-musl --start

安装后必做（脚本不会代劳）：
  1. 手工创建 /etc/ssm-agent/secrets.toml 与 /etc/ssm-relay/secrets.toml（权限 600）
     首次安装时脚本也会把 secrets.toml.example 放到配置目录里当参考。
  2. 按你自己的 host_id / relay URL 改 agent.toml、relay.toml
  3. systemctl restart ssm-agent ssm-relay
EOF
}

# ---------------------------------------------------------------------------
# 解析参数
# ---------------------------------------------------------------------------
parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --agent-only) DO_RELAY=0 ;;
      --relay-only) DO_AGENT=0 ;;
      --start)      DO_START=1 ;;
      --restart)    DO_START=2 ;;
      --src-dir)    shift; [[ $# -gt 0 ]] || die "--src-dir 需要一个路径参数"; SRC_DIR="$1" ;;
      --musl-target) shift; [[ $# -gt 0 ]] || die "--musl-target 需要一个三元组参数"; MUSL_TARGET="$1" ;;
      -h|--help)    usage; exit 0 ;;
      *)            err "未知参数：$1"; echo; usage; exit 1 ;;
    esac
    shift
  done
  [[ $DO_AGENT -eq 1 || $DO_RELAY -eq 1 ]] || die "agent 与 relay 不能同时排除"
}

# ---------------------------------------------------------------------------
# 前置检查
# ---------------------------------------------------------------------------
preflight() {
  [[ "$(id -u)" -eq 0 ]] || die "需要 root 权限，请用 sudo 运行"
  command -v systemctl >/dev/null 2>&1 || die "找不到 systemctl，本脚本只支持 systemd（Debian/Ubuntu）"
  [[ -d "$ETC_AGENT" || -d "$ETC_RELAY" || -d /etc ]] || die "找不到 /etc，系统环境异常"
  info "系统：$( (. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-未知}") || echo 未知 ) / $(uname -m)"
}

# ---------------------------------------------------------------------------
# 1. 用户与组
# ---------------------------------------------------------------------------
ensure_group() {
  local name="$1"
  if getent group "$name" >/dev/null 2>&1; then
    info "用户组 $name 已存在，跳过"
  else
    groupadd --system "$name"
    ok "创建用户组 $name"
  fi
}

ensure_user() {
  local name="$1"
  if id -u "$name" >/dev/null 2>&1; then
    info "用户 $name 已存在，跳过"
  else
    # 系统用户：无 home、无登录 shell
    useradd --system --gid "$name" --no-create-home \
            --home-dir "/nonexistent" --shell /usr/sbin/nologin "$name"
    ok "创建系统用户 $name"
  fi
}

# agent 需要读 journald（collectors/log_watcher.rs 跑 journalctl -u <unit>）
ensure_journal_group() {
  if ! getent group "$JOURNAL_GROUP" >/dev/null 2>&1; then
    warn "找不到用户组 $JOURNAL_GROUP：本机可能没启用 systemd-journald，日志规则将无法读取 journald"
    return 0
  fi
  if id -nG "$AGENT_USER" 2>/dev/null | tr ' ' '\n' | grep -qx "$JOURNAL_GROUP"; then
    info "用户 $AGENT_USER 已在 $JOURNAL_GROUP 组内，跳过"
  else
    usermod -aG "$JOURNAL_GROUP" "$AGENT_USER"
    ok "把 $AGENT_USER 加入 $JOURNAL_GROUP 组（读 journald 所需）"
  fi
}

# ---------------------------------------------------------------------------
# 2. 目录
# ---------------------------------------------------------------------------
ensure_dir() {
  local path="$1" owner="$2" mode="$3"
  if [[ -d "$path" ]]; then
    info "目录 $path 已存在，仅校正属主/权限"
  else
    info "创建目录 $path"
  fi
  install -d -m "$mode" -o "$owner" -g "$owner" "$path"
}

prepare_dirs() {
  if [[ $DO_AGENT -eq 1 ]]; then
    ensure_dir "$ETC_AGENT" root 0755
    ensure_dir "$LIB_AGENT" "$AGENT_USER" 0750
  fi
  if [[ $DO_RELAY -eq 1 ]]; then
    ensure_dir "$ETC_RELAY" root 0755
    ensure_dir "$LIB_RELAY" "$RELAY_USER" 0750
    # /run/ssm-relay 由 ssm-relay.service 的 RuntimeDirectory 自动创建（tmpfs），
    # 这里只提示，不手工建 —— 手工建会被 systemd 覆盖，反而造成误解。
    info "/run/ssm-relay（台账 tmpfs）由 systemd RuntimeDirectory 自动创建，无需手工建"
  fi
}

# ---------------------------------------------------------------------------
# 3. 二进制
# ---------------------------------------------------------------------------
# 打印一个「候选目录列表」，按优先级从高到低
#   --src-dir（用户显式指定，最高优先）
#   <repo>/target/<musl 三元组>/release   ← 优先 musl 静态
#   <repo>/target/release                 ← 回退 glibc 动态
#   deploy/ 与当前目录                     ← 手工 scp 单文件场景
binary_search_dirs() {
  local repo_root
  repo_root="$(cd -- "$SCRIPT_DIR/.." && pwd)"
  [[ -n "$SRC_DIR" ]] && printf '%s\n' "$SRC_DIR"
  printf '%s\n' \
    "$repo_root/target/$MUSL_TARGET/release" \
    "$repo_root/target/release" \
    "$SCRIPT_DIR" \
    "$PWD"
}

# 找二进制：优先 musl 静态，回退 target/release，再回退 --src-dir / 当前目录
find_binary() {
  local name="$1" dir
  while IFS= read -r dir; do
    [[ -n "$dir" ]] || continue
    if [[ -f "$dir/$name" ]]; then
      printf '%s\n' "$dir/$name"
      return 0
    fi
  done < <(binary_search_dirs)
  return 1
}

verify_static() {
  # 尽力而为的静态性检查：file 输出里出现 "statically linked" 更好，
  # 但 musl 二进制也可能显示 "static-pie linked"，所以只是提示不是拦截。
  local path="$1"
  command -v file >/dev/null 2>&1 || return 0
  local desc
  desc="$(file -b "$path" 2>/dev/null || true)"
  if [[ "$desc" == *"statically linked"* || "$desc" == *"static-pie"* ]]; then
    info "  $path 是静态二进制（musl），可跨发行版直接运行"
  else
    warn "  $path 看起来是动态链接（glibc）：目标机 glibc 版本更低时会报 GLIBC_x.xx not found。"
    warn "  建议改用 musl 静态构建：cargo build --release --target $MUSL_TARGET"
  fi
}

install_binaries() {
  local name path
  for name in $( [[ $DO_AGENT -eq 1 ]] && echo ssm-agent ) $( [[ $DO_RELAY -eq 1 ]] && echo ssm-relay ); do
    if ! path="$(find_binary "$name")"; then
      die "找不到二进制 $name。请先在仓库根目录执行：
    cargo build --release --target $MUSL_TARGET
  或用 --src-dir <路径> 指定二进制所在目录。"
    fi
    verify_static "$path"
    install -m 0755 -o root -g root "$path" "$BIN_DIR/$name"
    ok "安装 $BIN_DIR/$name  (来源：$path)"
  done
}

# ---------------------------------------------------------------------------
# 4. 配置
# ---------------------------------------------------------------------------
install_config() {
  local name="$1" src="$2" dst="$3" owner="$4" mode="$5"
  if [[ -e "$dst" ]]; then
    info "配置 $dst 已存在，**不覆盖**（如需重置请手工删除后重跑）"
    return 0
  fi
  [[ -f "$src" ]] || { warn "找不到模板 $src，跳过 $name"; return 0; }
  install -m "$mode" -o "$owner" -g "$owner" "$src" "$dst"
  ok "安装 $name：$dst（$mode，属主 $owner）"
}

install_configs() {
  if [[ $DO_AGENT -eq 1 ]]; then
    install_config "agent.toml" "$SCRIPT_DIR/agent.toml.example" \
                   "$ETC_AGENT/agent.toml" root 0644
    # secrets 模板只作参考；真 secret 必须用户自己填
    install_config "agent secrets 模板" "$SCRIPT_DIR/secrets.toml.example" \
                   "$ETC_AGENT/secrets.toml.example" root 0600
  fi
  if [[ $DO_RELAY -eq 1 ]]; then
    install_config "relay.toml" "$SCRIPT_DIR/relay.toml.example" \
                   "$ETC_RELAY/relay.toml" root 0644
    install_config "relay secrets 模板" "$SCRIPT_DIR/secrets.toml.example" \
                   "$ETC_RELAY/secrets.toml.example" root 0600
  fi
}

# ---------------------------------------------------------------------------
# 5. secrets：只检查、只提示、绝不生成
# ---------------------------------------------------------------------------
# 返回 0 = secrets 已存在；返回 1 = 缺失（调用方负责提示）
check_secrets() {
  local path="$1" owner="$2"
  if [[ -e "$path" ]]; then
    local mode
    mode="$(stat -c '%a' "$path" 2>/dev/null || echo '???')"
    if [[ "$mode" != "600" ]]; then
      warn "$path 权限是 $mode，应为 600，正在修正"
      chmod 0600 "$path"
    fi
    if [[ "$(stat -c '%U' "$path" 2>/dev/null || echo '')" != "$owner" ]]; then
      warn "$path 属主不是 $owner，600 权限下服务将读不到，正在修正"
      chown "$owner:$owner" "$path"
    fi
    ok "$path 已存在（600，属主 $owner）——脚本未改动其内容"
    return 0
  fi
  return 1
}

secrets_hint() {
  cat <<EOF

$(printf '\033[1;33m[注意]\033[0m') 以下 secrets 文件**尚未创建**，脚本按设计不代你生成（避免踩到弱密钥）：
EOF
  local needed=0
  if [[ $DO_AGENT -eq 1 ]] && [[ ! -e "$ETC_AGENT/secrets.toml" ]]; then
    needed=1
    cat <<EOF

  $ETC_AGENT/secrets.toml   需要字段：shared_secret
    sudo install -m 0600 -o $AGENT_USER -g $AGENT_USER \\
         "$ETC_AGENT/secrets.toml.example" "$ETC_AGENT/secrets.toml"
    sudo -e "$ETC_AGENT/secrets.toml"     # 填 openssl rand -hex 32 的输出
EOF
  fi
  if [[ $DO_RELAY -eq 1 ]] && [[ ! -e "$ETC_RELAY/secrets.toml" ]]; then
    needed=1
    cat <<EOF

  $ETC_RELAY/secrets.toml   需要字段：shared_secret、status_token、[bark] endpoint/key/group
    sudo install -m 0600 -o $RELAY_USER -g $RELAY_USER \\
         "$ETC_RELAY/secrets.toml.example" "$ETC_RELAY/secrets.toml"
    sudo -e "$ETC_RELAY/secrets.toml"
EOF
  fi
  if [[ $needed -eq 1 ]]; then
    cat <<EOF

  两端 shared_secret 必须**完全一致**，否则 agent 上报会被 relay 拒（401）。
  生成随机值：openssl rand -hex 32
  注意 600 权限下只有属主可读，所以属主必须是服务用户（见上面的 -o）。
EOF
  fi
}

# ---------------------------------------------------------------------------
# 6. systemd
# ---------------------------------------------------------------------------
install_units() {
  local name units=()
  [[ $DO_AGENT -eq 1 ]] && units+=("ssm-agent.service")
  [[ $DO_RELAY -eq 1 ]] && units+=("ssm-relay.service")

  for name in "${units[@]}"; do
    [[ -f "$SCRIPT_DIR/$name" ]] || die "找不到 unit 文件 $SCRIPT_DIR/$name"
    install -m 0644 -o root -g root "$SCRIPT_DIR/$name" "$UNIT_DIR/$name"
    ok "安装 $UNIT_DIR/$name"
  done

  # unit 的 Documentation=file:///usr/local/share/doc/ssm/README.md 指向这里，
  # 装一份部署手册方便 `systemctl help` / 事后排障时查阅。
  if [[ -f "$SCRIPT_DIR/README.md" ]]; then
    install -d -m 0755 /usr/local/share/doc/ssm
    install -m 0644 -o root -g root "$SCRIPT_DIR/README.md" /usr/local/share/doc/ssm/README.md
    ok "安装部署手册 /usr/local/share/doc/ssm/README.md"
  else
    warn "找不到 $SCRIPT_DIR/README.md，跳过文档安装（不影响服务运行）"
  fi

  systemctl daemon-reload
  ok "systemctl daemon-reload"
}

enable_units() {
  local name units=()
  [[ $DO_AGENT -eq 1 ]] && units+=("ssm-agent.service")
  [[ $DO_RELAY -eq 1 ]] && units+=("ssm-relay.service")

  for name in "${units[@]}"; do
    systemctl enable "$name" >/dev/null
    ok "systemctl enable $name"
  done
}

maybe_start_units() {
  local name units=()
  [[ $DO_AGENT -eq 1 ]] && units+=("ssm-agent.service")
  [[ $DO_RELAY -eq 1 ]] && units+=("ssm-relay.service")

  case "$DO_START" in
    1)
      for name in "${units[@]}"; do
        systemctl start "$name"
        ok "systemctl start $name"
      done
      ;;
    2)
      for name in "${units[@]}"; do
        systemctl restart "$name"
        ok "systemctl restart $name"
      done
      ;;
    *)
      info "未启动服务（按参数默认只 enable）。启动命令：sudo systemctl start ${units[*]}"
      ;;
  esac
}

# ---------------------------------------------------------------------------
# 7. 收尾自检
# ---------------------------------------------------------------------------
summary() {
  echo
  info "===== 安装摘要 ====="
  local name units=()
  [[ $DO_AGENT -eq 1 ]] && units+=("ssm-agent.service")
  [[ $DO_RELAY -eq 1 ]] && units+=("ssm-relay.service")
  for name in "${units[@]}"; do
    printf '  %-20s %s\n' "$name" "$(systemctl is-enabled "$name" 2>/dev/null || echo unknown) / $(systemctl is-active "$name" 2>/dev/null || echo inactive)"
  done
  echo
  info "常用命令："
  echo "  systemctl status ssm-relay"
  echo "  journalctl -u ssm-relay -f"
  echo "  curl -s http://127.0.0.1:9100/healthz"
  echo "  curl -s -H 'Authorization: Bearer <status_token>' http://127.0.0.1:9100/status"
  echo
  warn "本脚本不生成任何密钥，也不修改已有配置。别忘了手工创建 secrets.toml 并改 host_id / relay_url。"
}

# ---------------------------------------------------------------------------
main() {
  parse_args "$@"
  preflight

  echo
  info "===== 1/6 用户与组 ====="
  [[ $DO_AGENT -eq 1 ]] && { ensure_group "$AGENT_USER"; ensure_user "$AGENT_USER"; ensure_journal_group; }
  [[ $DO_RELAY -eq 1 ]] && { ensure_group "$RELAY_USER"; ensure_user "$RELAY_USER"; }

  echo
  info "===== 2/6 目录 ====="
  prepare_dirs

  echo
  info "===== 3/6 二进制 ====="
  install_binaries

  echo
  info "===== 4/6 配置 ====="
  install_configs

  echo
  info "===== 5/6 secrets 检查（不生成）====="
  local missing_agent=0 missing_relay=0
  [[ $DO_AGENT -eq 1 ]] && { check_secrets "$ETC_AGENT/secrets.toml" "$AGENT_USER" || missing_agent=1; }
  [[ $DO_RELAY -eq 1 ]] && { check_secrets "$ETC_RELAY/secrets.toml" "$RELAY_USER" || missing_relay=1; }
  if [[ $missing_agent -eq 1 || $missing_relay -eq 1 ]]; then
    secrets_hint
    warn "secrets 未就绪时服务会启动失败（EACCES / 缺少必填字段），请先补齐再启动。"
  fi

  echo
  info "===== 6/6 systemd ====="
  install_units
  enable_units
  maybe_start_units

  summary
}

main "$@"
