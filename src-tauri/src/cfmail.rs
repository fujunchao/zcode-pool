//! cloudflare_temp_email 邮箱源：建地址 + 取信 + 从原始 MIME 里抠验证链接。
//!
//! 目标是自部署的 dreamhunter2333/cloudflare_temp_email（2026-10 实测过的接口形状）：
//!
//! - 建地址 `POST {base}/admin/new_address`（头 `x-admin-auth: 管理员密码`）
//!   请求 `{"enablePrefix":false,"name":"xxx","domain":"可选"}`
//!   返回 `{"jwt":..,"address":"xxx@domain","password":..,"address_id":N}`
//! - 取信   `GET {base}/admin/mails?limit=N&offset=0&address=<addr>`（同上一把管理员密码）
//!   返回 `{"results":[{id,source,address,raw,created_at,..}]}`。
//!   ⚠ `raw` 就是**原始 MIME**，正文常见 base64 或 quoted-printable 编码 —— 必须解码后再扫链接；
//!   ⚠ 该接口**不带 offset 会 400**（本实例的行为）。
//! - 删地址 `DELETE {base}/admin/delete_address/{id}`
//!
//! created_at 形如 `2026-10-12 15:03:36`，是 **UTC**。

#![allow(dead_code)] // `delete_address` 暂无调用点（保留：将来做「从邮箱库移除即清理远端地址」）

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

/// 只认这个域下的链接（与 Outlook 那条线的 graph.rs 保持一致）
const LINK_HOST: &str = "https://chat.z.ai/";

#[derive(Clone, Debug)]
pub struct CfCfg {
    pub base_url: String,
    pub admin_password: String,
    pub domain: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Created {
    pub address: String,
    pub jwt: String,
    pub password: String,
    pub address_id: i64,
}

#[derive(Debug, Clone)]
pub struct RawMail {
    pub id: i64,
    pub raw: String,
    pub created_ms: Option<i64>,
    pub source: String,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
}

pub fn norm_base(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

impl CfCfg {
    pub fn is_ready(&self) -> bool {
        !norm_base(&self.base_url).is_empty() && !self.admin_password.trim().is_empty()
    }
    fn url(&self, path: &str) -> String {
        format!("{}{}", norm_base(&self.base_url), path)
    }
}

fn http_err(what: &str, e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let short: String = body.chars().take(160).collect();
            format!("{what} HTTP {code}：{short}")
        }
        other => format!("{what}：{other}"),
    }
}

