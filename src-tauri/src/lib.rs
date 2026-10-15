pub mod cli;
pub mod i18n;
mod cfmail;
mod claim;
mod driver;
mod flowlog;
mod gateway;
mod graph;
mod oauth;
mod pool;
mod prompt;
mod quota;
mod store;
mod usage;
mod zcrypto;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use store::*;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::AutoLaunchManager;
use tauri_plugin_dialog::DialogExt;

const TRAY_ID: &str = "main";

static STORE_LOCK: Mutex<()> = Mutex::new(());

struct PendingClaim {
    account_id: String,
    account_name: String,
    plan_id: String,
    plan_name: String,
    credentials: Value,
    config: Option<Value>,
    device_mid: String,
}

static PENDING_CLAIM: Mutex<Option<PendingClaim>> = Mutex::new(None);

fn pending_guard() -> std::sync::MutexGuard<'static, Option<PendingClaim>> {
    match PENDING_CLAIM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    match STORE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

fn tray_menu_inner(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let paths = Paths::detect();
    let state = match store::get_state(&paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tray state error: {e}");
            return MenuBuilder::new(app)
                .item(&MenuItem::with_id(app, "show", &i18n::tr("tray.show"), true, None::<&str>)?)
                .item(&MenuItem::with_id(app, "quit", &i18n::tr("tray.quit"), true, None::<&str>)?)
                .build();
        }
    };

    let b = MenuBuilder::new(app)
        .item(&MenuItem::with_id(app, "show", &i18n::tr("tray.show"), true, None::<&str>)?)
        .item(&MenuItem::with_id(app, "capture", &i18n::tr("tray.capture"), state.live_logged_in, None::<&str>)?)
        .separator()
        .item(&MenuItem::with_id(app, "launch", &i18n::tr("tray.launch"), state.zcode_path_ok && !state.zcode_running, None::<&str>)?)
        .item(&MenuItem::with_id(app, "kill", &i18n::tr("tray.kill"), state.zcode_running, None::<&str>)?)
        .separator()
        .item(&MenuItem::with_id(app, "quit", &i18n::tr("tray.quit"), true, None::<&str>)?);
    b.build()
}

pub fn rebuild_tray(app: &AppHandle) {
    let app2 = app.clone();
    let res = app.run_on_main_thread(move || {
        if let Some(tray) = app2.tray_by_id(TRAY_ID) {
            let paths = Paths::detect();
            let tip = match store::get_state(&paths) {
                Ok(s) => {
                    let cur = s
                        .accounts
                        .iter()
                        .find(|a| a.is_active)
                        .map(|a| a.name.clone())
                        .or_else(|| s.live_identity.as_ref().and_then(|i| i.label()))
                        .unwrap_or_else(|| if s.live_logged_in { i18n::tr("tray.unsaved") } else { i18n::tr("tray.logged_out") });
                    format!("Z·POOL · {cur}")
                }
                Err(_) => "Z·POOL".into(),
            };
            let _ = tray.set_tooltip(Some(&tip));
            if let Ok(menu) = tray_menu_inner(&app2) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
    if let Err(e) = res {
        eprintln!("rebuild_tray: {e}");
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn run_tray_action(app: AppHandle, action: String) {
    std::thread::spawn(move || {
        let paths = Paths::detect();
        let mut emit_payload = json!({ "action": action });
        let _guard = store_guard();
        let result: Result<serde_json::Value, String> = match action.as_str() {
            "capture" => store::capture_current(&paths, None).map(|a| json!({ "name": a.name })),
            "launch" => {
                let (p, ok) = effective_zcode_path(&paths);
                if ok { store::launch_zcode(&p).map(|_| json!({})) } else { Err(i18n::trf("err.zcode.path_invalid", &[("p", &p)])) }
            }
            "kill" => match store::kill_zcode() {
                Ok(true) => Ok(json!({})),
                Ok(false) => Err(i18n::tr("err.zcode.kill_timeout")),
                Err(e) => Err(e),
            },
            _ => Ok(json!({})),
        };
        match result {
            Ok(v) => {
                emit_payload["ok"] = json!(true);
                emit_payload["result"] = v;
            }
            Err(e) => {
                emit_payload["ok"] = json!(false);
                emit_payload["error"] = json!(e);
            }
        }
        let _ = app.emit("tray-action", &emit_payload);
        rebuild_tray(&app);
    });
}

#[tauri::command]
async fn get_state() -> Result<AppState, String> {
    store::get_state(&Paths::detect())
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn capture_current(app: AppHandle, name: Option<String>) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::capture_current(&Paths::detect(), name);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn rename_account(app: AppHandle, id: String, name: String) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::rename_account(&Paths::detect(), &id, &name);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn delete_account(app: AppHandle, id: String) -> Result<(), String> {
    let _guard = store_guard();
    let r = store::delete_account(&Paths::detect(), &id);
    rebuild_tray(&app);
    r
}

/// 批量删除账号库存档（容错：失效 id 跳过、不阻断整批；返回值区分 removed / skipped）。
#[tauri::command]
async fn delete_many(app: AppHandle, ids: Vec<String>) -> Result<serde_json::Value, String> {
    let _guard = store_guard();
    let (removed, skipped) = store::delete_accounts(&Paths::detect(), &ids);
    rebuild_tray(&app);
    Ok(json!({ "removed": removed, "skipped": skipped }))
}

#[tauri::command]
async fn update_account_from_live(app: AppHandle, id: String) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::update_account_from_live(&Paths::detect(), &id);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn switch_to(app: AppHandle, id: String, force: bool, restart: bool) -> Result<SwitchResult, String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let r = store::switch_to(&paths, &id, force, restart);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn get_live_quota() -> Result<quota::QuotaOverview, String> {
    store::live_quota(&Paths::detect())
}

#[tauri::command]
async fn get_account_quota(id: String) -> Result<quota::QuotaOverview, String> {
    store::account_quota(&Paths::detect(), &id)
}

#[tauri::command]
async fn claim_preview(id: String) -> Result<Vec<claim::ClaimPlan>, String> {
    let paths = Paths::detect();
    let mid = store::account_mid(&paths, &id)?;
    let acc = load_account(&paths, &id)?;
    claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRefreshResult {
    plans: Vec<claim::ClaimPlan>,
    activated: bool,
    activation_error: Option<String>,
}

#[tauri::command]
async fn claim_refresh(id: String) -> Result<ClaimRefreshResult, String> {
    let paths = Paths::detect();
    let mid = store::account_mid(&paths, &id)?;
    let acc = load_account(&paths, &id)?;
    let (activated, activation_error) =
        match claim::telemetry_user_id(&paths.home, &acc.credentials) {
            Some(uid) => match claim::report_activation_events(&uid, &mid) {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e)),
            },
            None => (false, None),
        };
    let plans = claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid))?;
    Ok(ClaimRefreshResult { plans, activated, activation_error })
}

#[tauri::command]
async fn claim_start(
    app: AppHandle,
    id: String,
    plan_id: String,
    auto: Option<bool>,
) -> Result<serde_json::Value, String> {
    claim_start_inner(&app, id, plan_id, auto.unwrap_or(false))
}

pub fn proxy_claim_start(
    app: &AppHandle,
    id: String,
    plan_id: String,
    auto: bool,
) -> Result<serde_json::Value, String> {
    claim_start_inner(app, id, plan_id, auto)
}

fn claim_start_inner(
    app: &AppHandle,
    id: String,
    plan_id: String,
    auto: bool,
) -> Result<serde_json::Value, String> {
    let paths = Paths::detect();
    let mid = store::account_mid(&paths, &id)?;
    let acc = load_account(&paths, &id)?;
    let plans = claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid.clone()))?;
    let plan = plans
        .iter()
        .find(|p| p.plan_id == plan_id)
        .ok_or_else(|| i18n::tr("err.claim.gone"))?;
    let display = if plan.name.is_empty() { plan.plan_id.clone() } else { plan.name.clone() };

    *pending_guard() = Some(PendingClaim {
        account_id: acc.id.clone(),
        account_name: acc.name.clone(),
        plan_id: plan.plan_id.clone(),
        plan_name: display.clone(),
        credentials: acc.credentials,
        config: acc.config,
        device_mid: mid,
    });
    open_captcha_window(app, auto)?;
    Ok(json!({ "account": acc.name, "plan": display, "captcha": true }))
}

static LAST_CLAIM: Mutex<Option<Value>> = Mutex::new(None);

fn last_claim_guard() -> std::sync::MutexGuard<'static, Option<Value>> {
    match LAST_CLAIM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub fn claim_last_result() -> Value {
    last_claim_guard().clone().unwrap_or(Value::Null)
}

fn record_claim_result(payload: &Value) {
    // ⚠ `at` 是给人看的字符串；前端要**比时间先后**，字符串比不了 ——
    //   它和 unix 秒数相减会得到 NaN，判断恒为 false，成功分支就永远进不去
    //   （症状：领取成功了却一直显示「正在领取中」然后超时）。所以另给一个数值 `atMs`。
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    *last_claim_guard() = Some(json!({ "at": store::now_ts(), "atMs": ms, "result": payload }));
}

pub fn emit_state_changed(app: &AppHandle) {
    let _ = app.emit("state-changed", ());
}

#[tauri::command]
async fn claim_captcha_config() -> Result<claim::CaptchaConfig, String> {
    claim::fetch_captcha_config()
}

#[tauri::command]
async fn claim_captcha_submit(
    app: AppHandle,
    param: String,
    region: Option<String>,
) -> Result<serde_json::Value, String> {
    let pending = pending_guard().take().ok_or_else(|| i18n::tr("err.claim.none_pending"))?;
    let paths = Paths::detect();
    let res = claim::submit_claim(
        &paths.home,
        &pending.credentials,
        pending.config.as_ref(),
        &pending.plan_id,
        &param,
        region.as_deref(),
        Some(pending.device_mid),
    );
    close_captcha_window(&app);
    let payload = match res {
        Ok(v) => {
            let ms = |k: &str| -> Option<i64> {
                v.pointer(&format!("/data/plan/{k}"))
                    .and_then(|x| x.as_i64())
                    .map(|s| s * 1000)
            };
            let server_time = v
                .pointer("/data/server_time")
                .and_then(|x| x.as_i64())
                .map(|s| s * 1000);
            let outcome = claim::ClaimOutcome {
                account_id: pending.account_id.clone(),
                account_name: pending.account_name.clone(),
                plan_name: pending.plan_name.clone(),
                starts_at: ms("starts_at"),
                ends_at: ms("ends_at"),
                server_time,
            };
            let p = serde_json::to_value(&outcome).unwrap_or(Value::Null);
            record_claim_result(&p);
            let _ = app.emit("claim://result", &p);
            p
        }
        Err(e) => {
            let p = claim::failure_payload(
                &pending.account_id,
                &pending.account_name,
                &pending.plan_name,
                &e,
            );
            record_claim_result(&p);
            let _ = app.emit("claim://result", &p);
            return Ok(p);
        }
    };
    Ok(payload)
}

#[tauri::command]
async fn claim_cancel(app: AppHandle) -> Result<(), String> {
    *pending_guard() = None;
    close_captcha_window(&app);
    Ok(())
}

fn close_captcha_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("captcha") {
        let _ = w.close();
    }
}

