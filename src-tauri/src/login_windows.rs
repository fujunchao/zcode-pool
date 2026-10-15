//! 登录窗口管理：名额在初始化前预占，窗口操作统一调度到主线程。
//! 状态锁内只有纯计算，不持锁等待原生窗口事件，避免关闭回调与布局互相等待。

#[path = "login_windows/layout.rs"]
mod layout;

use crate::{flowlog, i18n};
use layout::{Display, Placement, Rect, Registry, Size};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

pub(crate) use layout::Permit;
pub(crate) const WIDTH: f64 = layout::DEFAULT_WIDTH;
pub(crate) const HEIGHT: f64 = layout::DEFAULT_HEIGHT;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

fn operations() -> &'static tauri::async_runtime::Mutex<()> {
    static OPERATIONS: OnceLock<tauri::async_runtime::Mutex<()>> = OnceLock::new();
    OPERATIONS.get_or_init(|| tauri::async_runtime::Mutex::new(()))
}

fn layout_error(error: impl std::fmt::Display) -> String {
    i18n::trf("err.reg.layout", &[("e", &error.to_string())])
}

pub(crate) fn reserve(label: &str, capacity: usize) -> Result<Permit, String> {
    registry()
        .reserve(label, capacity)
        .ok_or_else(|| i18n::tr("err.reg.too_many"))
}

fn display(monitor: &tauri::Monitor) -> Display {
    let area = monitor.work_area();
    Display {
        name: monitor.name().cloned(),
        area: Rect {
            x: area.position.x,
            y: area.position.y,
            width: area.size.width as i32,
            height: area.size.height as i32,
        },
        scale: monitor.scale_factor(),
    }
}

fn screens(app: &AppHandle, windows: &[WebviewWindow]) -> Result<(Vec<Display>, Display), String> {
    let monitors = app.available_monitors().map_err(layout_error)?;
    let preferred = app
        .get_webview_window("main")
        .and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())
        .or_else(|| {
            windows
                .iter()
                .find_map(|w| w.current_monitor().ok().flatten())
        })
        .or_else(|| monitors.first().cloned())
        .ok_or_else(|| i18n::tr("err.reg.no_monitor"))?;
    let preferred = display(&preferred);
    let mut displays: Vec<_> = monitors.iter().map(display).collect();
    if !displays.contains(&preferred) {
        displays.push(preferred.clone());
    }
    Ok((displays, preferred))
}

fn decoration(windows: &[WebviewWindow]) -> Size {
    // 仅测边框，不抽样窗口内容尺寸；按来源屏幕缩放还原为逻辑单位。
    // 默认值留有余量；最大化/最小化窗口的临时外框不参与估算。
    let mut result = Size {
        width: 20.0,
        height: 54.0,
    };
    for w in windows {
        if w.is_minimized().unwrap_or(true) || w.is_maximized().unwrap_or(true) {
            continue;
        }
        if let (Ok(outer), Ok(inner), Ok(scale)) =
            (w.outer_size(), w.inner_size(), w.scale_factor())
        {
            if scale.is_finite() && scale > 0.0 {
                result.width = result
                    .width
                    .max(outer.width.saturating_sub(inner.width) as f64 / scale);
                result.height = result
                    .height
                    .max(outer.height.saturating_sub(inner.height) as f64 / scale);
            }
        }
    }
    result
}

async fn on_main_thread<T: Send + 'static>(
    app: &AppHandle,
    operation: impl FnOnce(&AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.try_send(operation(&handle));
    })
    .map_err(layout_error)?;
    rx.recv()
        .await
        .ok_or_else(|| layout_error("window operation was interrupted"))?
}

async fn with_window<T: Send + 'static>(
    app: &AppHandle,
    label: &str,
    operation: impl FnOnce(&WebviewWindow) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let label = label.to_string();
    on_main_thread(app, move |app| {
        let window = app
            .get_webview_window(&label)
            .ok_or_else(|| i18n::tr("err.reg.no_window"))?;
        operation(&window)
    })
    .await
}

struct Geometry {
    inner: PhysicalSize<u32>,
    outer: PhysicalSize<u32>,
    scale: f64,
}

