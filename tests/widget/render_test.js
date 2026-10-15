// =============================================================================
// tests/widget/render_test.js —— ios/widget/server_status.js 的渲染回归测试
// =============================================================================
//
// 跑法：node tests/widget/render_test.js   （或 ./tests/widget/run.sh）
// 退出码：0 = 全 PASS；1 = 有 FAIL 或加载失败。
//
// 它加载**真实脚本**（ios/widget/server_status.js），用 Scriptable API 的 mock
// 把它跑起来，然后对渲染出来的 widget 树做断言。
//
// 为什么值得存在（而不是只跑 node --check）：
//   node --check 只做语法解析。它看不到运行期的链路错误 —— 例如
//   renderOffline()/renderUnreachable() 少写一个 `return widget;`，
//   会让 main() 里 `widget.refreshAfterDate = ...` 抛 TypeError，
//   结果就是**唯独在最该工作的两条路径（服务器离线、连不上 relay）上白屏**。
//   本项目就真发生过这个 bug，见下面「回归 #1」。
// =============================================================================

import {
  createEnv,
  setResponse,
  setConfig,
  loadWidget,
  collectText,
  collectNodes,
  findTextNode,
  findTextContaining,
  collectDots,
  WIDGET_PATH,
  HERE,
  DEBUG,
} from "./scriptable_mock.js";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

// DEBUG 开关由 scriptable_mock.js 统一提供（见那里的说明：不能用环境变量，
// 因为 WSL 里的 node 是 Windows 的 node.exe，自定义环境变量传不进去）

// ---------------------------------------------------------------------------
// 迷你测试框架
// ---------------------------------------------------------------------------
let pass = 0;
const failures = [];
let currentScenario = "";

function scenario(title) {
  currentScenario = title;
  console.log(`\n[场景] ${title}`);
}

function check(name, cond, detail) {
  if (cond) {
    pass += 1;
    console.log(`  PASS  ${name}`);
  } else {
    failures.push({ scenario: currentScenario, name, detail });
    console.log(`  FAIL  ${name}${detail === undefined ? "" : "   -> " + detail}`);
  }
}

function eq(name, actual, expected) {
  check(name, actual === expected, `实际=${JSON.stringify(actual)} 期望=${JSON.stringify(expected)}`);
}

function includes(name, haystack, needle) {
  const s = Array.isArray(haystack) ? haystack.join("|") : String(haystack);
  check(name, s.includes(needle), `在 ${JSON.stringify(haystack)} 中找不到 ${JSON.stringify(needle)}`);
}

// ---------------------------------------------------------------------------
// 测试数据
// ---------------------------------------------------------------------------
const nowIso = (offsetSec) => new Date(Date.now() + offsetSec * 1000).toISOString();

/// docs/03 §4 的示例快照（字段严格按文档）
const ONLINE_SNAPSHOT = {
  host_id: "kebei-prod-01",
  state: "online",
  last_heartbeat: nowIso(-120), // 2 分钟前
  metrics: { cpu_percent: 12.4, mem_percent: 38.1, disk_percent: 42.1, uptime_sec: 1234567 },
  services: [
    { name: "web-frontend", ok: true },
    { name: "tailscale", ok: false },
  ],
  active_alerts: 2,
  generated_at: nowIso(0),
};

