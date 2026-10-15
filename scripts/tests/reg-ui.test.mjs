import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import vm from "node:vm";

async function panel({ invoke = async () => ({}) } = {}) {
  const nodes = [], listeners = new Map();
  const classList = { add() {}, remove() {}, toggle() {} };
  const document = {
    body: { classList, appendChild() {} }, documentElement: {},
    addEventListener() {}, querySelector(selector) { return selector === ".toasts" ? nodes.find(n => n.className === "toasts") : null; },
    createElement() {
      const el = {
        innerHTML: "", classList, addEventListener() {}, appendChild() {}, querySelector() { return null; }, remove() {},
        querySelectorAll(selector) {
          if (selector !== "[data-act]") return [];
          el.buttons = [...el.innerHTML.matchAll(/<button\b[^>]*\bdata-act="([^"]+)"[^>]*>/g)].map(([tag, act]) => ({
            dataset: { act, flow: tag.match(/data-flow="([^"]+)"/)?.[1] },
            addEventListener(name, callback) { if (name === "click") this.click = callback; },
          }));
          return el.buttons;
        },
      };
      nodes.push(el);
      return el;
    },
  };
  const context = vm.createContext({ document, window: {}, console, setTimeout: () => 1, clearTimeout() {}, Date });
  const cache = new Map();
  function module(url) {
    if (cache.has(url)) return cache.get(url);
    const m = new vm.SourceTextModule(fs.readFileSync(new URL(url), "utf8"), { context, identifier: url });
    cache.set(url, m);
    return m;
  }
  const main = module(new URL("../../src/reg.js", import.meta.url).href);
  const link = (specifier, ref) => {
    if (specifier.startsWith("@tauri-apps/api/")) {
      return new vm.SyntheticModule(["invoke", "listen"], function () {
        this.setExport("invoke", invoke);
        this.setExport("listen", async (name, callback) => { listeners.set(name, callback); return () => {}; });
      }, { context });
    }
    return module(new URL(specifier, ref.identifier).href);
  };
  await main.link(link);
  await main.evaluate();
  return {
    start(flow = "test-flow", email = "") { main.namespace.regStart(flow, "register", "zai", email); },
    close(flow) { main.namespace.regClose(flow); },
    terminated(callback) { main.namespace.setRegTerminated(callback); },
    async batch() {
      const batch = module(new URL("../../src/mbox.js", import.meta.url).href);
      await batch.link(link);
      await batch.evaluate();
      main.namespace.setRegTerminated(batch.namespace.mboxOnTerminated);
      return batch.namespace;
    },
    event(ev) { listeners.get("reg://event")({ payload: { flow: "test-flow", ...ev } }); },
    html() { return nodes.find(n => n.className === "drw")?.innerHTML || ""; },
    button(act) { return nodes.find(n => n.className === "drw")?.buttons.find(b => b.dataset.act === act); },
    toasts() { return nodes.filter(n => n.className === "toast err").map(n => n.innerHTML).join(""); },
  };
}

test("等待授权回调时进度卡片仍为运行中", async () => {
  const p = await panel();
  p.start();
  p.event({ kind: "phase", phase: "await-callback" });
  assert.match(p.html(), /reg-task-stage[^>]*>等待授权回调/);
  assert.doesNotMatch(p.html(), /reg-task-stage[^>]*>入库完成/);
});

test("宿主入库成功后才显示完成，迟到的页面消息不能倒退状态", async () => {
  const p = await panel();
  p.start();
  p.event({ kind: "result", ok: true });
  p.event({ kind: "phase", phase: "await-callback" });
  p.event({ kind: "closed" });
  assert.match(p.html(), /reg-task-stage[^>]*>入库完成/);
  assert.doesNotMatch(p.html(), /reg-task-stage[^>]*>等待授权回调/);
});

test("宿主超时失败显示原因，不能被旧网页的 done 消息覆盖", async () => {
  const p = await panel();
  p.start();
  p.event({ kind: "result", ok: false, error: "授权回调已过期" });
  p.event({ kind: "phase", phase: "done" });
  assert.match(p.html(), /reg-task-stage[^>]*>授权未完成/);
  assert.match(p.html(), /授权回调已过期/);
});

