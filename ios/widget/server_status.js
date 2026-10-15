// =============================================================================
// server_status.js —— Scriptable 小组件：服务器状态（server_status_monitoring）
// =============================================================================
//
// 用途：在 iPhone 主屏 / 锁屏小组件上显示 ssm-relay 的 GET /status 快照。
//       数据契约见 docs/03-protocol.md §4（StatusSnapshot）。
//
// ---------------------------------------------------------------------------
// 【怎么用】三步：
//   1. iPhone 装 Scriptable（App Store 免费）。
//   2. 把本文件全文粘进 Scriptable 新建的脚本里，改好下面「配置区」两个常量。
//      脚本名建议 server_status。
//   3. 回到主屏长按 → 加小组件 → Scriptable → 选 small 或 medium →
//      长按小组件 → 编辑小组件 → Script 选 server_status。
//
// 【配置区怎么填】
//   RELAY_URL：relay 的地址，**不要**带结尾斜杠，也**不要**带 /status。
//      本机调试（Windows 宿主 + WSL relay，见 docs/06 §3 portproxy）：
//          RELAY_URL = "http://192.168.1.23:9100"   ← 换成你 Windows 的局域网 IP
//      生产（N100 软路由上的 relay，走 Tailscale 或内网固定 IP）：
//          RELAY_URL = "http://100.x.y.z:9100"      ← relay 的 Tailscale 100.x 地址
//      脚本实际请求的是 `${RELAY_URL}/status`。
//   TOKEN：relay 的 status_token，必须与 relay 宿主上
//      /etc/ssm-relay/secrets.toml 里的 status_token **完全一致**（同一条字符串）。
//      下发时用的是请求头 `Authorization: Bearer <TOKEN>`。
//
// 【安全提醒】
//   本文件会被 git 提交，**绝对不要把真实 TOKEN 写进这个文件**。
//   建议保持下面的占位符，把真 token 放在 Scriptable 的「参数」(Parameter) 里：
//   编辑小组件 → Parameter 填 token → 脚本自动优先使用 Parameter 里的值。
//   （不给 Parameter 时才回落到 TOKEN 常量。）
//
// 【改完必须跑回归测试】本文件有 mock 渲染测试，改完请跑 `bash tests/widget/run.sh`
//   （语法检查 + 渲染断言；会捕捉「离线/连不上 relay 路径静默崩溃」这类语法检查发现不了的问题）。
//   详见 tests/widget/README.md。
// =============================================================================

// ============================== 配置区 =======================================
const RELAY_URL = "http://192.168.1.23:9100"; // relay 地址，不要结尾斜杠
const TOKEN = "CHANGE_ME_status_token";       // 与 relay secrets.toml 的 status_token 一致
// 最省事的一步：把上面的占位 IP/端口换成你自己的；若改回 "REPLACE_ME"，
// 脚本会直接提示「未配置 RELAY_URL」而不是发一个必然失败的请求。
// ============================================================================

// 请求超时（毫秒）。小组件渲染有系统时间预算，别设太大。
const REQUEST_TIMEOUT_MS = 8000;
// 超过这个秒数没有心跳，就认为「最后心跳」很久了（仅用于文案，不决定颜色）
const STALE_HINT_SEC = 90;

// ---------------------------------------------------------------------------
// 颜色：严格按 docs/03 §4 —— 颜色只由 state 决定
// online=绿 / suspect=黄 / offline=红
// ---------------------------------------------------------------------------
const STATE_COLOR = {
  online: Color.green(),
  suspect: Color.yellow(),
  offline: Color.red(),
};

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

function resolveToken() {
  // 优先用小组件 Parameter（避免把真 token 提交进 git）
  try {
    if (args.widgetParameter && String(args.widgetParameter).trim().length > 0) {
      return String(args.widgetParameter).trim();
    }
  } catch (e) {
    // args.widgetParameter 在非小组件环境下可能不存在，忽略
  }
  return TOKEN;
}

function buildUrl() {
  return String(RELAY_URL).replace(/\/+$/, "") + "/status";
}