/// 新建一个地址。`name` 是 @ 前面的本地部分（自己生成随机名，避免撞名）。
pub fn create_address(cfg: &CfCfg, name: &str) -> Result<Created, String> {
    if !cfg.is_ready() {
        return Err("还没有配置临时邮箱服务（base URL / 管理员密码）".into());
    }
    let body = serde_json::json!({
        "enablePrefix": false,
        "name": name.trim(),
        "domain": cfg.domain.as_deref().unwrap_or("").trim(),
    });
    let resp = agent()
        .post(&cfg.url("/admin/new_address"))
        .set("x-admin-auth", cfg.admin_password.trim())
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| http_err("建地址失败", e))?
        .into_string()
        .map_err(|e| format!("建地址返回读取失败：{e}"))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp.clone()));
    let address = v.get("address").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let jwt = v.get("jwt").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if address.is_empty() || jwt.is_empty() {
        let short: String = resp.chars().take(160).collect();
        return Err(format!("建地址返回缺字段：{short}"));
    }
    Ok(Created {
        address,
        jwt,
        password: v.get("password").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        address_id: v.get("address_id").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// 连通性/鉴权探测：拉一封邮件，返回**服务上可见的邮件总数**。
///
/// 注意别用 `results.len()` 当"总数"：`limit=1` 时它恒为 0/1，界面会显示成"可见 1 封"而误导。
/// 该实例的响应带顶层 `count`（总数），没有它才退回 `results` 长度。
pub fn probe(cfg: &CfCfg) -> Result<usize, String> {
    if !cfg.is_ready() {
        return Err("还没有配置临时邮箱服务（base URL / 管理员密码）".into());
    }
    let resp = agent()
        .get(&cfg.url("/admin/mails?limit=1&offset=0"))
        .set("x-admin-auth", cfg.admin_password.trim())
        .call()
        .map_err(|e| http_err("连接测试失败", e))?
        .into_string()
        .map_err(|e| format!("连接测试返回读取失败：{e}"))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    Ok(visible_count(&v))
}

/// 从 /admin/mails 的响应里取"可见邮件总数"：顶层 `count` 优先，没有才退回 `results` 长度。
fn visible_count(v: &Value) -> usize {
    if let Some(n) = v.get("count").and_then(|x| x.as_u64()) {
        return n as usize;
    }
    v.get("results")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// 删除地址（连同其邮件）。失败不致命，调用方自行决定是否忽略。
pub fn delete_address(cfg: &CfCfg, address_id: i64) -> Result<(), String> {
    if address_id == 0 {
        return Ok(());
    }
    agent()
        .delete(&cfg.url(&format!("/admin/delete_address/{address_id}")))
        .set("x-admin-auth", cfg.admin_password.trim())
        .call()
        .map_err(|e| http_err("删地址失败", e))?;
    Ok(())
}

/// 取某个地址最近的邮件（原始 MIME）。
pub fn fetch_mails(cfg: &CfCfg, address: &str, limit: usize) -> Result<Vec<RawMail>, String> {
    if !cfg.is_ready() {
        return Err("还没有配置临时邮箱服务（base URL / 管理员密码）".into());
    }
    let url = cfg.url(&format!(
        "/admin/mails?limit={}&offset=0&address={}",
        limit.clamp(1, 50),
        urlencode(address.trim())
    ));
    let resp = agent()
        .get(&url)
        .set("x-admin-auth", cfg.admin_password.trim())
        .call()
        .map_err(|e| http_err("取信失败", e))?
        .into_string()
        .map_err(|e| format!("取信返回读取失败：{e}"))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let arr = v
        .get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .map(|m| RawMail {
            id: m.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
            raw: m.get("raw").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            created_ms: m.get("created_at").and_then(|x| x.as_str()).and_then(created_at_ms),
            source: m.get("source").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
        .collect())
}

/// `2026-10-12 15:03:36`（UTC）→ epoch 毫秒
pub fn created_at_ms(s: &str) -> Option<i64> {
    use chrono::{NaiveDateTime, TimeZone, Utc};
    let dt = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&dt).timestamp_millis())
}

/// 从一批原始邮件里抠 chat.z.ai 链接；verify_email 的排在前面。
/// 返回 (links, scanned)；`since_ms` 用于只要新邮件。
pub fn links_from_mails(mails: &[RawMail], since_ms: Option<i64>) -> (Vec<String>, usize) {
    let mut verify: Vec<String> = vec![];
    let mut other: Vec<String> = vec![];
    let mut scanned = 0usize;
    for m in mails {
        if let (Some(since), Some(at)) = (since_ms, m.created_ms) {
            if at < since {
                continue;
            }
        }
        if m.raw.is_empty() {
            continue;
        }
        scanned += 1;
        for l in extract_links(&m.raw) {
            let is_verify = l.contains("verify_email");
            if verify.iter().any(|x| x == &l) || other.iter().any(|x| x == &l) {
                continue;
            }
            if is_verify {
                verify.push(l);
            } else {
                other.push(l);
            }
        }
    }
    verify.extend(other);
    (verify, scanned)
}

/// 给控制台「查看邮件」用的摘要：字段名与 Graph 通道对齐
/// （`messages[].{id,subject,from,receivedAt,preview,text,links}`），控制台前端不用改就能显示临时邮箱的邮件。
pub fn fetch_summaries(cfg: &CfCfg, address: &str, limit: usize) -> Result<Vec<Value>, String> {
    let mails = fetch_mails(cfg, address, limit)?;
    Ok(mails.iter().map(summary_of).collect())
}

/// 单封邮件的摘要（与 Graph 通道同形状）。抽出来是为了能脱离网络单测。
fn summary_of(m: &RawMail) -> Value {
    let (hd, _) = split_hb(&m.raw);
    let hs = parse_headers(hd);
    let subject = hget(&hs, "subject").map(decode_rfc2047).unwrap_or_default();
    let from = hget(&hs, "from").map(decode_rfc2047).unwrap_or_default();
    let mut texts: Vec<String> = vec![];
    walk(&m.raw, 0, &mut texts);
    let body = texts.join("\n").trim().to_string();
    let preview: String = body.chars().take(200).collect();
    let received = m
        .created_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();
    json!({
        "id": m.id,
        "subject": subject,
        "from": from,
        "receivedAt": received,
        "preview": preview,
        "text": body,
        "body": body,
        "links": extract_links(&m.raw),
    })
}

/// 解 RFC2047 编码字（`=?utf-8?b?…?=` / `=?utf-8?q?…?=`）——真实邮件里 Subject 就长这样
/// （`=?utf-8?b?6aqM6K+B5oKo55qE55S15a2Q6YKu566x?=` = 「验证您的电子邮箱」）。
/// 解不开的整段原样保留，别把信息弄丢。
fn decode_rfc2047(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("=?") {
        let Some(rel_end) = rest[start + 2..].find("?=") else {
            break;
        };
        let end = start + 2 + rel_end;
        let parts: Vec<&str> = rest[start + 2..end].splitn(3, '?').collect();
        let decoded = if parts.len() == 3 {
            match parts[1].to_ascii_lowercase().as_str() {
                "b" => B64
                    .decode(parts[2].as_bytes())
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned()),
                "q" => Some(decode_qp(&parts[2].replace('_', " "))),
                _ => None,
            }
        } else {
            None
        };
        out.push_str(&rest[..start]);
        match decoded {
            Some(d) => out.push_str(&d),
            None => out.push_str(&rest[start..end + 2]),
        }
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

// ---------- 原始 MIME 解码 ----------
/// 从 raw MIME 里提取 chat.z.ai 链接（解码 base64 / quoted-printable / 折行头）。
pub fn extract_links(raw: &str) -> Vec<String> {
    let mut texts: Vec<String> = vec![];
    walk(raw, 0, &mut texts);
    let mut out: Vec<String> = vec![];
    for t in texts {
        let t = unescape(&t);
        let mut from = 0usize;
        while let Some(rel) = t[from..].find(LINK_HOST) {
            let start = from + rel;
            let rest = &t[start..];
            let end = rest
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '\\' | '`' | '，' | '。')
                })
                .unwrap_or(rest.len());
            let mut link = rest[..end].to_string();
            while let Some(last) = link.chars().last() {
                if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '&' | '"' | '\'') {
                    link.pop();
                } else {
                    break;
                }
            }
            if link.len() > LINK_HOST.len() && !out.iter().any(|x| x == &link) {
                out.push(link);
            }
            from = start + end.max(1);
        }
    }
    out
}

