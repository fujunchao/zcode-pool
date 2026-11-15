import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, has, stripErr } from "./i18n.js";
import { ic } from "./icons.js";
import { esc, toast } from "./ui.js";

const STEPS = {
  register: [
    "open-signup", "signup-form", "await-email", "open-link",
    "complete-signup", "back-to-authorize", "confirm-authorize", "await-callback", "done",
  ],
  login: ["confirm-authorize", "await-callback", "done"],
  observe: [],
};

const PAGE_KEYS = {
  "zai-signup": "signup",
  "zai-auth": "zaiAuth",
  "zai-authorize": "zaiAuthorize",
  "zai-other": "zaiOther",
  "bigmodel-login": "bigmodel",
  "zcode-bridge": "bridge",
  other: "other",
};

const MAX_LOGS = 200;
const CARD_LOGS = 3;
const EARLY_EVENT_TTL = 120_000;
const MAX_EARLY_FLOWS = 10;

// 自动取链的等待预算按墙钟算：90 秒是真实截止时间，不是"30 次 × 3 秒"的推断值。
const LINK_WAIT_MS = 90_000;
const LINK_POLL_MS = 3000;
const LINK_MAX_AUTO = 3;
// 剩余时间不足这个额度就不再发起新查询：单次 Graph 调用最长 25s，发起了就可能越过截止点。
const LINK_MIN_ROUND_MS = 5000;
// 转手动粘贴后，后台继续等邮件的节奏与上限：邮件晚到时自动填入，用户不必手贴。
const LINK_BG_POLL_MS = 5000;
const LINK_BG_MAX_MS = 600_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const shortMail = (s) => String(s || "").split("@")[0] || "";

/** 并发注册：每个 OAuth 流程（flow）一个 session，并行并存。 */
let sessions = new Map();
// WebView 可能先于开窗 IPC 响应加载；按 flow 暂存，绑定批次邮箱后才处理请求。
const earlyEvents = new Map();
let drawer = null;
let tiling = false;
let modal = null;
let modalKind = null;
let modalFlow = null;

/** 手动输入请求队列：并发时多个任务都要人工填单，按序弹出，不互相覆盖。 */
let modalQueue = [];

let onTerminated = null;
export function setRegTerminated(fn) {
  onTerminated = fn;
}

function terminate(flow, reason, result) {
  if (onTerminated) onTerminated(flow, reason, result);
}

function bufferEarlyEvent(ev) {
  const now = Date.now();
  for (const [flow, pending] of earlyEvents) {
    if (now - pending.at >= EARLY_EVENT_TTL) earlyEvents.delete(flow);
  }
  if (!earlyEvents.has(ev.flow)) {
    if (earlyEvents.size >= MAX_EARLY_FLOWS) earlyEvents.delete(earlyEvents.keys().next().value);
    earlyEvents.set(ev.flow, { at: now, events: [] });
  }
  const pending = earlyEvents.get(ev.flow);
  pending.events.push(ev);
  if (pending.events.length > MAX_LOGS) {
    const index = pending.events.findIndex(e => !["ask", "result", "closed"].includes(e.kind));
    pending.events.splice(index < 0 ? 0 : index, 1);
  }
}

function shortFlow(flow) {
  return String(flow || "").slice(0, 12);
}

function mailOf(s) {
  return s.signupEmail || s.batchEmail || "";
}

function tr(group, key, params) {
  const k = `${group}.${key}`;
  return has(k) ? t(k, params) : `${key}`;
}

function nowLabel() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export function regActive() {
  return sessions.size > 0;
}