test("整理窗口进行中禁用重复操作，失败后提示并恢复按钮", async () => {
  let reject, requests = 0;
  const p = await panel({ invoke: command => {
    assert.equal(command, "reg_tile_windows");
    requests += 1;
    return new Promise((_, fail) => { reject = fail; });
  } });
  p.start();
  p.start("second-flow");
  const button = p.button("tile");
  const first = button.click();
  await button.click();
  assert.equal(requests, 1);
  assert.match(p.html(), /data-act="tile" disabled/);
  reject(new Error("模拟窗口定位失败"));
  await first;
  assert.match(p.toasts(), /模拟窗口定位失败/);
  assert.doesNotMatch(p.html(), /data-act="tile" disabled/);
});

test("页面填表请求早于开窗响应时，批次绑定邮箱后仍能自动填表", async () => {
  const calls = [];
  const p = await panel({ invoke: async (command, args) => {
    calls.push({ command, args });
    return { email: "fixture@example.invalid", password: "fixture-password" };
  } });
  p.event({ kind: "phase", phase: "signup-form" });
  p.event({ kind: "ask", ask: "signup" });
  assert.equal(calls.length, 0, "宿主尚未绑定 flow 时不得操作账号");
  p.start("test-flow", "fixture@example.invalid");
  for (let i = 0; i < 10; i++) await Promise.resolve();
  assert.equal(calls.filter(c => c.command === "pool_get").length, 1);
  assert.equal(calls.filter(c => c.command === "reg_input" && c.args.flow === "test-flow").length, 1);
  assert.match(p.html(), /reg-task-stage[^>]*>填写注册信息/);
});

test("开窗响应前已收到最终结果时，只重放结果而不执行过期填表请求", async () => {
  const calls = [], results = [];
  const p = await panel({ invoke: async command => { calls.push(command); return {}; } });
  p.terminated((...args) => results.push(args));
  p.event({ kind: "ask", ask: "signup" });
  p.event({ kind: "result", ok: true });
  p.start("test-flow", "fixture@example.invalid");
  assert.match(p.html(), /reg-task-stage[^>]*>入库完成/);
  assert.equal(calls.length, 0);
  assert.equal(results.length, 1, "早到的最终结果也必须让批次归还名额");
  assert.equal(results[0][0], "test-flow");
  assert.equal(results[0][1], "result");
});

test("关闭面板会清除尚未绑定的旧流程事件", async () => {
  const p = await panel();
  p.event({ kind: "result", ok: true });
  p.close();
  p.start();
  assert.doesNotMatch(p.html(), /reg-task-stage[^>]*>入库完成/);
});

for (const completed of [false, true]) {
  test(`真实批次模块接住开窗响应前的${completed ? "完成结果" : "单次填表请求"}`, async () => {
    const calls = [];
    let finishOpen;
    const email = "fixture@example.invalid";
    const p = await panel({ invoke: async (command, args) => {
      calls.push({ command, args });
      if (command === "pool_list") return { accounts: [{ email, status: "new" }] };
      if (command === "oauth_begin") return new Promise(resolve => { finishOpen = resolve; });
      return { email, password: "fixture-password" };
    } });
    const batch = await p.batch();
    await batch.mboxLoad();
    batch.mboxToggle(email, true);
    batch.setMboxConc(1);
    await batch.mboxVerify();
    p.event({ kind: "ask", ask: "signup" });
    if (completed) p.event({ kind: "result", ok: true });
    finishOpen({ flow: "test-flow" });
    for (let i = 0; i < 30; i++) await Promise.resolve();
    if (completed) {
      assert.equal(batch.mboxRunning(), false);
      assert.equal(calls.filter(c => c.command === "pool_mark_verified" && c.args.ok).length, 1);
      assert.equal(calls.filter(c => c.command === "reg_input").length, 0);
    } else {
      const inputs = calls.filter(c => c.command === "reg_input");
      assert.equal(inputs.length, 1);
      assert.equal(inputs[0].args.value.email, email);
    }
  });
}
