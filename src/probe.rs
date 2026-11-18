use gtk::glib;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::OnceLock, time::Duration};
use ureq::{
    Agent,
    config::RedirectAuthHeaders,
    http::{HeaderMap, StatusCode, header},
    tls::{RootCerts, TlsConfig},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HttpOptions {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub referer: String,
    #[serde(skip)]
    pub login: Option<Login>,
}

impl HttpOptions {
    pub fn new(
        user_agent: &str,
        referer: &str,
        username: &str,
        password: &str,
    ) -> Result<Self, &'static str> {
        let user_agent = user_agent.trim();
        let referer = referer.trim();
        if user_agent.chars().any(char::is_control) {
            return Err("User-Agent не може містити керівних символів.");
        }
        if !referer.is_empty()
            && (crate::engine::validate_url(referer).is_err()
                || crate::engine::url_has_login(referer))
        {
            return Err(
                "Вкажіть дійсну HTTP або HTTPS-адресу сторінки завантаження без даних для входу.",
            );
        }
        Ok(Self {
            user_agent: user_agent.into(),
            referer: referer.into(),
            login: Login::optional(username, password)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.user_agent.is_empty() && self.referer.is_empty()
    }
}

#[derive(Clone)]
pub struct Login {
    username: String,
    password: String,
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login").finish_non_exhaustive()
    }
}

impl Login {
    pub fn optional(username: &str, password: &str) -> Result<Option<Self>, &'static str> {
        if username.is_empty() && password.is_empty() {
            return Ok(None);
        }
        if username.is_empty() {
            return Err("Для HTTP Basic авторизації введіть ім’я користувача.");
        }
        if username.contains(':')
            || username
                .chars()
                .chain(password.chars())
                .any(char::is_control)
        {
            return Err(
                "Ім’я користувача не може містити двокрапку, а дані для входу — керівні символи.",
            );
        }
        Ok(Some(Self {
            username: username.into(),
            password: password.into(),
        }))
    }

    pub fn authenticate_url(&self, url: &str) -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        let user = glib::Uri::escape_string(&self.username, None, false);
        let password = glib::Uri::escape_string(&self.password, None, false);
        Some(format!("{scheme}://{user}:{password}@{rest}"))
    }

    fn header_value(&self) -> String {
        let credentials = format!("{}:{}", self.username, self.password);
        format!("Basic {}", glib::base64_encode(credentials.as_bytes()))
    }
}

#[derive(Clone, Copy, Default)]
pub enum RangeSupport {
    Available(u64),
    Unavailable,
    #[default]
    Unknown,
}

#[derive(Default)]
pub struct LinkCheck {
    pub range_support: RangeSupport,
    pub likely_indirect: bool,
}

pub fn check(url: &str, http: &HttpOptions) -> LinkCheck {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(5)
            .redirect_auth_headers(RedirectAuthHeaders::Never)
            .timeout_global(Some(Duration::from_secs(7)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into()
    });
    let mut request = agent
        .get(url)
        .header(header::RANGE, "bytes=0-0")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONNECTION, "close");
    if !http.user_agent.is_empty() {
        request = request.header(header::USER_AGENT, &http.user_agent);
    }
    if !http.referer.is_empty() {
        request = request.header(header::REFERER, &http.referer);
    }
    if let Some(login) = &http.login {
        request = request.header(header::AUTHORIZATION, login.header_value());
    }
    let Ok(response) = request.call() else {
        return LinkCheck::default();
    };

    // A server may ignore Range and start a full response. Read only headers.
    let status = response.status();
    let headers = response.headers();
    let main_value = |name| {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .split(';')
            .next()
            .map(str::trim)
    };
    let html = main_value(header::CONTENT_TYPE).is_some_and(|mime| {
        mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
    });
    let attachment = main_value(header::CONTENT_DISPOSITION)
        .is_some_and(|value| value.eq_ignore_ascii_case("attachment"));
    LinkCheck {
        range_support: range_support(status, headers),
        likely_indirect: status.is_success() && html && !attachment,
    }
}

fn range_support(status: StatusCode, headers: &HeaderMap) -> RangeSupport {
    match status {
        StatusCode::OK => RangeSupport::Unavailable,
        StatusCode::PARTIAL_CONTENT => {
            if headers.get(header::CONTENT_ENCODING).is_some_and(|value| {
                !value
                    .to_str()
                    .is_ok_and(|text| text.eq_ignore_ascii_case("identity"))
            }) {
                return RangeSupport::Unknown;
            }
            headers
                .get(header::CONTENT_RANGE)
                .and_then(|header| header.to_str().ok())
                .and_then(|text| {
                    let (unit, range) = text.split_once(' ')?;
                    if !unit.eq_ignore_ascii_case("bytes") {
                        return None;
                    }
                    let length = range.strip_prefix("0-0/")?.parse::<u64>().ok()?;
                    (length > 0).then_some(length)
                })
                .map_or(RangeSupport::Unknown, RangeSupport::Available)
        }
        _ => RangeSupport::Unknown,
    }
}
