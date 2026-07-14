//! Admin authentication for protected routes.

use super::response::{Body, Response};

const DEFAULT_ADMIN_PASSWORD: &str = "admin";
pub const ADMIN_SESSION_COOKIE: &str = "orgos_admin_session";

/// Get the admin password from env or use default (with warning).
pub fn get_admin_password() -> String {
    use std::sync::OnceLock;
    static PASSWORD: OnceLock<String> = OnceLock::new();
    static WARNED: OnceLock<()> = OnceLock::new();

    PASSWORD.get_or_init(|| {
        match std::env::var("ORGOS_ADMIN_PASSWORD") {
            Ok(pw) if !pw.is_empty() => pw,
            _ => {
                WARNED.get_or_init(|| {
                    crate::obs::warn(
                        "ORGOS_ADMIN_PASSWORD not set, using default password 'admin'. \
                         Set ORGOS_ADMIN_PASSWORD env var for production use."
                    );
                });
                DEFAULT_ADMIN_PASSWORD.to_string()
            }
        }
    }).clone()
}

/// Generate a simple session token from the password (HMAC-like hash).
pub fn generate_session_token(password: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    password.hash(&mut hasher);
    "orgos_".to_string() + &format!("{:x}", hasher.finish())
}

/// Check if the request has valid admin authentication.
/// Returns Ok(Some(session_token)) if authenticated via Basic Auth (needs cookie set),
/// Ok(None) if authenticated via session cookie (no new cookie needed),
/// Err(response) if not authenticated.
pub fn check_admin_auth(cookies: &str, auth_header: Option<&str>) -> Result<Option<String>, Response> {
    let password = get_admin_password();
    let valid_token = generate_session_token(&password);

    // Check session cookie first
    for part in cookies.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            if k.trim() == ADMIN_SESSION_COOKIE && v.trim() == valid_token {
                return Ok(None); // Already authenticated via cookie
            }
        }
    }

    // Check Basic Auth header
    if let Some(auth) = auth_header {
        if let Some(encoded) = auth.strip_prefix("Basic ") {
            if let Ok(decoded) = base64_decode(encoded.trim()) {
                if let Ok(creds) = String::from_utf8(decoded) {
                    // Format: username:password (username is ignored)
                    if let Some((_, pw)) = creds.split_once(':') {
                        if pw == password {
                            return Ok(Some(valid_token)); // Authenticated, set cookie
                        }
                    }
                }
            }
        }
    }

    // Not authenticated - return login page
    Err(login_response(false))
}

/// Simple base64 decode (no external crate needed for this basic case).
fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in input.bytes() {
        let val = CHARS.iter().position(|&x| x == c).ok_or(())? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// HTML login page with modern design.
pub fn login_page(error: bool) -> String {
    let error_msg = if error {
        r#"<div class="error">잘못된 비밀번호입니다</div>"#
    } else {
        ""
    };
    format!(r##"<!doctype html>
<html lang="ko"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>로그인 — Orgos</title>
<style>
:root {{ --accent: #0d9488; --bg: #f8fafc; --card: #ffffff; --border: #e2e8f0; --text: #1e293b; --muted: #64748b; --error: #ef4444; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #0f172a; --card: #1e293b; --border: #334155; --text: #f1f5f9; --muted: #94a3b8; }}
}}
* {{ box-sizing: border-box; }}
body {{ font-family: system-ui, -apple-system, sans-serif; background: var(--bg); margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center; padding: 1rem; }}
.card {{ background: var(--card); border: 1px solid var(--border); border-radius: 24px; padding: 2.5rem; width: 100%; max-width: 380px; box-shadow: 0 4px 24px rgba(0,0,0,.08); }}
.logo {{ display: flex; align-items: center; justify-content: center; gap: .5rem; margin-bottom: 2rem; color: var(--text); text-decoration: none; }}
.logo svg {{ width: 40px; height: 40px; }}
.logo span {{ font-size: 1.5rem; font-weight: 600; }}
h1 {{ font-size: 1.25rem; font-weight: 600; color: var(--text); text-align: center; margin: 0 0 .5rem; }}
.subtitle {{ color: var(--muted); text-align: center; font-size: .9rem; margin-bottom: 1.5rem; }}
.field {{ margin-bottom: 1rem; }}
.field label {{ display: block; font-size: .875rem; font-weight: 500; color: var(--text); margin-bottom: .5rem; }}
.field input {{ width: 100%; padding: .875rem 1rem; font-size: 1rem; border: 1px solid var(--border); border-radius: 12px; background: var(--bg); color: var(--text); outline: none; transition: border-color .15s, box-shadow .15s; }}
.field input:focus {{ border-color: var(--accent); box-shadow: 0 0 0 3px rgba(13,148,136,.15); }}
.btn {{ width: 100%; padding: 1rem; font-size: 1rem; font-weight: 600; border: none; border-radius: 12px; background: var(--accent); color: white; cursor: pointer; transition: opacity .15s; margin-top: .5rem; }}
.btn:hover {{ opacity: .9; }}
.error {{ background: rgba(239,68,68,.1); color: var(--error); padding: .75rem 1rem; border-radius: 10px; font-size: .875rem; margin-bottom: 1rem; text-align: center; }}
.back {{ display: block; text-align: center; margin-top: 1.5rem; color: var(--muted); font-size: .875rem; text-decoration: none; }}
.back:hover {{ color: var(--accent); }}
</style>
</head><body>
<div class="card">
  <a class="logo" href="/"><svg viewBox="0 0 64 64" fill="none"><circle cx="32" cy="32" r="28" stroke="currentColor" stroke-width="4"/><circle cx="32" cy="32" r="12" fill="currentColor"/></svg><span>Orgos</span></a>
  <h1>관리자 로그인</h1>
  <p class="subtitle">설정 페이지에 접근하려면 로그인이 필요합니다</p>
  {error_msg}
  <form method="post" action="/login">
    <div class="field">
      <label for="password">비밀번호</label>
      <input type="password" id="password" name="password" placeholder="비밀번호 입력" autofocus required>
    </div>
    <button type="submit" class="btn">로그인</button>
  </form>
  <a class="back" href="/">← 홈으로 돌아가기</a>
</div>
</body></html>"##, error_msg = error_msg)
}

/// Build login page response.
pub fn login_response(error: bool) -> Response {
    Response {
        status: if error { 401 } else { 200 },
        content_type: "text/html; charset=utf-8".to_string(),
        body: Body::Text(login_page(error)),
        cache: "no-store",
        location: None,
        set_cookie: None,
        rate_limit_remaining: None,
        www_authenticate: if error { Some("Basic realm=\"Orgos Admin\"".to_string()) } else { None },
    }
}

/// Handle login POST request.
pub fn handle_login(body: &str) -> Response {
    let password = get_admin_password();

    // Parse form data using url crate
    let fields: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    let pw_value = fields.get("password").map(|s| s.as_str()).unwrap_or("");

    if pw_value == password {
        let token = generate_session_token(&password);
        Response {
            status: 303,
            content_type: "text/html; charset=utf-8".to_string(),
            body: Body::Text("Redirecting...".to_string()),
            cache: "no-store",
            location: Some("/preferences".to_string()),
            set_cookie: Some(format!(
                "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400",
                ADMIN_SESSION_COOKIE, token
            )),
            rate_limit_remaining: None,
            www_authenticate: None,
        }
    } else {
        login_response(true)
    }
}