/// 跑一个场景：渲染完成后返回本次运行的完整证据包。
/// 每个场景都显式重置 config/args，避免上一个场景的尺寸/参数泄漏进来。
///
/// 返回的 `widget` 取的是「真正交付出去的那个对象」：
///   - 桌面模式（runsInWidget=true）：Script.setWidget 的实参
///   - 预览模式：presentSmall/presentMedium 的实参
/// 这比 env.lastWidget 可信 —— 这两个调用点都在 main() 的 type check **之后**，
/// 能走到这里说明 main() 拿到的 widget 是好的（即渲染函数确实 return 了）。
async function render(env, main, label, opts = {}) {
  setResponse(env, opts.response === undefined ? { json: ONLINE_SNAPSHOT, status: 200 } : opts.response);
  setConfig(env, {
    widgetFamily: opts.family === undefined ? "medium" : opts.family,
    runsInWidget: opts.runsInWidget === undefined ? true : opts.runsInWidget,
    widgetParameter: opts.parameter === undefined ? null : opts.parameter,
  });
  env.lastWidget = null;
  env.createdWidgets = [];
  env.setWidgetArgs = [];
  env.presentedArgs = [];
  env.completeCalls = 0;

  let threw = null;
  try {
    await main();
  } catch (e) {
    threw = e;
  }

  const delivered = env.setWidgetArgs[0] || (env.presentedArgs[0] && env.presentedArgs[0].widget) || null;
  const texts = delivered ? collectText(delivered) : [];
  if (threw) {
    console.log(`  （${label} 抛异常了：${threw && threw.message}）`);
  }
  if (DEBUG) {
    console.log(
      `      [debug] ${label}: threw=${!!threw} ` +
        `new出来的widget数=${env.createdWidgets.length} ` +
        `setWidget=${env.setWidgetArgs.length} present=${env.presentedArgs.length} ` +
        `交付对象=${delivered ? "有效" : "null/undefined"}`
    );
  }
  return { widget: delivered, env, texts, threw, error: threw };
}

