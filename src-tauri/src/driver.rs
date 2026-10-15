use serde_json::{json, Map, Value};
use tauri::{AppHandle, Emitter, Runtime, Webview};

pub const DRIVER_JS: &str = include_str!("driver/zpool-driver.js");

pub const MODES: &[&str] = &["observe", "login", "register"];

pub const DEFAULT_MODE: &str = "login";

const KINDS: &[&str] = &["ready", "log", "phase", "note", "ask", "page"];

const STRING_FIELDS: &[&str] = &[
    "kind", "page", "phase", "code", "level", "ask", "a", "b", "url", "mode", "provider",
];

pub fn is_valid_mode(mode: &str) -> bool {
    MODES.contains(&mode)
}

pub fn normalize_mode(mode: Option<&str>) -> String {
    let m = mode.unwrap_or(DEFAULT_MODE).trim().to_ascii_lowercase();
    if is_valid_mode(&m) {
        m
    } else {
        DEFAULT_MODE.to_string()
    }
}

pub fn bootstrap_script(cfg: &Value) -> String {
    let json = serde_json::to_string(cfg).unwrap_or_else(|_| "{}".to_string());
    format!("window.__ZPOOL_CFG = {json};\n{DRIVER_JS}\n")
}

/// 独立插件只提供消息上报，不给远程登录网页开放账号管理命令。
pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("reg-driver")
        .invoke_handler(tauri::generate_handler![report])
        .build()
}

#[tauri::command]
fn report<R: Runtime>(app: AppHandle<R>, webview: Webview<R>, message: Value) -> Result<(), String> {
    if !webview.label().starts_with("login-") {
        return Err("注册消息只能由登录窗口上报".into());
    }
    // 流程由宿主窗口身份决定，网页不能伪造 flow 来影响其他并发窗口。
    let flow = crate::pending_oauth_guard()
        .values()
        .find(|p| crate::login_label(&p.flow) == webview.label())
        .map(|p| p.flow.clone());
    let Some(flow) = flow else {
        // 正常关闭时可能还有旧文档的在途消息，直接忽略，不复活已完成的流程。
        return Ok(());
    };
    let mut msg = parse_message(&message).ok_or("非法的注册驱动消息")?;
    msg["flow"] = json!(flow);
    crate::flowlog::log(&flow, "drv", &summarize(&msg));
    app.emit("reg://event", msg).map_err(|e| e.to_string())
}

pub fn parse_message(message: &Value) -> Option<Value> {
    let input = message.as_object()?;
    let mut map = Map::new();
    for key in STRING_FIELDS {
        if let Some(s) = input.get(*key).and_then(Value::as_str) {
            map.insert((*key).into(), Value::String(s.chars().take(1000).collect()));
        }
    }
    let kind = map.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if !KINDS.contains(&kind) {
        return None;
    }
    if kind == "phase" && map.get("phase").and_then(Value::as_str) == Some("done") {
        // 账号是否入库只能由后台决定，网页的“授权成功”不具备这个含义。
        return None;
    }
    Some(Value::Object(map))
}

pub fn closed_event() -> Value {
    json!({ "kind": "closed" })
}

/// 导航日志只保留站点和路径；授权码、邮箱验证 token 及片段不落盘。
pub fn url_for_log(url: &tauri::Url) -> String {
    let mut clean = url.clone();
    clean.set_query(None);
    clean.set_fragment(None);
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    clean.to_string()
}

