//! Optional single-user login for self-hosted (e.g. containerized) deployments.
//!
//! Enabled by setting both `VK_AUTH_USERNAME` and `VK_AUTH_PASSWORD`. When
//! enabled, every route except the login endpoints and `/api/health` requires
//! a session cookie. Loopback requests that did not pass through a proxy are
//! trusted so that in-container tools (e.g. the MCP server) keep working.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Extension, Form, Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tower_http::validate_request::ValidateRequestHeaderLayer;

use crate::{error::ApiError, middleware::origin::validate_origin};

pub const USERNAME_ENV: &str = "VK_AUTH_USERNAME";
pub const PASSWORD_ENV: &str = "VK_AUTH_PASSWORD";

const SESSION_COOKIE: &str = "vk_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_FAILED_ATTEMPTS: usize = 10;
const FAILED_ATTEMPT_WINDOW: Duration = Duration::from_secs(15 * 60);

const LOGIN_PATH: &str = "/login";
const LOGOUT_PATH: &str = "/logout";
const HEALTH_PATH: &str = "/api/health";

const LOGIN_PAGE: &str = include_str!("local_auth_login.html");

#[derive(Clone)]
pub struct LocalAuth {
    inner: Arc<Inner>,
}

struct Inner {
    username_digest: [u8; 32],
    password_digest: [u8; 32],
    sessions: Mutex<HashMap<String, Instant>>,
    failed_attempts: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{USERNAME_ENV} and {PASSWORD_ENV} must be set together")]
pub struct LocalAuthConfigError;

impl LocalAuth {
    /// Reads credentials from the environment. Returns `Ok(None)` when login
    /// is disabled, and an error when only one of the variables is set so a
    /// misconfigured deployment does not start unprotected.
    pub fn from_env() -> Result<Option<Self>, LocalAuthConfigError> {
        Self::from_values(
            std::env::var(USERNAME_ENV).ok(),
            std::env::var(PASSWORD_ENV).ok(),
        )
    }