/// 每个场景通用的两条硬断言：不抛异常 + 确实交付了非空 widget
function checkRendered(res, label) {
  check(`未抛异常（${label}）`, res.threw === null, res.threw ? String(res.threw && res.threw.message) : undefined);
  check(`交付了非空 widget（${label}）`, !!res.widget, `交付对象=${String(res.widget)}`);
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------
async function runAll() {
  console.log("=".repeat(78));
  console.log("server_status.js 渲染回归测试");
  console.log("被测脚本：" + WIDGET_PATH);
  console.log("=".repeat(78));

  const env = createEnv({ widgetFamily: "medium" });
  const { main } = await loadWidget(env);

  // 场景之间会互相污染全局 config/args，所以每个场景开始前显式重置一次
  const resetConfig = (family = "medium", runsInWidget = true, parameter = null) =>
    setConfig(env, { widgetFamily: family, runsInWidget, widgetParameter: parameter });

  // =========================================================================
  scenario("1. online：三指标 + 服务红绿灯 + 相对时间 + 告警计数（medium）");
  // =========================================================================
  {
    const res = await render(env, main, "online medium", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "medium",
      runsInWidget: true,
    });
    checkRendered(res, "online medium");
    const w = res.widget;
    if (w) {
      const t = res.texts;
      includes("host_id 渲染", t, "kebei-prod-01");
      check("state 徽标为 ONLINE", t.includes("ONLINE"), JSON.stringify(t));
      check("CPU 12.4%", t.includes("12.4%"), JSON.stringify(t));
      check("内存 38.1%", t.includes("38.1%"), JSON.stringify(t));
      check("磁盘 42.1%", t.includes("42.1%"), JSON.stringify(t));
      check("CPU/内存/磁盘 三个中文标签都在", ["CPU", "内存", "磁盘"].every((x) => t.includes(x)), JSON.stringify(t));
      includes("服务名 web-frontend", t, "web-frontend");
      includes("服务名 tailscale", t, "tailscale");
      check(
        "『最后心跳 2 分钟前』",
        t.some((s) => s.includes("最后心跳") && s.includes("分钟前")),
        JSON.stringify(t)
      );
      includes("告警计数徽标", t, "2");
      includes("uptime 行", t, "已运行");
      // 颜色只由 state 决定 => online 必须是绿
      includes("背景色为绿 #34C759", String(w.backgroundColor), "#34C759");
      check("背景不是红/黄", !String(w.backgroundColor).includes("#FF3B30") && !String(w.backgroundColor).includes("#FFCC00"), String(w.backgroundColor));
      check("refreshAfterDate 已设置", w.refreshAfterDate instanceof Date, String(w.refreshAfterDate));
      check("在桌面运行 => Script.setWidget 被调用一次", res.env.setWidgetArgs.length === 1, String(res.env.setWidgetArgs.length));
      check("Script.complete 被调用", res.env.completeCalls === 1, String(res.env.completeCalls));
      check("没有走预览分支", w.presented === null, String(w.presented));

      const dots = collectDots(w);
      check("服务灯恰好 2 个", dots.length === 2, `实际 ${dots.length}`);
      includes("第 1 个灯是绿（ok:true）", String(dots[0]), "#34C759");
      includes("第 2 个灯是红（ok:false）", String(dots[1]), "#FF3B30");
    }
  }

  // =========================================================================
  scenario("2. suspect：黄底（颜色只由 state 决定）");
  // =========================================================================
  {
    const res = await render(env, main, "suspect", {
      response: { json: { ...ONLINE_SNAPSHOT, state: "suspect", active_alerts: 0 }, status: 200 },
    });
    checkRendered(res, "suspect");
    if (res.widget) {
      check("state 徽标为 SUSPECT", res.texts.includes("SUSPECT"), JSON.stringify(res.texts));
      includes("背景色为黄 #FFCC00", String(res.widget.backgroundColor), "#FFCC00");
      check("active_alerts=0 时不显示告警计数", !res.texts.some((s) => s.includes("活跃告警")), JSON.stringify(res.texts));
    }
  }

  // =========================================================================
  scenario("3. offline：红底 + 大字「离线」+「最后心跳 x 分钟前」");
  // =========================================================================
  {
    const res = await render(env, main, "offline", {
      response: {
        json: { ...ONLINE_SNAPSHOT, state: "offline", last_heartbeat: nowIso(-7 * 60) },
        status: 200,
      },
    });
    checkRendered(res, "offline");
    if (res.widget) {
      const t = res.texts;
      check("大字『离线』存在", t.includes("离线"), JSON.stringify(t));
      check(
        "『最后心跳 7 分钟前』",
        t.some((s) => s.includes("最后心跳") && s.includes("分钟前")),
        JSON.stringify(t)
      );
      includes("背景为纯红 #FF3B30", String(res.widget.backgroundColor), "#FF3B30");
      const big = findTextNode(res.widget, "离线");
      check("『离线』字号 >= 24（大字）", big && big.font && big.font.size >= 24, big && big.font ? String(big.font.size) : "未找到节点");
    }
  }

  // =========================================================================
  scenario("4. offline + last_heartbeat=null（relay 从未收到心跳就判离线）");
  // =========================================================================
  {
    const res = await render(env, main, "offline-null-hb", {
      response: {
        json: {
          host_id: "kebei-prod-01",
          state: "offline",
          last_heartbeat: null,
          metrics: null,
          services: [],
          active_alerts: 0,
          generated_at: nowIso(0),
        },
        status: 200,
      },
      family: "small",
    });
    checkRendered(res, "offline-null-hb");
    if (res.widget) {
      check(
        "显示『最后心跳 未知』",
        res.texts.some((s) => s.includes("最后心跳") && s.includes("未知")),
        JSON.stringify(res.texts)
      );
      includes("背景仍为红", String(res.widget.backgroundColor), "#FF3B30");
    }
  }

  // =========================================================================
  scenario("5. metrics=null（relay 从未收到心跳）→ 必须判空不崩");
  // =========================================================================
  {
    const res = await render(env, main, "metrics-null", {
      response: { json: { ...ONLINE_SNAPSHOT, metrics: null, active_alerts: 0 }, status: 200 },
      family: "medium",
    });
    checkRendered(res, "metrics-null");
    if (res.widget) {
      check(
        "显示『尚未收到心跳，无指标』",
        res.texts.some((s) => s.includes("尚未收到心跳")),
        JSON.stringify(res.texts)
      );
      check("没有渲染出百分比", !res.texts.some((s) => s.endsWith("%")), JSON.stringify(res.texts));
      check("服务灯仍然照常渲染", collectDots(res.widget).length === 2, String(collectDots(res.widget).length));
    }
  }

  // =========================================================================
  scenario("6. metrics 整个字段缺失 + services 缺失（畸形数据）");
  // =========================================================================
  {
    const res = await render(env, main, "malformed", {
      response: { json: { host_id: "bare-host", state: "online" }, status: 200 },
      family: "small",
    });
    checkRendered(res, "malformed");
    if (res.widget) {
      const t = res.texts;
      check("显示无指标文案", t.some((s) => s.includes("无指标") || s.includes("尚未收到心跳")), JSON.stringify(t));
      includes("提示无服务探针", t, "无服务探针");
      includes("背景仍是绿", String(res.widget.backgroundColor), "#34C759");
    }
  }

  // =========================================================================
  scenario("7. HTTP 401 / 403 → 「无法连接 relay」+ TOKEN 原因");
  // =========================================================================
  {
    const res = await render(env, main, "401", {
      response: { json: { error: "unauthorized" }, status: 401 },
      family: "medium",
    });
    checkRendered(res, "401");
    if (res.widget) {
      check("标题『无法连接 relay』", res.texts.includes("无法连接 relay"), JSON.stringify(res.texts));
      check("原因含 TOKEN / 401", res.texts.some((s) => s.includes("TOKEN") && s.includes("401")), JSON.stringify(res.texts));
    }
    const res403 = await render(env, main, "403", {
      response: { json: {}, status: 403 },
      family: "medium",
    });
    checkRendered(res403, "403");
    if (res403.widget) {
      check("403 也走同一分支", res403.texts.some((s) => s.includes("TOKEN") && s.includes("403")), JSON.stringify(res403.texts));
    }
  }

  // =========================================================================
  scenario("8. HTTP 5xx → 「无法连接 relay」+ 状态码");
  // =========================================================================
  {
    const res = await render(env, main, "502", {
      response: { json: null, status: 502 },
      family: "medium",
    });
    checkRendered(res, "502");
    if (res.widget) {
      check("标题『无法连接 relay』", res.texts.includes("无法连接 relay"), JSON.stringify(res.texts));
      includes("带 HTTP 502", res.texts, "502");
    }
  }

  // =========================================================================
  scenario("9. 请求超时 / 网络不可达（loadJSON 抛异常）→ 附简短原因");
  // =========================================================================
  {
    const res = await render(env, main, "timeout", {
      response: { throwAt: "load", throwMsg: "The request timed out.", status: 0 },
      family: "small",
    });
    checkRendered(res, "timeout");
    if (res.widget) {
      check("标题『无法连接 relay』", res.texts.includes("无法连接 relay"), JSON.stringify(res.texts));
      check(
        "原因含超时信息",
        res.texts.some((s) => s.includes("超时") || s.includes("timed out")),
        JSON.stringify(res.texts)
      );
    }
  }

  // =========================================================================
  scenario("10. 响应不是合法 JSON（loadJSON 返回 null）");
  // =========================================================================
  {
    const res = await render(env, main, "not-json", {
      response: { json: null, status: 200 },
      family: "medium",
    });
    checkRendered(res, "not-json");
    if (res.widget) {
      check("提示『响应不是合法 JSON』", res.texts.some((s) => s.includes("不是合法 JSON")), JSON.stringify(res.texts));
    }
  }

  // =========================================================================
  scenario("11. state 缺失 → 视为畸形响应（不猜成离线）");
  // =========================================================================
  {
    const res = await render(env, main, "no-state", {
      response: { json: { host_id: "h", metrics: null, services: [] }, status: 200 },
      family: "medium",
    });
    checkRendered(res, "no-state");
    if (res.widget) {
      check(
        "展示『响应缺少 state 字段』而不是猜成离线",
        res.texts.some((s) => s.includes("缺少 state")),
        JSON.stringify(res.texts)
      );
      check("没有把 state 缺失当成离线（不应出现大字『离线』）", !res.texts.includes("离线"), JSON.stringify(res.texts));
    }
  }

  // =========================================================================
  scenario("12. state 是未知值 → 灰色回落，不崩");
  // =========================================================================
  {
    const res = await render(env, main, "unknown-state", {
      response: { json: { ...ONLINE_SNAPSHOT, state: "weird" }, status: 200 },
      family: "medium",
    });
    checkRendered(res, "unknown-state");
    if (res.widget) {
      includes("原样展示 WEIRD", res.texts, "WEIRD");
      includes("回落灰色 #8E8E93", String(res.widget.backgroundColor), "#8E8E93");
    }
  }

  // =========================================================================
  scenario("13. App 内预览（runsInWidget=false）→ presentSmall / presentMedium");
  // =========================================================================
  {
    const resSmall = await render(env, main, "preview-small", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "small",
      runsInWidget: false,
    });
    checkRendered(resSmall, "preview-small");
    if (resSmall.widget) {
      eq("small 调用 presentSmall", resSmall.widget.presented, "small");
      check("预览模式不调 Script.setWidget", resSmall.env.setWidgetArgs.length === 0, String(resSmall.env.setWidgetArgs.length));
    }
    const resMedium = await render(env, main, "preview-medium", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "medium",
      runsInWidget: false,
    });
    checkRendered(resMedium, "preview-medium");
    if (resMedium.widget) {
      eq("medium 调用 presentMedium", resMedium.widget.presented, "medium");
    }
  }

  // =========================================================================
  scenario("14. widgetFamily 异常值 → 回落到 medium");
  // =========================================================================
  {
    const res = await render(env, main, "bad-family", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "extraLarge",
      runsInWidget: false,
    });
    checkRendered(res, "bad-family");
    if (res.widget) {
      eq("未知尺寸回落到 medium", res.widget.presented, "medium");
    }
  }

  // =========================================================================
  scenario("15. 认证：Parameter 优先于 TOKEN 常量，且两端空格被 trim");
  // =========================================================================
  {
    await render(env, main, "auth-param", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "medium",
      runsInWidget: true,
      parameter: "  param-token-123  ",
    });
    eq("Bearer 用 Parameter 值并 trim", env.lastRequest.headers.Authorization, "Bearer param-token-123");

    await render(env, main, "auth-fallback", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      parameter: null,
    });
    eq("无 Parameter 时回落 TOKEN 常量", env.lastRequest.headers.Authorization, "Bearer CHANGE_ME_status_token");
    eq("请求方法为 GET", env.lastRequest.method, "GET");
    eq("Accept 头存在", env.lastRequest.headers.Accept, "application/json");
  }

  // =========================================================================
  scenario("16. URL 拼接：RELAY_URL + /status（不带重复斜杠）");
  // =========================================================================
  {
    await render(env, main, "url", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "medium",
    });
    eq("请求 URL 正确", env.lastRequest.url, "http://192.168.1.23:9100/status");
    check("URL 只有一个 /status", (env.lastRequest.url.match(/\/status/g) || []).length === 1, env.lastRequest.url);
  }

  // =========================================================================
  // ★ 回归断言：锁死「离线 / 连不上 relay 两条路径静默崩溃」这个 bug
  // =========================================================================
  scenario("17. ★回归 #1：renderOffline() / renderUnreachable() 必须 return widget");
  {
    // 背景：这两个渲染函数曾漏写 `return widget;`，于是 main() 里
    //   widget.refreshAfterDate = new Date(...)
    // 抛 TypeError（Cannot set properties of undefined），
    // 结果恰好是「服务器离线」和「连不上 relay」这两条最关键的路径白屏 ——
    // 而 node --check 这类语法检查完全发现不了。
    // 下面三条断言把「返回了非空 widget」+「main() 后续赋值不抛异常」钉死。

    // ★为什么这么断言（踩过的坑）：
    //   最初这里写的是 `offline.widget !== null && typeof offline.widget === "object"`，
    //   而 widget 取自 env.lastWidget —— 那是 ListWidget **构造时**登记的，
    //   所以哪怕渲染函数忘了 return（widget 实际是 undefined），它也恒为非空对象，
    //   断言恒真、测了个寂寞（假阳性）。现在改成看「真正交付出去的对象」：
    //   Script.setWidget 的实参只可能在 main() 完成 `widget.refreshAfterDate = ...`
    //   **之后**才被赋值，所以它非空 ⟺ 整条链路（渲染函数 return → main 取到 →
    //   赋值成功 → 交付）都走通了。
    const offline = await render(env, main, "regression-offline", {
      response: {
        json: { ...ONLINE_SNAPSHOT, state: "offline", last_heartbeat: nowIso(-300) },
        status: 200,
      },
      family: "medium",
      runsInWidget: true,
    });
    check(
      "回归 #1a：离线路径 main() 不抛异常（曾抛 TypeError）",
      offline.threw === null,
      offline.threw ? String(offline.threw && offline.threw.message) : undefined
    );
    check(
      "回归 #1b：离线路径确实交付了非空 widget 给 Script.setWidget",
      offline.env.setWidgetArgs.length === 1 && !!offline.env.setWidgetArgs[0],
      `setWidget 调用次数=${offline.env.setWidgetArgs.length} 实参=${String(offline.env.setWidgetArgs[0])}`
    );
    check(
      "回归 #1c：交付出去的那个 widget 上 refreshAfterDate 已被 main() 成功赋值",
      !!(offline.env.setWidgetArgs[0] && offline.env.setWidgetArgs[0].refreshAfterDate instanceof Date),
      offline.env.setWidgetArgs[0] ? String(offline.env.setWidgetArgs[0].refreshAfterDate) : "无交付对象"
    );
    check(
      "回归 #1d：离线内容正确（大字『离线』+ 红底）",
      offline.texts.includes("离线") && String(offline.widget && offline.widget.backgroundColor).includes("#FF3B30"),
      JSON.stringify(offline.texts)
    );

    const unreachable = await render(env, main, "regression-unreachable", {
      response: { throwAt: "load", throwMsg: "The request timed out.", status: 0 },
      family: "medium",
      runsInWidget: true,
    });
    check(
      "回归 #1e：异常路径 main() 不抛异常（曾抛 TypeError）",
      unreachable.threw === null,
      unreachable.threw ? String(unreachable.threw && unreachable.threw.message) : undefined
    );
    check(
      "回归 #1f：异常路径确实交付了非空 widget 给 Script.setWidget",
      unreachable.env.setWidgetArgs.length === 1 && !!unreachable.env.setWidgetArgs[0],
      `setWidget 调用次数=${unreachable.env.setWidgetArgs.length} 实参=${String(unreachable.env.setWidgetArgs[0])}`
    );
    check(
      "回归 #1g：异常路径交付对象的 refreshAfterDate 已被成功赋值",
      !!(unreachable.env.setWidgetArgs[0] && unreachable.env.setWidgetArgs[0].refreshAfterDate instanceof Date),
      unreachable.env.setWidgetArgs[0] ? String(unreachable.env.setWidgetArgs[0].refreshAfterDate) : "无交付对象"
    );
    check(
      "回归 #1h：异常路径文案为『无法连接 relay』",
      unreachable.texts.includes("无法连接 relay"),
      JSON.stringify(unreachable.texts)
    );

    // 401 也属于「异常路径」，同样钉一遍
    const unauthorized = await render(env, main, "regression-401", {
      response: { json: { error: "nope" }, status: 401 },
      family: "small",
      runsInWidget: true,
    });
    check(
      "回归 #1i：401 路径不抛异常且交付了非空 widget",
      unauthorized.threw === null && unauthorized.env.setWidgetArgs.length === 1 && !!unauthorized.env.setWidgetArgs[0],
      unauthorized.threw ? String(unauthorized.threw.message) : `实参=${String(unauthorized.env.setWidgetArgs[0])}`
    );

    // 预览路径（presentSmall/presentMedium）同样要求交付非空对象
    const previewReg = await render(env, main, "regression-preview", {
      response: { json: { ...ONLINE_SNAPSHOT, state: "offline" }, status: 200 },
      family: "medium",
      runsInWidget: false,
    });
    check(
      "回归 #1j：预览模式下离线路径也交付了非空 widget 给 presentMedium",
      previewReg.threw === null && previewReg.env.presentedArgs.length === 1 && !!previewReg.env.presentedArgs[0].widget,
      `present 调用次数=${previewReg.env.presentedArgs.length}`
    );
  }

  // =========================================================================
  scenario("18. 契约核对：StatusSnapshot 的字段都被消费（docs/03 §4）");
  // =========================================================================
  {
    const res = await render(env, main, "contract", {
      response: { json: ONLINE_SNAPSHOT, status: 200 },
      family: "medium",
    });
    const t = res.texts.join("|");
    for (const field of ["host_id", "state", "last_heartbeat", "metrics", "services", "active_alerts", "generated_at"]) {
      const consumed =
        field === "host_id"
          ? t.includes("kebei-prod-01")
          : field === "state"
            ? t.includes("ONLINE")
            : field === "last_heartbeat"
              ? t.includes("最后心跳")
              : field === "metrics"
                ? t.includes("12.4%")
                : field === "services"
                  ? t.includes("web-frontend")
                  : field === "active_alerts"
                    ? t.includes("活跃告警")
                    : t.includes("快照");
      check(`快照字段 ${field} 被消费`, consumed, t);
    }
  }

  // =========================================================================
  // 汇总
  // =========================================================================
  console.log("\n" + "=".repeat(78));
  if (failures.length === 0) {
    console.log("汇总：PASS=" + pass + "  FAIL=0   ✅ 全部通过");
    console.log("=".repeat(78));
    return 0;
  }
  console.log("汇总：PASS=" + pass + "  FAIL=" + failures.length + "   ❌ 有失败");
  console.log("-".repeat(78));
  for (const f of failures) {
    console.log(`  [${f.scenario}] ${f.name}${f.detail === undefined ? "" : "\n      " + f.detail}`);
  }
  console.log("=".repeat(78));
  return 1;
}

