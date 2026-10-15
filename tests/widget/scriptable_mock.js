// =============================================================================
// tests/widget/scriptable_mock.js —— Scriptable API 的 Node mock（可复用）
// =============================================================================
//
// 目的：在 Node 里复现 Scriptable 运行小组件脚本时提供的那些**全局对象**，
//       让 ios/widget/server_status.js 能以真实代码在 CI / 命令行里跑起来，
//       从而对渲染结果做断言 —— 这是 `node --check` 做不到的（语法检查看不到
//       「离线路径漏 return 导致运行期 TypeError 白屏」这类问题）。
//
// 覆盖的全局：Request / Color / Font / ListWidget / Script / config / args
//
// 用法（ESM）：
//   import { createEnv, loadWidget, collectText, collectByText } from "./scriptable_mock.js";
//
// 设计原则：
//   1. 不依赖任何第三方包，只用 node: 内置模块（CI 里 npm i 都没有也能跑）。
//   2. 只 mock 小组件脚本真正用到的 API，不追求完整复刻 Scriptable。
//   3. mock 出来的节点与真实 Scriptable 一样是**链式**的（addText 返回文本节点，
//      addStack 返回栈节点），这样脚本里的 `t.font = ...` 才不会炸。
//   4. 所有「环境事实」都挂在一个 env 对象上，测试用例之间互不污染。
// =============================================================================

import { readFileSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// tests/widget/ 的绝对路径（不 hardcode，换机器/换路径都能跑）
export const HERE = dirname(fileURLToPath(import.meta.url));
// 仓库根目录
export const REPO_ROOT = resolve(HERE, "..", "..");
// 被测脚本：**始终加载真实文件**，绝不复制内容进测试
export const WIDGET_PATH = join(REPO_ROOT, "ios", "widget", "server_status.js");

/// 调试开关：命令行带 `DEBUG` 参数时打开（`node tests/widget/render_test.js DEBUG`）。
/// 注意：不能用环境变量 —— WSL 里的 `node` 是 Windows 的 node.exe，
/// 自定义环境变量不会透过 WSL 互操作传进 Windows 进程（实测被吞掉）。
export const DEBUG = process.argv.includes("DEBUG");

// ---------------------------------------------------------------------------
// Scriptable 节点 mock
// ---------------------------------------------------------------------------

/// 文本节点。Scriptable 的 addText 返回的对象可以设置 font/textColor/lineLimit
export class MockText {
  constructor(text) {
    this.text = String(text);
    this.font = null;
    this.textColor = null;
    this.lineLimit = null;
    this.minimumScaleFactor = null;
    this.__isText = true;
  }
  centerAlignText() {
    this.centered = true;
    return this;
  }
  leftAlignText() {
    return this;
  }
  rightAlignText() {
    return this;
  }
  setText(v) {
    this.text = String(v);
    return this;
  }
}

/// 栈节点（也是 widget 的基类）
export class MockStack {
  constructor(kind = "stack") {
    this.__kind = kind;
    this.children = [];
    this.spacing = 0;
  }
  addStack() {
    const s = new MockStack("stack");
    this.children.push(s);
    return s;
  }
  addText(text) {
    const t = new MockText(text);
    this.children.push(t);
    return t;
  }
  /// 真实 Scriptable 的 addSpacer() 无参也可用
  addSpacer(size) {
    this.children.push({ __isSpacer: true, size: size || 0 });
    return this;
  }
  addImage() {
    const img = { __isImage: true };
    this.children.push(img);
    return img;
  }
  centerAlignContent() {
    this.contentAlignment = "center";
    return this;
  }
  topAlignContent() {
    return this;
  }
  layoutHorizontally() {
    this.layout = "horizontal";
    return this;
  }
  layoutVertically() {
    this.layout = "vertical";
    return this;
  }
  setPadding(...args) {
    this.padding = args;
    return this;
  }
}

/// 小组件节点。构造时把自己登记到 env.lastWidget（仅作调试用）。
///
/// ⚠️ 注意：`env.lastWidget` **不能**用来断言「渲染函数返回了 widget」——
/// 只要 `new ListWidget()` 被求值过它就有值，哪怕渲染函数忘了 return，
/// 用它做断言会得到恒真的假阳性（本项目踩过这个坑，见 render_test.js 回归 #1 的注释）。
/// 要断言「确实产出了 widget」，请看 `env.setWidgetArgs` / `env.presentedArgs`，
/// 它们的赋值发生在 main() 的 type check 之后，是可信的证据。
export class MockWidget extends MockStack {
  constructor(env) {
    super("widget");
    this.backgroundColor = null;
    this.refreshAfterDate = null;
    this.presented = null;
    this.__env = env;
    if (env) {
      env.lastWidget = this;
      env.createdWidgets.push(this);
    }
  }
  async presentSmall() {
    this.presented = "small";
    if (this.__env) this.__env.presentedArgs.push({ family: "small", widget: this });
  }
  async presentMedium() {
    this.presented = "medium";
    if (this.__env) this.__env.presentedArgs.push({ family: "medium", widget: this });
  }
  async presentLarge() {
    this.presented = "large";
    if (this.__env) this.__env.presentedArgs.push({ family: "large", widget: this });
  }
  async presentAccessory() {
    this.presented = "accessory";
    if (this.__env) this.__env.presentedArgs.push({ family: "accessory", widget: this });
  }
}

/// 颜色。真实 Scriptable 里 Color 既能 new（new Color("#fff", 0.5)），
/// 也有 Color.green() / Color.dynamic() 这类静态方法 —— 两者都要支持。
export class MockColor {
  constructor(hex, alpha) {
    this.hex = hex === undefined ? undefined : String(hex);
    this.alpha = alpha;
    this.__isColor = true;
  }
  toString() {
    return `Color(${this.hex},${this.alpha})`;
  }
  static green() {
    return new MockColor("#34C759");
  }
  static yellow() {
    return new MockColor("#FFCC00");
  }
  static red() {
    return new MockColor("#FF3B30");
  }
  static orange() {
    return new MockColor("#FF9500");
  }
  static blue() {
    return new MockColor("#007AFF");
  }
  static gray() {
    return new MockColor("#8E8E93");
  }
  static lightGray() {
    return new MockColor("#D1D1D6");
  }
  static darkGray() {
    return new MockColor("#3A3A3C");
  }
  static white() {
    return new MockColor("#FFFFFF");
  }
  static black() {
    return new MockColor("#000000");
  }
  static dynamic(light, dark) {
    const l = light && light.hex ? light.hex : String(light);
    const d = dark && dark.hex ? dark.hex : String(dark);
    return new MockColor(`dynamic(${l},${d})`);
  }
}

/// 字体：只记录字号，测试用它断言排版没有离谱
export class MockFont {
  constructor(size) {
    this.size = size;
  }
  static systemFont(n) {
    return new MockFont(n);
  }
  static semiboldSystemFont(n) {
    return new MockFont(n);
  }
  static boldSystemFont(n) {
    return new MockFont(n);
  }
  static heavySystemFont(n) {
    return new MockFont(n);
  }
  static italicSystemFont(n) {
    return new MockFont(n);
  }
  static monospacedSystemFont(n) {
    return new MockFont(n);
  }
}

// ---------------------------------------------------------------------------
// 环境工厂
// ---------------------------------------------------------------------------

/**
 * 建一个干净的 mock 环境，并把 Scriptable 的全局对象挂到 globalThis 上。
 *
 * @param {object} [opts]
 * @param {object|null} [opts.response] 下一次请求的响应：
 *        { json, status, throwAt, throwMsg }
 *        - status：HTTP 状态码（进 req.response.statusCode）
 *        - json：要「解析」出的对象；null 模拟 Scriptable 解析不出 JSON 的行为
 *        - throwAt: "load" 让 loadJSON() 直接抛异常（模拟超时/网络不可达）
 * @param {string} [opts.widgetFamily] "small" | "medium" | ...
 * @param {boolean} [opts.runsInWidget] true = 在桌面运行；false = 在 App 内预览
 * @param {string|null} [opts.widgetParameter] 小组件 Parameter（可放 token）
 */
export function createEnv(opts = {}) {
  const env = {
    // 下一次请求会返回什么
    response: opts.response === undefined ? { json: null, status: 200 } : opts.response,
    // 最近一次请求的元信息，供热 token / URL 类断言使用
    lastRequest: null,
    // 最近一次「被 new 出来」的 widget（**仅供调试，不要拿它做断言**，见 MockWidget 注释）
    lastWidget: null,
    // 本次渲染中 new 出来的所有 widget
    createdWidgets: [],
    // Script.setWidget 收到的对象（**可信证据**：赋值发生在 main() 的 type check 之后）
    setWidgetArgs: [],
    // presentSmall/Medium 收到的对象（预览路径的可信证据）
    presentedArgs: [],
    completeCalls: 0,
  };

  class MockRequest {
    constructor(url) {
      this.url = String(url);
      this.method = "GET";
      this.headers = {};
      this.timeoutInterval = null;
      this.response = null;
      this.allowInsecureRequest = false;
      env.lastRequest = this;
    }
    async loadJSON() {
      const r = env.response;
      // 先设 response 再决定抛不抛，更贴近真实：超时也已经有 response 对象了
      this.response = { statusCode: r.status === undefined ? 200 : r.status };
      if (r.throwAt === "load") {
        throw new Error(r.throwMsg || "The request timed out.");
      }
      if (r.throwAt === "parse") {
        throw new Error("JSON Parse error");
      }
      return r.json === undefined ? null : r.json;
    }
    async loadString() {
      return JSON.stringify(env.response.json);
    }
  }

  globalThis.Request = MockRequest;
  globalThis.Color = MockColor;
  globalThis.Font = MockFont;
  globalThis.ListWidget = class ListWidget extends MockWidget {
    constructor() {
      super(env);
    }
  };
  globalThis.Script = {
    setWidget: (w) => {
      env.setWidgetArgs.push(w);
      // 调试用：由 render_test.js 的 DEBUG 环境变量开关
      if (DEBUG) {
        console.log(
          `      [mock] Script.setWidget(#${env.setWidgetArgs.length}) ` +
            `isNull=${w === null || w === undefined} sameAsLastWidget=${w === env.lastWidget}`
        );
      }
    },
    complete: () => {
      env.completeCalls += 1;
    },
  };
  globalThis.config = {
    // 注意：Scriptable 的字段名是 widgetFamily，不是 family —— 不接受简写，
    // 免得测试里写错键名却静默退化成 medium，测了个寂寞。
    widgetFamily: opts.widgetFamily === undefined ? "medium" : opts.widgetFamily,
    runsInWidget: opts.runsInWidget === undefined ? true : opts.runsInWidget,
  };
  globalThis.args = {
    widgetParameter: opts.widgetParameter === undefined ? null : opts.widgetParameter,
  };

  return env;
}

/// 修改当前环境的响应（同一个 env 跑多场景时用）
export function setResponse(env, response) {
  env.response = response;
  return env;
}

/// 修改当前环境的小组件尺寸 / 运行位置
export function setConfig(env, { widgetFamily, runsInWidget, widgetParameter } = {}) {
  if (widgetFamily !== undefined) globalThis.config.widgetFamily = widgetFamily;
  if (runsInWidget !== undefined) globalThis.config.runsInWidget = runsInWidget;
  if (widgetParameter !== undefined) globalThis.args.widgetParameter = widgetParameter;
  return env;
}

// ---------------------------------------------------------------------------
// 加载被测脚本
// ---------------------------------------------------------------------------

/// 多次 loadWidget 时复用的临时文件路径
let tmpCounter = 0;
const tmpFiles = [];
let excludeEnsured = false;

/// 运行期产物目录：tests/widget/.tmp/
export const TMP_DIR = join(HERE, ".tmp");

/**
 * 尽力把 .tmp/ 加进 .git/info/exclude —— 只影响本机 clone，不改仓库内容。
 *
 * 为什么需要：run.sh 的日志与 result.json、本模块生成的临时 .mjs 都在 .tmp/ 下，
 * 万一有别的 Agent / 人执行了 `git add -A`，这些运行期垃圾就会被提交进仓库。
 * .git/info/exclude 是 git 官方的「本地私有忽略」，适合放这种本机专属规则；
 * 它不修改受版本控制的 .gitignore，所以不越界、也不影响其他人。
 * 任何一步失败都静默忽略（例如不在 git 仓库里、或 .git 是文件形式的 worktree）。
 */
function ensureTmpExcluded() {
  if (excludeEnsured) return;
  excludeEnsured = true;
  try {
    const excludePath = join(REPO_ROOT, ".git", "info", "exclude");
    const current = readFileSync(excludePath, "utf8");
    const entry = "tests/widget/.tmp/";
    if (!current.split(/\r?\n/).includes(entry)) {
      writeFileSync(
        excludePath,
        current.replace(/\n?$/, "\n") +
          "# tests/widget 运行期产物（由 tests/widget/scriptable_mock.js 自动追加）\n" +
          entry +
          "\n",
        "utf8"
      );
    }
  } catch {
    /* 不在 git 仓库 / 无权限 —— 不影响测试本身，忽略 */
  }
}

/**
 * 加载**真实的** ios/widget/server_status.js 并执行它（等于在 Scriptable 里点运行）。
 *
 * 做法：读源文件 → 在末尾追加一行 `export { main }` → 写到 tests/widget/.tmp/
 *       → 动态 import。**源文件内容一字不改**（只追加 export），所以脚本改了测试必然跟着变，
 *       不存在「测试测的是一份拷贝」这种自欺欺人的情况。
 *
 * 为什么需要这一步：脚本末尾是顶层 `await main()`，而 ESM 的静态 import 会被提升到
 * 任何赋值之前执行，所以不能先 import 再设全局。这里改成「先 createEnv() 设好全局，
 * 再动态 import」，顺序才是对的。
 *
 * @returns {Promise<{ main: Function, env: object }>}
 */
export async function loadWidget(env) {
  const src = readFileSync(WIDGET_PATH, "utf8");

  // 追加导出。用换行锚定底部，避免误伤同名的其他出现位置。
  const instrumented = src.replace(
    /\nawait main\(\);\s*$/,
    "\nexport { main };\nawait main();\n"
  );
  if (instrumented === src) {
    throw new Error(
      "无法在 " +
        WIDGET_PATH +
        " 末尾找到 `await main();` —— 脚本入口结构变了，" +
        "请同步更新 tests/widget/scriptable_mock.js 的 loadWidget()。"
    );
  }

  ensureTmpExcluded();
  mkdirSync(TMP_DIR, { recursive: true });
  const tmpFile = join(TMP_DIR, `widget_${process.pid}_${tmpCounter++}.mjs`);
  writeFileSync(tmpFile, instrumented, "utf8");
  tmpFiles.push(tmpFile);

  // 动态 import：此时 globalThis 上的全局已经就位，顶层 await main() 能正常跑完
  const mod = await import(pathToFileURL(tmpFile).href);
  return { main: mod.main, env };
}

/// 清掉本次创建的临时文件。
/// 注意：**不能整个删掉 .tmp 目录** —— run.sh 会把 render_test.js 的输出日志与
/// 结果文件也放在那里，删目录会把它们一起带走（本项目实际踩过）。只删自己建的文件。
export function cleanupTmp() {
  while (tmpFiles.length > 0) {
    const f = tmpFiles.pop();
    try {
      rmSync(f, { force: true });
    } catch {
      /* 尽力而为 */
    }
  }
}

process.on("exit", cleanupTmp);

// ---------------------------------------------------------------------------
// 断言辅助：遍历渲染树
// ---------------------------------------------------------------------------

/// 收集渲染树里所有文本（按添加顺序，深度优先）
export function collectText(node, out = []) {
  if (!node) return out;
  if (typeof node.text === "string") out.push(node.text);
  const kids = node.children;
  if (Array.isArray(kids)) {
    for (const k of kids) collectText(k, out);
  }
  return out;
}

/// 收集渲染树里所有节点（含 spacer / image 这些占位节点）
export function collectNodes(node, out = []) {
  if (!node) return out;
  out.push(node);
  const kids = node.children;
  if (Array.isArray(kids)) {
    for (const k of kids) collectNodes(k, out);
  }
  return out;
}

/// 找到第一个文本等于 value 的节点（拿它的 font / textColor 做断言）
export function findTextNode(node, value) {
  for (const n of collectNodes(node)) {
    if (n.__isText && n.text === value) return n;
  }
  return null;
}

/// 找到第一个「文本包含 needle」的节点
export function findTextContaining(node, needle) {
  for (const n of collectNodes(node)) {
    if (n.__isText && n.text.includes(needle)) return n;
  }
  return null;
}

/// 收集所有服务灯圆点「●」节点的颜色
export function collectDots(widget) {
  return collectNodes(widget)
    .filter((n) => n.__isText && n.text === "●")
    .map((n) => n.textColor);
}
