import assert from "node:assert/strict";
import test from "node:test";
import { batchHarness } from "./helpers/batch-harness.mjs";

for (const concurrency of [2, 5]) {
  test(`并发 ${concurrency}：同时完成和慢初始化不得重复占用补位名额`, async () => {
    const h = await batchHarness(concurrency, 20);
    await h.start();
    for (let i = 0; i < concurrency; i++) {
      await h.advance(1200);
      await h.resolve();
    }
    for (const flow of [...h.windows]) await h.complete(flow);
    await h.advance(1600);
    await h.resolve();
    await h.advance(1200);
    assert.ok(h.load <= concurrency, `已开窗口 + 在途初始化 = ${h.load}，不得超过 ${concurrency}`);
    assert.ok(h.peak <= concurrency, `批次峰值 ${h.peak} 超过配置 ${concurrency}`);
  });
}

test("停止后重开批次：迟到的旧窗口关闭且不能覆盖新批次面板", async () => {
  const h = await batchHarness();
  await h.start();
  await h.advance(1200);
  const oldRequest = h.pending[0];
  h.api.mboxStop();
  await h.start();
  await h.advance(1200);
  const newRequest = h.pending.find(r => r !== oldRequest);
  const current = await h.resolve(newRequest);
  const stale = await h.resolve(oldRequest);
  assert.ok(h.windows.has(current), "当前批次的窗口应保留");
  assert.ok(!h.windows.has(stale), "停止后迟到的窗口必须关闭");
  assert.ok(!h.panels.includes(stale), "旧批次不能重建进度面板");
});

test("停止后的初始化失败不回写邮箱状态", async () => {
  const h = await batchHarness();
  await h.start();
  await h.advance(1200);
  h.api.mboxStop();
  await h.reject();
  assert.equal(h.calls.filter(c => c.command === "pool_mark_verified").length, 0);
});

test("初始化失败归还名额，最后一个任务完成后批次正常结束", async () => {
  const h = await batchHarness(1, 2);
  await h.start();
  await h.reject();
  assert.equal(h.pending.length, 1);
  const flow = await h.resolve();
  await h.complete(flow);
  await h.advance(400);
  assert.equal(h.api.mboxRunning(), false);
  assert.equal(h.calls.filter(c => c.command === "pool_mark_verified").length, 2);
});

test("重放的宿主最终结果与 OAuth 通知不会重复归还批次名额", async () => {
  const h = await batchHarness(1, 1);
  await h.start();
  const flow = await h.resolve();
  h.api.mboxOnTerminated(flow, "result", { ok: true });
  await h.complete(flow);
  await h.advance(400);
  assert.equal(h.api.mboxRunning(), false);
  const updates = h.calls.filter(c => c.command === "pool_mark_verified");
  assert.equal(updates.length, 1);
  assert.equal(updates[0].args.ok, true);
});