    fn from_values(
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Option<Self>, LocalAuthConfigError> {
        let username = username.filter(|value| !value.is_empty());
        let password = password.filter(|value| !value.is_empty());
        match (username, password) {
            (None, None) => Ok(None),
            (Some(username), Some(password)) => Ok(Some(Self::new(&username, &password))),
            _ => Err(LocalAuthConfigError),
        }
    }

    fn new(username: &str, password: &str) -> Self {
        Self {
            inner: Arc::new(Inner {
                username_digest: Sha256::digest(username).into(),
                password_digest: Sha256::digest(password).into(),
                sessions: Mutex::new(HashMap::new()),
                failed_attempts: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn credentials_match(&self, username: &str, password: &str) -> bool {
        let username_ok = digests_equal(
            &Sha256::digest(username).into(),
            &self.inner.username_digest,
        );
        let password_ok = digests_equal(
            &Sha256::digest(password).into(),
            &self.inner.password_digest,
        );
        username_ok & password_ok
    }

    fn create_session(&self) -> String {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);

        let now = Instant::now();
        let mut sessions = self.inner.sessions.lock().unwrap();
        sessions.retain(|_, expires_at| *expires_at > now);
        sessions.insert(token.clone(), now + SESSION_TTL);
        token
    }

    fn is_valid_session(&self, token: &str) -> bool {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(token)
            .is_some_and(|expires_at| *expires_at > Instant::now())
    }

    fn remove_session(&self, token: &str) {
        self.inner.sessions.lock().unwrap().remove(token);
    }

    fn is_rate_limited(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = self.inner.failed_attempts.lock().unwrap();
        let Some(times) = attempts.get_mut(&ip) else {
            return false;
        };
        times.retain(|at| now.duration_since(*at) < FAILED_ATTEMPT_WINDOW);
        times.len() >= MAX_FAILED_ATTEMPTS
    }

    fn record_failed_attempt(&self, ip: IpAddr) {
        self.inner
            .failed_attempts
            .lock()
            .unwrap()
            .entry(ip)
            .or_default()
            .push(Instant::now());
    }

    fn clear_failed_attempts(&self, ip: IpAddr) {
        self.inner.failed_attempts.lock().unwrap().remove(&ip);
    }

    fn is_authenticated(&self, headers: &HeaderMap) -> bool {
        session_cookie(headers).is_some_and(|token| self.is_valid_session(token))
    }
}

/// Adds the login/logout routes and puts every other route behind a session.
pub fn protect(router: Router, auth: LocalAuth) -> Router {
    let login_routes = Router::new()
        .route(LOGIN_PATH, get(login_page).post(login))
        .route(LOGOUT_PATH, get(logout).post(logout))
        .layer(ValidateRequestHeaderLayer::custom(validate_origin))
        .with_state(auth.clone());

    router
        .merge(login_routes)
        .layer(axum::middleware::from_fn_with_state(auth, require_session))
}

async fn require_session(State(auth): State<LocalAuth>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if path == LOGIN_PATH
        || path == LOGOUT_PATH
        || path == HEALTH_PATH
        || is_trusted_local_request(&request)
        || auth.is_authenticated(request.headers())
    {
        return next.run(request).await;
    }

    if path.starts_with("/api/") {
        ApiError::Unauthorized.into_response()
    } else {
        Redirect::to(LOGIN_PATH).into_response()
    }
}

async fn login_page(State(auth): State<LocalAuth>, headers: HeaderMap) -> Response {
    if auth.is_authenticated(&headers) {
        return Redirect::to("/").into_response();
    }
    render_login(StatusCode::OK, None)
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login(
    State(auth): State<LocalAuth>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let ip = connect_info
        .map(|Extension(ConnectInfo(addr))| addr.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));

    if auth.is_rate_limited(ip) {
        return render_login(
            StatusCode::TOO_MANY_REQUESTS,
            Some("Too many failed attempts. Try again later."),
        );
    }

    if !auth.credentials_match(&form.username, &form.password) {
        auth.record_failed_attempt(ip);
        tracing::warn!(%ip, "Failed login attempt");
        return render_login(
            StatusCode::UNAUTHORIZED,
            Some("Invalid username or password."),
        );
    }

    auth.clear_failed_attempts(ip);
    let token = auth.create_session();
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_TTL.as_secs(),
        if is_https(&headers) { "; Secure" } else { "" }
    );
    with_cookie(Redirect::to("/").into_response(), &cookie)
}

async fn logout(State(auth): State<LocalAuth>, headers: HeaderMap) -> Response {
    if let Some(token) = session_cookie(&headers) {
        auth.remove_session(token);
    }
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    with_cookie(Redirect::to(LOGIN_PATH).into_response(), &cookie)
}

fn render_login(status: StatusCode, error: Option<&str>) -> Response {
    let error_html = error
        .map(|message| format!(r#"<p class="error" role="alert">{message}</p>"#))
        .unwrap_or_default();
    (status, Html(LOGIN_PAGE.replace("{{error}}", &error_html))).into_response()
}

fn with_cookie(mut response: Response, cookie: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

/// Requests from the loopback interface come from processes inside the same
/// host or container (MCP server, agents). Requests carrying forwarding headers
/// came through a reverse proxy and are treated as remote.
fn is_trusted_local_request(request: &Request) -> bool {
    let is_loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(addr)| addr.ip().to_canonical().is_loopback());
    is_loopback && !has_forwarding_headers(request.headers())
}

fn has_forwarding_headers(headers: &HeaderMap) -> bool {
    [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-real-ip",
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request as HttpRequest};
    use tower::ServiceExt;

    use super::*;

    fn test_auth() -> LocalAuth {
        LocalAuth::from_values(Some("dev".into()), Some("secret".into()))
            .unwrap()
            .unwrap()
    }

    fn test_router(auth: LocalAuth) -> Router {
        let router = Router::new()
            .route("/", get(|| async { "app" }))
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/info", get(|| async { "info" }));
        protect(router, auth)
    }

    fn request(path: &str, peer: Option<&str>) -> HttpRequest<Body> {
        let mut request = HttpRequest::builder()
            .uri(path)
            .body(Body::empty())
            .unwrap();
        if let Some(peer) = peer {
            let addr: SocketAddr = peer.parse().unwrap();
            request.extensions_mut().insert(ConnectInfo(addr));
        }
        request
    }

    fn login_request(username: &str, password: &str) -> HttpRequest<Body> {
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri(LOGIN_PATH)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "username={username}&password={password}"
            )))
            .unwrap();
        let addr: SocketAddr = "172.17.0.1:5000".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(addr));
        request
    }

    #[test]
    fn config_requires_both_values() {
        assert!(LocalAuth::from_values(None, None).unwrap().is_none());
        assert!(
            LocalAuth::from_values(Some(String::new()), Some(String::new()))
                .unwrap()
                .is_none()
        );
        assert!(LocalAuth::from_values(Some("dev".into()), None).is_err());
        assert!(LocalAuth::from_values(None, Some("secret".into())).is_err());
    }

    #[test]
    fn credentials_must_both_match() {
        let auth = test_auth();
        assert!(auth.credentials_match("dev", "secret"));
        assert!(!auth.credentials_match("dev", "wrong"));
        assert!(!auth.credentials_match("other", "secret"));
    }

    #[test]
    fn session_cookie_is_parsed_among_other_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; vk_session=abc; other=1"),
        );
        assert_eq!(session_cookie(&headers), Some("abc"));
    }

