// 执行真实的 src/reg.js：只替换宿主 IPC、DOM 与时钟，不改写取链逻辑。
import fs from "node:fs";
import vm from "node:vm";

const START_AT = 1_700_000_000_000;

export async function regLinkHarness({ fetchLink, latency = 200, account = "fixture@example.invalid", failLinkInput = false } = {}) {
  let clock = START_AT;
  let seq = 0;
  let fetches = 0;
  let listener = null;
  let currentFlow = "";
  const timers = new Map();
  const calls = [];
  const inputs = [];
  const toasts = [];
  const terminated = [];
  const elements = [];
  const fetchStarts = [];

  const flush = async () => { for (let i = 0; i < 200; i++) await Promise.resolve(); };

  const setTimer = (fn, ms = 0) => {
    const id = ++seq;
    timers.set(id, { fn, at: clock + Math.max(0, Number(ms) || 0) });
    return id;
  };
  const clearTimer = (id) => { timers.delete(id); };

  class FakeDate extends Date {
    constructor(...args) { if (args.length) super(...args); else super(clock); }
    static now() { return clock; }
  }

  const makeEl = (tag = "div") => {
    const node = {
      tagName: String(tag).toUpperCase(),
      className: "", innerHTML: "", hidden: false, value: "", removed: false,
      scrollTop: 0, scrollHeight: 0, children: [], style: {}, dataset: {},
      createdAt: clock,
      classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
      appendChild(child) { node.children.push(child); return child; },
      remove() { node.removed = true; },
      focus() {},
      scrollIntoView() {},
      textContent: "",
      addEventListener(name, cb) { (node.handlers || (node.handlers = {}))[name] = cb; },
      querySelector(selector) {
        if (!node.q) node.q = new Map();
        if (!node.q.has(selector)) node.q.set(selector, makeEl());
        return node.q.get(selector);
      },
      querySelectorAll() { return []; },
    };
    elements.push(node);
    return node;
  };

  const body = makeEl("body");
  const document = {
    body,
    documentElement: {},
    addEventListener() {},
    querySelector() { return null; },
    createElement(tag) { return makeEl(tag); },
  };

  const invoke = (command, args = {}) => {
    calls.push({ command, args, at: clock });
    if (command === "pool_get") return Promise.resolve({ email: args.email || account, password: "secret1" });
    if (command === "reg_input") {
      inputs.push({ ...args, at: clock });
      if (failLinkInput && args.ask === "link") return Promise.reject(new Error("窗口已关闭"));
      return Promise.resolve({});
    }
    if (command === "reg_fetch_link") {
      const n = ++fetches;
      fetchStarts.push(clock);
      return new Promise((resolve, reject) => {
        setTimer(() => {
          try {
            resolve(fetchLink ? fetchLink({ n, args, at: clock }) : { links: [], scanned: 0 });
          } catch (e) {
            reject(e);
          }
        }, latency);
      });
    }
    return Promise.resolve({});
  };

  const i18n = {
    t: (key, params) => (params && Object.keys(params).length ? key + " " + JSON.stringify(params) : key),
    has: () => true,
    stripErr: (e) => String(e),
  };

  const context = vm.createContext({
    document, window: {}, navigator: undefined, console,
    Date: FakeDate, Math, Promise,
    setTimeout: setTimer, clearTimeout: clearTimer,
  });

  const synthetic = (values) => new vm.SyntheticModule(Object.keys(values), function () {
    for (const [key, value] of Object.entries(values)) this.setExport(key, value);
  }, { context });

  const cache = new Map();
  const load = (url) => {
    if (cache.has(url)) return cache.get(url);
    const m = new vm.SourceTextModule(fs.readFileSync(new URL(url), "utf8"), { context, identifier: url });
    cache.set(url, m);
    return m;
  };

  const reg = load(new URL("../../../src/reg.js", import.meta.url).href);
  await reg.link((specifier) => {
    if (specifier === "@tauri-apps/api/core") return synthetic({ invoke });
    if (specifier === "@tauri-apps/api/event") {
      return synthetic({ listen: (name, cb) => { listener = cb; return Promise.resolve(() => {}); } });
    }
    if (specifier === "./i18n.js") return synthetic(i18n);
    if (specifier === "./icons.js") return synthetic({ ic: () => "" });
    if (specifier === "./ui.js") {
      return synthetic({ esc: (v) => String(v ?? ""), toast: (msg, kind, detail) => toasts.push({ msg, kind, detail }) });
    }
    throw new Error("未配置的模块依赖：" + specifier);
  });
  await reg.evaluate();
  const api = reg.namespace;
  api.setRegTerminated((flow, reason, result) => terminated.push({ flow, reason, result, at: clock }));

  const emitTo = (flow, ev) => listener({ payload: { flow, ...ev } });

  return {
    api, calls, inputs, toasts, terminated, fetchStarts, account,
    get now() { return clock; },
    get fetches() { return fetches; },
    get modals() { return elements.filter((e) => e.className === "ov reg bottom"); },
    drawer() { return elements.find((e) => e.className === "drw") || null; },
    html() { return elements.find((e) => e.className === "drw")?.innerHTML || ""; },
    emit(ev) { emitTo(currentFlow, ev); },
    emitTo,
    start(flow = "flow-1", batchEmail = account) {
      currentFlow = flow;
      api.regStart(flow, "register", "zai", batchEmail);
      return flow;
    },
    /** 走完"填注册信息 → 提交 → 进入等邮件"的真实事件序列。 */
    async begin(flow = "flow-1", batchEmail = account) {
      this.start(flow, batchEmail);
      emitTo(flow, { kind: "phase", phase: "signup-form" });
      emitTo(flow, { kind: "ask", ask: "signup" });
      await this.advance(300);
      emitTo(flow, { kind: "phase", phase: "await-email" });
      emitTo(flow, { kind: "ask", ask: "link" });
      return flow;
    },
    async advance(ms) {
      const end = clock + ms;
      for (let i = 0; i < 50000; i++) {
        await flush();
        let next = null;
        for (const [id, task] of timers) {
          if (task.at > end) continue;
          if (!next || task.at < next.task.at || (task.at === next.task.at && id < next.id)) next = { id, task };
        }
        if (!next) break;
        timers.delete(next.id);
        clock = Math.max(clock, next.task.at);
        next.task.fn();
      }
      clock = end;
      await flush();
    },
  };
}
