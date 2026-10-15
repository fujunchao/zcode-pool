// 执行真实驱动；只替代浏览器、宿主 IPC 和时钟等系统边界，不改写驱动内部函数。
import fs from "node:fs";
import vm from "node:vm";

export function driverHarness({ redirectDelay = 50, hostDelay = 0, ipc = true, rejectIpc = false } = {}) {
  let now = 0, seq = 0, buttons = [], callbackStarted = false, callbackAborted = false;
  let callbackReached = false;
  const tasks = new Map(), messages = [], navigations = [], errors = [];
  const setTimeout = (fn, delay = 0) => {
    const id = ++seq;
    tasks.set(id, { fn, at: now + Number(delay) });
    return id;
  };
  const oldUrl = new URL("https://chat.z.ai/auth/oauth/authorize");
  const location = {
    hostname: oldUrl.hostname, pathname: oldUrl.pathname, search: "",
    get href() { return oldUrl.href; },
    set href(value) {
      const u = new URL(value);
      navigations.push({ at: now, url: u.href });
      if (u.protocol === "zpool-driver:") {
        messages.push(Object.fromEntries(u.searchParams));
        if (callbackStarted) callbackAborted = true;
        return;
      }
      callbackStarted = true;
      setTimeout(() => { callbackReached = !callbackAborted; }, 600);
    },
    reload() { navigations.push({ at: now, url: "reload" }); },
  };
  const body = { innerText: "", innerHTML: "" };
  const button = {
    tagName: "BUTTON", innerText: "继续", disabled: false, offsetParent: {},
    getAttribute() { return null; },
    click() {
      body.innerText = "授权成功 正在跳转到应用...";
      buttons = [];
      setTimeout(() => { location.href = "https://zcode.z.ai/api/v1/oauth/cli/callback/zai"; }, redirectDelay);
    },
  };
  const checkbox = { offsetParent: {}, checked: true, getAttribute() { return null; } };
  const document = {
    readyState: "complete", body,
    addEventListener() {},
    querySelectorAll(selector) {
      if (selector === "button, a, div, span, [role=button]") return buttons;
      if (selector.includes('input[type="checkbox"]')) return [checkbox];
      return [];
    },
  };
  class FakeDate extends Date { static now() { return 1_700_000_000_000 + now; } }
  // 与真实跨页面 sessionStorage 相同的输入：邮箱验证已经完成，返回授权页。
  const storage = new Map([["zpool-driver-state", JSON.stringify({ mode: "register", auto: true, backDone: true })]]);
  const context = {
    document, location, URL, URLSearchParams, Date: FakeDate,
    console: { log() {}, warn: (...v) => errors.push(v), error: (...v) => errors.push(v) },
    setTimeout, clearTimeout: id => tasks.delete(id),
    sessionStorage: { getItem: k => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v) },
    __ZPOOL_CFG: { mode: "register", auto: true, provider: "zai" },
  };
  const installIpc = () => {
    context.__TAURI_INTERNALS__ = {
      invoke(command, args) {
        if (command !== "plugin:reg-driver|report") return Promise.reject(new Error("未知 IPC 命令"));
        return new Promise((resolve, reject) => setTimeout(() => {
          if (rejectIpc) return reject(new Error("模拟宿主连接失败"));
          messages.push(JSON.parse(JSON.stringify(args.message)));
          resolve();
        }, hostDelay));
      },
    };
  };
  if (ipc) installIpc();
  context.window = context;
  context.top = context;
  context.self = context;
  vm.createContext(context);
  const source = fs.readFileSync(new URL("../../../src-tauri/src/driver/zpool-driver.js", import.meta.url), "utf8");
  vm.runInContext(source, context, { filename: "zpool-driver.js" });
  return {
    messages, navigations, errors, installIpc,
    get callbackReached() { return callbackReached; },
    showAuthorize() { body.innerText = "授权请求"; buttons = [button]; },
    fromHost(message) { context.__zpool.fromHost(message); },
    async advance(ms) {
      const end = now + ms;
      for (let i = 0; i < 10000; i++) {
        for (let j = 0; j < 12; j++) await Promise.resolve();
        if (!tasks.size) break;
        const [id, task] = [...tasks].sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (task.at > end) break;
        tasks.delete(id);
        now = task.at;
        task.fn();
      }
      now = end;
    },
  };
}