    #[test]
    fn rate_limit_kicks_in_after_max_failures() {
        let auth = test_auth();
        let ip = IpAddr::from([10, 0, 0, 1]);
        for _ in 0..MAX_FAILED_ATTEMPTS {
            assert!(!auth.is_rate_limited(ip));
            auth.record_failed_attempt(ip);
        }
        assert!(auth.is_rate_limited(ip));
        assert!(!auth.is_rate_limited(IpAddr::from([10, 0, 0, 2])));
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_rejected() {
        let router = test_router(test_auth());

        let page = router
            .clone()
            .oneshot(request("/", Some("172.17.0.1:5000")))
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::SEE_OTHER);
        assert_eq!(page.headers()[header::LOCATION], LOGIN_PATH);

        let api = router
            .clone()
            .oneshot(request("/api/info", Some("172.17.0.1:5000")))
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);

        let health = router
            .oneshot(request("/api/health", Some("172.17.0.1:5000")))
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn loopback_is_trusted_unless_proxied() {
        let router = test_router(test_auth());

        let local = router
            .clone()
            .oneshot(request("/api/info", Some("127.0.0.1:5000")))
            .await
            .unwrap();
        assert_eq!(local.status(), StatusCode::OK);

        let mut proxied = request("/api/info", Some("127.0.0.1:5000"));
        proxied
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        let proxied = router.oneshot(proxied).await.unwrap();
        assert_eq!(proxied.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_grants_session_and_logout_revokes_it() {
        let router = test_router(test_auth());

        let failed = router
            .clone()
            .oneshot(login_request("dev", "wrong"))
            .await
            .unwrap();
        assert_eq!(failed.status(), StatusCode::UNAUTHORIZED);
        assert!(failed.headers().get(header::SET_COOKIE).is_none());

        let response = router
            .clone()
            .oneshot(login_request("dev", "secret"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        let cookie = set_cookie.split(';').next().unwrap().to_string();

        let mut authed = request("/api/info", Some("172.17.0.1:5000"));
        authed
            .headers_mut()
            .insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        let authed = router.clone().oneshot(authed).await.unwrap();
        assert_eq!(authed.status(), StatusCode::OK);

        let mut logout = request(LOGOUT_PATH, Some("172.17.0.1:5000"));
        logout
            .headers_mut()
            .insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        router.clone().oneshot(logout).await.unwrap();

        let mut after_logout = request("/api/info", Some("172.17.0.1:5000"));
        after_logout
            .headers_mut()
            .insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        let after_logout = router.oneshot(after_logout).await.unwrap();
        assert_eq!(after_logout.status(), StatusCode::UNAUTHORIZED);
    }
}
