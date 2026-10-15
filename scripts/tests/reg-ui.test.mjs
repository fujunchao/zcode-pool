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
  await main.link((specifier, ref) => {
    if (specifier.startsWith("@tauri-apps/api/")) {
      return new vm.SyntheticModule(["invoke", "listen"], function () {
        this.setExport("invoke", invoke);
        this.setExport("listen", async (name, callback) => { listeners.set(name, callback); return () => {}; });
      }, { context });
    }
    return module(new URL(specifier, ref.identifier).href);
  });
  await main.evaluate();
  return {
    start(flow = "test-flow") { main.namespace.regStart(flow, "register", "zai"); },
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
