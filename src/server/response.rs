//! HTTP response types and builders.

/// Response body types.
pub enum Body {
    Text(String),
    Bytes(Vec<u8>),
}

/// HTTP response.
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Body,
    pub cache: &'static str,
    pub location: Option<String>,
    pub set_cookie: Option<String>,
    pub rate_limit_remaining: Option<u32>,
    pub www_authenticate: Option<String>,
}

impl Response {
    pub fn text(status: u16, content_type: &str, body: String) -> Self {
        Response {
            status,
            content_type: content_type.to_string(),
            body: Body::Text(body),
            cache: "no-store",
            location: None,
            set_cookie: None,
            rate_limit_remaining: None,
            www_authenticate: None,
        }
    }

    pub fn rate_limited() -> Self {
        Response {
            status: 429,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: Body::Text("Too Many Requests".to_string()),
            cache: "no-store",
            location: None,
            set_cookie: None,
            rate_limit_remaining: Some(0),
            www_authenticate: None,
        }
    }

    pub fn with_rate_limit_remaining(mut self, remaining: u32) -> Self {
        self.rate_limit_remaining = Some(remaining);
        self
    }

    pub fn html(body: String) -> Self {
        Self::text(200, "text/html; charset=utf-8", body)
    }

    pub fn json(body: String) -> Self {
        Self::text(200, "application/json; charset=utf-8", body)
    }

    pub fn bytes(data: Vec<u8>, content_type: &str) -> Self {
        Response {
            status: 200,
            content_type: content_type.to_string(),
            body: Body::Bytes(data),
            cache: "no-store",
            location: None,
            set_cookie: None,
            rate_limit_remaining: None,
            www_authenticate: None,
        }
    }

    pub fn not_found() -> Self {
        Self::text(404, "text/plain", "not found".to_string())
    }

    pub fn redirect(url: String) -> Self {
        Response {
            status: 302,
            content_type: "text/html; charset=utf-8".to_string(),
            body: Body::Text(format!(
                "<!doctype html><meta charset=utf-8><title>Redirecting…</title>\
                 <p>Redirecting to <a href=\"{}\">{}</a></p>",
                escape(&url),
                escape(&url)
            )),
            cache: "no-store",
            location: Some(url),
            set_cookie: None,
            rate_limit_remaining: None,
            www_authenticate: None,
        }
    }
}

/// HTML-escape special characters.
pub fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Security headers applied to all HTTP responses.
pub const SECURITY_HEADERS: &str = "\
X-Content-Type-Options: nosniff\r\n\
X-Frame-Options: DENY\r\n\
X-XSS-Protection: 1; mode=block\r\n\
Referrer-Policy: strict-origin-when-cross-origin\r\n\
Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src * data: blob:; connect-src *; media-src * blob: data:\r\n";