/// Tauri 的 Finished 不区分导航成功/失败，Windows 额外记录 WebView2 的真实错误码。
#[cfg(windows)]
pub fn observe_navigation(window: &tauri::WebviewWindow, flow: &str) {
    use webview2_com::NavigationCompletedEventHandler;
    let flow = flow.to_string();
    let setup_flow = flow.clone();
    let result = window.with_webview(move |view| unsafe {
        let webview = match view.controller().CoreWebView2() {
            Ok(v) => v,
            Err(e) => {
                crate::flowlog::log(&flow, "nav-observer-fail", &e.to_string());
                return;
            }
        };
        let event_flow = flow.clone();
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            let mut success = Default::default();
            let mut id = 0;
            args.IsSuccess(&mut success)?;
            args.NavigationId(&mut id)?;
            if !success.as_bool() {
                let mut status = Default::default();
                args.WebErrorStatus(&mut status)?;
                crate::flowlog::log(&event_flow, "nav-failed", &format!("navigation_id={id} web_error_status={}", status.0));
            }
            Ok(())
        }));
        let mut token = 0;
        if let Err(e) = webview.add_NavigationCompleted(&handler, &mut token) {
            crate::flowlog::log(&flow, "nav-observer-fail", &e.to_string());
        }
    });
    if let Err(e) = result {
        crate::flowlog::log(&setup_flow, "nav-observer-fail", &e.to_string());
    }
}