struct PendingOAuth {
    provider: String,
    state: String,
    flow: String,
    email: Option<String>,
}

#[derive(Clone)]
struct PollCfg {
    url: String,
    token: String,
    expires_at_ms: u128,
    interval_ms: u64,
}

/// 并发生效：OAuth 流程按 flow 键控，允许多个注册窗口并存。
/// 用 BTreeMap（::new 是 const，可作 static 初始化；规模 ≤5 无性能差异）。
static PENDING_OAUTH: Mutex<BTreeMap<String, PendingOAuth>> = Mutex::new(BTreeMap::new());

fn pending_oauth_guard() -> std::sync::MutexGuard<'static, BTreeMap<String, PendingOAuth>> {
    match PENDING_OAUTH.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn pending_insert(p: PendingOAuth) {
    pending_oauth_guard().insert(p.flow.clone(), p);
}

/// 移除该 flow 的 pending；返回"本次是否真的移除了"——作为收口门闸，
/// 保证 done / closed / soft 多条收口路径里只有第一条真正生效（防双发事件）。
fn pending_remove(flow: &str) -> bool {
    pending_oauth_guard().remove(flow).is_some()
}

fn flow_still_ours(flow: &str) -> bool {
    pending_oauth_guard().contains_key(flow)
}

fn pending_email(flow: &str) -> Option<String> {
    pending_oauth_guard()
        .get(flow)
        .and_then(|p| p.email.clone())
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
}

/// 登录窗口上限：并发需求（前端的并发泵）之外的兜底保护。
const MAX_LOGIN_WINDOWS: usize = 5;

fn login_label(flow: &str) -> String {
    format!("login-{}", flow.chars().take(12).collect::<String>())
}

fn find_login_window(app: &AppHandle, flow: &str) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(&login_label(flow))
}

