import assert from "node:assert/strict";
import test from "node:test";
import { driverHarness } from "./helpers/driver-harness.mjs";

test("授权成功后的快速回调不被驱动消息导航打断", async () => {
  const h = driverHarness({ redirectDelay: 50 });
  await h.advance(2500);
  h.showAuthorize();
  await h.advance(4000);
  assert.equal(h.callbackReached, true, "页面显示授权成功后，应完成回调而不是留在旧页面");
  assert.equal(h.navigations.filter(x => x.url.startsWith("zpool-driver:")).length, 0,
    "状态消息不能占用顶层导航");
});

for (const redirectDelay of [0, 100, 160, 200, 500]) {
  test(`回调延迟 ${redirectDelay}ms 时只发生真正的业务导航`, async () => {
    const h = driverHarness({ redirectDelay });
    await h.advance(2500);
    h.showAuthorize();
    await h.advance(4000);
    assert.equal(h.callbackReached, true);
    assert.deepEqual(h.navigations.map(x => x.url), ["https://zcode.z.ai/api/v1/oauth/cli/callback/zai"]);
  });
}

test("宿主 IPC 响应慢也不阻塞或打断授权回调", async () => {
  const h = driverHarness({ hostDelay: 300 });
  await h.advance(2500);
  h.showAuthorize();
  await h.advance(8000);
  assert.equal(h.callbackReached, true);
  assert.equal(h.messages.some(x => x.kind === "phase" && x.phase === "await-callback"), true);
});

test("IPC 尚未就绪时保留消息，就绪后恢复且不降级到导航", async () => {
  const h = driverHarness({ ipc: false });
  await h.advance(2000);
  assert.equal(h.messages.length, 0);
  assert.equal(h.navigations.length, 0);
  h.installIpc();
  await h.advance(2000);
  assert.equal(h.messages[0].kind, "ready");
  assert.equal(h.navigations.length, 0);
});

test("IPC 拒绝消息时仍然不能劫持主窗口导航", async () => {
  const h = driverHarness({ rejectIpc: true });
  await h.advance(2500);
  h.showAuthorize();
  await h.advance(4000);
  assert.equal(h.callbackReached, true);
  assert.equal(h.navigations.some(x => x.url.startsWith("zpool-driver:")), false);
  assert.ok(h.errors.length > 0);
});

test("改为 IPC 后，宿主下发的邮箱验证链接仍可正常打开", async () => {
  const h = driverHarness();
  await h.advance(2500);
  h.fromHost({ t: "setMode", mode: "register" });
  h.fromHost({ t: "input", ask: "link", value: "https://chat.z.ai/auth/verify_email?token=fixture" });
  await h.advance(600);
  assert.deepEqual(h.navigations.map(x => x.url), ["https://chat.z.ai/auth/verify_email?token=fixture"]);
  assert.equal(h.messages.some(x => x.kind === "phase" && x.phase === "open-link"), true);
});

test("五个并发流程的宿主消息与导航不互相干扰", async () => {
  const all = [0, 50, 100, 200, 500].map(redirectDelay => driverHarness({ redirectDelay, hostDelay: 30 }));
  await Promise.all(all.map(async h => {
    await h.advance(2500);
    h.showAuthorize();
    await h.advance(5000);
    assert.equal(h.callbackReached, true);
    assert.equal(h.messages.filter(x => x.kind === "phase" && x.phase === "await-callback").length, 1);
  }));
});

test("只点击授权但尚未收到宿主结果时不能报告入库完成", async () => {
  const h = driverHarness();
  await h.advance(2500);
  h.showAuthorize();
  await h.advance(4000);
  assert.equal(h.messages.some(x => x.kind === "phase" && x.phase === "done"), false);
  assert.equal(h.messages.some(x => x.kind === "phase" && x.phase === "await-callback"), true);
});
