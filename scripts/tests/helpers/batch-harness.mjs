// 运行原始邮箱批次模块，只替代宿主 IPC、界面和计时器，不复制调度算法。
import fs from "node:fs";
import vm from "node:vm";

export async function batchHarness(concurrency = 2, total = 12) {
  let now = 0, timerId = 0, flowId = 0, peak = 0;
  const timers = new Map(), requests = [], windows = new Set(), calls = [], panels = [];
  const accounts = Array.from({ length: total }, (_, i) => ({ email: `fixture-${i}@example.invalid`, status: "new" }));
  const flush = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };
  const pending = () => requests.filter(r => !r.settled);
  const observe = () => { peak = Math.max(peak, windows.size + pending().length); };
  const invoke = async (command, args = {}) => {
    calls.push({ command, args });
    if (command === "pool_list") return { accounts };
    if (command === "oauth_begin") {
      return new Promise((resolve, reject) => {
        requests.push({ email: args.email, resolve, reject, settled: false });
        observe();
      });
    }
    if (command === "reg_close_window") {
      if (args.flow) windows.delete(args.flow);
      else windows.clear();
    }
    return {};
  };
  const math = Object.create(Math);
  math.random = () => 0;
  const context = vm.createContext({
    console, Math: math, document: { getElementById: () => null },
    setTimeout(fn, ms = 0) { const id = ++timerId; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout: id => timers.delete(id),
  });
  const dependencies = {
    "@tauri-apps/api/core": { invoke },
    "./i18n.js": { t: key => key },
    "./icons.js": { ic: () => "" },
    "./ui.js": { esc: value => String(value ?? ""), toast() {}, openLineModal() {} },
    "./reg.js": { regStart: flow => panels.push(flow), regClose() {} },
  };
  const module = new vm.SourceTextModule(fs.readFileSync(new URL("../../../src/mbox.js", import.meta.url), "utf8"), { context });
  await module.link(specifier => {
    const values = dependencies[specifier];
    if (!values) throw new Error(`未配置的模块依赖：${specifier}`);
    return new vm.SyntheticModule(Object.keys(values), function () {
      for (const [key, value] of Object.entries(values)) this.setExport(key, value);
    }, { context });
  });
  await module.evaluate();
  const api = module.namespace;
  await api.mboxLoad();
  accounts.forEach(a => api.mboxToggle(a.email, true));
  api.setMboxConc(concurrency);
  return {
    api, calls, windows, panels,
    get pending() { return pending(); },
    get load() { return windows.size + pending().length; },
    get peak() { return peak; },
    async start() { await api.mboxVerify(); await flush(); },
    async complete(flow) { windows.delete(flow); api.mboxOnFlowDone(flow, true, ""); await flush(); },
    async resolve(request = pending()[0]) {
      if (!request) throw new Error("没有待完成的初始化请求");
      request.settled = true;
      const flow = `flow-${++flowId}`;
      windows.add(flow);
      request.resolve({ flow });
      observe();
      await flush();
      return flow;
    },
    async reject(request = pending()[0]) {
      if (!request) throw new Error("没有待完成的初始化请求");
      request.settled = true;
      request.reject(new Error("模拟初始化失败"));
      await flush();
    },
    async advance(ms) {
      const end = now + ms;
      for (let i = 0; i < 10000; i++) {
        await flush();
        const next = [...timers].sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (!next || next[1].at > end) break;
        const [id, task] = next;
        timers.delete(id);
        now = task.at;
        task.fn();
      }
      now = end;
      await flush();
    },
  };
}
