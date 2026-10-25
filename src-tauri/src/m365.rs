//! Microsoft 365 catch-all 邮箱源（委派模式，走 Microsoft Graph）。
//!
//! 与 `graph.rs`（个人 Microsoft 账户，consumers 权威 + /me）的关键差异：
//! - 权威/租户来自设置（工作账号必须用 organizations 或租户 ID）；
//! - **一个 catch-all 邮箱服务所有并发注册流程**，取信必须按收件人归因 + 去重；
//! - 列表接口的正文只有 HTML；先取薄列表（不含 body），命中后再单封补正文，避免拉一堆营销邮件；
//! - 每邮箱 4 并发 / 10 分钟 10k 请求的官方限制，因此所有流程共用一份短 TTL 快照（单飞）。

use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

use crate::i18n;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const AUTH: &str = "https://login.microsoftonline.com";
const SCOPE: &str = "https://graph.microsoft.com/Mail.Read offline_access";
const TIMEOUT_SECS: u64 = 25;
const THIN_TOP: usize = 50;
const CACHE_TTL_MS: u64 = 1500;
const MAX_BODY_FETCH: usize = 3;
const MAX_CONSUMED: usize = 200;
const CONSUMED_TTL_MS: u64 = 3_600_000;

/// 邮箱源的连接配置（来自 settings.json；refresh_token 不在 Cfg 里，由调用方单独保管）。
#[derive(Clone, Debug)]
pub struct Cfg {
    pub tenant: String,
    pub client_id: String,
    pub mailbox: String,
    pub domain: String,
}

impl Cfg {
    pub fn is_ready(&self) -> bool {
        !self.tenant.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.mailbox.trim().is_empty()
            && !self.domain.trim().is_empty()
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn auth_url(cfg: &Cfg, tail: &str) -> String {
    format!(
        "{}/{}/oauth2/v2.0/{}",
        AUTH,
        cfg.tenant.trim().trim_matches('/'),
        tail
    )
}

fn http_err(e: &str) -> String {
    i18n::trf("err.m365.http", &[("e", e)])
}

fn json_err(e: std::io::Error) -> String {
    http_err(&e.to_string())
}

fn oauth_detail(v: &Value) -> String {
    v.get("error_description")
        .and_then(Value::as_str)
        .or_else(|| v.get("error").and_then(Value::as_str))
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// 把 AAD/OAuth 错误翻成可操作的提示（前端 reg.js 的 isCredentialError 依赖 invalid_grant/AADSTS 字样）。
fn oauth_error(v: &Value) -> String {
    let desc = oauth_detail(v);
    let raw = format!(
        "{} {}",
        v.get("error").and_then(Value::as_str).unwrap_or(""),
        desc
    );
    if raw.contains("AADSTS7000218") {
        return i18n::tr("err.m365.public_client");
    }
    if raw.contains("AADSTS65001") || raw.contains("consent_required") {
        return i18n::tr("err.m365.consent");
    }
    if raw.contains("invalid_grant")
        || raw.contains("AADSTS70000")
        || raw.contains("AADSTS70008")
        || raw.contains("AADSTS50173")
        || raw.contains("AADSTS7000215")
    {
        return i18n::tr("err.m365.reauth");
    }
    http_err(&desc)
}

fn parse_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn status_value(r: ureq::Response) -> Value {
    parse_body(&r.into_string().unwrap_or_default())
}

pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

pub enum LoginPoll {
    Pending,
    Done(String),
}

/// 发起设备码登录（委派）。scope 只要 Mail.Read + offline_access。
pub fn login_begin(cfg: &Cfg) -> Result<DeviceCode, String> {
    if !cfg.is_ready() {
        return Err(i18n::tr("err.m365.noconfig"));
    }
    let resp = agent()
        .post(&auth_url(cfg, "devicecode"))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[("client_id", cfg.client_id.trim()), ("scope", SCOPE)]);
    let v = match resp {
        Ok(r) => r.into_json::<Value>().map_err(json_err)?,
        Err(ureq::Error::Status(_, r)) => return Err(oauth_error(&status_value(r))),
        Err(e) => return Err(http_err(&e.to_string())),
    };
    Ok(DeviceCode {
        device_code: v.get("device_code").and_then(Value::as_str).unwrap_or("").to_string(),
        user_code: v.get("user_code").and_then(Value::as_str).unwrap_or("").to_string(),
        verification_uri: v
            .get("verification_uri")
            .and_then(Value::as_str)
            .unwrap_or("https://microsoft.com/devicelogin")
            .to_string(),
        interval: v.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1),
        expires_in: v.get("expires_in").and_then(Value::as_u64).unwrap_or(900),
    })
}

/// 轮询设备码。Pending 表示用户还没点完；Done(refresh_token) 表示拿到凭据。
pub fn login_poll(cfg: &Cfg, device_code: &str) -> Result<LoginPoll, String> {
    if !cfg.is_ready() {
        return Err(i18n::tr("err.m365.noconfig"));
    }
    let resp = agent()
        .post(&auth_url(cfg, "token"))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", cfg.client_id.trim()),
            ("device_code", device_code),
        ]);
    match resp {
        Ok(r) => {
            let v = r.into_json::<Value>().map_err(json_err)?;
            match v.get("refresh_token").and_then(Value::as_str) {
                Some(t) if !t.trim().is_empty() => Ok(LoginPoll::Done(t.to_string())),
                _ => Ok(LoginPoll::Pending),
            }
        }
        Err(ureq::Error::Status(_, r)) => {
            let v = status_value(r);
            let err = v.get("error").and_then(Value::as_str).unwrap_or("");
            if err == "authorization_pending" || err == "slow_down" {
                return Ok(LoginPoll::Pending);
            }
            if err == "expired_token" {
                return Err(i18n::tr("err.m365.login_expired"));
            }
            Err(oauth_error(&v))
        }
        Err(e) => Err(http_err(&e.to_string())),
    }
}