/// 已打开的全部登录窗口（label 前缀 `login-`），用于数量保护与"全部关闭"。
fn login_windows(app: &AppHandle) -> Vec<(String, tauri::WebviewWindow)> {
    let mut v: Vec<(String, tauri::WebviewWindow)> = app
        .webview_windows()
        .into_iter()
        .filter(|(label, _)| label.starts_with("login-"))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn close_login_windows(app: &AppHandle, flow: Option<&str>) {
    match flow {
        Some(f) => {
            if let Some(w) = find_login_window(app, f) {
                let _ = w.close();
            }
        }
        None => {
            for (_, w) in login_windows(app) {
                let _ = w.close();
            }
        }
    }
}

/// 登录窗口槽位布局：一次"窗口会话"内布局参数冻结在首次落位时刻 ——
/// 新窗口只按槽位落位，**绝不重排已有窗口**（避免打扰正在过验证码的操作）。
/// 唯一例外是用户主动点"整理窗口"（tile_login_windows）。
#[derive(Clone, Copy)]
struct LoginLayout {
    x0: i32,
    y0: i32,
    ow: i32,
    oh: i32,
    gap: i32,
    cols: i32,
}

/// 窗口 label（`login-<flow 前 12 位>`）→ 槽位序号。窗口关闭即释放，
/// 新窗口取最小空闲槽位（优先复用刚消失窗口的位置）。
static LOGIN_SLOTS: Mutex<BTreeMap<String, i32>> = Mutex::new(BTreeMap::new());
/// 布局参数缓存：首个窗口落位时按 reg_concurrency 容量构建；
/// 全部窗口关闭后失效重建（自动适配显示器/并发设置变化）。
static LOGIN_LAYOUT: Mutex<Option<LoginLayout>> = Mutex::new(None);

fn login_slots_guard() -> std::sync::MutexGuard<'static, BTreeMap<String, i32>> {
    match LOGIN_SLOTS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn login_layout_guard() -> std::sync::MutexGuard<'static, Option<LoginLayout>> {
    match LOGIN_LAYOUT.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 最小空闲槽位（0 起）。纯函数，便于测试。
fn next_free_slot(used: &std::collections::BTreeSet<i32>) -> i32 {
    let mut slot = 0;
    while used.contains(&slot) {
        slot += 1;
    }
    slot
}

/// 网格列数：容量（预计同时打开的窗口数）已定，挑"整组放得下"的最多列；都放不下则用物理上限。
fn grid_cols(capacity: i32, avail_w: i32, avail_h: i32, ow: i32, oh: i32, gap: i32) -> i32 {
    // 不重叠前提下每行最多几列
    let cap = ((avail_w + gap) / (ow + gap)).clamp(1, capacity.max(1));
    // 从大到小挑列数：优先整体高度放得下的最多列（行数最少）；都放不下则用 cap（纵向溢出、顶部对齐）
    let mut cols = cap;
    for c in (1..=cap).rev() {
        let rows = (capacity + c - 1) / c;
        if rows * oh + (rows - 1) * gap <= avail_h {
            cols = c;
            break;
        }
    }
    cols
}

/// 槽位 → 物理坐标（行优先网格）。
fn slot_xy(l: &LoginLayout, slot: i32) -> (i32, i32) {
    let cols = l.cols.max(1);
    let (r, c) = (slot / cols, slot % cols);
    (l.x0 + c * (l.ow + l.gap), l.y0 + r * (l.oh + l.gap))
}

/// 计算一组布局参数（沿用旧平铺的显示器/尺寸/间距策略；容量替代原来的"当前窗口数"，
/// 使槽位坐标与后续开窗数量无关 —— 这是"新窗口不打乱已有窗口"的前提）。
fn compute_login_layout(app: &AppHandle, capacity: i32) -> Option<LoginLayout> {
    let wins = login_windows(app);
    // 目标显示器：主窗口所在屏 → 系统主屏 → 任一登录窗口所在屏
    let mon = app
        .get_webview_window("main")
        .and_then(|m| m.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())
        .or_else(|| wins.iter().find_map(|(_, w)| w.current_monitor().ok().flatten()))?;
    let (mp, ms, scale) = (mon.position(), mon.size(), mon.scale_factor());
    if scale <= 0.0 || ms.width < 320 || ms.height < 240 {
        return None;
    }
    // 实测窗口外框（物理像素，所有登录窗口同尺寸）；取不到就按 inner 480x680 + 系统边框估算
    let (ow, oh) = wins
        .iter()
        .find_map(|(_, w)| w.outer_size().ok().map(|s| (s.width as i32, s.height as i32)))
        .filter(|(w, h)| *w > 0 && *h > 0)
        .unwrap_or(((500.0 * scale).round() as i32, (734.0 * scale).round() as i32));

    let gap = (10.0 * scale).round() as i32;
    let margin = (10.0 * scale).round() as i32;
    let bottom = (52.0 * scale).round() as i32; // 底部预留（任务栏）
    let avail_w = (ms.width as i32 - margin * 2).max(ow);
    let avail_h = (ms.height as i32 - margin - bottom).max(oh);

    let capacity = capacity.clamp(1, MAX_LOGIN_WINDOWS as i32);
    let cols = grid_cols(capacity, avail_w, avail_h, ow, oh, gap);
    let rows = (capacity + cols - 1) / cols;
    let total_w = cols * ow + (cols - 1) * gap;
    let total_h = rows * oh + (rows - 1) * gap;
    let x0 = mp.x + margin + ((avail_w - total_w) / 2).max(0);
    let y0 = mp.y + margin + ((avail_h - total_h) / 2).max(0);
    Some(LoginLayout { x0, y0, ow, oh, gap, cols })
}

/// 取/建布局缓存（容量参数只在首次构建时生效）。
fn ensure_login_layout(app: &AppHandle, capacity: i32) -> Option<LoginLayout> {
    if let Some(l) = *login_layout_guard() {
        return Some(l);
    }
    let l = compute_login_layout(app, capacity)?;
    *login_layout_guard() = Some(l);
    Some(l)
}

/// 为窗口分配槽位：最小空闲优先（刚关闭窗口的位置优先复用）。
fn alloc_login_slot(app: &AppHandle, label: &str) -> i32 {
    let mut g = login_slots_guard();
    // 兜底清理：未走 CloseRequested/Destroyed 就消失的窗口（异常销毁等）
    let stale: Vec<String> = g
        .keys()
        .filter(|k| app.get_webview_window(k).is_none())
        .cloned()
        .collect();
    for k in stale {
        g.remove(&k);
    }
    let used: std::collections::BTreeSet<i32> = g.values().copied().collect();
    let slot = next_free_slot(&used);
    g.insert(label.to_string(), slot);
    slot
}

/// 释放槽位（窗口关闭/销毁时调用；幂等）。全部窗口退场即结束布局会话。
fn release_login_slot(label: &str) {
    let empty = {
        let mut g = login_slots_guard();
        g.remove(label);
        g.is_empty()
    };
    if empty {
        *login_layout_guard() = None;
    }
}

/// 新窗口落位：分配最小空闲槽位 → 按槽位一次到位，不触碰其它窗口。
fn place_login_window(app: &AppHandle, flow: &str, capacity: i32) {
    let label = login_label(flow);
    let slot = alloc_login_slot(app, &label);
    let Some(l) = ensure_login_layout(app, capacity) else {
        return;
    };
    if let Some(w) = app.get_webview_window(&label) {
        let (x, y) = slot_xy(&l, slot);
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
        flowlog::log(flow, "slot", &format!("slot={slot} cols={} pos=({x},{y})", l.cols));
    }
}

/// 手动"整理窗口"：把全部登录窗口整组重排为紧凑网格（唯一允许移动已有窗口的路径，
/// 由 `reg_tile_windows` 命令在用户主动点击时触发）。随后按重排结果重建槽位映射与布局缓存，
/// 后续新窗口继续循新布局落位。
fn tile_login_windows(app: &AppHandle) {
    let mut wins = login_windows(app);
    let n = wins.len() as i32;
    if n == 0 {
        login_slots_guard().clear();
        *login_layout_guard() = None;
        return;
    }
    // 稳定顺序：已分配槽位的按槽位（= 视觉顺序），未记录的排最后
    {
        let g = login_slots_guard();
        wins.sort_by_key(|(label, _)| g.get(label).copied().unwrap_or(i32::MAX));
    }
    let Some(l) = compute_login_layout(app, n) else {
        return;
    };
    *login_layout_guard() = Some(l);
    {
        let mut g = login_slots_guard();
        g.clear();
        for (i, (label, _)) in wins.iter().enumerate() {
            g.insert(label.clone(), i as i32);
        }
    }
    for (i, (_, w)) in wins.iter().enumerate() {
        let (x, y) = slot_xy(&l, i as i32);
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// 登录窗口标题：统一拼 "标题 · email"（无邮箱则只标题）。
fn login_title(email: Option<&str>) -> String {
    let base = i18n::tr("title.login");
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("{base} · {e}"),
        None => base,
    }
}

fn new_flow_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[tauri::command]
async fn oauth_providers() -> Result<Vec<oauth::OAuthProvider>, String> {
    Ok(oauth::OAUTH_PROVIDERS.to_vec())
}

#[tauri::command]
async fn oauth_begin(
    app: AppHandle,
    provider: String,
    mode: Option<String>,
    batch: Option<bool>,
    email: Option<String>,
) -> Result<serde_json::Value, String> {
    if !oauth::OAUTH_PROVIDERS.iter().any(|p| p.id == provider) {
        return Err(i18n::trf("err.oauth.unknown_provider", &[("provider", &provider)]));
    }
    let assist_mode = driver::normalize_mode(mode.as_deref());
    let auto = batch.unwrap_or(false);
    let email = email
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    let settings = load_settings(&Paths::detect());
    let proxy_url: Option<tauri::Url> = match settings.auth_proxy() {
        Some(p) => {
            let norm = oauth::parse_proxy_url(p)?;
            Some(norm.parse::<tauri::Url>().map_err(|e| i18n::trf("err.proxy.invalid", &[("e", &e.to_string())]))?)
        }
        None => None,
    };
    // 布局容量：按当前并发设置预排网格，新窗口落位时不再依赖"当时窗口数"
    let reg_capacity = settings.reg_concurrency() as i32;
    if login_windows(&app).len() >= MAX_LOGIN_WINDOWS {
        return Err(i18n::tr("err.reg.too_many"));
    }
    let flow = new_flow_id();
    flowlog::log(&flow, "begin", &format!("provider={provider} auto={auto} proxy={} client_ver={}", if proxy_url.is_some() { "on" } else { "off" }, quota::zcode_app_version()));
    let mid = uuid::Uuid::new_v4().to_string();
    let (p_init, m_init) = (provider.clone(), mid.clone());
    let init = match tauri::async_runtime::spawn_blocking(move || oauth::init_flow(&p_init, &m_init))
        .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            flowlog::log(&flow, "init-fail", &e);
            return Err(e);
        }
        Err(e) => {
            let m = i18n::trf("err.oauth.flow", &[("e", &e.to_string())]);
            flowlog::log(&flow, "init-fail", &m);
            return Err(m);
        }
    };
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let srv_flow = init.poll_url.rsplit('/').next().unwrap_or("");
        let expires_in = init.expires_at_ms.saturating_sub(now) / 1000;
        flowlog::log(
            &flow,
            "init-ok",
            &format!("server_flow={srv_flow} expires_in={expires_in}s interval={}ms", init.poll_interval_ms),
        );
    }
    let url = init.authorize_url.clone();
    let state = init.state.clone();
    let poll_cfg = PollCfg {
        url: init.poll_url.clone(),
        token: init.poll_token.clone(),
        expires_at_ms: init.expires_at_ms,
        interval_ms: init.poll_interval_ms,
    };
    pending_insert(PendingOAuth {
        provider: provider.clone(),
        state: state.clone(),
        flow: flow.clone(),
        email: email.clone(),
    });

    let login_root = app
        .path()
        .app_local_data_dir()
        .map_err(|e| i18n::trf("err.oauth.appdata", &[("e", &e.to_string())]))?
        .join("login-webview");
    sweep_login_profiles(&login_root);
    let profile_dir = login_root.join(&flow);

    let app2 = app.clone();
    let (provider2, state2, flow2, mid2) = (provider.clone(), state.clone(), flow.clone(), mid.clone());
    let flow_close = flow.clone();
    let driver_script = driver::bootstrap_script(&json!({
        "mode": assist_mode,
        "provider": provider,
        "return_url": url,
        "auto": auto,
    }));
    let flow_driver = flow.clone();
    let flow_load = flow.clone();
    let mut builder = tauri::WebviewWindowBuilder::new(
        &app,
        login_label(&flow),
        tauri::WebviewUrl::External(url.parse::<tauri::Url>().map_err(|e| i18n::trf("err.oauth.bad_authorize_url", &[("e", &e.to_string())]))?),
    )
    .title(login_title(email.as_deref()))
    .theme(Some(tauri::Theme::Dark))
    .inner_size(480.0, 680.0)
    .min_inner_size(420.0, 560.0)
    .resizable(true)
    .focused(!auto)
    // UA 不做覆盖：使用 WebView2 默认 UA（= Edge 真实 UA，随内核升级自动更新）。
    // 曾写死 Chrome/131 假 UA（与内核脱节、且覆盖 UA 会清掉 Sec-CH-UA-* 提示头），2026-10-14 移除。
    .initialization_script(&driver_script)
    .data_directory(profile_dir);
    if let Some(u) = proxy_url {
        builder = builder.proxy_url(u);
    }
    builder
    .on_page_load(move |_, payload| {
        let event = match payload.event() {
            tauri::webview::PageLoadEvent::Started => "load-start",
            tauri::webview::PageLoadEvent::Finished => "load-finished",
        };
        flowlog::log(&flow_load, event, &driver::url_for_log(payload.url()));
    })
    .on_navigation(move |url| {
        if url.scheme() == "zpool-driver" {
            // 不再接受导航式消息，也不把旧协议交给操作系统打开。
            return false;
        }
        if url.scheme() == "http" || url.scheme() == "https" {
            let short: String = driver::url_for_log(url).chars().take(170).collect();
            flowlog::log(&flow_driver, "nav", &short);
        }
        if url.scheme() != "zcode" {
            return true;
        }
        let full = url.to_string();
        let (app3, p3, s3, f3, m3) = (app2.clone(), provider2.clone(), state2.clone(), flow2.clone(), mid2.clone());
        tauri::async_runtime::spawn(async move {
            finish_oauth(&app3, p3, s3, f3, m3, &full).await;
        });
        false
    })
    .build()
    .map_err(|e| {
        pending_remove(&flow);
        let m = i18n::trf("err.oauth.window", &[("e", &e.to_string())]);
        flowlog::log(&flow, "window-fail", &m);
        m
    })?;
    // 新窗口落位：按槽位一次到位 —— 优先复用已关闭窗口腾出的位置，
    // 且不移动任何已有窗口（正过验证码的窗口不会被打断）。
    place_login_window(&app, &flow, reg_capacity);
    if let Some(w) = find_login_window(&app, &flow) {
        #[cfg(windows)]
        driver::observe_navigation(&w, &flow);
        let app_close = app.clone();
        let flow_evt2 = flow.clone();
        let slot_key = login_label(&flow);
        w.on_window_event(move |e| {
            // 关闭/销毁即归还槽位，供后续新窗口复用（幂等）
            if matches!(
                e,
                tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
            ) {
                release_login_slot(&slot_key);
            }
            if let tauri::WindowEvent::CloseRequested { .. } = e {
                if pending_remove(&flow_close) {
                    flowlog::log(&flow_close, "cancelled", "");
                    let mut ev = driver::closed_event();
                    ev["flow"] = json!(flow_evt2.as_str());
                    let _ = app_close.emit("reg://event", ev);
                }
            }
        });
    }
    spawn_poll_loop(app.clone(), provider.clone(), flow.clone(), mid, poll_cfg);
    Ok(json!({ "opened": true, "provider": provider, "mode": assist_mode, "flow": flow, "email": email }))
}

