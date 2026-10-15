//! Actions 上运行的原生 WebView 回归：真实驱动 + 真实 IPC + 本地授权/回调页面。
//! 不调用真实 OAuth，不读取账号库，不启动本程序的网关或自动任务。
#![allow(dead_code)]

#[path = "../src/driver.rs"]
mod driver;
#[path = "../src/flowlog.rs"]
mod flowlog;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{Listener, Manager};

struct PendingOAuth { flow: String }
static PENDING: Mutex<BTreeMap<String, PendingOAuth>> = Mutex::new(BTreeMap::new());
fn pending_oauth_guard() -> std::sync::MutexGuard<'static, BTreeMap<String, PendingOAuth>> {
    PENDING.lock().unwrap()
}
fn login_label(flow: &str) -> String { format!("login-{}", &flow[..12]) }

fn main() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", server.server_addr().to_ip().unwrap());
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let body = if request.url() == "/callback" {
                std::thread::sleep(Duration::from_millis(600));
                "<!doctype html><html><body>回调完成</body></html>"
            } else {
                r#"<!doctype html><html><body>
                <p>授权请求</p><input type="checkbox" checked><button id="allow">继续</button>
                <script>document.getElementById('allow').onclick = () => {
                    document.body.innerHTML = '<p>授权成功，正在跳转到应用...</p>';
                    setTimeout(() => { location.href = '/callback'; }, 50);
                };</script></body></html>"#
            };
            let response = tiny_http::Response::from_string(body)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap());
            let _ = request.respond(response);
        }
    });
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = "com.zpool.reg-driver-smoke".into();
    context.config_mut().app.windows.clear();
    let flow = uuid::Uuid::new_v4().to_string();
    pending_oauth_guard().insert(flow.clone(), PendingOAuth { flow: flow.clone() });
    let profile = std::env::temp_dir().join(format!("zpool-reg-smoke-{flow}"));
    let navigations = Arc::new(AtomicUsize::new(0));
    let passed = Arc::new(AtomicBool::new(false));
    let done = passed.clone();
    let nav_count = navigations.clone();
    let app = tauri::Builder::default()
        .plugin(driver::plugin())
        .setup(move |app| {
            let label = login_label(&flow);
            // 测试页权限只在这个独立测试进程内添加，产品权限仍仅允许正式登录站点。
            app.add_capability(json!({
                "identifier":"smoke-local-only", "local":false, "windows":[label],
                "remote":{"urls":[format!("{origin}/*")]},
                "permissions":["reg-driver:allow-report"]
            }).to_string())?;
            let callback_url = format!("{origin}/callback");
            let handle = app.handle().clone();
            let success = done.clone();
            let count = nav_count.clone();
            app.listen("reg://event", move |event| {
                let msg: Value = serde_json::from_str(event.payload()).unwrap();
                if msg["kind"] == "ready" && msg["url"].as_str() == Some(callback_url.as_str()) {
                    let ok = count.load(Ordering::SeqCst) == 0;
                    success.store(ok, Ordering::SeqCst);
                    println!("原生回调完成，导航式消息数={}，通过={ok}", count.load(Ordering::SeqCst));
                    handle.exit(if ok { 0 } else { 1 });
                }
            });
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(60));
                eprintln!("原生回调测试超时");
                handle.exit(1);
            });
            let count = nav_count.clone();
            tauri::WebviewWindowBuilder::new(app, label,
                tauri::WebviewUrl::External(format!("{origin}/authorize").parse()?))
                .visible(false)
                .focused(false)
                .data_directory(profile)
                .additional_browser_args("--no-proxy-server --disable-background-timer-throttling --disable-renderer-backgrounding")
                .initialization_script(driver::bootstrap_script(&json!({"mode":"login","auto":false,"provider":"zai"})))
                .on_navigation(move |url| {
                    if url.scheme() == "zpool-driver" {
                        count.fetch_add(1, Ordering::SeqCst);
                        return false;
                    }
                    true
                })
                .build()?;
            Ok(())
        })
        .build(context)
        .expect("创建原生测试窗口失败（需要 WebView2 Runtime）");
    let code = app.run_return(|_, _| {});
    std::process::exit(if passed.load(Ordering::SeqCst) { code } else { 1 });
}