/// refresh_token 换 access_token；返回 (access, 轮换后的 refresh_token)。
pub fn access_token(cfg: &Cfg, refresh_token: &str) -> Result<(String, Option<String>), String> {
    if !cfg.is_ready() {
        return Err(i18n::tr("err.m365.noconfig"));
    }
    let resp = agent()
        .post(&auth_url(cfg, "token"))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("client_id", cfg.client_id.trim()),
            ("refresh_token", refresh_token.trim()),
            ("scope", SCOPE),
        ]);
    let v = match resp {
        Ok(r) => r.into_json::<Value>().map_err(json_err)?,
        Err(ureq::Error::Status(_, r)) => return Err(oauth_error(&status_value(r))),
        Err(e) => return Err(http_err(&e.to_string())),
    };
    let at = v
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| i18n::tr("err.m365.reauth"))?;
    let new_rt = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(String::from)
        .filter(|s| s.trim() != refresh_token.trim());
    Ok((at.to_string(), new_rt))
}

#[derive(Clone, Debug, Default)]
pub struct Candidate {
    pub id: String,
    pub subject: String,
    pub received_ms: i64,
    pub to: Vec<String>,
    pub cc: Vec<String>,
}

fn addr_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    x.pointer("/emailAddress/address")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_string())
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_candidates(v: &Value) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Some(arr) = v.get("value").and_then(Value::as_array) else {
        return out;
    };
    for m in arr {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let subject = m.get("subject").and_then(Value::as_str).unwrap_or("").to_string();
        let received_ms = m
            .get("receivedDateTime")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis())
            .unwrap_or(0);
        out.push(Candidate {
            id,
            subject,
            received_ms,
            to: addr_list(m.get("toRecipients")),
            cc: addr_list(m.get("ccRecipients")),
        });
    }
    out
}