#[tauri::command]
async fn reg_input(app: AppHandle, flow: String, ask: String, value: Value) -> Result<(), String> {
    if ask != "signup" && ask != "link" {
        return Err(i18n::trf("err.reg.bad_ask", &[("ask", &ask)]));
    }
    let win = find_login_window(&app, &flow).ok_or_else(|| i18n::tr("err.reg.no_window"))?;
    let js = format!(
        "window.__zpool && window.__zpool.fromHost({});",
        json!({ "t": "input", "ask": ask, "value": value })
    );
    win.eval(&js)
        .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))
}

#[tauri::command]
async fn reg_action(app: AppHandle, flow: String, action: String) -> Result<(), String> {
    let win = find_login_window(&app, &flow).ok_or_else(|| i18n::tr("err.reg.no_window"))?;
    match action.as_str() {
        "reveal" => {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            Ok(())
        }
        "retry" | "confirm-authorize" | "open-signup" => {
            let js = format!(
                "window.__zpool && window.__zpool.fromHost({});",
                json!({ "t": "action", "action": action })
            );
            win.eval(&js)
                .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))
        }
        other => Err(i18n::trf("err.reg.bad_action", &[("a", other)])),
    }
}

#[tauri::command]
async fn reg_fetch_link(
    email: String,
    limit: Option<usize>,
    since: Option<i64>,
    flow: Option<String>,
) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let accounts = pool::load(&root);
    let acc = pool::find(&accounts, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?
        .clone();
    let cur_flow = flow.unwrap_or_default();

    // 临时邮箱源（cloudflare_temp_email）：走管理接口取信，自己解码 raw MIME 抠链接
    if acc.is_cf() {
        let paths = Paths::detect();
        match cf_fetch_links(&paths, &acc.email, since) {
            Ok(v) => {
                if !cur_flow.is_empty() {
                    let scanned = v.get("scanned").and_then(|x| x.as_u64()).unwrap_or(0);
                    let n = v.get("links").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
                    flowlog::log(
                        &cur_flow,
                        "link-fetch",
                        &format!("cf address={} scanned={scanned} links={n}", acc.email),
                    );
                }
                return Ok(v);
            }
            Err(e) => {
                // 与 Graph 分支对称：失败也留痕，便于回头查是配置错还是服务端问题
                if !cur_flow.is_empty() {
                    let short: String = e.chars().take(200).collect();
                    flowlog::log(&cur_flow, "link-fetch-fail", &short);
                }
                return Err(e);
            }
        }
    }

    let (found, new_rt) = match graph::fetch_links(
        &acc.client_id,
        &acc.refresh_token,
        limit.unwrap_or(12),
        since,
    ) {
        Ok(v) => v,
        Err(e) => {
            if !cur_flow.is_empty() {
                let short: String = e.chars().take(200).collect();
                flowlog::log(&cur_flow, "link-fetch-fail", &short);
            }
            if graph::is_credential_error(&e) {
                let _ = pool::set_status(&root, &email, pool::STATUS_INVALID, Some(i18n::tr("err.graph.reauth")));
            }
            return Err(e);
        }
    };
    if !cur_flow.is_empty() {
        let newest: String = found
            .newest_subject
            .clone()
            .unwrap_or_default()
            .chars()
            .take(40)
            .collect();
        flowlog::log(
            &cur_flow,
            "link-fetch",
            &format!(
                "found={} scanned={} newest={newest}",
                found.links.len(),
                found.scanned
            ),
        );
    }
    if let Some(rt) = new_rt {
        // 并发加固：回写前重新加载 + 持 store_guard，避免多个 flow 同时取信时互相覆盖。
        let _guard = store_guard();
        let mut fresh = pool::load(&root);
        if let Some(a) = pool::find_mut(&mut fresh, &email) {
            a.refresh_token = rt;
            let _ = pool::save(&root, &fresh);
        }
    }
    Ok(json!({
        "links": found.links,
        "scanned": found.scanned,
        "newestSubject": found.newest_subject,
        "newestAt": found.newest_at,
    }))
}

#[tauri::command]
async fn pool_list() -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let items: Vec<_> = pool::load(&root).iter().map(|a| a.to_summary()).collect();
    Ok(json!({ "accounts": items }))
}

#[tauri::command]
async fn pool_import_pick(app: AppHandle) -> Result<Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.txt"), &["txt"])
        .blocking_pick_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp
        .into_path()
        .map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| i18n::trf("err.pool.read", &[("e", &e.to_string())]))?;

    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let cf_domain = load_settings(&Paths::detect()).cf_domain;
    let mut accounts = pool::load(&root);
    let r = pool::import(&mut accounts, &raw, store::now_ts(), cf_domain.as_deref());
    pool::save(&root, &accounts)?;
    drop(_guard);

    let items: Vec<_> = accounts.iter().map(|a| a.to_summary()).collect();
    Ok(json!({
        "picked": true,
        "added": r.added,
        "skipped": r.skipped,
        "parsed": r.total_parsed,
        "accounts": items,
    }))
}

#[tauri::command]
async fn pool_remove(email: String) -> Result<Value, String> {
    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let mut accounts = pool::load(&root);
    let before = accounts.len();
    accounts.retain(|a| !a.email.eq_ignore_ascii_case(email.trim()));
    pool::save(&root, &accounts)?;
    drop(_guard);
    Ok(json!({ "removed": before - accounts.len() }))
}

#[tauri::command]
async fn pool_remove_many(emails: Vec<String>) -> Result<Value, String> {
    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let mut accounts = pool::load(&root);
    let before = accounts.len();
    let targets: Vec<String> = emails.iter().map(|e| e.trim().to_ascii_lowercase()).collect();
    accounts.retain(|a| !targets.contains(&a.email.trim().to_ascii_lowercase()));
    pool::save(&root, &accounts)?;
    drop(_guard);
    Ok(json!({ "removed": before - accounts.len() }))
}

#[tauri::command]
async fn pool_get(email: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let accounts = pool::load(&root);
    let a = pool::find(&accounts, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?;
    Ok(json!({ "email": a.email, "password": a.password }))
}

#[tauri::command]
async fn pool_mark_verified(
    app: AppHandle,
    email: String,
    ok: bool,
    note: Option<String>,
) -> Result<(), String> {
    {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        let mut accounts = pool::load(&root);
        if let Some(a) = pool::find_mut(&mut accounts, &email) {
            if ok {
                a.status = pool::STATUS_VERIFIED.to_string();
                a.verified_at = Some(store::now_ts());
                a.note = None;
            } else {
                a.status = pool::STATUS_FAILED.to_string();
                a.note = note;
            }
            pool::save(&root, &accounts)?;
        }
    }
    let _ = app.emit("pool-changed", ());
    Ok(())
}

#[tauri::command]
async fn reg_close_window(app: AppHandle, flow: Option<String>) -> Result<(), String> {
    close_login_windows(&app, flow.as_deref());
    Ok(())
}

/// 手动把全部登录窗口重新平铺归位（用户拖动乱之后）。
#[tauri::command]
async fn reg_tile_windows(app: AppHandle) -> Result<(), String> {
    tile_login_windows(&app);
    Ok(())
}

#[tauri::command]
async fn pool_reset(app: AppHandle, email: String) -> Result<(), String> {
    {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        pool::set_status(&root, &email, pool::STATUS_NEW, None)?;
    }
    let _ = app.emit("pool-changed", ());
    Ok(())
}

#[tauri::command]
async fn pool_upsert(app: AppHandle, line: String) -> Result<Value, String> {
    let Some((email, password, client_id, refresh_token)) = pool::parse_lines(&line).into_iter().next() else {
        return Err(i18n::tr("err.pool.bad_line"));
    };
    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let cf_domain = load_settings(&Paths::detect()).cf_domain;
    let mut accounts = pool::load(&root);
    let hint = pool::infer_kind(&email, cf_domain.as_deref());
    let updated = if let Some(a) = pool::find_mut(&mut accounts, &email) {
        a.password = password;
        a.client_id = client_id;
        a.refresh_token = refresh_token;
        a.status = pool::STATUS_NEW.to_string();
        a.note = None;
        // 只做单向修正（outlook → cf）：域名对得上就说明这行是临时邮箱地址
        if hint == pool::KIND_CF {
            a.kind = pool::KIND_CF.to_string();
        }
        true
    } else {
        accounts.push(pool::MailAccount {
            email: email.clone(),
            password,
            client_id,
            refresh_token,
            status: pool::STATUS_NEW.to_string(),
            verified_at: None,
            note: None,
            created_at: store::now_ts(),
            kind: hint.to_string(),
            remote_id: None,
        });
        false
    };
    pool::save(&root, &accounts)?;
    drop(_guard);
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "email": email, "updated": updated }))
}

