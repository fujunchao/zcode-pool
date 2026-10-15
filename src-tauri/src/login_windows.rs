//! 登录窗口管理：名额在初始化前预占，窗口操作统一调度到主线程。
//! 状态锁内只有纯计算，不持锁等待原生窗口事件，避免关闭回调与布局互相等待。

#[path = "login_windows/layout.rs"]
mod layout;

use crate::{flowlog, i18n};
use layout::{Display, Placement, Rect, Registry, Size};
use std::sync::OnceLock;
use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

pub(crate) use layout::Permit;
pub(crate) const WIDTH: f64 = layout::DEFAULT_WIDTH;
pub(crate) const HEIGHT: f64 = layout::DEFAULT_HEIGHT;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

fn layout_error(error: impl std::fmt::Display) -> String {
    i18n::trf("err.reg.layout", &[("e", &error.to_string())])
}

pub(crate) fn reserve(label: &str, capacity: usize) -> Result<Permit, String> {
    registry().reserve(label, capacity).ok_or_else(|| i18n::tr("err.reg.too_many"))
}

fn display(monitor: &tauri::Monitor) -> Display {
    let area = monitor.work_area();
    Display {
        name: monitor.name().cloned(),
        area: Rect {
            x: area.position.x, y: area.position.y,
            width: area.size.width as i32, height: area.size.height as i32,
        },
        scale: monitor.scale_factor(),
    }
}

fn screens(app: &AppHandle, windows: &[WebviewWindow]) -> Result<(Vec<Display>, Display), String> {
    let monitors = app.available_monitors().map_err(layout_error)?;
    let preferred = app.get_webview_window("main").and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())
        .or_else(|| windows.iter().find_map(|w| w.current_monitor().ok().flatten()))
        .or_else(|| monitors.first().cloned())
        .ok_or_else(|| i18n::tr("err.reg.no_monitor"))?;
    let preferred = display(&preferred);
    let mut displays: Vec<_> = monitors.iter().map(display).collect();
    if !displays.contains(&preferred) { displays.push(preferred.clone()); }
    Ok((displays, preferred))
}

fn decoration(windows: &[WebviewWindow]) -> Size {
    // 仅测边框，不抽样窗口内容尺寸；按来源屏幕缩放还原为逻辑单位。
    // 默认值留有余量；最大化/最小化窗口的临时外框不参与估算。
    let mut result = Size { width: 20.0, height: 54.0 };
    for w in windows {
        if w.is_minimized().unwrap_or(true) || w.is_maximized().unwrap_or(true) { continue; }
        if let (Ok(outer), Ok(inner), Ok(scale)) = (w.outer_size(), w.inner_size(), w.scale_factor()) {
            if scale.is_finite() && scale > 0.0 {
                result.width = result.width.max(outer.width.saturating_sub(inner.width) as f64 / scale);
                result.height = result.height.max(outer.height.saturating_sub(inner.height) as f64 / scale);
            }
        }
    }
    result
}

fn apply(window: &WebviewWindow, placement: Placement) -> Result<(), String> {
    if window.is_maximized().map_err(layout_error)? { window.unmaximize().map_err(layout_error)?; }
    if window.is_minimized().map_err(layout_error)? { window.unminimize().map_err(layout_error)?; }
    // 先移入目标屏幕，再设置逻辑尺寸，不能复用移动前的物理窗口尺寸。
    window.set_min_size(None::<LogicalSize<f64>>).map_err(layout_error)?;
    window.set_position(PhysicalPosition::new(placement.outer.x, placement.outer.y)).map_err(layout_error)?;
    window.set_size(LogicalSize::new(placement.inner.width, placement.inner.height)).map_err(layout_error)?;
    let area = placement.work_area;
    // 原生主题/边框可能与估算略有差异，以实际外框做最终可见性校验。
    for _ in 0..2 {
        let outer = window.outer_size().map_err(layout_error)?;
        let inner = window.inner_size().map_err(layout_error)?;
        let excess_w = (outer.width as i64 - area.width as i64).max(0) as u32;
        let excess_h = (outer.height as i64 - area.height as i64).max(0) as u32;
        if excess_w == 0 && excess_h == 0 { break; }
        window.set_size(PhysicalSize::new(
            inner.width.saturating_sub(excess_w).max(1),
            inner.height.saturating_sub(excess_h).max(1),
        )).map_err(layout_error)?;
    }
    let outer = window.outer_size().map_err(layout_error)?;
    if outer.width as i64 > area.width as i64 || outer.height as i64 > area.height as i64 {
        return Err(layout_error("native window exceeds monitor work area"));
    }
    let x = placement.outer.x.clamp(area.x, area.x + area.width - outer.width as i32);
    let y = placement.outer.y.clamp(area.y, area.y + area.height - outer.height as i32);
    window.set_position(PhysicalPosition::new(x, y)).map_err(layout_error)?;
    let inner = window.inner_size().map_err(layout_error)?;
    let scale = window.scale_factor().map_err(layout_error)?;
    window.set_min_size(Some(LogicalSize::new(
        420.0_f64.min(inner.width as f64 / scale),
        560.0_f64.min(inner.height as f64 / scale),
    ))).map_err(layout_error)?;
    flowlog::log(window.label().trim_start_matches("login-"), "layout", &format!(
        "slot={} pos=({x},{y}) outer={}x{} layered={}", placement.slot, outer.width, outer.height, placement.layered,
    ));
    Ok(())
}

async fn on_main_thread<T: Send + 'static>(
    app: &AppHandle, operation: impl FnOnce(&AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    let handle = app.clone();
    app.run_on_main_thread(move || { let _ = tx.try_send(operation(&handle)); }).map_err(layout_error)?;
    rx.recv().await.ok_or_else(|| layout_error("window operation was interrupted"))?
}

pub(crate) async fn place(app: &AppHandle, label: &str, focus: bool) -> Result<(), String> {
    let label = label.to_string();
    on_main_thread(app, move |app| {
        let window = app.get_webview_window(&label).ok_or_else(|| i18n::tr("err.reg.no_window"))?;
        let windows = [window.clone()];
        let (displays, preferred) = screens(app, &windows)?;
        let plan = registry().place(&label, &displays, &preferred, decoration(&windows)).map_err(layout_error)?;
        apply(&window, plan)?;
        window.show().map_err(layout_error)?;
        if focus { window.set_focus().map_err(layout_error)?; }
        Ok(())
    }).await
}

pub(crate) async fn arrange(app: &AppHandle) -> Result<(), String> {
    on_main_thread(app, |app| {
        // 只整理已经落位的窗口；刚建好、还未安装销毁回调的隐藏窗口不提前显示。
        let windows: Vec<_> = registry().placed_labels().iter()
            .filter_map(|label| app.get_webview_window(label)).collect();
        if windows.is_empty() { return Ok(()); }
        let (_, preferred) = screens(app, &windows)?;
        let labels: Vec<_> = windows.iter().map(|w| w.label().to_string()).collect();
        let plans = registry().arrange(&labels, &preferred, decoration(&windows)).map_err(layout_error)?;
        let mut failures = Vec::new();
        for (label, plan) in plans {
            if let Some(window) = app.get_webview_window(&label) {
                if let Err(error) = apply(&window, plan) {
                    flowlog::log(label.trim_start_matches("login-"), "layout-fail", &error);
                    failures.push(error);
                }
            }
        }
        if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
    }).await
}