export function regStart(flow, mode, provider, batchEmail = "") {
  sessions.set(flow, {
    flow,
    mode,
    provider,
    phase: "",
    note: "",
    logs: [],
    closed: false,
    failed: false,
    finished: false,
    startedAt: Date.now(),
    signupEmail: "",
    batchEmail,
    linkAuto: 0,
    submittedAt: 0,
    linkBusy: false,
    // 取链兜底：linkBg=后台轮询中，linkStop=让在跑的轮询尽快收口，
    // pushedLink=已下发的链接（去重），linkLastFail=上次报错文案（日志去重）。
    linkBg: false,
    linkStop: false,
    pushedLink: "",
    linkLastFail: "",
  });
  pushLog(sessions.get(flow), "info", "connected", provider);
  render();
  const pending = earlyEvents.get(flow);
  earlyEvents.delete(flow);
  if (pending && Date.now() - pending.at < EARLY_EVENT_TTL) {
    // 已收口的流程不能再执行缓存中的填表/取信请求；result 比迟到的 closed 更权威。
    const newest = pending.events.slice().reverse();
    const terminal = newest.find(e => e.kind === "result") || newest.find(e => e.kind === "closed");
    for (const event of terminal ? [terminal] : pending.events) regEvent(event);
  }
}

/** 无参 = 全部关闭；带 flow = 只移除一个 session。 */
export function regClose(flow) {
  if (flow) {
    earlyEvents.delete(flow);
    sessions.delete(flow);
    if (modalFlow === flow) closeModal();
    modalQueue = modalQueue.filter((q) => q.flow !== flow);
  } else {
    earlyEvents.clear();
    sessions = new Map();
    modalQueue = [];
    closeModal();
  }
  if (!sessions.size && drawer) {
    drawer.remove();
    drawer = null;
    document.body.classList.remove("reg-open");
    return;
  }
  render();
}

export function regEvent(ev) {
  if (!ev || typeof ev !== "object") return;
  if (typeof ev.flow !== "string" || !ev.flow) return;
  const s = sessions.get(ev.flow);
  if (!s) return bufferEarlyEvent(ev);
  // 宿主已收口后，旧页面的在途状态消息不能把成功/失败改回“运行中”。
  if (s.finished && ev.kind !== "log") return;
  switch (ev.kind) {
    case "result":
      s.finished = true;
      s.closed = false;
      s.failed = ev.ok === false;
      s.phase = s.failed ? "failed" : "done";
      s.note = s.failed ? String(ev.error || "") : "";
      clearLinkAsk(s.flow);
      terminate(s.flow, "result", { ok: !s.failed, error: s.note });
      break;
    case "ready":
      pushLog(s, "info", "connected", ev.provider || s.provider);
      break;
    case "page":
      pushLog(s, "info", "page", tr("reg.page", PAGE_KEYS[ev.page] || "other"));
      break;
    case "phase":
      s.phase = ev.phase;
      if (ev.phase === "unsupported") s.failed = true;
      // 一旦离开"等邮件"（链接已打开 / 已回授权页）就立刻收口，
      // 否则后台兜底迟到的链接会把驱动又拽回打开链接那一步。
      if (ev.phase !== "await-email") s.linkStop = true;
      break;
    case "note":
      s.note = !ev.code || ev.code === "clear" ? "" : tr("reg.note", ev.code, { a: ev.a });
      break;
    case "ask":
      if (s.batchEmail) {
        if (ev.ask === "signup") autoSignup(s);
        else autoFetchLink(s);
      } else if (ev.ask === "link") {
        autoFetchLink(s);
      } else {
        enqueueAsk(s.flow, ev.ask);
      }
      break;
    case "log":
      if (ev.code === "submitted") s.submittedAt = Date.now();
      pushLog(s, ev.level || "info", ev.code, ev.a, ev.b);
      break;
    case "closed":
      if (!s.closed) {
        s.closed = true;
        s.note = "";
        pushLog(s, "warn", "windowClosed");
        clearLinkAsk(s.flow);
        terminate(s.flow, "closed");
      }
      break;
    default:
      return;
  }
  render();
}

function pushLog(s, level, code, a, b) {
  pushLogV(s, level, code, { a: a || "", b: b || "" });
}