/// 抓取 /status，永不抛异常：返回 { ok:true, snapshot } 或 { ok:false, reason }
async function fetchSnapshot() {
  if (!RELAY_URL || RELAY_URL.indexOf("REPLACE_ME") >= 0) {
    return { ok: false, reason: "未配置 RELAY_URL，请编辑脚本顶部配置区" };
  }

  const req = new Request(buildUrl());
  req.method = "GET";
  req.timeoutInterval = REQUEST_TIMEOUT_MS;
  req.headers = {
    Authorization: "Bearer " + resolveToken(),
    Accept: "application/json",
  };

  let json = null;
  try {
    json = await req.loadJSON();
  } catch (e) {
    return { ok: false, reason: "网络不可达或超时（" + briefError(e) + "）" };
  }

  const status = req.response ? req.response.statusCode : 0;
  if (status === 401 || status === 403) {
    return { ok: false, reason: "TOKEN 无效（HTTP " + status + "）" };
  }
  if (status < 200 || status >= 300) {
    return { ok: false, reason: "relay 返回 HTTP " + status };
  }

  // Scriptable 在响应不是 JSON 时返回 null
  if (!json || typeof json !== "object") {
    return { ok: false, reason: "响应不是合法 JSON" };
  }
  if (!json.state) {
    return { ok: false, reason: "响应缺少 state 字段" };
  }
  return { ok: true, snapshot: json };
}

function briefError(e) {
  const msg = e && e.message ? String(e.message) : String(e);
  return msg.length > 60 ? msg.slice(0, 57) + "..." : msg;
}

/// RFC3339 -> Date。解析不出来返回 null（不抛）
function parseDate(value) {
  if (!value || typeof value !== "string") return null;
  const d = new Date(value);
  if (isNaN(d.getTime())) return null;
  return d;
}

/// 相对时间（统一中文）
function relativeTime(date) {
  if (!date) return "未知";
  const sec = Math.floor((Date.now() - date.getTime()) / 1000);
  if (sec < 0) return "刚刚"; // 时钟偏差
  if (sec < 60) return sec + " 秒前";
  const min = Math.floor(sec / 60);
  if (min < 60) return min + " 分钟前";
  const hour = Math.floor(min / 60);
  if (hour < 24) return hour + " 小时前";
  return Math.floor(hour / 24) + " 天前";
}

/// HH:MM:SS（本地时区），异常返回 "--:--:--"
function clockTime(date) {
  if (!date) return "--:--:--";
  const p = (n) => (n < 10 ? "0" + n : String(n));
  return p(date.getHours()) + ":" + p(date.getMinutes()) + ":" + p(date.getSeconds());
}

/// 只显示第一个 token 的简称，避免小组件里名字被截断得太难看
function shortName(name) {
  const s = String(name || "?");
  return s.length <= 14 ? s : s.slice(0, 13) + "…";
}

function pctText(v) {
  if (typeof v !== "number" || !isFinite(v)) return "--";
  return String(Math.round(v * 10) / 10) + "%";
}