// setter 返回只说明请求已入队；让出事件循环，等实际几何生效后才能校验或继续修正。
async fn wait_geometry(
    app: &AppHandle,
    label: &str,
    size: Option<tauri::Size>,
    position: Option<PhysicalPosition<i32>>,
) -> Result<Geometry, String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let geometry = with_window(app, label, move |window| {
            if window.is_maximized().map_err(layout_error)?
                || window.is_minimized().map_err(layout_error)?
            {
                return Ok(None);
            }
            let scale = window.scale_factor().map_err(layout_error)?;
            if !scale.is_finite() || scale <= 0.0 {
                return Err(layout_error("invalid native window scale"));
            }
            let inner = window.inner_size().map_err(layout_error)?;
            if let Some(size) = size {
                let expected = size.to_physical::<u32>(scale);
                if inner.width.abs_diff(expected.width) > 1
                    || inner.height.abs_diff(expected.height) > 1
                {
                    return Ok(None);
                }
            }
            if let Some(position) = position {
                let actual = window.outer_position().map_err(layout_error)?;
                if actual.x.abs_diff(position.x) > 1 || actual.y.abs_diff(position.y) > 1 {
                    return Ok(None);
                }
            }
            Ok(Some(Geometry {
                inner,
                outer: window.outer_size().map_err(layout_error)?,
                scale,
            }))
        })
        .await?;
        if let Some(geometry) = geometry {
            return Ok(geometry);
        }
        if Instant::now() >= deadline {
            let detail = with_window(app, label, move |window| {
                Ok(format!(
                    "native window geometry did not settle: expected size={size:?} position={position:?}; actual inner={:?} outer={:?} position={:?} scale={:?}",
                    window.inner_size(), window.outer_size(), window.outer_position(), window.scale_factor(),
                ))
            }).await?;
            return Err(layout_error(detail));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn apply(app: &AppHandle, label: &str, placement: Placement) -> Result<(), String> {
    with_window(app, label, |window| {
        if window.is_maximized().map_err(layout_error)? {
            window.unmaximize().map_err(layout_error)?;
        }
        if window.is_minimized().map_err(layout_error)? {
            window.unminimize().map_err(layout_error)?;
        }
        window
            .set_min_size(None::<LogicalSize<f64>>)
            .map_err(layout_error)
    })
    .await?;
    wait_geometry(app, label, None, None).await?;

    let target = PhysicalPosition::new(placement.outer.x, placement.outer.y);
    with_window(app, label, move |window| {
        window.set_position(target).map_err(layout_error)
    })
    .await?;
    // Windows 跨屏移动会改变 DPI；macOS 移动则排入主队列，必须等移动生效再定尺寸。
    // GTK 隐藏窗口没有有效的几何事件，先提交初始位置/尺寸，再映射后确认。
    #[cfg(not(target_os = "linux"))]
    wait_geometry(app, label, None, Some(target)).await?;

    let mut size: tauri::Size =
        LogicalSize::new(placement.inner.width, placement.inner.height).into();
    with_window(app, label, move |window| {
        window.set_size(size).map_err(layout_error)?;
        #[cfg(target_os = "linux")]
        if !window.is_visible().map_err(layout_error)? {
            window.set_focusable(false).map_err(layout_error)?;
            window.show().map_err(layout_error)?;
        }
        Ok(())
    })
    .await?;
    let mut geometry = wait_geometry(app, label, Some(size), None).await?;
    let area = placement.work_area;
    // 只有目标尺寸真实生效后，才根据实际边框做屏内兜底，不能拿旧尺寸反复减差值。
    for _ in 0..2 {
        let excess_w = (geometry.outer.width as i64 - area.width as i64).max(0) as u32;
        let excess_h = (geometry.outer.height as i64 - area.height as i64).max(0) as u32;
        if excess_w == 0 && excess_h == 0 {
            break;
        }
        size = PhysicalSize::new(
            geometry.inner.width.saturating_sub(excess_w).max(1),
            geometry.inner.height.saturating_sub(excess_h).max(1),
        )
        .into();
        with_window(app, label, move |window| {
            window.set_size(size).map_err(layout_error)
        })
        .await?;
        geometry = wait_geometry(app, label, Some(size), None).await?;
    }
    if geometry.outer.width as i64 > area.width as i64
        || geometry.outer.height as i64 > area.height as i64
    {
        return Err(layout_error("native window exceeds monitor work area"));
    }
    let x = target
        .x
        .clamp(area.x, area.x + area.width - geometry.outer.width as i32);
    let y = target
        .y
        .clamp(area.y, area.y + area.height - geometry.outer.height as i32);
    let target = PhysicalPosition::new(x, y);
    with_window(app, label, move |window| {
        window.set_position(target).map_err(layout_error)
    })
    .await?;
    let geometry = wait_geometry(app, label, Some(size), Some(target)).await?;
    with_window(app, label, move |window| {
        window
            .set_min_size(Some(LogicalSize::new(
                420.0_f64.min(geometry.inner.width as f64 / geometry.scale),
                560.0_f64.min(geometry.inner.height as f64 / geometry.scale),
            )))
            .map_err(layout_error)?;
        #[cfg(target_os = "linux")]
        window.set_focusable(true).map_err(layout_error)?;
        flowlog::log(
            window.label().trim_start_matches("login-"),
            "layout",
            &format!(
                "slot={} pos=({x},{y}) outer={}x{} layered={}",
                placement.slot, geometry.outer.width, geometry.outer.height, placement.layered,
            ),
        );
        Ok(())
    })
    .await
}

fn present(window: &WebviewWindow, focus: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if !focus {
        // Tauri/Tao 的 show() 调用 makeKeyAndOrderFront，会抢走正在输入验证码的焦点。
        // 仅在主线程、窗口仍存活时访问 Tauri 提供的 NSWindow，orderFront 不改变 key window。
        let native = window.ns_window().map_err(layout_error)?;
        let native = unsafe { native.cast::<objc2::runtime::AnyObject>().as_ref() }
            .ok_or_else(|| layout_error("native window is unavailable"))?;
        unsafe {
            let _: () = objc2::msg_send![native, orderFront: None::<&objc2::runtime::AnyObject>];
        }
        return Ok(());
    }
    if !window.is_visible().map_err(layout_error)? {
        window.show().map_err(layout_error)?;
    }
    if focus {
        window.set_focus().map_err(layout_error)?;
    }
    Ok(())
}

pub(crate) async fn place(app: &AppHandle, label: &str, focus: bool) -> Result<(), String> {
    // 只串行化布局操作，不阻塞事件线程，也不让 Destroyed 回调等待此异步锁。
    let _operation = operations().lock().await;
    let label_owned = label.to_string();
    let plan = on_main_thread(app, move |app| {
        let window = app
            .get_webview_window(&label_owned)
            .ok_or_else(|| i18n::tr("err.reg.no_window"))?;
        let windows = [window.clone()];
        let (displays, preferred) = screens(app, &windows)?;
        registry()
            .place(&label_owned, &displays, &preferred, decoration(&windows))
            .map_err(layout_error)
    })
    .await?;
    apply(app, label, plan).await?;
    with_window(app, label, move |window| present(window, focus)).await
}

pub(crate) async fn arrange(app: &AppHandle) -> Result<(), String> {
    let _operation = operations().lock().await;
    let plans = on_main_thread(app, |app| {
        // 只整理已经落位的窗口；刚建好、还未安装销毁回调的隐藏窗口不提前显示。
        let windows: Vec<_> = registry()
            .placed_labels()
            .iter()
            .filter_map(|label| app.get_webview_window(label))
            .collect();
        if windows.is_empty() {
            return Ok(Vec::new());
        }
        let (_, preferred) = screens(app, &windows)?;
        let labels: Vec<_> = windows.iter().map(|w| w.label().to_string()).collect();
        registry()
            .arrange(&labels, &preferred, decoration(&windows))
            .map_err(layout_error)
    })
    .await?;
    let mut failures = Vec::new();
    for (label, plan) in plans {
        if let Err(error) = apply(app, &label, plan).await {
            // 等待原生事件期间用户可以关窗，已消失的窗口无需再报告整理失败。
            if app.get_webview_window(&label).is_some() {
                flowlog::log(label.trim_start_matches("login-"), "layout-fail", &error);
                failures.push(error);
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}
