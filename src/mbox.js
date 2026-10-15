import { invoke } from "@tauri-apps/api/core";
import { t } from "./i18n.js";
import { ic } from "./icons.js";
import { esc, toast, openLineModal } from "./ui.js";
import { regStart, regBatchSet, regClose } from "./reg.js";

let rerender = () => {};
export function setMboxRerender(fn) {
  rerender = fn;
}

const tr = (group, key) => t(`${group}.${key}`);

const M = {
  list: [],
  filter: "all",
  sel: new Set(),
  conc: 2,
  batch: { running: false, concurrency: 2, starting: 0, pumping: false, queue: [], total: 0, done: 0, active: new Map(), results: [] },
  lastRun: null,
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** 并发数从 main.js（state/settings）同步；运行中不可改，下一批生效。 */
export function setMboxConc(n) {
  const v = Math.min(5, Math.max(1, Number(n) || 2));
  M.conc = v;
  if (!M.batch.running) M.batch.concurrency = v;
}

export function mboxRunning() {
  return M.batch.running;
}

export function mboxStats() {
  const unverified = M.list.filter((a) => a.status !== "verified").length;
  return { total: M.list.length, unverified };
}

export async function mboxLoad() {
  try {
    const r = await invoke("pool_list");
    M.list = (r && r.accounts) || [];
  } catch {
    M.list = [];
  }
  const live = new Set(M.list.map((a) => a.email));
  for (const e of [...M.sel]) if (!live.has(e)) M.sel.delete(e);
}

function filtered() {
  if (M.filter === "new") return M.list.filter((a) => a.status !== "verified");
  if (M.filter === "verified") return M.list.filter((a) => a.status === "verified");
  return M.list;
}

function badge(a) {
  const map = { verified: ["ok", "verified"], failed: ["err", "failed"], invalid: ["err", "invalid"] };
  const [k, key] = map[a.status] || ["new", "new"];
  const note = a.note ? ` title="${esc(a.note)}"` : "";
  return `<span class="chip ${k}"${note}>${esc(tr("mb.status", key))}</span>`;
}

// 邮箱来源标记：只标 cf（现场建的临时地址），Outlook 是默认来源不标，免得列表变花。
function kindChip(a) {
  if (a.kind !== "cf") return "";
  return `<span class="chip acc" title="${esc(t("mb.cfKindTip"))}">${esc(t("mb.cfKind"))}</span>`;
}

export function mboxPage() {
  const b = M.batch;
  const verifyN = selectedNewEmails().length; // 「批量验证」仅覆盖选中项里的待验证（new）
  const verified = M.list.filter((a) => a.status === "verified").length;
  const attention = M.list.filter((a) => a.status === "failed" || a.status === "invalid").length;
  const activeList = [...b.active.values()].map((v) => v.email);
  const progress = b.running
    ? `<div class="prog">
         <div class="prog-bar"><i style="width:${b.total ? Math.round((b.done / b.total) * 100) : 0}%"></i></div>
         <div class="prog-txt">${esc(t("mb.batch.progress", { done: b.done, total: b.total }))}${activeList.length ? ` · ${esc(t("mb.batch.active", { list: activeList.join(" · ") }))}` : ""}</div>
       </div>`
    : "";
  const rows = filtered();
  const listHtml = rows.length
    ? rows
        .map(
          (a) => `
      <div class="row" data-email="${esc(a.email)}">
        <input type="checkbox" class="cbx mb-check" aria-label="${esc(a.email)}" data-email="${esc(a.email)}" ${M.sel.has(a.email) ? "checked" : ""}
          ${b.running ? "disabled" : ""}>
        <span class="mail" title="${esc(a.email)}">${esc(a.email)}</span>
        ${badge(a)}
        ${kindChip(a)}
        <span class="when">${esc(a.verified_at || "")}</span>
        ${a.status === "invalid" ? `<button class="btn sm mb-reauth" data-reauth="${esc(a.email)}">${esc(t("mb.reauth"))}</button>` : ""}
        <button class="iconbtn mb-edit" data-edit="${esc(a.email)}" title="${esc(t("mb.update"))}" ${b.running ? "disabled" : ""}>${ic("pen", 14)}</button>
        <button class="iconbtn mb-del" data-del="${esc(a.email)}" title="${esc(t("mb.remove"))}" ${b.running ? "disabled" : ""}>${ic("x", 14)}</button>
      </div>`
        )
        .join("")
    : `<div class="empty">
         <div class="glyph">${ic("empty", 34)}</div>
         <div>${esc(t("mb.empty"))}</div>
         <div class="fmt">
           <div class="fmt-hint">${esc(t("mb.formatHint"))}</div>
           <code>email----password----client_id----refresh_token</code>
         </div>
       </div>`;

  return `
    <div class="content-h">
      <h1>${t("mb.title")}</h1>
      <span class="sub">${t("m.count", { count: M.list.length })}</span>
      <span class="spacer"></span>
      <div class="seg mb-filter" role="group" aria-label="${t("mb.title")}">
        ${["all", "new", "verified"]
          .map((f) => `<button class="${M.filter === f ? "on" : ""}" aria-pressed="${M.filter === f}" click="actions.mboxFilter('${f}')">${tr("mb.filter", f)}</button>`)
          .join("")}
      </div>
    </div>
    <dl class="mail-overview">
      <div><dt>${t("mb.overview.total")}</dt><dd>${M.list.length}</dd></div>
      <div><dt>${t("mb.status.new")}</dt><dd>${M.list.length - verified - attention}</dd></div>
      <div class="verified"><dt>${t("mb.status.verified")}</dt><dd>${verified}</dd></div>
      <div class="attention"><dt>${t("mb.overview.attention")}</dt><dd>${attention}</dd></div>
    </dl>
    <section class="toolbar">
      <button class="btn p" click="actions.mboxImport()" ${b.running ? "disabled" : ""}>${ic("import", 15)} ${t("mb.import")}</button>
      <select class="sel mb-cf-n" id="mb-cf-n" title="${esc(t("mb.cfCount"))}" aria-label="${esc(t("mb.cfCount"))}" ${b.running ? "disabled" : ""}>
        ${[1, 3, 5, 10].map((n) => `<option value="${n}">${n}</option>`).join("")}
      </select>
      <button class="btn" click="actions.mboxCfCreate()" title="${esc(t("mb.cfCreateTip"))}" ${b.running ? "disabled" : ""}>${ic("mail", 14)} ${t("mb.cfCreate")}</button>
      <button class="btn" click="actions.mboxExport()" ${M.list.length && !b.running ? "" : "disabled"}>${ic("export", 14)} ${t("mb.export")}</button>
      <label class="conc-wrap" title="${esc(t("mb.concTip"))}">
        <span>${esc(t("mb.conc"))}</span>
        <select class="sel" id="mb-conc" aria-label="${esc(t("mb.conc"))}" ${b.running ? "disabled" : ""}>
          ${[1, 2, 3, 4, 5].map((n) => `<option value="${n}" ${M.conc === n ? "selected" : ""}>${n}</option>`).join("")}
        </select>
      </label>
      <button class="btn" click="actions.mboxVerify()" ${!verifyN || b.running ? "disabled" : ""}>${ic("play", 14)} ${t("mb.verify")}${verifyN ? ` (${verifyN})` : ""}</button>
      <button class="btn" click="actions.mboxStop()" ${b.running ? "" : "disabled"}>${ic("power", 14)} ${t("mb.stop")}</button>
      <span class="spacer"></span>
      <button class="btn d" click="actions.mboxDelete()" ${M.sel.size && !b.running ? "" : "disabled"}>${ic("trash", 14)} ${t("mb.delete")}${M.sel.size ? ` (${M.sel.size})` : ""}</button>
    </section>
    ${!b.running && M.lastRun
      ? (() => {
          const rs = M.lastRun.results || [];
          const ok = rs.filter((r) => r.ok).length;
          const fails = rs.filter((r) => !r.ok);
          return `<div class="mb-result">
            <span class="mb-result-txt">${esc(t("mb.batch.done", { ok, total: M.lastRun.total }))}</span>
            <span class="spacer"></span>
            ${fails.length ? `<button class="btn sm" click="actions.mboxRetryFailed()">${ic("refresh", 13)} ${esc(t("mb.retryFailed", { n: fails.length }))}</button>` : ""}
            <button class="iconbtn" click="actions.mboxDismissResult()" title="${esc(t("mb.dismiss"))}" style="color:var(--label-2)">${ic("x", 14)}</button>
          </div>
          ${fails.length ? `<div class="mb-fails">${fails.map((r) => `<div class="mb-fail"><span class="mb-fail-mail">${esc(r.email)}</span><span class="mb-fail-why">${esc(r.error || t("m.unknownErr"))}</span></div>`).join("")}</div>` : ""}`;
        })()
      : ""}
    ${M.list.length && !b.running ? `
      <div class="mb-fmt">
        <span>${esc(t("mb.formatLabel"))}</span>
        <code>email----password----client_id----refresh_token</code>
      </div>
      <div class="mb-tip">${ic("alert", 13)}<span>${esc(t("mb.tip"))}</span></div>` : ""}
    ${progress}
    <div class="list">
      ${rows.length
        ? `<div class="list-head">
             <label class="selall">
               <input type="checkbox" class="cbx" id="mb-all" ${selAllOn(rows) ? "checked" : ""} ${b.running ? "disabled" : ""}>
               <span>${t("mb.selectAll")}</span>
             </label>
             <span class="selinfo">${M.sel.size ? t("mb.selected", { n: M.sel.size }) : ""}</span>
             <span class="spacer"></span>
             ${M.sel.size ? `<button class="btn g sm" click="actions.mboxSelectNone()">${t("mb.selectNone")}</button>` : ""}
           </div>`
        : ""}
      <div class="group">${listHtml}</div>
    </div>`;
}

/** 当前筛选视图的选择统计：已选行数 / 总行数（全选态与部分选中态共用） */
function selStats(rows) {
  const on = rows.reduce((n, a) => n + (M.sel.has(a.email) ? 1 : 0), 0);
  return { on, total: rows.length };
}

function selAllOn(rows) {
  const s = selStats(rows);
  return s.total > 0 && s.on === s.total;
}

/** 同步「全选」复选框：全选 / 部分选中（indeterminate）/ 未选。DOM 挂载后调用。 */
export function mboxHeadSync() {
  const all = document.getElementById("mb-all");
  if (!all) return;
  const s = selStats(filtered());
  all.checked = s.total > 0 && s.on === s.total;
  all.indeterminate = s.on > 0 && s.on < s.total;
}

/** 选中集中「待验证」（status === "new"）的邮箱 —— 批量验证与其按钮计数共用 */
function selectedNewEmails() {
  return [...M.sel].filter((e) => {
    const a = M.list.find((x) => x.email === e);
    return a && a.status === "new";
  });
}


export function mboxFilter(f) {
  M.filter = f;
  rerender();
}

export function mboxToggle(email, on) {
  if (on) M.sel.add(email);
  else M.sel.delete(email);
}

/** 全选 / 取消全选：作用于当前筛选视图的全部行（与行级勾选一致，不限状态）。 */
export function mboxSelectAll(on) {
  if (M.batch.running) return;
  for (const a of filtered()) {
    if (on) M.sel.add(a.email);
    else M.sel.delete(a.email);
  }
  rerender();
}

export function mboxSelectNone() {
  M.sel.clear();
  rerender();
}

export function mboxToggleRow(email) {
  if (M.batch.running || !email) return;
  if (M.sel.has(email)) M.sel.delete(email);
  else M.sel.add(email);
  rerender();
}

export async function mboxImport() {
  try {
    const r = await invoke("pool_import_pick");
    if (!r || !r.picked) return;
    await mboxLoad();
    rerender();
    if (r.added > 0) toast(t("mb.importOk", { added: r.added, skipped: r.skipped }), "ok");
    else if (r.parsed === 0) toast(t("mb.importEmpty"), "err");
    else toast(t("mb.importDup", { skipped: r.skipped }), "warn");
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
}

/** 现场建临时地址（cloudflare_temp_email）并直接入库：建完就能勾选去跑注册。 */
export async function mboxCfCreate() {
  if (M.batch.running) return;
  const n = Number(document.querySelector("#mb-cf-n")?.value || 1) || 1;
  try {
    const r = await invoke("cf_create", { count: n });
    const created = (r && r.created) || [];
    const errors = (r && r.errors) || [];
    await mboxLoad();
    rerender();
    if (created.length) {
      const head = t("mb.cfCreated", { n: created.length }) + (errors.length ? ` · ${t("mb.cfPartial", { n: errors.length })}` : "");
      const detail = [created.map((c) => c.email).join("\n"), errors[0] || ""].filter(Boolean).join("\n");
      toast(head, errors.length ? "warn" : "ok", detail);
    } else {
      toast(t("mb.cfNone"), "err", errors[0] || t("mb.cfNoConfig"));
    }
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
}

export async function mboxRemove(email) {
  try {
    await invoke("pool_remove", { email });
    M.sel.delete(email);
    await mboxLoad();
    rerender();
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
}

export function mboxSelection() {
  return [...M.sel];
}

export async function mboxDelete() {
  const emails = [...M.sel];
  if (!emails.length) return 0;
  try {
    const r = await invoke("pool_remove_many", { emails });
    M.sel.clear();
    await mboxLoad();
    rerender();
    toast(t("mb.deleted", { n: r.removed }), "ok");
    return r.removed;
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
    return 0;
  }
}

export async function mboxExport() {
  try {
    const r = await invoke("pool_export");
    if (!r || !r.picked) return;
    toast(t("m.exportSavedAll", { count: r.count }), "ok", r.path);
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
}

export function mboxUpdateLine(email) {
  openLineModal({
    title: t("mb.updateTitle", { email }),
    hint: t("mb.updateHint"),
    label: t("mb.updateLine"),
    placeholder: "email----password----client_id----refresh_token",
    onSubmit: async (line) => {
      const r = await invoke("pool_upsert", { line });
      await mboxLoad();
      rerender();
      toast(t("mb.updated", { email: r.email }), "ok");
    },
  });
}

export async function mboxReauth(email) {
  let info;
  try {
    info = await invoke("outlook_reauth_begin", { email });
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
    return;
  }
  const ov = document.createElement("div");
  ov.className = "ov reauth";
  ov.innerHTML = `
    <div class="modal" role="dialog" aria-modal="true">
      <div class="mhead">${ic("lock", 16)} <span>${esc(t("mb.reauthTitle", { email }))}</span></div>
      <div class="mbody">
        <div class="mnote" style="margin-bottom:12px">${esc(t("mb.reauthHint"))}</div>
        <div class="reauth-code">${esc(info.user_code)}</div>
        <div class="line" style="margin-top:10px">
          <button class="btn g ra-open">${ic("export", 14)} ${esc(t("mb.reauthOpen"))}</button>
          <button class="btn g ra-copy">${esc(t("mb.reauthCopy"))}</button>
        </div>
        <div class="errline ra-err"></div>
      </div>
      <div class="mfoot"><button class="btn g ra-close">${t("common.cancel")}</button></div>
    </div>`;
  document.body.appendChild(ov);
  const errEl = ov.querySelector(".ra-err");
  let stopped = false;
  const close = () => {
    stopped = true;
    ov.remove();
  };
  ov.querySelector(".ra-open").addEventListener("click", () => invoke("open_external", { url: info.verification_uri }).catch(() => {}));
  ov.querySelector(".ra-copy").addEventListener("click", () => { navigator.clipboard?.writeText(info.user_code).catch(() => {}); });
  ov.querySelector(".ra-close").addEventListener("click", close);
  ov.addEventListener("click", (e) => { if (e.target === ov) close(); });

  const gap = Math.max(3, info.interval || 5) * 1000;
  const max = Math.ceil((info.expires_in || 900) / Math.max(3, info.interval || 5));
  let tries = 0;
  const tick = async () => {
    if (stopped) return;
    tries += 1;
    if (tries > max) {
      errEl.textContent = t("mb.reauthExpired");
      return;
    }
    try {
      const r = await invoke("outlook_reauth_poll", { email, clientId: info.client_id, deviceCode: info.device_code });
      if (!r.pending) {
        toast(t("mb.reauthOk", { email }), "ok");
        close();
        await mboxLoad();
        rerender();
        return;
      }
    } catch (e) {
      const msg = String(e).replace(/^[a-z_]+:/, "");
      if (/expired|declined|denied|bad_verification|not.?found|invalid/i.test(msg)) {
        errEl.textContent = msg;
        return;
      }
    }
    setTimeout(tick, gap);
  };
  setTimeout(tick, 1500);
}


export async function mboxVerify() {
  if (M.batch.running) return;
  const emails = selectedNewEmails();
  if (!emails.length) {
    toast(t("mb.nothingToVerify"), "warn");
    return;
  }
  regClose(); // 清掉上一批遗留的面板，重新开始
  M.batch = {
    running: true,
    concurrency: M.conc,
    starting: 0,
    pumping: false,
    queue: emails.slice(),
    total: emails.length,
    done: 0,
    active: new Map(),
    results: [],
  };
  rerender();
  pump();
}

/** 单批次只允许一个补位循环；错峰等待和初始化也占用并发名额。 */
async function pump(b = M.batch) {
  if (!b.running || b !== M.batch || b.pumping) return;
  b.pumping = true;
  try {
    while (b.running && b === M.batch && b.active.size + b.starting < b.concurrency && b.queue.length) {
      await startOne(b);
    }
    if (b.running && b === M.batch && !b.active.size && !b.starting && !b.queue.length) finishBatch(b);
  } finally {
    b.pumping = false;
  }
}

async function startOne(b) {
  const email = b.queue.shift();
  if (!email) return;
  b.starting += 1;
  try {
    // 错峰启动（并发 >1 时）：1.2s 起跳 + 随机抖动，避免同一秒齐发。
    if (b.concurrency > 1) await sleep(1200 + Math.floor(Math.random() * 800));
    if (!b.running || b !== M.batch) return;
    let flow = "";
    try {
      const r = await invoke("oauth_begin", { provider: "zai", mode: "register", batch: true, email });
      flow = String((r && r.flow) || "");
      if (!flow) throw new Error(t("m.unknownErr"));
    } catch (e) {
      if (!b.running || b !== M.batch) return;
      const msg = String(e).replace(/^[a-z_]+:/, "");
      b.done += 1;
      b.results.push({ email, ok: false, error: msg });
      invoke("pool_mark_verified", { email, ok: false, note: msg }).catch(() => {});
      mboxLoad().then(rerender);
      return;
    }
    // 停止无法撤回在途 IPC；只关闭迟到的旧 flow，不影响随后启动的新批次。
    if (!b.running || b !== M.batch) {
      await invoke("reg_close_window", { flow }).catch(() => {});
      return;
    }
    b.active.set(flow, { email });
    regStart(flow, "register", "zai");
    regBatchSet(flow, email);
    rerender();
  } finally {
    b.starting -= 1;
  }
}

/** 单任务收口（按 flow 配对）：OAuth 完成 / 跳过 / 关窗 共用。 */
export function mboxOnFlowDone(flow, ok, note) {
  const b = M.batch;
  if (!b.running || !flow) return false;
  const info = b.active.get(flow);
  if (!info) return false;
  b.active.delete(flow);
  b.done += 1;
  b.results.push({ email: info.email, ok, error: note || "" });
  invoke("pool_mark_verified", { email: info.email, ok, note: note || null }).catch(() => {});
  mboxLoad().then(rerender);
  setTimeout(() => pump(b), 400);
  return true;
}

/** reg.js 的终止回调（skip 按钮 / 窗口被用户关掉）：只对属于本批次的 flow 收口。 */
export function mboxOnTerminated(flow, reason) {
  const b = M.batch;
  if (!b.running || !b.active.has(flow)) return;
  if (reason === "skipped") {
    const info = b.active.get(flow);
    if (info) toast(t("mb.batch.skippedToast", { email: info.email }), "warn");
  }
  const note = reason === "closed" ? t("mb.batch.closedFail") : t("mb.batch.skipped");
  mboxOnFlowDone(flow, false, note);
}

export function mboxStop() {
  const b = M.batch;
  if (!b.running) return;
  b.running = false;
  const flows = [...b.active.keys()];
  b.active.clear();
  b.queue.length = 0;
  for (const flow of flows) invoke("reg_close_window", { flow }).catch(() => {});
  toast(t("mb.batch.stopped"), "warn");
  rerender();
}

function finishBatch(b) {
  const ok = b.results.filter((r) => r.ok).length;
  b.running = false;
  M.lastRun = { total: b.total, results: b.results.slice() };
  toast(t("mb.batch.done", { ok, total: b.total }), ok === b.total ? "ok" : "warn");
  rerender();
}

export async function mboxRetryFailed() {
  const fails = (M.lastRun?.results || []).filter((r) => !r.ok).map((r) => r.email).filter(Boolean);
  if (!fails.length) return;
  for (const email of fails) {
    await invoke("pool_reset", { email }).catch(() => {});
  }
  await mboxLoad();
  M.sel = new Set(fails);
  M.lastRun = null;
  rerender();
  await mboxVerify();
}

export function mboxDismissResult() {
  M.lastRun = null;
  rerender();
}