/** 需要 3 个以上占位符的日志走这里（例如取链的"已等 / 还剩 / 第几次"）。 */
function pushLogV(s, level, code, params) {
  if (!s) return;
  const text = tr("reg.log", code, params || {});
  s.logs.push({ level, text, at: nowLabel(), ts: Date.now(), mail: shortMail(mailOf(s)) });
  if (s.logs.length > MAX_LOGS) s.logs.splice(0, s.logs.length - MAX_LOGS);
}

async function autoSignup(s) {
  const email = s && s.batchEmail;
  if (!email) return;
  try {
    const r = await invoke("pool_get", { email });
    if (sessions.get(s.flow) !== s || s.closed || s.finished) return;
    s.signupEmail = r.email;
    await invoke("reg_input", {
      flow: s.flow,
      ask: "signup",
      value: { name: randName(), email: r.email, password: r.password },
    });
    pushLog(s, "info", "batchSignup", r.email);
  } catch (e) {
    pushLog(s, "warn", "batchSignupFail", stripErr(e));
  }
  render();
}

function pickLink(links) {
  const verify = links.find((u) => /verify|token=|confirm|activ|signup/i.test(u));
  return verify || links[0] || "";
}

function isCredentialError(msg) {
  return /invalid_grant|AADSTS70000|AADSTS70008|AADSTS50173|AADSTS7000215/i.test(String(msg || ""));
}

/** 同一条错误只在首次（或文案变化）记一次，避免 90 秒里刷满重复告警。 */
function logLinkFail(s, msg) {
  if (!s || !msg || s.linkLastFail === msg) return;
  s.linkLastFail = msg;
  pushLog(s, "warn", "fetchLinkErr", msg);
  render();
}

/**
 * 一次取链查询：到点就放弃等待，返回 { value } / { error } / { timedOut }。
 * 单次 Graph 调用最长 25s，不能让它把整个等待窗口拖过截止时间。
 */
function linkQuery(email, sinceMs, flow, waitMs) {
  return new Promise((resolve) => {
    let settled = false;
    const done = (v) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(v);
    };
    const timer = setTimeout(() => done({ timedOut: true }), Math.max(1, waitMs));
    let call;
    try {
      call = invoke("reg_fetch_link", { email, limit: 15, since: sinceMs, flow });
    } catch (e) {
      done({ error: stripErr(e) });
      return;
    }
    Promise.resolve(call).then(
      (value) => done({ value }),
      (e) => done({ error: stripErr(e) }),
    );
  });
}

/**
 * 取链轮询核心：主窗口与"手动框背后的兜底"共用同一套逻辑。
 * 瞬时失败（网络 / 5xx / 超时）只记一条日志并继续等，只有凭据失效才终止流程。
 * 预算按墙钟 deadline 算，剩余不足一轮就不再发起新查询。
 */
async function linkPollCore(s, { deadline, interval, background }) {
  const email = s.signupEmail || "";
  const sinceMs = (s.submittedAt || s.startedAt || Date.now()) - 60_000;
  const beganAt = Date.now();
  s.linkStop = false;
  s.linkBusy = true;
  if (background) s.linkBg = true;
  let tries = 0;
  let fails = 0;
  let fail = "";
  let info = "";
  try {
    while (!s.linkStop && !s.closed && !s.finished && sessions.get(s.flow) === s && s.phase === "await-email") {
      const remaining = deadline - Date.now();
      if (remaining <= LINK_MIN_ROUND_MS) break;
      tries += 1;
      const r = await linkQuery(email, sinceMs, s.flow, remaining);
      if (s.linkStop || s.closed || sessions.get(s.flow) !== s) break;
      if (r.timedOut) {
        pushLog(s, "info", "fetchLinkSlow");
        render();
        break;
      }
      if (r.error) {
        if (isCredentialError(r.error)) {
          toast(t("reg.linkDead"), "err", r.error);
          clearLinkAsk(s.flow);
          terminate(s.flow, "skipped");
          return { link: "", fail: r.error, info, dead: true };
        }
        fails += 1;
        fail = r.error;
        logLinkFail(s, r.error);
      } else {
        const links = (r.value && r.value.links) || [];
        if (links.length) return { link: pickLink(links), fail, info };
        const n = Number(r.value && r.value.scanned) || 0;
        const subj = String((r.value && r.value.newestSubject) || "").trim();
        info = subj ? t("reg.linkNewest", { n, s: subj }) : t("reg.linkEmptyBox", { n });
        fail = "";
        s.linkLastFail = "";
      }
      // 后台兜底不刷屏：只在取到 / 放弃时留痕，正常轮询不写日志。
      if (!background) {
        const elapsed = Math.round((Date.now() - beganAt) / 1000);
        const left = Math.max(0, Math.round((deadline - Date.now()) / 1000));
        if (fails) pushLogV(s, "info", "fetchLinkPollingRetry", { a: elapsed, b: left, c: tries, d: fails });
        else pushLogV(s, "info", "fetchLinkPolling", { a: elapsed, b: left, c: tries });
        render();
      }
      const wait = Math.min(interval, deadline - Date.now());
      if (wait <= 0) break;
      await sleep(wait);
    }
  } finally {
    s.linkBusy = false;
    if (background) s.linkBg = false;
  }
  return { link: "", fail, info };
}

