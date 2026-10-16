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

fn inspect<T: Send + 'static>(
    window: &WebviewWindow,
    operation: impl FnOnce(&WebviewWindow) -> SmokeResult<T> + Send + 'static,
) -> SmokeResult<T> {
    // Monitor 转换也会访问 GDK；整个查询必须在主线程，不能只把 setter 串行化。
    let (tx, rx) = std::sync::mpsc::channel();
    let native = window.clone();
    window.run_on_main_thread(move || {
        let _ = tx.send(operation(&native));
    })?;
    rx.recv_timeout(Duration::from_secs(5))?
}

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
    inspect(window, |window| {
        let p = window.outer_position()?;
        let s = window.outer_size()?;
        Ok((p.x, p.y, s.width, s.height))
    })
}

fn content_size(window: &WebviewWindow) -> SmokeResult<tauri::PhysicalSize<u32>> {
    inspect(window, |window| {
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            let native = window.gtk_window()?;
            let (width, height) = native.size();
            // Tao 的 inner_size 在 CSD 下包含阴影/标题栏，不能用它断言客户区统一。
            return Ok(LogicalSize::new(width as u32, height as u32)
                .to_physical(native.scale_factor() as f64));
        }
        #[cfg(not(target_os = "linux"))]
        Ok(window.inner_size()?)
    })
}

fn assert_visible(window: &WebviewWindow, positioned: bool) -> SmokeResult<()> {
    inspect(window, move |window| {
        let monitor = window.current_monitor()?.ok_or("窗口没有所在显示器")?;
        let area = monitor.work_area();
        let pos = window.outer_position()?;
        let size = window.outer_size()?;
        let (x, y, width, height) = (pos.x, pos.y, size.width, size.height);
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
    })
}

