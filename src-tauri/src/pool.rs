use serde::{Deserialize, Serialize};
use std::path::Path;

pub const STATUS_NEW: &str = "new";
pub const STATUS_VERIFIED: &str = "verified";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_INVALID: &str = "invalid";

/// 邮箱来源：Outlook（历史数据，默认）或 cloudflare_temp_email 临时邮箱
pub const KIND_OUTLOOK: &str = "outlook";
pub const KIND_CF: &str = "cf";
/// Microsoft 365 catch-all：地址是源级共享邮箱上的"标签"，凭据在 settings.json
pub const KIND_M365: &str = "m365";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MailAccount {
    pub email: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub verified_at: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub created_at: String,
    /// 邮箱源：`outlook` 或 `cf`（serde default 保证旧 mail.json 无痛兼容）
    #[serde(default = "default_kind")]
    pub kind: String,
    /// cf 源在实例上的地址 id（用于清理；outlook 为 None）
    #[serde(default)]
    pub remote_id: Option<i64>,
}

fn default_kind() -> String {
    KIND_OUTLOOK.to_string()
}

fn default_status() -> String {
    STATUS_NEW.to_string()
}

#[derive(Serialize, Clone, Debug)]
pub struct Summary {
    pub email: String,
    pub status: String,
    pub verified_at: Option<String>,
    pub note: Option<String>,
    pub has_token: bool,
    pub created_at: String,
    pub kind: String,
}

impl MailAccount {
    pub fn is_cf(&self) -> bool {
        self.kind == KIND_CF
    }
    pub fn is_m365(&self) -> bool {
        self.kind == KIND_M365
    }
    pub fn to_summary(&self) -> Summary {
        Summary {
            email: self.email.clone(),
            status: self.status.clone(),
            verified_at: self.verified_at.clone(),
            note: self.note.clone(),
            has_token: !self.refresh_token.trim().is_empty(),
            created_at: self.created_at.clone(),
            kind: self.kind.clone(),
        }
    }
}

fn pool_file(root: &Path) -> std::path::PathBuf {
    root.join("mail.json")
}

pub fn load(root: &Path) -> Vec<MailAccount> {
    let raw = match std::fs::read_to_string(pool_file(root)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Array(a)) => {
            serde_json::from_value(serde_json::Value::Array(a)).unwrap_or_default()
        }
        Ok(v) => serde_json::from_value(v.get("accounts").cloned().unwrap_or(serde_json::Value::Array(vec![]))).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save(root: &Path, accounts: &[MailAccount]) -> Result<(), String> {
    std::fs::create_dir_all(root)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))?;
    let doc = serde_json::json!({
        "format": "zpool-mail-pool",
        "version": 1,
        "accounts": accounts,
    });
    let body = serde_json::to_string_pretty(&doc)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))?;
    std::fs::write(pool_file(root), body)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))
}

pub fn find<'a>(accounts: &'a [MailAccount], email: &str) -> Option<&'a MailAccount> {
    let e = email.trim().to_ascii_lowercase();
    accounts.iter().find(|a| a.email.to_ascii_lowercase() == e)
}

pub fn set_status(root: &Path, email: &str, status: &str, note: Option<String>) -> Result<(), String> {
    let mut accounts = load(root);
    if let Some(a) = find_mut(&mut accounts, email) {
        a.status = status.to_string();
        a.note = note;
        if status == STATUS_VERIFIED {
            a.verified_at = Some(crate::store::now_ts());
        }
        save(root, &accounts)?;
    }
    Ok(())
}

pub fn find_mut<'a>(accounts: &'a mut [MailAccount], email: &str) -> Option<&'a mut MailAccount> {
    let e = email.trim().to_ascii_lowercase();
    accounts.iter_mut().find(|a| a.email.to_ascii_lowercase() == e)
}

pub fn parse_lines(raw: &str) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split("----").collect();
        let email = f.first().copied().unwrap_or("").trim();
        if !email.contains('@') {
            continue;
        }
        let password = f.get(1).copied().unwrap_or("").trim();
        let client_id = f.get(2).copied().unwrap_or("").trim();
        let refresh_token = f.get(3).copied().unwrap_or("").trim();
        out.push((
            email.to_string(),
            password.to_string(),
            client_id.to_string(),
            refresh_token.to_string(),
        ));
    }
    out
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ImportResult {
    pub added: usize,
    pub skipped: usize,
    pub total_parsed: usize,
}

/// 按地址推断邮箱来源：与「临时邮箱服务」配置的收信域名同域（含其子域）→ cf，否则 outlook。
///
/// 为什么需要：txt 行里只有 `email----password----client_id----refresh_token`，没有来源字段，
/// 而手工维护（导入 txt / 更新一行）恰恰是 cf 地址进入邮箱库的主要路径 —— 不推断的话它们会被
/// 当成 Outlook 去 Graph 取信，永远取不到验证链接。域名是配置里现成的信息，不必让用户再选一次。
pub fn infer_kind(email: &str, cf_domain: Option<&str>, m365_domain: Option<&str>) -> &'static str {
    let Some((_, d)) = email.rsplit_once('@') else {
        return KIND_OUTLOOK;
    };
    let d = d.to_ascii_lowercase();
    let hit = |dom: Option<&str>| -> bool {
        let dom = dom
            .unwrap_or("")
            .trim()
            .trim_start_matches('@')
            .to_ascii_lowercase();
        !dom.is_empty() && (d == dom || d.ends_with(&format!(".{dom}")))
    };
    if hit(m365_domain) {
        KIND_M365
    } else if hit(cf_domain) {
        KIND_CF
    } else {
        KIND_OUTLOOK
    }
}