/** 下发链接给驱动；同一链接只下发一次（后台兜底和手动提交可能撞车）。 */
async function pushLink(s, link) {
  if (!s || !link) return false;
  // 已经下发过同一链接就不再重复：驱动收到 input 会立刻回到"打开链接"，
  // 重复下发会把已经走到授权页的流程拽回上一步。
  if (s.pushedLink === link) return false;
  s.pushedLink = link;
  try {
    await invoke("reg_input", { flow: s.flow, ask: "link", value: link });
    pushLog(s, "ok", "fetchLinkFound", link);
    clearLinkAsk(s.flow);
    // 只有真正下发成功才计数：自动取链达上限后转为纯手动。
    s.linkAuto += 1;
    return true;
  } catch (e) {
    // 下发失败（窗口已关等）：保留粘贴框，让用户还能手动补一次；
    // 同一链接不会自动重试，避免弹窗反复开关。
    pushLog(s, "warn", "fetchLinkPushFail", stripErr(e));
    startManualLink(s, "pushFail");
    return false;
  } finally {
    render();
  }
}

/** 主窗口：等满 90 秒墙钟；取不到就转手动，同时保留后台兜底。 */
async function autoFetchLink(s) {
  if (!s || s.linkBusy) return;
  const email = s.signupEmail;
  if (!email) return startManualLink(s, "noEmail");
  if (s.linkAuto >= LINK_MAX_AUTO) return startManualLink(s, "loop");

  pushLog(s, "info", "fetchLinkStart", email);
  render();

  const { link, fail, info, dead } = await linkPollCore(s, {
    deadline: Date.now() + LINK_WAIT_MS,
    interval: LINK_POLL_MS,
    background: false,
  });
  if (dead) return;
  if (link) return pushLink(s, link);
  // 流程已收口或已走过等邮件阶段，就不要再用弹窗打扰用户。
  if (s.linkStop || s.closed || s.finished || sessions.get(s.flow) !== s || s.phase !== "await-email") return;
  startManualLink(s, fail || info || "timeout");
}

/** 转人工：弹粘贴框，同时让后台继续等邮件（取到会自动填入并提交）。 */
function startManualLink(s, reason) {
  if (!s) return;
  if (reason) pushLog(s, "warn", "fetchLinkManual", reason);
  render();
  if (!(modal && modalFlow === s.flow && modalKind === "link")) enqueueAsk(s.flow, "link");
  watchLink(s).catch((e) => console.warn("[reg] 后台取链兜底异常：", e));
}

