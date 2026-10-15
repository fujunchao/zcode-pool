//! 仅在 CI 创建本地空白 WebView 窗口，验证真实布局，不连接 OAuth 或读取账号库。
#![allow(dead_code)]

#[path = "../src/flowlog.rs"]
mod flowlog;
#[path = "../src/login_windows.rs"]
mod login_windows;

mod i18n {
    pub fn tr(key: &str) -> String {
        key.to_string()
    }
    pub fn trf(key: &str, values: &[(&str, &str)]) -> String {
        format!("{key}: {values:?}")
    }
}

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, LogicalSize, Manager, WebviewWindow};

type SmokeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn wait_until(description: &str, mut ready: impl FnMut() -> bool) -> SmokeResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        if Instant::now() >= deadline {
            return Err(format!("等待{description}超时").into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn bounds(window: &WebviewWindow) -> SmokeResult<(i32, i32, u32, u32)> {
    let p = window.outer_position()?;
    let s = window.outer_size()?;
    Ok((p.x, p.y, s.width, s.height))
}

fn assert_visible(window: &WebviewWindow, positioned: bool) -> SmokeResult<()> {
    let monitor = window.current_monitor()?.ok_or("窗口没有所在显示器")?;
    let area = monitor.work_area();
    let (x, y, width, height) = bounds(window)?;
    if !window.is_visible()?
        || width > area.size.width
        || height > area.size.height
        || (positioned
            && (x < area.position.x
                || y < area.position.y
                || x as i64 + width as i64 > area.position.x as i64 + area.size.width as i64
                || y as i64 + height as i64 > area.position.y as i64 + area.size.height as i64))
    {
        return Err(format!(
            "{} 未完整落在工作区：({x},{y}) {width}×{height} / {area:?}",
            window.label()
        )
        .into());
    }
    Ok(())
}

async fn open(app: &AppHandle, label: &str) -> SmokeResult<WebviewWindow> {
    println!("创建本地测试窗口：{label}");
    let permit = login_windows::reserve(label, 5)?;
    let profile = std::env::temp_dir().join(format!("zpool-layout-smoke-{}", uuid::Uuid::new_v4()));
    let window = tauri::WebviewWindowBuilder::new(
        app,
        label,
        tauri::WebviewUrl::External("about:blank".parse()?),
    )
    .inner_size(login_windows::WIDTH, login_windows::HEIGHT)
    .min_inner_size(420.0, 560.0)
    .visible(false)
    .focused(false)
    .resizable(true)
    .data_directory(profile)
    .build()?;
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            permit.release();
        }
    });
    login_windows::place(app, label, false).await?;
    println!("落位完成：{label} {:?}", bounds(&window)?);
    Ok(window)
}

fn close(app: &AppHandle, window: &WebviewWindow) -> SmokeResult<()> {
    window.close()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.get_webview_window(window.label()).is_some() {
        if Instant::now() >= deadline {
            return Err("等待窗口销毁超时".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

async fn smoke(app: &AppHandle) -> SmokeResult<()> {
    let mut windows = Vec::new();
    for n in 0..5 {
        windows.push(open(app, &format!("login-layout-{n}")).await?);
    }
    let positioned = login_windows::supports_position(app, windows[0].label()).await?;
    if std::env::var_os("ZPOOL_SMOKE_WAYLAND").is_some() && positioned {
        return Err("没有识别实际 Wayland 后端".into());
    }
    std::thread::sleep(Duration::from_millis(250));
    for window in &windows {
        assert_visible(window, positioned)?;
    }
    let before: Vec<_> = windows.iter().map(bounds).collect::<SmokeResult<_>>()?;
    let before_area = windows[0].current_monitor()?.map(|m| m.work_area().clone());
    if positioned {
        windows[1].set_focus()?;
        wait_until("旧窗口获得焦点", || {
            windows[1].is_focused().unwrap_or(false)
        })?;
    }
    let lost_focus = Arc::new(AtomicUsize::new(0));
    let focus_events = lost_focus.clone();
    windows[1].on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Focused(false)) {
            focus_events.fetch_add(1, Ordering::SeqCst);
        }
    });
    close(app, &windows[0])?;
    let replacement = open(app, "login-layout-replacement").await?;
    std::thread::sleep(Duration::from_millis(150));
    let retained_focus =
        !positioned || (windows[1].is_focused()? && lost_focus.load(Ordering::SeqCst) == 0);
    println!("补位全程不抢旧窗口焦点：{retained_focus}");
    if positioned && bounds(&replacement)? != before[0] {
        return Err(format!(
            "补位没有复用原槽位：原={:?} 新={:?} 工作区原={before_area:?} 新={:?}",
            before[0],
            bounds(&replacement)?,
            replacement
                .current_monitor()?
                .map(|m| m.work_area().clone())
        )
        .into());
    }
    for n in 1..5 {
        if positioned && bounds(&windows[n])? != before[n] {
            return Err("补位移动或缩放了已有窗口".into());
        }
    }
    println!("五窗口均在工作区内；关闭后补位不移动已有窗口：通过");

    for window in &windows[2..] {
        close(app, window)?;
    }
    let area = windows[1]
        .current_monitor()?
        .ok_or("没有显示器")?
        .work_area()
        .clone();
    let scale = windows[1].scale_factor()?;
    windows[1].set_size(LogicalSize::new(
        area.size.width as f64 / scale + 200.0,
        area.size.height as f64 / scale + 200.0,
    ))?;
    replacement.maximize()?;
    std::thread::sleep(Duration::from_millis(150));
    login_windows::arrange(app).await?;
    std::thread::sleep(Duration::from_millis(150));
    assert_visible(&windows[1], positioned)?;
    assert_visible(&replacement, positioned)?;
    if replacement.is_maximized()? {
        return Err("整理没有恢复最大化窗口".into());
    }
    let one = windows[1].inner_size()?;
    let two = replacement.inner_size()?;
    if one != two {
        return Err(format!("整理未统一内容尺寸：{one:?} / {two:?}").into());
    }
    for n in 5..8 {
        let window = open(app, &format!("login-layout-{n}")).await?;
        std::thread::sleep(Duration::from_millis(100));
        assert_visible(&window, positioned)?;
    }
    if !retained_focus {
        return Err("补位抢走了旧窗口的焦点".into());
    }
    println!("调整大小/最大化后整理，以及后续三次补位：通过");
    Ok(())
}

fn main() {
    // macOS 的原生 run_return 可能忽略 exit(code) 的 code，独立保存测试结论防止假绿。
    let exit_code = Arc::new(AtomicI32::new(1));
    let outcome_code = exit_code.clone();
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = "com.zpool.reg-layout-smoke".into();
    context.config_mut().app.windows.clear();
    let app = tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let outcome = tauri::async_runtime::block_on(smoke(&handle));
                match outcome {
                    Ok(()) => {
                        outcome_code.store(0, Ordering::SeqCst);
                        handle.exit(0);
                    }
                    Err(error) => {
                        eprintln!("原生布局回归失败：{error}");
                        handle.exit(1);
                    }
                }
            });
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(90));
                eprintln!("原生布局回归超时");
                handle.exit(1);
            });
            Ok(())
        })
        .build(context)
        .expect("创建原生布局测试环境失败");
    app.run_return(|_, _| {});
    std::process::exit(exit_code.load(Ordering::SeqCst));
}