fn thin_url(cfg: &Cfg, junk: bool) -> String {
    let folder = if junk { "mailFolders/junkemail/messages" } else { "messages" };
    format!(
        "{}/users/{}/{}?$top={}&$select=id,subject,receivedDateTime,toRecipients,ccRecipients&$orderby=receivedDateTime%20desc",
        GRAPH,
        cfg.mailbox.trim(),
        folder,
        THIN_TOP
    )
}

fn graph_get(url: &str, token: &str) -> Result<Value, String> {
    match agent()
        .get(url)
        .set("Authorization", &format!("Bearer {}", token))
        .call()
    {
        Ok(r) => r.into_json::<Value>().map_err(json_err),
        Err(ureq::Error::Status(429, _)) => Err(i18n::tr("err.m365.throttled")),
        Err(ureq::Error::Status(code, r)) => {
            let detail: String = r.into_string().unwrap_or_default().chars().take(160).collect();
            Err(http_err(&format!("HTTP {code} {detail}")))
        }
        Err(e) => Err(http_err(&e.to_string())),
    }
}

fn fetch_body(cfg: &Cfg, token: &str, id: &str) -> Result<String, String> {
    let url = format!("{}/users/{}/messages/{}?$select=body", GRAPH, cfg.mailbox.trim(), id);
    let v = graph_get(&url, token)?;
    Ok(v.pointer("/body/content").and_then(Value::as_str).unwrap_or("").to_string())
}

static SNAP: Mutex<Option<(u64, Vec<Candidate>, Vec<Candidate>)>> = Mutex::new(None);

/// 收件箱 + 垃圾箱的薄列表快照：1.5s 内所有流程复用，避免并发超过 Graph 的 4 限制。
fn snapshot(cfg: &Cfg, token: &str) -> Result<(Vec<Candidate>, Vec<Candidate>), String> {
    if let Ok(g) = SNAP.lock() {
        if let Some((at, inbox, junk)) = g.as_ref() {
            if now_ms().saturating_sub(*at) < CACHE_TTL_MS {
                return Ok((inbox.clone(), junk.clone()));
            }
        }
    }
    let inbox = parse_candidates(&graph_get(&thin_url(cfg, false), token)?);
    // 垃圾箱读失败不影响主流程（验证信多数进收件箱）
    let junk = parse_candidates(&graph_get(&thin_url(cfg, true), token).unwrap_or(Value::Null));
    if let Ok(mut g) = SNAP.lock() {
        *g = Some((now_ms(), inbox.clone(), junk.clone()));
    }
    Ok((inbox, junk))
}

fn hexv(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hexv(b[i + 1]), hexv(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn is_url_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '\\' | '`' | '，' | '。' | '）' | '、' | '“' | '”'
        )
}

fn param_value(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.eq_ignore_ascii_case(key) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 只认 /auth/verify_email 且带 token 的链接；若链接带 email= 参数，必须与目标地址一致。
pub fn extract_verify_links(html: &str, address: &str) -> Vec<String> {
    let hay = html_unescape(html);
    let mut out: Vec<String> = Vec::new();
    const NEEDLES: [&str; 2] = [
        "https://chat.z.ai/auth/verify_email?",
        "http://chat.z.ai/auth/verify_email?",
    ];
    for needle in NEEDLES {
        let mut i = 0usize;
        while let Some(rel) = hay[i..].find(needle) {
            let start = i + rel;
            let rest = &hay[start..];
            let end = rest.find(is_url_end).unwrap_or(rest.len());
            let mut url = rest[..end].to_string();
            i = start + end.max(1);
            while let Some(last) = url.chars().last() {
                if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '&' | '"' | '\'') {
                    url.pop();
                } else {
                    break;
                }
            }
            if !url.contains("token=") {
                continue;
            }
            if let Some(email) = param_value(&url, "email") {
                if !pct_decode(&email).trim().eq_ignore_ascii_case(address.trim()) {
                    continue;
                }
            }
            if !out.iter().any(|u| u == &url) {
                out.push(url);
            }
        }
    }
    out
}