// ---------------------------------------------------------------------------
// 结果落盘：给 run.sh / CI 一个**不依赖退出码**的判据
// ---------------------------------------------------------------------------
// 原因：WSL 里的 `node` 是 Windows 的 node.exe，经 WSL 互操作调用时
// `process.exit(1)` 会被吞掉，进程退出码恒为 0（实测：`node -e "process.exit(1)"; echo $?`
// 输出 0）。所以除了设置退出码之外，这里再把结果写进文件 + 打印一行机器可读摘要，
// run.sh 以文件为准判断成败 —— 这样在原生 Linux node 和 WSL 借用 node.exe 两种
// 环境下，run.sh 的退出码都靠得住。
const RESULT_FILE = process.env.WIDGET_TEST_RESULT_FILE || join(HERE, ".tmp", "result.json");

function writeResult(passCount, failCount, extra) {
  const payload = {
    pass: passCount,
    fail: failCount,
    ok: failCount === 0,
    widget: WIDGET_PATH,
    ...(extra || {}),
  };
  try {
    mkdirSync(dirname(RESULT_FILE), { recursive: true });
    writeFileSync(RESULT_FILE, JSON.stringify(payload, null, 2), "utf8");
  } catch (e) {
    console.error("（写结果文件失败，忽略：" + (e && e.message) + "）");
  }
  // 机器可读摘要：run.sh 在拿不到结果文件时会解析这一行
  console.log("WIDGET_TEST_RESULT " + JSON.stringify(payload));
  return payload;
}

let code = 1;
try {
  code = await runAll();
  writeResult(pass, failures.length);
} catch (e) {
  console.error("\n测试框架自身出错：");
  console.error(e);
  writeResult(pass, failures.length + 1, { frameworkError: String((e && e.message) || e) });
  code = 1;
}
process.exit(code);