/** 手动框开着的时候后台继续等邮件；链接晚到时自动填入并关掉弹窗。 */
async function watchLink(s) {
  if (!s || !s.signupEmail || s.linkBusy || s.linkBg || s.closed || s.finished) return;
  if (sessions.get(s.flow) !== s || s.phase !== "await-email") return;
  pushLog(s, "info", "fetchLinkBgStart");
  render();
  const { link } = await linkPollCore(s, {
    deadline: Date.now() + LINK_BG_MAX_MS,
    interval: LINK_BG_POLL_MS,
    background: true,
  });
  // linkStop = 用户已经手贴过 / 流程已走过等邮件阶段：静默收口，别再说"后台等超时"。
  if (s.closed || s.finished || s.linkStop || sessions.get(s.flow) !== s || s.phase !== "await-email") return;
  if (!link) {
    pushLog(s, "warn", "fetchLinkBgGiveup");
    render();
    return;
  }
  await pushLink(s, link);
}

function modeLabel(mode) {
  return tr("reg.mode", mode);
}

function ensureDrawer() {
  if (!drawer) {
    drawer = document.createElement("aside");
    drawer.className = "drw";
    document.body.appendChild(drawer);
  }
  return drawer;
}

function statusOf(s) {
  if (s.closed) return { key: "closed", cls: "off" };
  if (s.failed) return { key: "attention", cls: "warn" };
  if (s.phase === "done") return { key: "done", cls: "ok" };
  return { key: "live", cls: "run" };
}

function stageText(s) {
  if (!s.phase) return "";
  const k = `reg.step.${s.phase}`;
  return has(k) ? t(k) : "";
}

function logLine(l, withMail) {
  const head = withMail && l.mail ? `${l.mail} ` : "";
  return `
    <div class="drw-line ${esc(l.level)}">
      <span class="at">${esc(l.at)}</span>
      <span class="tx">${esc(head + l.text)}</span>
    </div>`;
}

function render() {
  if (!sessions.size) return;
  const root = ensureDrawer();
  document.body.classList.add("reg-open");
  const list = [...sessions.values()];
  const single = list.length === 1;
  const activeN = list.filter((s) => !s.closed && s.phase !== "done" && !s.failed).length;
  const allEnded = list.every((s) => s.closed || s.phase === "done" || s.failed);

  const cardsHtml = list
    .map((s) => {
      const st = statusOf(s);
      const mail = mailOf(s);
      const busy = !s.closed && s.phase !== "done" && !s.failed;
      return `
    <div class="reg-task ${esc(st.cls)}">
      <div class="reg-task-h">
        <span class="dot ${esc(st.cls)}"></span>
        <span class="reg-task-mail" title="${esc(mail || s.flow)}">${esc(mail || shortFlow(s.flow))}</span>
        <span class="chip ${st.cls === "off" ? "off" : st.cls}">${esc(tr("reg.status", st.key))}</span>
        ${stageText(s) ? `<span class="reg-task-stage">${esc(stageText(s))}</span>` : ""}
        <span class="spacer"></span>
        ${busy ? `<button class="btn sm" data-act="reveal" data-flow="${esc(s.flow)}" title="${esc(t("reg.btn.reveal"))}">${ic("play", 11)}</button>` : ""}
        ${busy && s.batchEmail ? `<button class="btn sm" data-act="skip" data-flow="${esc(s.flow)}">${ic("x", 11)} ${esc(t("reg.btn.skip"))}</button>` : ""}
      </div>
      ${s.note ? `<div class="reg-task-note">${ic("alert", 13)}<span>${esc(s.note)}</span></div>` : ""}
      <div class="reg-task-logs">${s.logs.slice(-CARD_LOGS).map((l) => logLine(l, false)).join("")}</div>
    </div>`;
    })
    .join("");

  const allLogs = list
    .flatMap((s) => s.logs)
    .sort((a, b) => a.ts - b.ts)
    .slice(-MAX_LOGS);
  const withMail = !single;

  const singleFlow = single ? list[0].flow : "";
  const barHtml = single
    ? `<div class="bar" style="border-bottom:1px solid var(--line);padding:9px 14px;gap:6px">
        ${list[0].mode === "register" ? "" : `<button class="btn sm" data-act="to-register">${ic("userPlus", 12)} ${esc(t("reg.btn.toRegister"))}</button>`}
        <button class="btn sm" data-act="reveal" data-flow="${esc(singleFlow)}">${ic("play", 12)} ${esc(t("reg.btn.reveal"))}</button>
        <button class="btn sm" data-act="retry" data-flow="${esc(singleFlow)}">${ic("refresh", 12)} ${esc(t("reg.btn.retry"))}</button>
        ${list[0].batchEmail ? `<button class="btn sm" data-act="skip" data-flow="${esc(singleFlow)}">${ic("x", 12)} ${esc(t("reg.btn.skip"))}</button>` : ""}
      </div>`
    : "";

  root.innerHTML = `
    <div class="drw-h">
      <span class="dot ${allEnded ? "ok" : "run"}"></span>
      <span class="drw-title">${esc(t("reg.title"))} · ${esc(modeLabel(list[0].mode))}</span>
      ${list.length > 1 ? `<span class="chip new">${esc(t("reg.runningCount", { n: activeN }))}</span>` : ""}
      <span class="spacer"></span>
      ${list.length > 1 ? `<button class="btn sm" data-act="tile" ${tiling ? "disabled" : ""} title="${esc(t("reg.btn.tile"))}">${ic("tile", 12)} ${esc(t("reg.btn.tile"))}</button>` : ""}
      <button class="iconbtn" data-act="close" title="${esc(t("reg.btn.close"))}" style="color:var(--ink-2)">${ic("x", 14)}</button>
    </div>
    ${barHtml}
    <div class="reg-tasks">${cardsHtml}</div>
    <div class="drw-log" id="reg-log">${allLogs.map((l) => logLine(l, withMail)).join("") || `<div class="drw-line"><span class="tx" style="color:var(--ink-3)">${esc(t("reg.waiting"))}</span></div>`}</div>
    ${list.every((s) => s.closed) ? `<div style="padding:14px"><button class="btn g" data-act="close">${esc(t("reg.btn.close"))}</button></div>` : ""}
  `;

  const log = root.querySelector("#reg-log");
  if (log) log.scrollTop = log.scrollHeight;

  root.querySelectorAll("[data-act]").forEach((b) => {
    b.addEventListener("click", () => onAction(b.dataset.act, b.dataset.flow));
  });
}