// ---------- 临时邮箱服务（cloudflare_temp_email）----------

fn rand_from_chars(chars: &[u8], n: usize) -> String {
    let mut buf = vec![0u8; n];
    crate::oauth::getrandom_fallback(&mut buf);
    buf.iter().map(|b| chars[(*b as usize) % chars.len()] as char).collect()
}

fn rand_local_name() -> String {
    rand_from_chars(b"abcdefghijklmnopqrstuvwxyz0123456789", 10)
}

/// 给 z.ai 注册表单用的随机密码（去掉易混淆字符）
fn rand_reg_password() -> String {
    rand_from_chars(b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789", 16)
}

/// 保存临时邮箱服务配置。admin_password 传 None = 保持原样（界面不回显密码）。
#[tauri::command]
async fn set_cf_settings(
    app: AppHandle,
    base_url: Option<String>,
    admin_password: Option<String>,
    domain: Option<String>,
) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    let clean = |v: String| -> Option<String> {
        let t = v.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };
    if let Some(v) = base_url {
        s.cf_base_url = clean(v);
    }
    if let Some(v) = admin_password {
        s.cf_admin_password = clean(v);
    }
    if let Some(v) = domain {
        s.cf_domain = clean(v);
    }
    let r = save_settings(&paths, &s);
    drop(_guard);
    let _ = app.emit("state-changed", ());
    r
}

/// 测试连通性：优先用传入的值（还没保存也能测），否则用已保存的。返回可见邮件条数。
#[tauri::command]
async fn cf_test(
    base_url: Option<String>,
    admin_password: Option<String>,
    domain: Option<String>,
) -> Result<Value, String> {
    let paths = Paths::detect();
    let saved = load_settings(&paths);
    let pick = |given: Option<String>, old: Option<String>| -> String {
        given
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or(old)
            .unwrap_or_default()
    };
    let cfg = cfmail::CfCfg {
        base_url: pick(base_url, saved.cf_base_url.clone()),
        admin_password: pick(admin_password, saved.cf_admin_password.clone()),
        domain: {
            let d = pick(domain, saved.cf_domain.clone());
            if d.is_empty() { None } else { Some(d) }
        },
    };
    let n = cfmail::probe(&cfg)?;
    Ok(json!({ "ok": true, "visible": n }))
}

/// 现场取信 + 抠验证链接（GUI 的 reg_fetch_link 与 `--cli cf-links` 共用同一实现）
pub(crate) fn cf_fetch_links(paths: &Paths, email: &str, since: Option<i64>) -> Result<Value, String> {
    let cfg = load_settings(paths)
        .cf_cfg()
        .ok_or_else(|| i18n::tr("err.cf.noconfig").to_string())?;
    let mails = cfmail::fetch_mails(&cfg, email, 10)?;
    let (links, scanned) = cfmail::links_from_mails(&mails, since);
    let newest_at = mails
        .iter()
        .filter_map(|m| m.created_ms)
        .max()
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms))
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string());
    Ok(json!({
        "links": links,
        "scanned": scanned,
        "newestSubject": "",
        "newestAt": newest_at,
    }))
}

/// 现场创建 N 个临时邮箱地址并写进邮箱库（kind=cf；password 是给 z.ai 注册用的随机串）。
/// GUI 命令与 `--cli cf-create` 共用。
pub(crate) fn cf_create_into_pool(paths: &Paths, count: usize) -> Result<(Vec<Value>, Vec<String>), String> {
    let cfg = load_settings(paths)
        .cf_cfg()
        .ok_or_else(|| i18n::tr("err.cf.noconfig").to_string())?;
    let root = paths.store_dir();
    let mut accounts = pool::load(&root);
    let n = count.clamp(1, 20);
    let mut created: Vec<Value> = vec![];
    let mut errors: Vec<String> = vec![];
    for _ in 0..n {
        let name = rand_local_name();
        match cfmail::create_address(&cfg, &name) {
            Ok(c) => {
                if pool::find(&accounts, &c.address).is_some() {
                    errors.push(format!("{} 已在邮箱库，跳过", c.address));
                    continue;
                }
                accounts.push(pool::MailAccount {
                    email: c.address.clone(),
                    password: rand_reg_password(),
                    client_id: String::new(),
                    refresh_token: String::new(),
                    status: pool::STATUS_NEW.to_string(),
                    verified_at: None,
                    note: None,
                    created_at: store::now_ts(),
                    kind: pool::KIND_CF.to_string(),
                    remote_id: Some(c.address_id),
                });
                created.push(json!({ "email": c.address, "address_id": c.address_id }));
            }
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    if !created.is_empty() {
        pool::save(&root, &accounts)?;
    }
    Ok((created, errors))
}

#[tauri::command]
async fn cf_create(app: AppHandle, count: Option<usize>) -> Result<Value, String> {
    let _guard = store_guard();
    let (created, errors) = cf_create_into_pool(&Paths::detect(), count.unwrap_or(1))?;
    drop(_guard);
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "created": created, "errors": errors }))
}

#[tauri::command]
async fn outlook_reauth_begin(email: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let accounts = pool::load(&root);
    let acc = pool::find(&accounts, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?;
    let cid = acc.client_id.clone();
    let d = graph::device_code_begin(&cid)?;
    Ok(json!({
        "client_id": cid,
        "device_code": d.device_code,
        "user_code": d.user_code,
        "verification_uri": d.verification_uri,
        "interval": d.interval,
        "expires_in": d.expires_in,
    }))
}

#[tauri::command]
async fn outlook_reauth_poll(
    app: AppHandle,
    email: String,
    client_id: String,
    device_code: String,
) -> Result<Value, String> {
    let p = graph::device_code_poll(&client_id, &device_code)?;
    if p.pending {
        return Ok(json!({ "pending": true }));
    }
    let Some(rt) = p.refresh_token else {
        return Ok(json!({ "pending": true }));
    };
    {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        let mut accounts = pool::load(&root);
        if let Some(a) = pool::find_mut(&mut accounts, &email) {
            a.refresh_token = rt;
            if a.client_id.trim().is_empty() {
                a.client_id = client_id;
            }
            a.status = pool::STATUS_NEW.to_string();
            a.note = None;
            pool::save(&root, &accounts)?;
        }
    }
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "pending": false, "ok": true }))
}

#[tauri::command]
async fn reg_set_mode(app: AppHandle, flow: String, mode: String) -> Result<String, String> {
    let m = mode.trim().to_ascii_lowercase();
    if !driver::is_valid_mode(&m) {
        return Err(i18n::trf("err.reg.bad_mode", &[("m", &mode)]));
    }
    let win = find_login_window(&app, &flow).ok_or_else(|| i18n::tr("err.reg.no_window"))?;
    let js = format!(
        "window.__zpool && window.__zpool.fromHost({});",
        json!({ "t": "setMode", "mode": m })
    );
    win.eval(&js)
        .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))?;
    Ok(m)
}

fn sweep_login_profiles(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 3600);
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() && meta.modified().map(|m| m < cutoff).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[tauri::command]
async fn set_auth_proxy(app: AppHandle, on: bool, url: Option<String>) -> Result<(), String> {
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let trimmed = url.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let normalized = match trimmed {
            Some(s) => Some(oauth::parse_proxy_url(s)?),
            None => None,
        };
        if on && normalized.is_none() {
            return Err(i18n::tr("err.proxy.need_url"));
        }
        let mut s = load_settings(&paths);
        s.auth_proxy_on = Some(on);
        s.auth_proxy_url = normalized;
        save_settings(&paths, &s)?;
    }
    let _ = app.emit("state-changed", ());
    Ok(())
}

