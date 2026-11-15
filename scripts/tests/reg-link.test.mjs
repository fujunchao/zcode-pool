import assert from "node:assert/strict";
import test from "node:test";
import { regLinkHarness } from "./helpers/reg-link-harness.mjs";

const LINK = "https://chat.z.ai/auth/verify_email?token=verify-abc&email=fixture%40example.invalid";

test("取链遇到瞬时网络错误不放弃：下一轮取到后自动下发", async () => {
  const h = await regLinkHarness({
    latency: 200,
    fetchLink: ({ n }) => {
      if (n === 1) throw new Error("M365 请求失败：https://graph.microsoft.com/... : Network Error");
      return { links: [LINK], scanned: 50 };
    },
  });
  await h.begin();
  await h.advance(10_000);

  assert.equal(h.fetches, 2, "第一次失败后应继续查询，而不是直接转手动");
  const pushed = h.inputs.filter((i) => i.ask === "link");
  assert.equal(pushed.length, 1, "取到链接后应下发一次");
  assert.equal(pushed[0].value, LINK);
  assert.equal(h.modals.length, 0, "不该弹出手动粘贴框");
  assert.match(h.html(), /reg\.log\.fetchLinkErr/, "失败仍要留痕");
});

test("等待窗口是 90 秒墙钟：到点才转手动，而不是按查询次数提前结束", async () => {
  const h = await regLinkHarness({ latency: 2000, fetchLink: () => ({ links: [], scanned: 50 }) });
  await h.begin();
  const first = h.fetchStarts[0];
  await h.advance(120_000);

  assert.equal(h.modals.length, 1, "等满窗口后应转手动粘贴");
  const waited = h.modals[0].createdAt - first;
  assert.ok(waited >= 78_000 && waited <= 95_000, `手动框应在 90 秒墙钟附近出现，实际 ${waited}ms`);
  assert.match(h.html(), /reg\.log\.fetchLinkPolling/);
  assert.ok(
    h.fetchStarts.some((at) => at - first > 95_000),
    "转到手动之后后台仍应继续等邮件",
  );
});

test("手动框背后的兜底：邮件晚到时自动填入并关掉弹窗", async () => {
  let first = null;
  const h = await regLinkHarness({
    latency: 200,
    fetchLink: ({ at }) => {
      if (first === null) first = at;
      return at - first < 96_000 ? { links: [], scanned: 50 } : { links: [LINK], scanned: 50 };
    },
  });
  await h.begin();
  await h.advance(120_000);

  assert.match(h.html(), /reg\.log\.fetchLinkBgStart/, "转手动时应说明后台还在等");
  assert.equal(h.modals.length, 1);
  const pushed = h.inputs.filter((i) => i.ask === "link");
  assert.equal(pushed.length, 1, "后台取到后应自动下发，用户不必手贴");
  assert.equal(pushed[0].value, LINK);
  assert.equal(h.modals[0].removed, true, "自动填入后应关掉手动粘贴框");
});

test("凭据失效立即终止，不再继续轮询", async () => {
  const h = await regLinkHarness({
    latency: 50,
    fetchLink: () => { throw new Error("invalid_grant: AADSTS70008 凭据已过期"); },
  });
  await h.begin();
  await h.advance(30_000);

  assert.equal(h.fetches, 1, "凭据类错误不该重试");
  assert.equal(h.terminated.length, 1);
  assert.equal(h.terminated[0].reason, "skipped");
  assert.equal(h.toasts.length, 1);
  assert.equal(h.modals.length, 0);
});

test("并发批次里 A 的链接晚到：撤掉 A 的粘贴框，B 的排队框顶上", async () => {
  let firstA = null;
  const linkA = LINK + "&who=a";
  const h = await regLinkHarness({
    latency: 200,
    fetchLink: ({ args, at }) => {
      if (args.email !== "a@example.invalid") return { links: [], scanned: 50 };
      if (firstA === null) firstA = at;
      return at - firstA >= 96_000 ? { links: [linkA], scanned: 50 } : { links: [], scanned: 50 };
    },
  });
  await h.begin("flow-a", "a@example.invalid");
  await h.begin("flow-b", "b@example.invalid");
  await h.advance(130_000);

  const pushed = h.inputs.filter((i) => i.ask === "link");
  assert.equal(pushed.length, 1, "只应下发 A 的链接");
  assert.equal(pushed[0].value, linkA);
  assert.equal(pushed[0].flow, "flow-a");
  assert.equal(h.modals.length, 2, "A 的框关掉后应立刻顶上 B 的框");
  assert.equal(h.modals[0].removed, true, "A 的粘贴框应被撤掉");
  assert.equal(h.modals[1].removed, false, "B 仍在等自己的邮件");
});

test("链接下发失败时保留粘贴框，同一链接也不会反复重发", async () => {
  const h = await regLinkHarness({
    latency: 100,
    failLinkInput: true,
    fetchLink: () => ({ links: [LINK], scanned: 50 }),
  });
  await h.begin();
  await h.advance(30_000);

  assert.match(h.html(), /reg\.log\.fetchLinkPushFail/);
  assert.equal(h.modals.length, 1, "下发失败要留给用户手动补一次");
  assert.equal(h.modals[0].removed, false, "粘贴框不能被后台兜底又关掉");
  assert.equal(h.inputs.filter((i) => i.ask === "link").length, 1, "同一链接不重复下发");
});

test("离开等邮件阶段后，后台兜底立刻收口", async () => {
  const h = await regLinkHarness({ latency: 100, fetchLink: () => ({ links: [], scanned: 50 }) });
  await h.begin();
  await h.advance(95_000);
  assert.equal(h.modals.length, 1);
  assert.match(h.html(), /reg\.log\.fetchLinkBgStart/);

  h.emit({ kind: "phase", phase: "open-link" });
  const before = h.fetchStarts.length;
  await h.advance(60_000);
  assert.equal(h.fetchStarts.length, before, "链接已打开后不许再把流程拽回上一步");
});