fn split_hb(s: &str) -> (&str, &str) {
    if let Some(i) = s.find("\r\n\r\n") {
        return (&s[..i], &s[i + 4..]);
    }
    if let Some(i) = s.find("\n\n") {
        return (&s[..i], &s[i + 2..]);
    }
    (s, "")
}

/// 解析头部（含折行续行合并）；键统一小写。
fn parse_headers(hd: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for line in hd.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    out
}

fn hget<'a>(hs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    hs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str())
}

fn boundary_of(ct: &str) -> Option<String> {
    let lower = ct.to_ascii_lowercase();
    let i = lower.find("boundary=")? + "boundary=".len();
    let rest = ct[i..].trim_start();
    let val = if let Some(r) = rest.strip_prefix('"') {
        r.split('"').next().unwrap_or("").to_string()
    } else {
        rest.split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    };
    (!val.is_empty()).then_some(val)
}

fn walk(part: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 8 {
        return;
    }
    let (hd, body) = split_hb(part);
    let hs = parse_headers(hd);
    let ct = hget(&hs, "content-type").unwrap_or("").to_string();
    if ct.trim().to_ascii_lowercase().starts_with("multipart/") {
        if let Some(b) = boundary_of(&ct) {
            for seg in split_parts(body, &b) {
                walk(&seg, depth + 1, out);
            }
            return;
        }
    }
    let cta = hget(&hs, "content-transfer-encoding").unwrap_or("");
    out.push(decode_body(body, cta));
}

/// 按 boundary 切段（行首匹配；兼容 \r\n 与 \n）。
/// 返回所有权切片：内部可能先把 \r\n 规范化到临时缓冲，借出去会触发 E0515。
fn split_parts(body: &str, boundary: &str) -> Vec<String> {
    let norm_owned;
    let norm: &str = if body.contains("\r\n") {
        norm_owned = body.replace("\r\n", "\n");
        &norm_owned
    } else {
        body
    };
    let marker = format!("--{boundary}");
    let mut segs: Vec<String> = vec![];
    let mut rest: &str = match norm.find(&marker) {
        Some(i) => &norm[i + marker.len()..],
        None => return segs,
    };
    loop {
        if rest.starts_with("--") {
            break;
        }
        rest = rest.trim_start_matches('\n');
        // 找下一个「行首 boundary」
        let line_marker = format!("\n{marker}");
        match rest.find(&line_marker) {
            Some(i) => {
                segs.push(rest[..i].to_string());
                rest = &rest[i + 1 + marker.len()..];
            }
            None => {
                segs.push(rest.to_string());
                break;
            }
        }
    }
    segs.retain(|s| !s.trim().is_empty());
    segs
}