pub fn import(
    accounts: &mut Vec<MailAccount>,
    raw: &str,
    now: String,
    cf_domain: Option<&str>,
    m365_domain: Option<&str>,
) -> ImportResult {
    let parsed = parse_lines(raw);
    let mut r = ImportResult {
        total_parsed: parsed.len(),
        ..Default::default()
    };
    for (email, password, client_id, refresh_token) in parsed {
        if find(accounts, &email).is_some() {
            r.skipped += 1;
            continue;
        }
        let kind = infer_kind(&email, cf_domain, m365_domain).to_string();
        accounts.push(MailAccount {
            email,
            password,
            client_id,
            refresh_token,
            status: STATUS_NEW.to_string(),
            verified_at: None,
            note: None,
            created_at: now.clone(),
            kind,
            remote_id: None,
        });
        r.added += 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_four_fields() {
        let raw = "someone@example.com----pw123----00000000-0000----T.SAMPLE_token";
        let v = parse_lines(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "someone@example.com");
        assert_eq!(v[0].1, "pw123");
        assert_eq!(v[0].2, "00000000-0000");
        assert_eq!(v[0].3, "T.SAMPLE_token");
    }

    #[test]
    fn skips_bom_blank_comment_and_malformed() {
        let raw = "\u{feff}a@b.com----p1----c----r\n\n# c\nnot-an-email----pw\nfoo@bar.com\n";
        let v = parse_lines(raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "a@b.com");
        assert_eq!(v[1].0, "foo@bar.com");
        assert_eq!(v[1].1, "");
    }

    #[test]
    fn import_dedups_by_email_keeps_existing() {
        let mut acc = vec![MailAccount {
            email: "a@b.com".into(),
            password: "old".into(),
            client_id: String::new(),
            refresh_token: String::new(),
            status: STATUS_VERIFIED.into(),
            verified_at: None,
            note: None,
            created_at: String::new(),
            kind: KIND_OUTLOOK.into(),
            remote_id: None,
        }];
        let r = import(
            &mut acc,
            "a@b.com----new----c----r\nc@d.com----p----c----r\n",
            "now".into(),
            None,
            None,
        );
        assert_eq!(r.added, 1);
        assert_eq!(r.skipped, 1);
        assert_eq!(acc.len(), 2);
        assert_eq!(acc[0].password, "old", "已存在的不能被覆盖");
        assert_eq!(acc[1].status, STATUS_NEW);
        assert_eq!(acc[1].kind, KIND_OUTLOOK, "没配收信域名时一律 outlook");
    }

    #[test]
    fn import_marks_cf_addresses_by_domain() {
        // 手工导入是 cf 地址进库的主要路径：域名对得上就必须标成 cf，否则会去 Graph 白跑
        let mut acc: Vec<MailAccount> = vec![];
        let raw = "u1@mail.example.com----p----c----r\nu2@em.example.com----p----c----r\nu3@other.com----p----c----r\n";
        let r = import(&mut acc, raw, "now".into(), Some("mail.example.com"), None);
        assert_eq!(r.added, 3);
        assert_eq!(acc[0].kind, KIND_CF);
        assert_eq!(acc[1].kind, KIND_OUTLOOK, "同 apex 的子域但不匹配收信域名");
        assert_eq!(acc[2].kind, KIND_OUTLOOK);

        // 配的是 apex 域名时，其子域也算同一服务
        let mut acc2: Vec<MailAccount> = vec![];
        import(
            &mut acc2,
            "u4@mail.example.com----p----c----r\n",
            "now".into(),
            Some("example.com"),
            None,
        );
        assert_eq!(acc2[0].kind, KIND_CF);

        // 域名大小写 / 带 @ 前缀 / 带空格都要认；后缀匹配必须卡在 `.` 上（notexample.com 不算）
        assert_eq!(infer_kind("u5@Mail.Example.COM", Some(" @mail.example.com "), None), KIND_CF);
        assert_eq!(infer_kind("u6@notexample.com", Some("example.com"), None), KIND_OUTLOOK);
        assert_eq!(infer_kind("u7@x.example.com", None, Some("example.com")), KIND_M365);
        assert_eq!(infer_kind("u8@x.example.com", Some("example.com"), Some("m365.example.com")), KIND_CF);
    }

    #[test]
    fn import_marks_m365_addresses_by_domain() {
        // M365 catch-all 域名必须标成 m365，否则会被当成 Outlook 去 Graph 白跑
        let mut acc: Vec<MailAccount> = vec![];
        import(
            &mut acc,
            "m1@catch.example.com----p----c----r\nm2@example.com----p----c----r\n",
            "now".into(),
            None,
            Some("catch.example.com"),
        );
        assert_eq!(acc[0].kind, KIND_M365);
        assert_eq!(acc[1].kind, KIND_OUTLOOK);
    }

    #[test]
    fn old_json_defaults_to_outlook_kind() {
        // 旧版 mail.json 没有 kind 字段 —— 必须无痛兼容成 outlook
        let raw = r#"{"email":"a@b.com","password":"p","client_id":"","refresh_token":"","status":"new","created_at":""}"#;
        let a: MailAccount = serde_json::from_str(raw).unwrap();
        assert_eq!(a.kind, KIND_OUTLOOK);
        assert!(a.remote_id.is_none());
        assert!(!a.is_cf());
    }
}