async fn open(app: &AppHandle, label: &str, capacity: usize) -> SmokeResult<WebviewWindow> {
    println!("创建本地测试窗口：{label}");
    let permit = login_windows::reserve(label, capacity)?;
    let profile = std::env::temp_dir().join(format!("zpool-layout-smoke-{}", uuid::Uuid::new_v4()));
    let window = tauri::WebviewWindowBuilder::new(
        app,
        label,
        tauri::WebviewUrl::External("about:blank".parse()?),
    )
    .inner_size(login_windows::WIDTH, login_windows::HEIGHT)
    .min_inner_size(login_windows::MIN_WIDTH, login_windows::MIN_HEIGHT)
    .visible(false)
    .focused(false)
    .resizable(true)
    .data_directory(profile)
    .build()?;
    login_windows::track(&window, permit);
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
    // 产品已有主窗口；稳定应用激活和标题栏状态后再测批量无焦点显示。
    let main = tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::External("about:blank".parse()?),
    )
    .inner_size(300.0, 200.0)
    .visible(true)
    .focused(true)
    .build()?;
    std::thread::sleep(Duration::from_millis(250));
    #[cfg(windows)]
    focused_close_smoke(app, &main).await?;
    #[cfg(not(windows))]
    let _ = main;
    let mut windows = Vec::new();
    for n in 0..5 {
        windows.push(open(app, &format!("login-layout-{n}"), 5).await?);
    }
    let positioned = login_windows::supports_position(app, windows[0].label()).await?;
    if std::env::var_os("ZPOOL_SMOKE_WAYLAND").is_some() && positioned {
        return Err("没有识别实际 Wayland 后端".into());
    }
    std::thread::sleep(Duration::from_millis(250));
    for window in &windows {
        assert_visible(window, positioned)?;
    }
    if positioned {
        windows[1].set_focus()?;
        wait_until("旧窗口获得焦点", || {
            windows[1].is_focused().unwrap_or(false)
        })?;
    }
    std::thread::sleep(Duration::from_millis(100));
    let before: Vec<_> = windows.iter().map(bounds).collect::<SmokeResult<_>>()?;
    let before_area = inspect(&windows[0], |w| {
        Ok(w.current_monitor()?.map(|m| m.work_area().clone()))
    })?;
    let lost_focus = Arc::new(AtomicUsize::new(0));
    let focus_events = lost_focus.clone();
    windows[1].on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Focused(false)) {
            focus_events.fetch_add(1, Ordering::SeqCst);
        }
    });
    close(app, &windows[0])?;
    let focused_after_close = windows[1].is_focused()?;
    let losses_after_close = lost_focus.load(Ordering::SeqCst);
    let replacement = open(app, "login-layout-replacement", 5).await?;
    std::thread::sleep(Duration::from_millis(150));
    let retained_focus =
        !positioned || (windows[1].is_focused()? && lost_focus.load(Ordering::SeqCst) == 0);
    println!("补位全程不抢旧窗口焦点：{retained_focus}");
    if !retained_focus {
        eprintln!("焦点诊断：关旧窗后 focused={focused_after_close} losses={losses_after_close}；新窗显示后 old_focused={} new_focused={} losses={}",
            windows[1].is_focused()?, replacement.is_focused()?, lost_focus.load(Ordering::SeqCst));
    }
    let after = bounds(&replacement)?;
    if positioned && (after.0, after.1) != (before[0].0, before[0].1) {
        return Err(format!(
            "补位没有复用原槽位：原={:?} 新={:?} 工作区原={before_area:?} 新={:?}",
            before[0],
            bounds(&replacement)?,
            inspect(&replacement, |w| Ok(w
                .current_monitor()?
                .map(|m| m.work_area().clone())))?
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
    let area = inspect(&windows[1], |w| {
        Ok(w.current_monitor()?
            .ok_or("没有显示器")?
            .work_area()
            .clone())
    })?;
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
    let one = content_size(&windows[1])?;
    let two = content_size(&replacement)?;
    // 与原生就绪判定一致：允许逻辑尺寸转换和主题取整带来的 1 个物理像素差异。
    if one.width.abs_diff(two.width) > 1 || one.height.abs_diff(two.height) > 1 {
        return Err(format!("整理未统一内容尺寸：{one:?} / {two:?}").into());
    }
    for n in 5..8 {
        let window = open(app, &format!("login-layout-{n}"), 5).await?;
        std::thread::sleep(Duration::from_millis(100));
        assert_visible(&window, positioned)?;
    }
    if !retained_focus {
        return Err("补位抢走了旧窗口的焦点".into());
    }
    println!("调整大小/最大化后整理，以及后续三次补位：通过");
    Ok(())
}

#[cfg(windows)]
fn is_foreground(window: &WebviewWindow) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    window
        .hwnd()
        .is_ok_and(|handle| unsafe { GetForegroundWindow() == handle.0 as isize })
}

#[cfg(windows)]
static WATCHED_MAIN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
#[cfg(windows)]
static MAIN_ACTIVATIONS: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
unsafe extern "system" fn foreground_event(
    _hook: isize,
    _event: u32,
    window: isize,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if window == WATCHED_MAIN.load(Ordering::SeqCst) {
        MAIN_ACTIVATIONS.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(windows)]
struct ForegroundWatch {
    main: WebviewWindow,
    hook: isize,
}

#[cfg(windows)]
impl ForegroundWatch {
    fn new(main: &WebviewWindow) -> SmokeResult<Self> {
        let hook = inspect(main, |window| {
            use windows_sys::Win32::UI::Accessibility::SetWinEventHook;
            WATCHED_MAIN.store(window.hwnd()?.0 as isize, Ordering::SeqCst);
            // 与现场记录相同：观察系统前台事件，不把 Tao 的缓存状态当作实时焦点。
            let hook = unsafe {
                SetWinEventHook(3, 3, 0, Some(foreground_event), std::process::id(), 0, 0)
            };
            if hook == 0 {
                return Err("无法订阅系统前台事件".into());
            }
            Ok(hook)
        })?;
        Ok(Self {
            main: main.clone(),
            hook,
        })
    }
}

#[cfg(windows)]
impl Drop for ForegroundWatch {
    fn drop(&mut self) {
        let hook = self.hook;
        let _ = inspect(&self.main, move |_| {
            unsafe {
                windows_sys::Win32::UI::Accessibility::UnhookWinEvent(hook);
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
async fn focused_close_smoke(app: &AppHandle, main: &WebviewWindow) -> SmokeResult<()> {
    let _watch = ForegroundWatch::new(main)?;
    for round in 0..3 {
        main.show()?;
        main.set_focus()?;
        wait_until("主窗口初始焦点", || is_foreground(main))?;
        let mut old = Vec::new();
        for n in 0..3 {
            let window = open(app, &format!("login-focus-{round}-old-{n}"), 3).await?;
            window.set_focus()?;
            wait_until("旧登录窗口焦点", || is_foreground(&window))?;
            old.push(window);
        }
        // 与现场一致：旧窗逐个完成，中间的新补位窗只显示、从未被激活。
        close(app, &old[1])?;
        let next = open(app, &format!("login-focus-{round}-next"), 3).await?;
        close(app, &old[0])?;
        if !is_foreground(&old[2]) || is_foreground(&next) {
            return Err("焦点关闭回归的初始状态不成立".into());
        }
        let next_before = bounds(&next)?;
        std::thread::sleep(Duration::from_millis(100));
        MAIN_ACTIVATIONS.store(0, Ordering::SeqCst);
        close(app, &old[2])?;
        wait_until("焦点交给从未激活的补位窗", || {
            is_foreground(&next)
        })?;
        std::thread::sleep(Duration::from_millis(100));
        if MAIN_ACTIVATIONS.load(Ordering::SeqCst) != 0 || is_foreground(main) {
            return Err("关闭当前注册窗时主界面曾抢到前台".into());
        }
        if bounds(&next)? != next_before {
            return Err("焦点交接移动或缩放了补位窗".into());
        }
        println!(
            "第 {} 轮：关闭当前焦点窗，主界面未抢前台且补位窗接到焦点：通过",
            round + 1
        );

        // 用户主动回到主界面后，关掉后台注册窗不能再把焦点抢回来。
        let background = open(app, &format!("login-focus-{round}-background"), 3).await?;
        main.set_focus()?;
        wait_until("用户主动切回主窗口", || is_foreground(main))?;
        close(app, &next)?;
        if !is_foreground(main) || is_foreground(&background) {
            return Err("关闭后台注册窗干扰了用户选择的焦点".into());
        }
        close(app, &background)?;
    }
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