fn decode_body(body: &str, cta: &str) -> String {
    let c = cta.to_ascii_lowercase();
    if c.contains("base64") {
        let clean: String = body.chars().filter(|ch| ch.is_ascii_alphanumeric() || *ch == '+' || *ch == '/' || *ch == '=').collect();
        return B64
            .decode(clean.as_bytes())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| body.to_string());
    }
    if c.contains("quoted-printable") {
        return decode_qp(body);
    }
    body.to_string()
}

fn decode_qp(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'=' {
            if i + 2 < b.len() && b[i + 1] == b'\r' && b[i + 2] == b'\n' {
                i += 3;
                continue;
            }
            if i + 1 < b.len() && b[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 < b.len() {
                if let (Some(x), Some(y)) = (hexv(b[i + 1]), hexv(b[i + 2])) {
                    out.push(x << 4 | y);
                    i += 3;
                    continue;
                }
            }
            out.push(b'=');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexv(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_LINK: &str = "https://chat.z.ai/auth/verify_email?token=verify-1314b38449d7fce835389bff2e59e3cc&email=a%40mail.example.com&username=godpej&language=zh";

    /// 真实验证信的结构（取自实例实信，值已匿名化）：
    /// multipart/alternative；顶层 Content-Type **折行**（boundary 落在第二行）；
    /// text/plain 走 base64，text/html 走 quoted-printable —— 而 QP 的软换行会把
    /// URL **拦腰截断**（真实样本里 verify token 与 `godpej` 都被切在中间）。
    ///
    /// 注意：发件人保证用软换行（`=` ⏎）折行，所以能拼回整条；若将来遇到
    /// 硬折行（直接断行、无 `=`）的发件人，这里得另加跨行拼接逻辑。
    fn real_shape_mail() -> String {
        let plain = B64.encode(format!("你的验证码是……\r\n{FULL_LINK}\r\n"));
        format!(
            "Received: from x\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative;\r\n boundary=\"b1_abc\"\r\n\r\n\
preamble\r\n\
--b1_abc\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\n\
{plain}\r\n\
--b1_abc\r\n\
Content-Type: text/html; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\r\n\
<tr><td align=3D\"center\">\r\n\
  <a href=3D\"https://chat.z.ai/auth/verify_email?token=3Dverify-1314b38449=\r\n\
d7fce835389bff2e59e3cc&amp;email=3Da%40mail.example.com&amp;username=3Dgo=\r\n\
dpej&amp;language=3Dzh\" target=3D\"_blank\">=E9=AA=8C=E8=AF=81</a>\r\n\
--b1_abc--\r\n"
        )
    }

    #[test]
    fn qp_multipart_extracts_full_link() {
        let links = extract_links(&real_shape_mail());
        // base64 段与 QP 段给出同一条链接 → 去重后只留 1 条
        assert_eq!(links.len(), 1, "links={links:?}");
        assert_eq!(links[0], FULL_LINK);
    }

    /// 真实样本里 token 正好被软换行切开（`…1314b38449=` ⏎ `d7fce8…`），必须拼回整条；
    /// 拼不回来的话会得到一条「看似合法但必然验不过」的截断链接。
    #[test]
    fn token_split_by_soft_break_is_rejoined() {
        let sample = "Content-Type: text/html; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\r\n\
<a href=3D\"https://chat.z.ai/auth/verify_email?token=3Dverify-1314b38449=\r\n\
d7fce835389bff2e59e3cc\">x</a>\r\n";
        let links = extract_links(sample);
        assert_eq!(links, vec!["https://chat.z.ai/auth/verify_email?token=verify-1314b38449d7fce835389bff2e59e3cc".to_string()]);
    }

    #[test]
    fn base64_part_extracts_link() {
        let html = "<a href=\"https://chat.z.ai/auth/verify_email?token=verify-b64&email=b%40x.com\">ok</a>";
        let b64 = B64.encode(html.as_bytes());
        let sample = format!(
            "Content-Type: text/html; charset=\"utf-8\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{b64}\r\n"
        );
        let links = extract_links(&sample);
        assert_eq!(links, vec!["https://chat.z.ai/auth/verify_email?token=verify-b64&email=b%40x.com".to_string()]);
    }

    #[test]
    fn soft_break_inside_url_is_removed() {
        let sample = "Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
https://chat.z.ai/auth/verify_email?token=verify-soft=\r\nbreak&x=1\r\n";
        let links = extract_links(sample);
        assert_eq!(links[0], "https://chat.z.ai/auth/verify_email?token=verify-softbreak&x=1");
    }

    /// `limit=1` 时 `results` 恒为 1 —— 总数必须取顶层 `count`，否则界面会显示"可见 1 封"。
    #[test]
    fn visible_count_prefers_top_level_count() {
        let with_count: Value =
            serde_json::from_str(r#"{"count":232,"results":[{"id":1,"raw":"x"}]}"#).unwrap();
        assert_eq!(visible_count(&with_count), 232);
        // 没有 count 字段的老实例：退回 results 长度
        let no_count: Value = serde_json::from_str(r#"{"results":[{},{}]}"#).unwrap();
        assert_eq!(visible_count(&no_count), 2);
        assert_eq!(visible_count(&Value::Null), 0);
    }

    /// 真实样本的 Subject 就是 RFC2047 编码字，必须解出来（控制台里要能看懂）
    #[test]
    fn rfc2047_subject_decodes() {
        assert_eq!(decode_rfc2047("=?utf-8?b?6aqM6K+B5oKo55qE55S15a2Q6YKu566x?="), "验证您的电子邮箱");
        // 编码字与明文混排（真实 #232 就是「=?utf-8?b?…?= Z.ai」这种）
        assert_eq!(decode_rfc2047("=?utf-8?b?5qyi6L+O5Yqg5YWl?= Z.ai"), "欢迎加入 Z.ai");
        // Q 编码：下划线代表空格
        assert_eq!(decode_rfc2047("=?utf-8?Q?=E9=AA=8C=E8=AF=81_ok?="), "验证 ok");
        // 解不开的整段原样保留，别丢信息
        assert_eq!(decode_rfc2047("plain subject"), "plain subject");
        assert_eq!(decode_rfc2047("=?utf-8?b?@@@?="), "=?utf-8?b?@@@?=");
    }

    /// 控制台「查看邮件」走这条：字段名与 Graph 通道一致，并且要带上抠出来的链接
    #[test]
    fn summary_matches_graph_shape() {
        let raw = format!(
            "From: Z.ai <noreply@email.z.ai>\r\nSubject: =?utf-8?b?6aqM6K+B5oKo55qE55S15a2Q6YKu566x?=\r\n{}",
            real_shape_mail()
        );
        let m = RawMail {
            id: 7,
            raw,
            created_ms: Some(1_791_817_380_000),
            source: "noreply@email.z.ai".into(),
        };
        let v = summary_of(&m);
        assert_eq!(v["id"], 7);
        assert_eq!(v["subject"], "验证您的电子邮箱");
        assert_eq!(v["from"], "Z.ai <noreply@email.z.ai>");
        assert_eq!(v["receivedAt"], "2026-10-12T15:03:00Z");
        let links = v["links"].as_array().unwrap();
        assert_eq!(links.len(), 1, "links={links:?}");
        assert_eq!(links[0], FULL_LINK);
        assert!(v["text"].as_str().unwrap().contains("chat.z.ai"), "正文应含链接");
    }

    #[test]
    fn created_at_parses_utc() {
        // 2026-10-12 15:03:36 UTC = 1786460616000 ms? 用相对校验更稳
        let ms = created_at_ms("2026-10-12 15:03:36").unwrap();
        let diff = ms - created_at_ms("2026-10-12 15:03:35").unwrap();
        assert_eq!(diff, 1000);
        assert!(created_at_ms("不是时间").is_none());
    }

    #[test]
    fn links_from_mails_filters_by_since_and_prefers_verify() {
        let mails = vec![
            RawMail { id: 1, raw: "Content-Type: text/plain\r\n\r\nhttps://chat.z.ai/help\r\n".into(), created_ms: Some(1000), source: "a".into() },
            RawMail { id: 2, raw: "Content-Type: text/plain\r\n\r\nhttps://chat.z.ai/auth/verify_email?token=t2\r\n".into(), created_ms: Some(2000), source: "b".into() },
            RawMail { id: 3, raw: "Content-Type: text/plain\r\n\r\nhttps://chat.z.ai/auth/verify_email?token=t0\r\n".into(), created_ms: Some(500), source: "c".into() },
        ];
        let (links, scanned) = links_from_mails(&mails, Some(1500));
        assert_eq!(scanned, 1, "since 之后的只有 1 封");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "https://chat.z.ai/auth/verify_email?token=t2");

        let (all, scanned_all) = links_from_mails(&mails, None);
        assert_eq!(scanned_all, 3);
        assert_eq!(all[0], "https://chat.z.ai/auth/verify_email?token=t2");
        assert_eq!(all[1], "https://chat.z.ai/auth/verify_email?token=t0");
        assert_eq!(all[2], "https://chat.z.ai/help");
    }
}