function doSkip(flow) {
  const s = sessions.get(flow);
  if (s && !s.failed && !s.closed && s.phase !== "done") {
    s.failed = true;
    s.note = "";
    pushLog(s, "warn", "skipFlow");
    render();
  }
  terminate(flow, "skipped");
}

async function onAction(act, flow) {
  if (act === "close") return regClose();
  if (act === "skip") return doSkip(flow);
  if (act === "tile") {
    if (tiling) return;
    tiling = true;
    render();
    try {
      await invoke("reg_tile_windows");
    } catch (e) {
      toast(stripErr(e), "err");
    } finally {
      tiling = false;
      render();
    }
    return;
  }
  try {
    if (act === "reveal") await invoke("reg_action", { flow, action: "reveal" });
    else if (act === "retry") {
      const s = sessions.get(flow);
      if (s) s.failed = false;
      await invoke("reg_action", { flow, action: "retry" });
    } else if (act === "to-register") {
      const m = await invoke("reg_set_mode", { flow, mode: "register" });
      const s = sessions.get(flow);
      if (s) {
        s.mode = m;
        s.note = "";
        s.logs.push({ level: "info", text: tr("reg.log", "modeSwitch", { a: modeLabel(m) }), at: nowLabel(), ts: Date.now(), mail: shortMail(mailOf(s)) });
      }
    }
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
  render();
}

function closeModal() {
  if (modal) {
    modal.remove();
    modal = null;
  }
  modalKind = null;
  modalFlow = null;
}

/**
 * 链接已经有了结果（自动取到 / 流程结束）：撤掉该流程还排着队、或正开着的手动粘贴请求，
 * 免得自动填入之后又弹一个"请手动粘贴"的空框。
 */
function clearLinkAsk(flow) {
  const before = modalQueue.length;
  modalQueue = modalQueue.filter((q) => !(q.flow === flow && q.kind === "link"));
  const showing = !!modal && modalFlow === flow && modalKind === "link";
  if (showing) closeModal();
  if (showing || modalQueue.length !== before) pumpModal();
}

/** 单例 modal 的排队：并发任务的“人工填单”请求按序弹出。 */
function enqueueAsk(flow, kind) {
  if (modal && modalFlow === flow && modalKind === kind) return;
  if (modalQueue.some((q) => q.flow === flow && q.kind === kind)) return;
  modalQueue.push({ flow, kind });
  pumpModal();
}

function pumpModal() {
  if (modal) return;
  while (modalQueue.length) {
    const next = modalQueue.shift();
    if (!sessions.has(next.flow)) continue;
    openAsk(next.flow, next.kind);
    return;
  }
}

function field(id, label, type, placeholder, extra = "") {
  return `
    <label class="fld">
      <span>${esc(label)}</span>
      <input id="${id}" class="inp" type="${type}" placeholder="${esc(placeholder)}"
        autocomplete="off" spellcheck="false" ${extra}>
    </label>`;
}

function randName() {
  const A = "abcdefghijklmnopqrstuvwxyz";
  let s = "";
  for (let i = 0; i < 6; i++) s += A[Math.floor(Math.random() * A.length)];
  return s;
}

async function importAccounts(mask) {
  const err = mask.querySelector(".errline");
  const btn = mask.querySelector(".reg-import-btn");
  const sel = mask.querySelector(".reg-acct");
  btn.disabled = true;
  try {
    const r = await invoke("pool_list");
    const list = ((r && r.accounts) || []).filter((a) => a.status !== "verified");
    if (!list.length) {
      err.textContent = t("mb.nothingToVerify");
      return;
    }
    err.textContent = "";
    sel.innerHTML = list
      .map((a) => `<option value="${esc(a.email)}">${esc(a.email)}</option>`)
      .join("");
    sel.hidden = false;
    sel.onchange = () => fillFromPool(mask, sel.value);
    fillFromPool(mask, list[0].email);
  } catch (e) {
    err.textContent = stripErr(e);
  } finally {
    btn.disabled = false;
  }
}

async function fillFromPool(mask, email) {
  try {
    const r = await invoke("pool_get", { email });
    const em = mask.querySelector("#reg-email");
    const pw = mask.querySelector("#reg-pw");
    if (em) em.value = r.email || "";
    if (pw) pw.value = r.password || "";
  } catch (e) {
    const err = mask.querySelector(".errline");
    if (err) err.textContent = stripErr(e);
  }
}

function openAsk(flow, kind) {
  closeModal();
  modalFlow = flow;
  modalKind = kind;
  const isSignup = kind === "signup";
  const mask = document.createElement("div");
  mask.className = "ov reg bottom";
  mask.innerHTML = `
    <div class="sheet" role="dialog" aria-modal="true">
      <div class="grabber"></div>
      <div class="mhead">${ic(isSignup ? "userPlus" : "mail", 16)} <span>${esc(tr("reg.ask", `${kind}.label`))}</span></div>
      <div class="mbody">
        <div class="mnote" style="margin-bottom:14px">${esc(tr("reg.ask", `${kind}.hint`))}</div>
        ${isSignup
          ? `<div class="importrow">
               <button type="button" class="btn sm reg-import-btn">${ic("import", 13)} ${esc(t("reg.ask.signup.import"))}</button>
               <select class="inp reg-acct" hidden></select>
             </div>`
            + field("reg-name", t("reg.ask.signup.name"), "text", t("reg.ask.signup.namePh"))
            + field("reg-email", t("reg.ask.signup.email"), "email", t("reg.ask.signup.emailPh"))
            + field("reg-pw", t("reg.ask.signup.password"), "password", t("reg.ask.signup.passwordPh"))
          : `<div class="importrow">
               <button type="button" class="btn sm reg-paste-btn">${ic("import", 13)} ${esc(t("reg.ask.link.paste"))}</button>
             </div>
             <label class="fld">
               <span>${esc(t("reg.ask.link.ph"))}</span>
               <textarea id="reg-link" class="inp" rows="3"
                 placeholder="${esc(t("reg.ask.link.ph"))}" spellcheck="false"></textarea>
             </label>`}
        <div class="errline"></div>
      </div>
      <div class="mfoot">
        <button class="btn g reg-cancel">${esc(t("common.cancel"))}</button>
        <button class="btn p reg-go">${ic("check", 14)} ${esc(t("reg.ask.submit"))}</button>
      </div>
    </div>`;
  document.body.appendChild(mask);
  modal = mask;

  if (isSignup) {
    const nameEl = mask.querySelector("#reg-name");
    if (nameEl && !nameEl.value) nameEl.value = randName();
    const importBtn = mask.querySelector(".reg-import-btn");
    if (importBtn) importBtn.addEventListener("click", () => importAccounts(mask));
  }

  const err = mask.querySelector(".errline");

  // 邮件晚到时手贴很烦：能读剪贴板就一键填入；读不到就让用户按 Ctrl+V，不算错误。
  const pasteBtn = mask.querySelector(".reg-paste-btn");
  if (pasteBtn) {
    if (typeof navigator === "undefined" || !navigator.clipboard || !navigator.clipboard.readText) {
      pasteBtn.hidden = true;
    } else {
      pasteBtn.addEventListener("click", async () => {
        const ta = mask.querySelector("#reg-link");
        try {
          const text = String((await navigator.clipboard.readText()) || "").trim();
          if (ta && text) {
            ta.value = text;
            err.textContent = "";
          } else {
            err.textContent = t("reg.err.clipboard");
          }
        } catch (e) {
          err.textContent = t("reg.err.clipboard");
        }
        if (ta) ta.focus();
      });
    }
  }

  const close = () => {
    closeModal();
    pumpModal();
  };
  mask.addEventListener("click", (e) => { if (e.target === mask) close(); });
  mask.querySelector(".reg-cancel").addEventListener("click", close);

  const first = mask.querySelector("input, textarea");
  if (first) first.focus();

  const submit = async () => {
    const s = sessions.get(flow);
    if (!s) {
      closeModal();
      return;
    }
    let value;
    if (isSignup) {
      const name = mask.querySelector("#reg-name").value.trim();
      const email = mask.querySelector("#reg-email").value.trim();
      const password = mask.querySelector("#reg-pw").value;
      if (!/^[^\s@]+@[^\s@]+\.[^\s@]{2,}$/.test(email)) return (err.textContent = t("reg.err.email"));
      if (password.length < 6) return (err.textContent = t("reg.err.password"));
      s.signupEmail = email;
      value = { name, email, password };
    } else {
      const link = mask.querySelector("#reg-link").value.trim();
      if (!/^https?:\/\//i.test(link)) return (err.textContent = t("reg.err.link"));
      s.linkAuto = LINK_MAX_AUTO;
      // 手贴的链接算已下发：停掉后台兜底，避免同一份链接被再下发一次。
      s.pushedLink = link;
      s.linkStop = true;
      value = link;
    }
    const go = mask.querySelector(".reg-go");
    go.disabled = true;
    try {
      await invoke("reg_input", { flow, ask: kind, value });
      closeModal();
      pushLog(s, "info", isSignup ? "submittedSignup" : "submittedLink");
      render();
      if (drawer) drawer.scrollIntoView({ block: "end" });
      pumpModal();
    } catch (e) {
      err.textContent = String(e).replace(/^[a-z_]+:/, "");
      go.disabled = false;
    }
  };
  mask.querySelector(".reg-go").addEventListener("click", submit);
  mask.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) submit();
    else if (e.key === "Enter" && !isSignup) submit();
  });
}

listen("reg://event", (ev) => regEvent(ev.payload)).catch(() => {});