fn recipient_match(c: &Candidate, address: &str) -> bool {
    c.to
        .iter()
        .chain(c.cc.iter())
        .any(|a| a.trim().eq_ignore_ascii_case(address.trim()))
}

/// 纯函数：按 收件人 → 时间窗 → 已消费 → 链接自证 的顺序挑一封。
pub fn pick_link(
    address: &str,
    since_ms: i64,
    items: &[(Candidate, String)],
    consumed: &[String],
) -> Option<(String, String)> {
    for (c, html) in items {
        if !recipient_match(c, address) {
            continue;
        }
        if c.received_ms < since_ms {
            continue;
        }
        if consumed.iter().any(|id| id == &c.id) {
            continue;
        }
        if let Some(link) = extract_verify_links(html, address).into_iter().next() {
            return Some((link, c.id.clone()));
        }
    }
    None
}

static CONSUMED: Mutex<Vec<(String, String, u64)>> = Mutex::new(Vec::new());

fn consumed_ids(flow: &str) -> Vec<String> {
    let now = now_ms();
    let Ok(mut g) = CONSUMED.lock() else {
        return Vec::new();
    };
    g.retain(|(_, _, at)| now.saturating_sub(*at) < CONSUMED_TTL_MS);
    g.iter()
        .filter(|(f, _, _)| f == flow)
        .map(|(_, id, _)| id.clone())
        .collect()
}

fn mark_consumed(flow: &str, id: &str) {
    let Ok(mut g) = CONSUMED.lock() else { return };
    g.push((flow.to_string(), id.to_string(), now_ms()));
    while g.len() > MAX_CONSUMED {
        g.remove(0);
    }
}

fn iso_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

/// 取信主入口：返回结构与 graph.rs / cf 分支一致（links / scanned / newestSubject / newestAt）。
pub fn fetch_for(
    cfg: &Cfg,
    access: &str,
    address: &str,
    since_ms: i64,
    flow: &str,
) -> Result<Value, String> {
    let (inbox, junk) = snapshot(cfg, access)?;
    let newest = inbox.first().or_else(|| junk.first());
    let newest_subject = newest
        .map(|c| c.subject.chars().take(80).collect::<String>())
        .unwrap_or_default();
    let newest_at = newest.map(|c| iso_of(c.received_ms)).unwrap_or_default();
    let scanned = inbox.len() + junk.len();

    let mut candidates: Vec<Candidate> = inbox
        .iter()
        .chain(junk.iter())
        .filter(|c| recipient_match(c, address) && c.received_ms >= since_ms)
        .cloned()
        .collect();
    candidates.sort_by(|a, b| b.received_ms.cmp(&a.received_ms));
    candidates.truncate(MAX_BODY_FETCH);

    let consumed = consumed_ids(flow);
    let mut items: Vec<(Candidate, String)> = Vec::new();
    for c in candidates {
        if consumed.iter().any(|id| id == &c.id) {
            continue;
        }
        if let Ok(html) = fetch_body(cfg, access, &c.id) {
            items.push((c, html));
        }
    }

    let links: Vec<String> = match pick_link(address, since_ms, &items, &consumed) {
        Some((link, id)) => {
            mark_consumed(flow, &id);
            vec![link]
        }
        None => Vec::new(),
    };

    Ok(json!({
        "links": links,
        "scanned": scanned,
        "newestSubject": newest_subject,
        "newestAt": newest_at,
    }))
}

/// 测试连通性：拉一封信，返回最新主题（没有邮件返回 None）。
pub fn probe(cfg: &Cfg, access: &str) -> Result<Option<String>, String> {
    let url = format!(
        "{}/users/{}/messages?$top=1&$select=id,subject,receivedDateTime",
        GRAPH,
        cfg.mailbox.trim()
    );
    let v = graph_get(&url, access)?;
    Ok(parse_candidates(&v).first().map(|c| c.subject.clone()))
}

/// 生成 <10 位小写字母数字>@domain —— 与现有 cf 地址风格一致，且便于在邮件里肉眼识别。
pub fn random_address(domain: &str) -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("{}@{}", &raw[..10], domain.trim().trim_start_matches('@'))
}