async fn finish_oauth(app: &AppHandle, provider: String, state: String, flow: String, mid: String, callback_url: &str) {
    let result = {
        let provider = provider.clone();
        let state = state.clone();
        let flow = flow.clone();
        let mid = mid.clone();
        let callback_url = callback_url.to_string();
        tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
            if !flow_still_ours(&flow) {
                flowlog::log(&flow, "deeplink-superseded", "");
                return Err("__superseded__".into());
            }
            let (code, cb_state) = match oauth::parse_callback(&callback_url) {
                Ok(oauth::CallbackKind::Code { code, state }) => (code, state),
                Ok(oauth::CallbackKind::Attribution) => {
                    flowlog::log(&flow, "deeplink-attribution", "");
                    return Err("__attribution__".into());
                }
                Err(e) => {
                    flowlog::log(&flow, "deeplink-parse-fail", &e);
                    return Err(e);
                }
            };
            if cb_state != state {
                flowlog::log(&flow, "deeplink-state-mismatch", "");
                return Err(i18n::tr("err.oauth.state"));
            }
            let exchanged = match oauth::exchange_token(&provider, &code, &state, &mid) {
                Ok(v) => v,
                Err(e) => {
                    flowlog::log(&flow, "exchange-fail", &e);
                    return Err(e);
                }
            };
            match persist_oauth_account(&Paths::detect(), &provider, &exchanged["raw"], &flow, &mid, false) {
                Ok(v) => {
                    flowlog::log(
                        &flow,
                        "persist-ok",
                        &format!("channel=deeplink duplicate={}", v.get("duplicate").is_some()),
                    );
                    Ok(v)
                }
                Err(e) => {
                    if e != "__superseded__" {
                        flowlog::log(&flow, "persist-fail", &format!("channel=deeplink {e}"));
                    }
                    Err(e)
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(i18n::trf("err.oauth.flow", &[("e", &e.to_string())])))
    };
    if let Err(e) = &result {
        if deeplink_err_soft(e) {
            if flow_still_ours(&flow) {
                flowlog::log(&flow, "soft-fail", e);
                let email = pending_email(&flow);
                let _ = app.emit(
                    "oauth://done",
                    &json!({ "ok": false, "soft": true, "error": e, "flow": flow.as_str(), "email": email }),
                );
            }
            return;
        }
    }
    finalize_oauth_result(app, &flow, result);
}

fn deeplink_err_soft(e: &str) -> bool {
    e != "__superseded__" && e != "__attribution__"
}

fn persist_oauth_account(
    paths: &Paths,
    provider: &str,
    raw: &serde_json::Value,
    flow: &str,
    mid: &str,
    poll_ready: bool,
) -> Result<serde_json::Value, String> {
    if !flow_still_ours(flow) {
        return Err("__superseded__".into());
    }
    let jwt = raw
        .pointer("/data/token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| i18n::tr("err.oauth.no_token"))?
        .to_string();
    let t_all = std::time::Instant::now();
    let t_biz = std::time::Instant::now();
    let raw_access = oauth::extract_access_token(provider, raw).unwrap_or_default();
    let access_token = if provider == "zai" && !raw_access.is_empty() {
        match oauth::resolve_zai_business_token_diag(&raw_access) {
            (Some(t), _) => t,
            (None, why) => {
                flowlog::log(flow, "zai-business-fail", &why);
                return Err(i18n::tr("err.oauth.zai_business"));
            }
        }
    } else {
        raw_access
    };
    let ms_biz = t_biz.elapsed().as_millis();
    let t_user = std::time::Instant::now();
    let userinfo = if poll_ready {
        oauth::extract_poll_user_profile(raw)
    } else {
        oauth::extract_user_profile(provider, raw)
    }
    .or_else(|| {
        (!access_token.is_empty())
            .then(|| oauth::fetch_userinfo(provider, &access_token))
            .flatten()
    });
    let refresh_token = oauth::extract_refresh_token(provider, raw);
    let ms_user = t_user.elapsed().as_millis();
    let t_cfg = std::time::Instant::now();
    let credentials = oauth::assemble_credentials_with_token(
        provider,
        &jwt,
        userinfo.as_ref(),
        (!access_token.is_empty()).then_some(access_token.as_str()),
        refresh_token.as_deref(),
    );
    let config = oauth::assemble_config(provider, &jwt, &access_token);
    flowlog::log(
        flow,
        "persist-t",
        &format!(
            "biz={ms_biz}ms user={ms_user}ms cfg={}ms net-total={}ms",
            t_cfg.elapsed().as_millis(),
            t_all.elapsed().as_millis()
        ),
    );

    let _lock = store_guard();
    if !flow_still_ours(flow) {
        return Err("__superseded__".into());
    }
    let accounts = list_accounts(paths)?;
    let hash = canonical_hash(&credentials);
    if let Some(i) = store::find_same_login(&credentials, &hash, &accounts, &paths.home) {
        let mut dup = accounts[i].clone();
        dup.hash = hash.clone();
        dup.credentials = credentials;
        dup.config = Some(config);
        dup.updated_at = now_ts();
        if dup.virtual_device_mid.as_deref().map_or(true, |m| m.trim().is_empty()) {
            dup.virtual_device_mid = Some(mid.to_string());
        }
        if !flow_still_ours(flow) {
            return Err("__superseded__".into());
        }
        save_account(paths, &dup)?;
        // 防双收口：成功后立即移除 pending（另一个通道再来即 superseded）；
        // email 带进返回 payload，供 finalize 组装 oauth://done 事件。
        let email = pending_email(flow);
        pending_remove(flow);
        return Ok(json!({ "id": dup.id, "name": dup.name, "provider": provider, "duplicate": true, "email": email }));
    }
    let base = credentials
        .get(format!("oauth:{provider}:user_info"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|u| u.get("username").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_else(|| match provider {
            "zai" => "z.ai".to_string(),
            _ => "BigModel".to_string(),
        });
    let name = unique_name(&accounts, &base);
    let ts = now_ts();
    let acc = Account {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        created_at: ts.clone(),
        updated_at: ts,
        hash,
        credentials,
        config: Some(config),
        virtual_device_mid: Some(mid.to_string()),
        virtual_arms_uid: Some(store::new_arms_uid()),
    };
    if !flow_still_ours(flow) {
        return Err("__superseded__".into());
    }
    save_account(paths, &acc)?;
    let email = pending_email(flow);
    pending_remove(flow);
    Ok(json!({ "id": acc.id, "name": acc.name, "provider": provider, "email": email }))
}

fn finalize_oauth_result(app: &AppHandle, flow: &str, result: Result<serde_json::Value, String>) {
    if let Err(e) = &result {
        if e == "__superseded__" || e == "__attribution__" {
            return;
        }
    }
    // email 优先取 persist 成功时带回的（此时 pending 已被移除）；失败路径再兜底查 pending。
    let email = result
        .as_ref()
        .ok()
        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(String::from))
        .filter(|e| !e.trim().is_empty())
        .or_else(|| pending_email(flow));
    pending_remove(flow);
    // 网页上的“授权成功”不代表账号已入库；只有宿主的最终结果能结束进度卡片。
    let _ = app.emit("reg://event", &json!({
        "kind": "result", "flow": flow, "ok": result.is_ok(),
        "error": result.as_ref().err(),
    }));
    if let Some(w) = find_login_window(app, flow) {
        let _ = w.close();
    }
    let payload = match result {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("flow".into(), json!(flow));
                if let Some(e) = &email {
                    obj.insert("email".into(), json!(e));
                }
            }
            v
        }
        Err(e) => json!({ "ok": false, "error": e, "flow": flow, "email": email }),
    };
    let _ = app.emit("oauth://done", &payload);
}

fn spawn_poll_loop(app: AppHandle, provider: String, flow: String, mid: String, cfg: PollCfg) {
    std::thread::spawn(move || {
        let ours = || flow_still_ours(&flow);
        let mut retry_errors = 0u32;
        let mut last_retry_log: Option<std::time::Instant> = None;
        let deadline = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            std::cmp::min(cfg.expires_at_ms, now + u128::from(oauth::FLOW_TIMEOUT_MS))
        };
        loop {
            if !ours() {
                flowlog::log(&flow, "poll-exit", "flow-done-or-replaced");
                return;
            }
            match oauth::poll_flow_once(&cfg.url, &cfg.token, &mid) {
                Ok(oauth::PollOutcome::Pending) => {
                    if retry_errors > 0 {
                        flowlog::log(&flow, "poll-recovered", &format!("retry_errors={retry_errors}"));
                        retry_errors = 0;
                        last_retry_log = None;
                    }
                }
                Ok(oauth::PollOutcome::Retry(reason)) => {
                    retry_errors = retry_errors.saturating_add(1);
                    // 每个流程最多每 30 秒记录一次，避免断网时刷屏；不改变重试和总超时策略。
                    if last_retry_log.map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(30)) {
                        flowlog::log(&flow, "poll-retry", &format!("retry_errors={retry_errors} {reason}"));
                        last_retry_log = Some(std::time::Instant::now());
                    }
                }
                Ok(oauth::PollOutcome::Ready(data)) => {
                    flowlog::log(&flow, "poll-ready", "");
                    let raw = json!({ "code": 0, "data": data });
                    let result = persist_oauth_account(&Paths::detect(), &provider, &raw, &flow, &mid, true);
                    match &result {
                        Ok(v) => flowlog::log(
                            &flow,
                            "persist-ok",
                            &format!("channel=poll duplicate={}", v.get("duplicate").is_some()),
                        ),
                        Err(e) if e != "__superseded__" => {
                            flowlog::log(&flow, "persist-fail", &format!("channel=poll {e}"));
                        }
                        Err(_) => {}
                    }
                    finalize_oauth_result(&app, &flow, result);
                    return;
                }
                Err(e) => {
                    if !ours() {
                        flowlog::log(&flow, "poll-exit", "superseded");
                        return;
                    }
                    flowlog::log(&flow, "poll-fail", &e);
                    finalize_oauth_result(&app, &flow, Err(e));
                    return;
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if now >= deadline {
                if !ours() {
                    return;
                }
                flowlog::log(&flow, "poll-timeout", &format!("retry_errors={retry_errors}"));
                finalize_oauth_result(&app, &flow, Err(i18n::tr("err.oauth.expired")));
                return;
            }
            let sleep = (deadline - now).min(cfg.interval_ms as u128) as u64;
            std::thread::sleep(std::time::Duration::from_millis(sleep));
        }
    });
}

fn open_captcha_window(app: &AppHandle, auto: bool) -> Result<(), String> {
    let (w, h) = (380.0, 320.0);
    if let Some(win) = app.get_webview_window("captcha") {
        let _ = win.eval("location.reload()");
        center_over_main(app, &win, w, h);
        if auto {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
        return Ok(());
    }

    const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --no-proxy-server --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows";
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "captcha",
        tauri::WebviewUrl::App("captcha.html".into()),
    )
    .title(i18n::tr("title.captcha"))
    .theme(Some(tauri::Theme::Dark))
    .background_color(tauri::window::Color(0, 0, 0, 255))
    .inner_size(w, h)
    .min_inner_size(340.0, 280.0)
    .maximizable(false)
    .resizable(false)
    .additional_browser_args(BROWSER_ARGS)
    .visible(false)
    .build()
    .map_err(|e| {
        flowlog::log("captcha", "build-err", &e.to_string());
        e.to_string()
    })?;
    center_over_main(app, &win, w, h);
    if !auto {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

#[tauri::command]
async fn captcha_show(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("captcha") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

#[tauri::command]
async fn kill_zcode(app: AppHandle) -> Result<(), String> {
    let _guard = store_guard();
    let r = if store::kill_zcode()? { Ok(()) } else { Err(i18n::tr("err.zcode.kill_timeout")) };
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn set_behavior(
    app: AppHandle,
    launch_after_switch: Option<bool>,
    close_to_tray: Option<bool>,
    auto_claim: Option<bool>,
) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    if let Some(v) = launch_after_switch {
        s.launch_after_switch = Some(v);
    }
    if let Some(v) = close_to_tray {
        s.close_to_tray = Some(v);
    }
    if let Some(v) = auto_claim {
        s.auto_claim = Some(v);
    }
    let r = save_settings(&paths, &s);
    rebuild_tray(&app);
    let _ = app.emit("state-changed", ());
    r
}

#[tauri::command]
async fn set_reg_concurrency(app: AppHandle, n: u32) -> Result<u32, String> {
    let v = n.clamp(1, 5);
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let mut s = load_settings(&paths);
        s.reg_concurrency = Some(v);
        save_settings(&paths, &s)?;
    }
    let _ = app.emit("state-changed", ());
    Ok(v)
}

#[tauri::command]
async fn set_language(app: AppHandle, lang: String) -> Result<(), String> {
    let l = i18n::Lang::parse(&lang)
        .ok_or_else(|| i18n::trf("err.lang.unknown", &[("lang", &lang)]))?;
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let mut s = load_settings(&paths);
        s.language = Some(l.as_str().to_string());
        save_settings(&paths, &s)?;
    }
    i18n::set(l);
    rebuild_tray(&app);
    if let Some(w) = app.get_webview_window("captcha") {
        let _ = w.set_title(&i18n::tr("title.captcha"));
    }
    // 并发生效后登录窗口可能有多扇（login-*）：标题按各自邮箱重组。
    let logins: Vec<(String, Option<String>)> = {
        let pend = pending_oauth_guard();
        pend.iter().map(|(f, p)| (f.clone(), p.email.clone())).collect()
    };
    for (flow, email) in logins {
        if let Some(w) = app.get_webview_window(&login_label(&flow)) {
            let _ = w.set_title(&login_title(email.as_deref()));
        }
    }
    let _ = app.emit("state-changed", ());
    Ok(())
}

#[tauri::command]
async fn reveal_main(app: AppHandle) -> Result<(), String> {
    let win = app.get_webview_window("main").ok_or_else(|| i18n::tr("err.main.missing"))?;
    win.show().map_err(|e| e.to_string())?;
    let _ = win.set_focus();
    Ok(())
}

fn center_over_main(app: &AppHandle, win: &tauri::WebviewWindow, w: f64, h: f64) {
    let Some(m) = app.get_webview_window("main") else { return };
    let (Ok(p), Ok(s), Ok(scale)) = (m.outer_position(), m.outer_size(), m.scale_factor()) else { return };
    let (mx, my) = (p.x as f64 / scale, p.y as f64 / scale);
    let (mw, mh) = (s.width as f64 / scale, s.height as f64 / scale);
    let x = mx + (mw - w).max(0.0) / 2.0;
    let y = my + (mh - h).max(0.0) / 2.0;
    let _ = win.set_position(tauri::LogicalPosition::new(x, y));
}

#[tauri::command]
async fn autostart_status(app: AppHandle) -> Result<bool, String> {
    let al = app.state::<AutoLaunchManager>();
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
async fn autostart_set(app: AppHandle, enable: bool) -> Result<bool, String> {
    let al = app.state::<AutoLaunchManager>();
    if enable {
        al.enable().map_err(|e| e.to_string())?;
    } else {
        al.disable().map_err(|e| e.to_string())?;
    }
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
async fn export_one(app: AppHandle, id: String) -> Result<serde_json::Value, String> {
    let acc = load_account(&Paths::detect(), &id)?;
    let default_name = format!("{}.json", sanitize_filename(&acc.name));
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .set_file_name(&default_name)
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let payload = store::export_bundle_value(std::slice::from_ref(&acc));
    let body = serde_json::to_string_pretty(&payload).unwrap() + "\n";
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "name": acc.name }))
}

#[tauri::command]
async fn export_all(app: AppHandle) -> Result<serde_json::Value, String> {
    let accounts = list_accounts(&Paths::detect())?;
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty"));
    }
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .set_file_name("zcode-accounts.json")
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let payload = store::export_bundle_value(&accounts);
    let body = serde_json::to_string_pretty(&payload).unwrap() + "\n";
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "count": accounts.len() }))
}