function uptimeText(sec) {
  if (typeof sec !== "number" || !isFinite(sec) || sec <= 0) return null;
  const d = Math.floor(sec / 86400);
  const h = Math.floor((sec % 86400) / 3600);
  const m = Math.floor((sec % 3600) / 60);
  if (d > 0) return "已运行 " + d + " 天 " + h + " 小时";
  if (h > 0) return "已运行 " + h + " 小时 " + m + " 分";
  return "已运行 " + m + " 分钟";
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

/// 顶部一行：主机名 + state 徽标（+ 告警数）
function addHeader(widget, hostId, state, activeAlerts, compact) {
  const row = widget.addStack();
  row.centerAlignContent();

  const title = row.addText(hostId || "unknown-host");
  title.font = Font.semiboldSystemFont(compact ? 11 : 13);
  title.lineLimit = 1;
  title.minimumScaleFactor = 0.7;

  row.addSpacer();

  if (activeAlerts > 0) {
    const badge = row.addText("⚠︎" + activeAlerts);
    badge.font = Font.semiboldSystemFont(compact ? 10 : 12);
    badge.textColor = Color.orange();
    row.addSpacer(4);
  }

  const stateText = row.addText(String(state).toUpperCase());
  stateText.font = Font.boldSystemFont(compact ? 10 : 12);
  stateText.textColor = STATE_COLOR[state] || Color.gray();
  return row;
}

/// 服务灯：ok=true 绿点 / false 红点
function addServiceLights(widget, services, compact) {
  const list = Array.isArray(services) ? services : [];
  if (list.length === 0) {
    const t = widget.addText("无服务探针");
    t.font = Font.systemFont(compact ? 9 : 10);
    t.textColor = Color.gray();
    return;
  }
  const row = widget.addStack();
  row.layoutHorizontally();
  row.spacing = compact ? 6 : 8;

  for (const svc of list) {
    const ok = svc && svc.ok === true;
    const item = row.addStack();
    item.layoutHorizontally();
    item.centerAlignContent();
    item.spacing = 2;

    const dot = item.addText("●"); // ● 实心圆
    dot.font = Font.systemFont(compact ? 7 : 8);
    dot.textColor = ok ? Color.green() : Color.red();

    const label = item.addText(shortName(svc && svc.name));
    label.font = Font.systemFont(compact ? 9 : 10);
    label.textColor = ok ? Color.white() : Color.red();
    label.lineLimit = 1;
  }
}

/// medium：CPU / 内存 / 磁盘 三行百分比
function addMetricRows(widget, metrics, compact) {
  if (metrics === null || metrics === undefined) {
    const t = widget.addText("尚未收到心跳，无指标");
    t.font = Font.systemFont(compact ? 9 : 11);
    t.textColor = Color.lightGray();
    return;
  }
  const rows = [
    ["CPU", metrics.cpu_percent],
    ["内存", metrics.mem_percent],
    ["磁盘", metrics.disk_percent],
  ];
  for (const [label, value] of rows) {
    const row = widget.addStack();
    row.centerAlignContent();
    const l = row.addText(label);
    l.font = Font.systemFont(compact ? 9 : 11);
    l.textColor = Color.lightGray();
    row.addSpacer();
    const v = row.addText(pctText(value));
    v.font = Font.semiboldSystemFont(compact ? 11 : 13);
    v.textColor = Color.white();
  }
}

/// small：一行压缩版「CPU 12% 内存 38% 磁盘 42%」
function addCompactMetrics(widget, metrics, compact) {
  if (metrics === null || metrics === undefined) {
    const t = widget.addText("无指标（未收到心跳）");
    t.font = Font.systemFont(compact ? 9 : 10);
    t.textColor = Color.lightGray();
    return;
  }
  const parts = [
    "CPU " + pctText(metrics.cpu_percent),
    "内存 " + pctText(metrics.mem_percent),
    "磁盘 " + pctText(metrics.disk_percent),
  ];
  const t = widget.addText(parts.join("  "));
  t.font = Font.systemFont(compact ? 9 : 10);
  t.textColor = Color.white();
  t.lineLimit = 2;
  t.minimumScaleFactor = 0.7;
}

function addHeartbeatLine(widget, snapshot, compact) {
  const when = parseDate(snapshot.last_heartbeat);
  let text = "最后心跳 " + (when ? relativeTime(when) : "未知");
  const sec = when ? Math.floor((Date.now() - when.getTime()) / 1000) : Infinity;
  const stale = sec > STALE_HINT_SEC;
  if (!when || stale) {
    text += "（" + clockTime(when) + "）";
  }
  const t = widget.addText(text);
  t.font = Font.systemFont(compact ? 9 : 10);
  t.textColor = stale ? Color.orange() : Color.lightGray();
  t.lineLimit = 1;
  t.minimumScaleFactor = 0.7;
}

/// 离线态：红底 + 大字「离线」+「最后心跳 x 分钟前」
function renderOffline(widget, snapshot) {
  widget.backgroundColor = STATE_COLOR.offline;
  widget.setPadding(10, 14, 10, 14);

  const head = widget.addStack();
  head.centerAlignContent();
  const host = head.addText((snapshot && snapshot.host_id) || "unknown-host");
  host.font = Font.semiboldSystemFont(11);
  host.textColor = Color.white();
  host.lineLimit = 1;
  host.minimumScaleFactor = 0.7;

  widget.addSpacer();

  const big = widget.addText("离线");
  big.font = Font.boldSystemFont(30);
  big.textColor = Color.white();
  big.centerAlignText();

  const when = parseDate(snapshot && snapshot.last_heartbeat);
  const sub = widget.addText("最后心跳 " + relativeTime(when));
  sub.font = Font.systemFont(11);
  sub.textColor = Color.white();
  sub.centerAlignText();
  sub.lineLimit = 1;
  sub.minimumScaleFactor = 0.8;

  const when2 = widget.addText(clockTime(when));
  when2.font = Font.systemFont(9);
  when2.textColor = Color.white();
  when2.centerAlignText();

  // 快照生成时间：用来判断 relay 本身是否也停了（快照很旧 = 问题在 relay 侧）
  const gen = parseDate(snapshot && snapshot.generated_at);
  if (gen) {
    const genText = widget.addText("快照 " + clockTime(gen));
    genText.font = Font.systemFont(9);
    genText.textColor = Color.white();
    genText.centerAlignText();
    genText.lineLimit = 1;
  }

  return widget; // 必须返回，main() 还要设置 refreshAfterDate / setWidget
}

/// 异常态：连不上 relay / 401 / 超时 —— 不崩，显示原因
function renderUnreachable(widget, reason, compact) {
  widget.backgroundColor = Color.dynamic(new Color("#3A3A3C"), new Color("#3A3A3C"));
  widget.setPadding(compact ? 8 : 12, compact ? 10 : 14, compact ? 8 : 12, compact ? 10 : 14);

  const head = widget.addStack();
  head.centerAlignContent();
  const title = head.addText("无法连接 relay");
  title.font = Font.boldSystemFont(compact ? 10 : 12);
  title.textColor = Color.white();
  title.lineLimit = 1;
  title.minimumScaleFactor = 0.7;

  widget.addSpacer();

  const r = widget.addText(String(reason || "未知原因"));
  r.font = Font.systemFont(compact ? 9 : 10);
  r.textColor = Color.lightGray();
  r.lineLimit = compact ? 3 : 2;
  r.minimumScaleFactor = 0.7;

  if (!compact) {
    widget.addSpacer();
    const u = widget.addText(buildUrl());
    u.font = Font.systemFont(9);
    u.textColor = Color.gray();
    u.lineLimit = 1;
    u.minimumScaleFactor = 0.6;
  }

  return widget; // 必须返回，main() 还要设置 refreshAfterDate / setWidget
}

function renderSnapshot(snapshot, family) {
  const widget = new ListWidget();
  const compact = family === "small";
  const state = String(snapshot.state || "offline").toLowerCase();

  // 离线态：整块红底 + 大字
  if (state === "offline") {
    return renderOffline(widget, snapshot);
  }

  // 颜色只由 state 决定：online=绿 / suspect=黄 / offline=红
  const stateColor = STATE_COLOR[state] || Color.gray();
  widget.backgroundColor = new Color(
    stateColor.hex,
    state === "online" ? 0.85 : 0.9
  );
  widget.setPadding(compact ? 8 : 12, compact ? 10 : 14, compact ? 8 : 12, compact ? 10 : 14);

  addHeader(
    widget,
    snapshot.host_id,
    state,
    typeof snapshot.active_alerts === "number" ? snapshot.active_alerts : 0,
    compact
  );

  widget.addSpacer(compact ? 4 : 6);

  if (compact) {
    addCompactMetrics(widget, snapshot.metrics, compact);
  } else {
    addMetricRows(widget, snapshot.metrics, compact);
  }

  widget.addSpacer(compact ? 4 : 6);

  addServiceLights(widget, snapshot.services, compact);

  widget.addSpacer();

  addHeartbeatLine(widget, snapshot, compact);

  // 附加信息行（metric 里的 uptime + 告警计数）
  const m = snapshot.metrics;
  const extras = [];
  if (m && typeof m === "object") {
    const up = uptimeText(m.uptime_sec);
    if (up) extras.push(up);
  }
  const alerts = typeof snapshot.active_alerts === "number" ? snapshot.active_alerts : 0;
  if (alerts > 0) extras.push("活跃告警 " + alerts + " 条");
  // generated_at = 快照生成时刻，用来区分「心跳停了」和「小组件数据本身是旧的」
  const gen = parseDate(snapshot.generated_at);
  if (gen) extras.push("快照 " + clockTime(gen));
  if (extras.length > 0) {
    const t = widget.addText(extras.join(" · "));
    t.font = Font.systemFont(compact ? 8 : 9);
    t.textColor = Color.lightGray();
    t.lineLimit = 1;
    t.minimumScaleFactor = 0.7;
  }

  return widget;
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------
async function main() {
  const family =
    config.widgetFamily === "small" || config.widgetFamily === "medium"
      ? config.widgetFamily
      : "medium";

  const result = await fetchSnapshot();
  const widget = result.ok
    ? renderSnapshot(result.snapshot, family)
    : renderUnreachable(new ListWidget(), result.reason, family === "small");

  // 小组件由系统按预算调度，这里手动给一个刷新提示（Scriptable 会按系统策略刷新）
  widget.refreshAfterDate = new Date(Date.now() + 15 * 60 * 1000);

  if (config.runsInWidget) {
    Script.setWidget(widget);
  } else {
    // 在 Scriptable App 内预览
    if (family === "small") {
      await widget.presentSmall();
    } else {
      await widget.presentMedium();
    }
  }
  Script.complete();
}

await main();
