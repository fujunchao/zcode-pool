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

const LINK_POLL_MS = 3000;
const LINK_POLL_TRIES = 30;
const LINK_MAX_AUTO = 3;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const shortMail = (s) => String(s || "").split("@")[0] || "";

/** 并发注册：每个 OAuth 流程（flow）一个 session，并行并存。 */
let sessions = new Map();
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

function terminate(flow, reason) {
  if (onTerminated) onTerminated(flow, reason);
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

export function regStart(flow, mode, provider) {
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
    batchEmail: "",
    linkAuto: 0,
    submittedAt: 0,
    linkBusy: false,
  });
  pushLog(sessions.get(flow), "info", "connected", provider);
  render();
}

export function regBatchSet(flow, email) {
  const s = sessions.get(flow);
  if (s) s.batchEmail = email || "";
}

/** 无参 = 全部关闭；带 flow = 只移除一个 session。 */
export function regClose(flow) {
  if (flow) {
    sessions.delete(flow);
    if (modalFlow === flow) closeModal();
    modalQueue = modalQueue.filter((q) => q.flow !== flow);
  } else {
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
  const s = sessions.get(ev.flow);
  if (!s) return;
  // 宿主已收口后，旧页面的在途状态消息不能把成功/失败改回“运行中”。
  if (s.finished && ev.kind !== "log") return;
  switch (ev.kind) {
    case "result":
      s.finished = true;
      s.closed = false;
      s.failed = ev.ok === false;
      s.phase = s.failed ? "failed" : "done";
      s.note = s.failed ? String(ev.error || "") : "";
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
        terminate(s.flow, "closed");
      }
      break;
    default:
      return;
  }
  render();
}

function pushLog(s, level, code, a, b) {
  if (!s) return;
  const text = tr("reg.log", code, { a: a || "", b: b || "" });
  s.logs.push({ level, text, at: nowLabel(), ts: Date.now(), mail: shortMail(mailOf(s)) });
  if (s.logs.length > MAX_LOGS) s.logs.splice(0, s.logs.length - MAX_LOGS);
}

async function autoSignup(s) {
  const email = s && s.batchEmail;
  if (!email) return;
  try {
    const r = await invoke("pool_get", { email });
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

async function autoFetchLink(s) {
  if (!s || s.linkBusy) return;
  const email = s.signupEmail;
  if (!email) return startManualLink(s, "noEmail");
  if (s.linkAuto >= LINK_MAX_AUTO) return startManualLink(s, "loop");

  s.linkBusy = true;
  pushLog(s, "info", "fetchLinkStart", email);
  render();

  const anchor = s.submittedAt || s.startedAt || Date.now();
  const sinceMs = anchor - 60_000;

  let link = "";
  let fail = "";
  let info = "";
  for (let i = 0; i < LINK_POLL_TRIES && !s.closed; i++) {
    try {
      const r = await invoke("reg_fetch_link", { email, limit: 15, since: sinceMs, flow: s.flow });
      const links = (r && r.links) || [];
      if (links.length) {
        link = pickLink(links);
        break;
      }
      const n = Number(r && r.scanned) || 0;
      const subj = String((r && r.newestSubject) || "").trim();
      info = subj ? t("reg.linkNewest", { n, s: subj }) : t("reg.linkEmptyBox", { n });
      fail = "";
    } catch (e) {
      fail = stripErr(e);
      pushLog(s, "warn", "fetchLinkErr", fail);
      if (isCredentialError(fail)) {
        toast(t("reg.linkDead"), "err", fail);
        terminate(s.flow, "skipped");
        return;
      }
      break;
    }
    pushLog(s, "info", "fetchLinkPolling", String(Math.round(((i + 1) * LINK_POLL_MS) / 1000)), String(i + 1));
    render();
    await sleep(LINK_POLL_MS);
  }
  s.linkBusy = false;

  if (!link) return startManualLink(s, fail || info || "timeout");

  s.linkAuto += 1;
  try {
    await invoke("reg_input", { flow: s.flow, ask: "link", value: link });
    pushLog(s, "ok", "fetchLinkFound", link);
  } catch (e) {
    pushLog(s, "warn", "fetchLinkPushFail", stripErr(e));
    startManualLink(s, "pushFail");
  }
  render();
}

function startManualLink(s, reason) {
  if (!s) return;
  if (modal && modalFlow === s.flow && modalKind === "link") return;
  if (reason) pushLog(s, "warn", "fetchLinkManual", reason);
  render();
  enqueueAsk(s.flow, "link");
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
          : `<label class="fld">
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