/// 批量导出**选中的**账号：与 export_all 同为 bundle v2，只含传入子集。
/// 单个 id 读不出来（已被删 / 文件损坏）跳过不阻断整批，返回值带 skipped 计数。
#[tauri::command]
async fn export_selected(app: AppHandle, ids: Vec<String>) -> Result<serde_json::Value, String> {
    let paths = Paths::detect();
    let mut accounts = Vec::new();
    let mut skipped = 0usize;
    for id in &ids {
        match store::load_account(&paths, id) {
            Ok(a) => accounts.push(a),
            Err(_) => skipped += 1,
        }
    }
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty"));
    }
    let default_name = if accounts.len() == 1 {
        format!("{}.json", sanitize_filename(&accounts[0].name))
    } else {
        "zcode-accounts-selected.json".to_string()
    };
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .set_file_name(&default_name)
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let payload = store::export_bundle_value(&accounts);
    let body = serde_json::to_string_pretty(&payload).unwrap() + "\n";
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({
        "picked": true,
        "path": path.to_string_lossy(),
        "count": accounts.len(),
        "skipped": skipped,
    }))
}

/// 导出为 zcode2api-plus 的 providers v1 导入文件（可直接在其管理台上传导入）。
/// 与 export_all 的形状不同：只含账号名 + 登录 JWT，供对方 ImportAccounts 消费。
#[tauri::command]
async fn export_z2a(app: AppHandle) -> Result<serde_json::Value, String> {
    let paths = Paths::detect();
    let accounts = list_accounts(&paths)?;
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty"));
    }
    let out = store::export_z2a_value(&paths, &accounts);
    if out.included.is_empty() {
        return Err(i18n::trf("err.z2a.none", &[("n", &accounts.len().to_string())]));
    }
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .set_file_name("zcode2api-plus-accounts.json")
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let body = serde_json::to_string_pretty(&out.payload).unwrap() + "\n";
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({
        "picked": true,
        "path": path.to_string_lossy(),
        "count": out.included.len(),
        "skipped": out.skipped,
    }))
}

#[tauri::command]
async fn import_files(app: AppHandle) -> Result<ImportReport, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .blocking_pick_files();
    let Some(files) = picked else {
        return Ok(ImportReport { picked: false, ..Default::default() });
    };
    let _guard = store_guard();
    let mut bundles = vec![];
    let mut errors = vec![];
    for fp in files {
        let path = match fp.into_path() {
            Ok(p) => p,
            Err(e) => {
                errors.push(i18n::trf("err.path.conv", &[("e", &e.to_string())]));
                continue;
            }
        };
        let fname = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(v) => bundles.push((fname, v)),
                Err(e) => errors.push(i18n::trf("err.import.json", &[("fname", fname.as_str()), ("e", &e.to_string())])),
            },
            Err(e) => errors.push(i18n::trf("err.import.read", &[("fname", fname.as_str()), ("e", &e.to_string())])),
        }
    }
    let mut report = if bundles.is_empty() {
        ImportReport { picked: true, ..Default::default() }
    } else {
        import_values(&Paths::detect(), &bundles)?
    };
    report.picked = true;
    report.errors.extend(errors);
    rebuild_tray(&app);
    Ok(report)
}