pub fn summarize(msg: &Value) -> String {
    let get = |k: &str| msg.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    for k in ["kind", "level", "page", "phase", "code", "ask"] {
        let v = get(k);
        if !v.is_empty() {
            out.push_str(&format!("{k}={v} "));
        }
    }
    for k in ["a", "b", "url"] {
        let v = get(k);
        if k == "b" && get("code") == "loginClicked" {
            continue;
        }
        if !v.is_empty() {
            let clean = tauri::Url::parse(v).ok()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .map(|u| url_for_log(&u))
                .unwrap_or_else(|| v.to_string());
            let t: String = clean.chars().take(140).collect();
            out.push_str(&format!("{k}={t} "));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_message() {
        let v = parse_message(&json!({"kind":"log","level":"ok","code":"clickedCreate","a":"me@z.ai"}))
            .unwrap();
        assert_eq!(v["kind"], "log");
        assert_eq!(v["level"], "ok");
        assert_eq!(v["code"], "clickedCreate");
        assert_eq!(v["a"], "me@z.ai");
    }

    #[test]
    fn parses_note_and_phase() {
        let v = parse_message(&json!({"kind":"note","code":"settle"})).unwrap();
        assert_eq!(v["code"], "settle");
        let v = parse_message(&json!({"kind":"phase","phase":"signup-form"})).unwrap();
        assert_eq!(v["phase"], "signup-form");
    }

    #[test]
    fn rejects_unknown_kind_and_non_object() {
        assert!(parse_message(&json!({"kind":"evil"})).is_none());
        assert!(parse_message(&json!("https://example.com/?kind=log")).is_none());
        assert!(parse_message(&Value::Null).is_none());
    }

    #[test]
    fn drops_non_whitelisted_fields() {
        let v = parse_message(&json!({"kind":"log","code":"page","token":"secret","flow":"other-flow"})).unwrap();
        assert!(v.get("token").is_none());
        assert!(v.get("flow").is_none());
        assert_eq!(v["code"], "page");
    }

    #[test]
    fn truncates_oversized_field() {
        let big = "x".repeat(5000);
        let v = parse_message(&json!({"kind":"log","code":"page","a":big})).unwrap();
        assert_eq!(v["a"].as_str().unwrap().chars().count(), 1000);
    }

    #[test]
    fn normalize_mode_defaults_to_login() {
        assert_eq!(normalize_mode(Some("register")), "register");
        assert_eq!(normalize_mode(Some("REGISTER")), "register");
        assert_eq!(normalize_mode(Some("nope")), "login");
        assert_eq!(normalize_mode(None), "login");
        assert!(!is_valid_mode("nope"));
    }

    #[test]
    fn page_cannot_report_account_completion() {
        assert!(parse_message(&json!({"kind":"phase","phase":"done"})).is_none());
        assert!(parse_message(&json!({"kind":"result","ok":true})).is_none());
        assert!(parse_message(&json!({"kind":"phase","phase":"await-callback"})).is_some());
    }

    #[test]
    fn summarize_is_one_compact_line() {
        let v = parse_message(&json!({"kind":"log","level":"ok","code":"clickedCreate","a":"me@z.ai"}))
            .unwrap();
        let s = summarize(&v);
        assert!(s.contains("kind=log"));
        assert!(s.contains("level=ok"));
        assert!(s.contains("code=clickedCreate"));
        assert!(s.contains("a=me@z.ai"));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn summarize_truncates_long_values() {
        let big = "x".repeat(5000);
        let v = parse_message(&json!({"kind":"log","code":"page","a":big})).unwrap();
        let s = summarize(&v);
        assert!(s.chars().count() < 300);
    }

    #[test]
    fn summary_does_not_store_authorization_secrets() {
        let msg = json!({"kind":"page", "url":"https://zcode.z.ai/callback?code=secret-code&state=secret-state#secret-fragment"});
        let s = summarize(&msg);
        assert!(s.contains("https://zcode.z.ai/callback"));
        assert!(!s.contains("secret-"), "日志不能存授权参数：{s}");
        let s = summarize(&json!({"kind":"log","code":"loginClicked","a":"fixture-phone","b":"123456"}));
        assert!(!s.contains("123456"), "日志不能存短信验证码");
    }

    #[tauri::command]
    fn privileged_probe() -> bool { true }

    #[test]
    fn ipc_permissions_only_allow_login_pages_to_report_their_own_flow() {
        use std::sync::{Arc, Mutex};
        use tauri::Listener;
        let app = tauri::test::mock_builder()
            .plugin(plugin())
            .invoke_handler(tauri::generate_handler![privileged_probe])
            .build(crate::app_context())
            .unwrap();
        let flow = uuid::Uuid::new_v4().to_string();
        let label = crate::login_label(&flow);
        let login = tauri::WebviewWindowBuilder::new(&app, &label,
            tauri::WebviewUrl::External("https://chat.z.ai/auth".parse().unwrap())).build().unwrap();
        let main = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        let request = |cmd: &str, url: &str| tauri::webview::InvokeRequest {
            cmd: cmd.into(), callback: tauri::ipc::CallbackFn(0), error: tauri::ipc::CallbackFn(1),
            url: url.parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(json!({"message":{"kind":"ready","flow":"forged-flow","token":"secret"}})),
            headers: Default::default(), invoke_key: tauri::test::INVOKE_KEY.into(),
        };
        crate::pending_insert(crate::PendingOAuth { provider: "zai".into(), state: "fixture".into(), flow: flow.clone(), email: None });
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let events = seen.clone();
        app.listen("reg://event", move |e| events.lock().unwrap().push(serde_json::from_str(e.payload()).unwrap()));
        let result = tauri::test::get_ipc_response(&login, request("plugin:reg-driver|report", "https://chat.z.ai/auth"));
        crate::pending_remove(&flow);
        assert!(result.is_ok(), "允许的登录页面必须能使用实际 IPC：{result:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["flow"], flow);
        assert!(seen[0].get("token").is_none());
        drop(seen);
        for origin in ["https://example.com/", "https://chat.z.ai.evil.test/", "http://chat.z.ai/", "https://chat.z.ai@evil.test/"] {
            assert!(tauri::test::get_ipc_response(&login, request("plugin:reg-driver|report", origin)).is_err(), "不能放行 {origin}");
        }
        assert!(tauri::test::get_ipc_response(&main, request("plugin:reg-driver|report", "https://chat.z.ai/auth")).is_err());
        assert!(tauri::test::get_ipc_response(&login, request("privileged_probe", "https://chat.z.ai/auth")).is_err());
        let local_origin = if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" };
        assert!(tauri::test::get_ipc_response(&main, request("privileged_probe", local_origin)).is_ok(), "不能破坏主窗口现有命令权限");
    }
}