pub fn is_credential_error(msg: &str) -> bool {
    msg.contains("invalid_grant")
        || msg.contains("AADSTS70000")
        || msg.contains("AADSTS70008")
        || msg.contains("AADSTS50173")
        || msg.contains("AADSTS7000215")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, to: &[&str], cc: &[&str], at: i64) -> Candidate {
        Candidate {
            id: id.to_string(),
            subject: format!("s-{id}"),
            received_ms: at,
            to: to.iter().map(|s| s.to_string()).collect(),
            cc: cc.iter().map(|s| s.to_string()).collect(),
        }
    }

    const VERIFY_HTML: &str = r#"<p>点这里验证：<a href="https://chat.z.ai/auth/verify_email?token=verify-abc&amp;email=abc123def4%40example.onmicrosoft.com&amp;username=someone&amp;language=zh">验证邮箱</a></p>"#;
    const WELCOME_HTML: &str = r#"<p>欢迎加入 <a href="https://chat.z.ai/auth">Z.ai</a></p>"#;

    #[test]
    fn extracts_verify_link_and_unescapes_amp() {
        let links = extract_verify_links(VERIFY_HTML, "abc123def4@example.onmicrosoft.com");
        assert_eq!(links.len(), 1);
        assert!(links[0].starts_with("https://chat.z.ai/auth/verify_email?token=verify-abc&email="));
    }

    #[test]
    fn ignores_welcome_page_link() {
        assert!(extract_verify_links(WELCOME_HTML, "abc123def4@example.onmicrosoft.com").is_empty());
    }

    #[test]
    fn rejects_link_for_another_address() {
        assert!(extract_verify_links(VERIFY_HTML, "other@example.onmicrosoft.com").is_empty());
    }

    #[test]
    fn matches_address_case_insensitively() {
        let links = extract_verify_links(VERIFY_HTML, "ABC123DEF4@example.onmicrosoft.com");
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn pick_link_uses_recipient_attribution() {
        let a = cand("m1", &["abc123def4@example.onmicrosoft.com"], &[], 1_000);
        let b = cand("m2", &["other@example.onmicrosoft.com"], &[], 2_000);
        let items = vec![(b, VERIFY_HTML.to_string()), (a, VERIFY_HTML.to_string())];
        let hit = pick_link("abc123def4@example.onmicrosoft.com", 0, &items, &[]);
        assert_eq!(hit.map(|(_, id)| id).as_deref(), Some("m1"));
    }

    #[test]
    fn pick_link_accepts_cc_recipient() {
        let c = cand("m1", &[], &["abc123def4@example.onmicrosoft.com"], 1_000);
        let items = vec![(c, VERIFY_HTML.to_string())];
        assert!(pick_link("abc123def4@example.onmicrosoft.com", 0, &items, &[]).is_some());
    }

    #[test]
    fn pick_link_honours_since_and_consumed() {
        let c = cand("m1", &["abc123def4@example.onmicrosoft.com"], &[], 1_000);
        let items = vec![(c.clone(), VERIFY_HTML.to_string())];
        assert!(pick_link("abc123def4@example.onmicrosoft.com", 2_000, &items, &[]).is_none());
        assert!(pick_link("abc123def4@example.onmicrosoft.com", 0, &items, &["m1".to_string()]).is_none());
    }

    #[test]
    fn pct_decode_handles_at_sign() {
        assert_eq!(pct_decode("a%40b.com"), "a@b.com");
    }

    #[test]
    fn random_address_has_expected_shape() {
        let a = random_address("example.onmicrosoft.com");
        let (local, domain) = a.split_once('@').unwrap();
        assert_eq!(domain, "example.onmicrosoft.com");
        assert_eq!(local.len(), 10);
        assert!(local.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn credential_error_detection() {
        assert!(is_credential_error("AADSTS70008: expired"));
        assert!(!is_credential_error("AADSTS65001: consent"));
    }
}