#[tauri::command]
async fn pool_export(app: AppHandle) -> Result<serde_json::Value, String> {
    let root = Paths::detect().store_dir();
    let accounts = pool::load(&root);
    if accounts.is_empty() {
        return Err(i18n::tr("err.pool.empty"));
    }
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.txt"), &["txt"])
        .set_file_name("mailboxes.txt")
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let body: String = accounts
        .iter()
        .map(|a| format!("{}----{}----{}----{}\n", a.email, a.password, a.client_id, a.refresh_token))
        .collect();
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "count": accounts.len() }))
}

#[tauri::command]
async fn pick_zcode_path(app: AppHandle) -> Result<serde_json::Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.exe"), &["exe"])
        .blocking_pick_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy() }))
}

#[tauri::command]
async fn set_zcode_path(app: AppHandle, path: String) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    s.zcode_path = store::normalize_zcode_path(&path);
    let r = save_settings(&paths, &s);
    rebuild_tray(&app);
    let _ = app.emit("state-changed", ());
    r
}

#[tauri::command]
async fn launch_zcode(app: AppHandle) -> Result<(), String> {
    let paths = Paths::detect();
    let (p, ok) = effective_zcode_path(&paths);
    if !ok {
        return Err(i18n::trf("err.zcode.path_invalid_hint", &[("p", &p)]));
    }
    let r = store::launch_zcode(&p);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn open_external(url: String) -> Result<(), String> {
    store::open_url(&url)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]

static RELAY: std::sync::OnceLock<gateway::Gateway> = std::sync::OnceLock::new();

fn relay() -> &'static gateway::Gateway {
    RELAY.get_or_init(gateway::Gateway::new)
}

#[tauri::command]
async fn relay_status() -> Result<Value, String> {
    Ok(relay().status(&Paths::detect()))
}

#[tauri::command]
async fn relay_refresh(app: AppHandle, passes: Option<usize>) -> Result<Value, String> {
    let paths = Paths::detect();
    let gw = relay();
    let warm = gw.warm_quota(&paths, passes.unwrap_or(3), |p| {
        let _ = app.emit("relay-warm", p);
    });
    Ok(json!({ "warm": warm, "status": gw.status(&Paths::detect()) }))
}

#[tauri::command]
async fn relay_policy(policy: String, pinned: Option<String>) -> Result<Value, String> {
    relay().set_policy(policy, pinned)?;
    Ok(relay().status(&Paths::detect()))
}

#[tauri::command]
async fn relay_model_mode(mode: String, pinned: Option<String>) -> Result<Value, String> {
    let paths = Paths::detect();
    relay().set_model_mode(&paths, mode, pinned)?;
    Ok(relay().status(&paths))
}

#[tauri::command]
async fn relay_external(on: bool) -> Result<Value, String> {

    relay().set_external_and_run(&Paths::detect(), on)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(driver::plugin())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_state,
            app_version,
            capture_current,
            rename_account,
            delete_account,
            delete_many,
            update_account_from_live,
            switch_to,
            get_live_quota,
            get_account_quota,
            claim_preview,
            claim_refresh,
            claim_start,
            claim_captcha_config,
            claim_captcha_submit,
            claim_cancel,
            captcha_show,
            oauth_providers,
            oauth_begin,
            reg_input,
            reg_action,
            reg_set_mode,
            reg_close_window,
            reg_tile_windows,
            reg_fetch_link,
            pool_list,
            pool_import_pick,
            pool_remove,
            pool_remove_many,
            pool_get,
            pool_mark_verified,
            pool_reset,
            pool_upsert,
            outlook_reauth_begin,
            outlook_reauth_poll,
            set_auth_proxy,
            kill_zcode,
            set_behavior,
            set_language,
            set_reg_concurrency,
            autostart_status,
            autostart_set,
            export_one,
            export_all,
            export_selected,
            export_z2a,
            import_files,
            pool_export,
            set_cf_settings,
            cf_test,
            cf_create,
            pick_zcode_path,
            set_zcode_path,
            launch_zcode,
            open_external,
            relay_status,
            relay_refresh,
            relay_policy,
            relay_model_mode,
            relay_external,
            reveal_main,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let paths = Paths::detect();
                    if store::load_settings(&paths).close_to_tray() {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                if window.label() == "captcha" {
                    *pending_guard() = None;
                }
            }
        })
        .setup(|app| {
            i18n::init_from_settings(&store::load_settings(&Paths::detect()));
            if let Ok(data_dir) = app.path().app_local_data_dir() {
                flowlog::init(&data_dir);
            }

            let paths = Paths::detect();

            relay().set_app(app.handle().clone());

            relay().restore_persisted(&paths);
            // ⚠ 设置**只读一次**。以前这里读两遍（先判断开关、再取端口），
            //   两次之间文件若被原子替换，第二遍会读失败 → 回默认值 → 端口悄悄变 8899。
            let st0 = store::load_settings(&paths);
            if st0.relay_external == Some(true) {

                let want_port = st0
                    .relay_port
                    .unwrap_or(gateway::DEFAULT_PORT);
                match relay().start(paths, want_port) {
                    Ok(_) => relay().set_external(true),
                    Err(e) => crate::flowlog::log("relay", "start-fail", &e),
                }
            }
            let _tray = TrayIconBuilder::with_id(TRAY_ID)
                .icon(app.default_window_icon().expect("no window icon").clone())
                .tooltip("Z·POOL")
                .menu(&tray_menu_inner(app.handle())?)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    let id = event.id().as_ref().to_string();
                    match id.as_str() {
                        "show" => show_main(app),
                        "quit" => app.exit(0),
                        other => run_tray_action(app.clone(), other.to_string()),
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t_layout(cols: i32) -> LoginLayout {
        LoginLayout { x0: 100, y0: 50, ow: 500, oh: 734, gap: 10, cols }
    }

    #[test]
    fn next_free_slot_reuses_gaps() {
        let set = |v: &[i32]| -> std::collections::BTreeSet<i32> { v.iter().copied().collect() };
        assert_eq!(next_free_slot(&set(&[])), 0);
        assert_eq!(next_free_slot(&set(&[0, 1, 2])), 3);
        assert_eq!(next_free_slot(&set(&[1, 2])), 0);
        assert_eq!(next_free_slot(&set(&[0, 2])), 1);
        assert_eq!(next_free_slot(&set(&[1, 3])), 0);
    }

    #[test]
    fn grid_cols_fits_capacity() {
        // 1080p：可用 1900×1018，窗口 500×734，gap 10
        assert_eq!(grid_cols(3, 1900, 1018, 500, 734, 10), 3); // 一行三列放得下
        assert_eq!(grid_cols(2, 1900, 1018, 500, 734, 10), 2);
        // 容量 5 时两行放不下（已知物理局限）→ 退到物理上限 3 列
        assert_eq!(grid_cols(5, 1900, 1018, 500, 734, 10), 3);
        // 窄屏：物理上限 2 列
        assert_eq!(grid_cols(4, 1346, 1018, 500, 734, 10), 2);
    }

    #[test]
    fn slot_xy_places_grid_row_major() {
        let l = t_layout(3);
        assert_eq!(slot_xy(&l, 0), (100, 50));
        assert_eq!(slot_xy(&l, 1), (610, 50));
        assert_eq!(slot_xy(&l, 2), (1120, 50));
        assert_eq!(slot_xy(&l, 3), (100, 794));
        assert_eq!(slot_xy(&l, 4), (610, 794));
    }

    /// v0.2.11 回归防线：并行注册 —— A 注册完毕关闭、新窗口 D 出现时，
    /// D 必须占用 A 腾出的槽位（= 位置），且 B/C 的槽位与坐标全程不变。
    /// 旧实现每次新窗口都对全部窗口重排，B/C 会被移动（过验证码的窗口被打断）。
    #[test]
    fn new_window_reuses_released_slot_without_moving_others() {
        let mut slots: BTreeMap<String, i32> = BTreeMap::new();
        let alloc = |m: &mut BTreeMap<String, i32>, k: &str| {
            let used: std::collections::BTreeSet<i32> = m.values().copied().collect();
            m.insert(k.to_string(), next_free_slot(&used));
        };
        alloc(&mut slots, "a");
        alloc(&mut slots, "b");
        alloc(&mut slots, "c");
        assert_eq!((slots["a"], slots["b"], slots["c"]), (0, 1, 2));
        let l = t_layout(3);
        let before_b = slot_xy(&l, slots["b"]);
        let before_c = slot_xy(&l, slots["c"]);

        slots.remove("a"); // A 注册完毕/取消 → 窗口关闭释放槽位
        alloc(&mut slots, "d"); // 新窗口出现

        assert_eq!(slots["d"], 0, "新窗口应占用 A 腾出的槽位");
        assert_eq!(slots["b"], 1, "已有窗口槽位不得变动");
        assert_eq!(slots["c"], 2, "已有窗口槽位不得变动");
        assert_eq!(slot_xy(&l, slots["b"]), before_b);
        assert_eq!(slot_xy(&l, slots["c"]), before_c);
    }
}
