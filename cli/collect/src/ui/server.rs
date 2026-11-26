//! The HTTP side: routes, the session check, and the headers every answer carries.
//!
//! Every request must name this server in `Host` by address, and carry the session cookie, which the first request
//! gets by bringing the printed token once. A POST must also carry the form token and, when the browser sends an
//! `Origin`, come from this server. Every answer forbids framing and loading anything from anywhere else.

use super::pages;
use super::state::{Shared, same};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

const COOKIE: &str = "qloudpulse_collect_session";

/// Everything the page loads comes from here. Inline scripts and styles are refused, so markup injected into a page
/// could not run.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; \
                       form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

const CSS: &str = include_str!("assets/app.css");
const JS: &str = include_str!("assets/app.js");
const MARK: &str = include_str!("assets/mark.svg");

type Form = axum::Form<HashMap<String, String>>;
type Params = Query<HashMap<String, String>>;

/// Binds `bind` and serves on a thread of its own. The address is checked before this returns, so a port in use is
/// an error here, not a log line later.
pub fn start(config_path: PathBuf, options: super::Options, run: Option<super::state::RunLog>) -> Result<Arc<Shared>> {
    let bind = options.bind;
    let listener = std::net::TcpListener::bind(bind).with_context(|| format!("the UI cannot listen on {bind}"))?;
    listener.set_nonblocking(true)?;
    let shared = Arc::new(Shared::new(
        config_path,
        listener.local_addr()?,
        options.allow_hosts,
        run,
    ));
    let app = router(shared.clone());
    std::thread::Builder::new().name("ui".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                crate::log::warn("ui stopped", &[("error", e.to_string())]);
                return;
            }
        };
        runtime.block_on(async move {
            let result = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => axum::serve(l, app).await.map_err(anyhow::Error::from),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = result {
                crate::log::warn("ui stopped", &[("error", format!("{e:#}"))]);
            }
        });
    })?;
    Ok(shared)
}

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/analyze") }))
        .route("/analyze", get(analyze))
        .route("/sample", post(sample))
        .route("/sample/forget", post(forget))
        .route("/sample/load", post(load_snapshot))
        .route("/rules", get(rules).post(rules_draft))
        .route("/rules/preview", post(rules_preview))
        .route("/rules/save", post(rules_save))
        .route("/rules/download", post(rules_download))
        .route("/bundles", get(bundles))
        .route("/bundles/{name}", get(bundle))
        .route("/runs", get(runs))
        .route("/status", get(status))
        .route(
            "/assets/app.css",
            get(|| async { asset("text/css; charset=utf-8", CSS) }),
        )
        .route(
            "/assets/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", JS) }),
        )
        .route("/assets/mark.svg", get(|| async { asset("image/svg+xml", MARK) }))
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                Html(pages::message("Not found", "No such page.")),
            )
        })
        .layer(middleware::from_fn_with_state(shared.clone(), guard))
        .with_state(shared)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}

async fn guard(State(s): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !s.host_ok(host) {
        return secure(
            (
                StatusCode::MISDIRECTED_REQUEST,
                "This UI answers only when addressed by IP address, as localhost, or by a name given with --ui-allow-host.",
            )
                .into_response(),
        );
    }
    let path = req.uri().path().to_string();
    if path.starts_with("/assets/") {
        return secure(next.run(req).await);
    }
    if let Some(token) = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
    }) {
        if !same(&token, &s.token) {
            return secure(unauthorized());
        }
        // The token leaves the address bar at once: it lives on as a cookie only this server reads.
        let mut resp = Redirect::to(&path).into_response();
        // Behind an Ingress that ends TLS, the browser only ever sends it back over HTTPS.
        let tls = req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"));
        let cookie = format!(
            "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/{}",
            s.token,
            if tls { "; Secure" } else { "" }
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().insert(header::SET_COOKIE, v);
        }
        return secure(resp);
    }
    if !cookie(req.headers()).is_some_and(|c| same(&c, &s.token)) {
        return secure(unauthorized());
    }
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let origin = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok());
        if let Some(o) = origin
            && !o
                .strip_prefix("http://")
                .or_else(|| o.strip_prefix("https://"))
                .is_some_and(|h| s.host_ok(h))
        {
            return secure(
                (
                    StatusCode::FORBIDDEN,
                    "A form from another site cannot change this client.",
                )
                    .into_response(),
            );
        }
    }
    secure(next.run(req).await)
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Html(pages::message(
            "Open the address the client printed",
            "This page needs the session token. qloudpulse-collect printed an address with it when the UI started: \
             open that one. The token changes every time the UI starts.",
        )),
    )
        .into_response()
}

fn secure(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    for (k, v) in [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        // Not `no-referrer`: under it a browser sends `Origin: null` with this UI's own forms, which the check above
        // must refuse. `same-origin` still keeps every address out of requests to anywhere else.
        (header::REFERRER_POLICY, "same-origin"),
        (header::CACHE_CONTROL, "no-store"),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    resp
}

/// Runs a page off the async thread: pages read files, gunzip bundles and run the redactor.
async fn page(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(pages::message("Something went wrong", &e.to_string())),
        )
            .into_response()
    })
}

fn csrf_ok(s: &Shared, form: &HashMap<String, String>) -> bool {
    form.get("csrf").is_some_and(|t| same(t, &s.csrf))
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Html(pages::message(
            "Refused",
            "The form is out of date: reload the page and try again.",
        )),
    )
        .into_response()
}

async fn analyze(State(s): State<Arc<Shared>>, Query(q): Params) -> Response {
    page(move || pages::analyze(&s, &q)).await
}

async fn sample(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    super::start_sample(s);
    Redirect::to("/analyze").into_response()
}

async fn forget(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    super::forget_sample(&s);
    Redirect::to("/analyze").into_response()
}

async fn load_snapshot(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    let label = form.get("snapshot").cloned().unwrap_or_default();
    page(move || match super::load_sample(&s, &label) {
        Ok(()) => Redirect::to("/analyze").into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Html(pages::message("Not loaded", &format!("{e:#}"))),
        )
            .into_response(),
    })
    .await
}

async fn rules(State(s): State<Arc<Shared>>, Query(q): Params) -> Response {
    page(move || pages::rules(&s, None, &q)).await
}

async fn rules_draft(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    page(move || pages::rules(&s, Some(&form), &HashMap::new())).await
}

async fn rules_preview(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    page(move || pages::rules_preview(&s, &form)).await
}

async fn rules_save(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    page(move || pages::rules_save(&s, &form)).await
}

async fn rules_download(State(s): State<Arc<Shared>>, axum::Form(form): Form) -> Response {
    if !csrf_ok(&s, &form) {
        return forbidden();
    }
    page(move || pages::rules_download(&s, &form)).await
}

async fn bundles(State(s): State<Arc<Shared>>) -> Response {
    page(move || pages::bundles(&s)).await
}

async fn bundle(State(s): State<Arc<Shared>>, UrlPath(name): UrlPath<String>, Query(q): Params) -> Response {
    page(move || pages::bundle(&s, &name, &q)).await
}

async fn runs(State(s): State<Arc<Shared>>) -> Response {
    page(move || pages::runs(&s)).await
}

/// The status strip alone: the page's script asks for it every few seconds.
async fn status(State(s): State<Arc<Shared>>) -> Response {
    pages::status(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn app() -> (Arc<Shared>, Router) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("c.toml");
        std::fs::write(&path, crate::config::starter()).unwrap();
        let s = Arc::new(Shared::new(path, "127.0.0.1:7781".parse().unwrap(), vec![], None));
        (s.clone(), router(s))
    }

    fn call(app: Router, req: axum::http::Request<Body>) -> Response {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(app.oneshot(req))
            .unwrap()
    }

    fn get(uri: &str, host: &str, cookie: Option<&str>) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::get(uri).header("host", host);
        if let Some(c) = cookie {
            b = b.header("cookie", format!("{COOKIE}={c}"));
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn needs_our_host_and_the_token() {
        let (s, app) = app();
        let r = call(app.clone(), get("/analyze", "evil.example:7781", Some(&s.token)));
        assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
        let r = call(app.clone(), get("/analyze", "127.0.0.1:7781", None));
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], CSP);
        let r = call(app.clone(), get("/analyze", "127.0.0.1:7781", Some("wrong")));
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = call(
            app.clone(),
            get(&format!("/runs?token={}", s.token), "127.0.0.1:7781", None),
        );
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/runs");
        let set = r.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
        let r = call(app.clone(), get("/runs", "localhost:7781", Some(&s.token)));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::X_FRAME_OPTIONS], "DENY");
        let r = call(app, get("/assets/app.css", "127.0.0.1:7781", None));
        assert_eq!(r.status(), StatusCode::OK, "assets need no session");
    }

    fn body(r: Response) -> String {
        let bytes = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(axum::body::to_bytes(r.into_body(), usize::MAX))
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Every `<a ...>` of a page that leaves this server.
    fn outside_links(page: &str) -> Vec<&str> {
        page.split("<a ")
            .skip(1)
            .map(|a| a.split('>').next().unwrap_or(""))
            .filter(|a| a.contains("href=\"http"))
            .collect()
    }

    #[cfg(feature = "upload")]
    #[test]
    fn links_to_qloudpulse_carry_nothing_of_this_page() {
        let (s, app) = app();
        for path in ["/analyze", "/rules", "/bundles", "/runs"] {
            let page = body(call(app.clone(), get(path, "127.0.0.1:7781", Some(&s.token))));
            let links = outside_links(&page);
            assert_eq!(links.len(), 2, "{path}: the app and its collectors page: {links:?}");
            assert!(links[0].contains(r#"href="https://app.qloudpulse.com""#), "{links:?}");
            assert!(
                links[1].contains(r#"href="https://app.qloudpulse.com/account/aws""#),
                "{links:?}"
            );
            for link in links {
                // The UI's first address carries the session token: no Referer may ever reach qloudpulse, and the
                // page it opens gets no handle on this one.
                assert!(link.contains(r#"rel="noreferrer noopener""#), "{link}");
                assert!(link.contains(r#"target="_blank""#), "{link}");
            }
            assert!(!page.contains(&s.token), "the token is in no page");
        }
        let r = call(app, get("/runs", "127.0.0.1:7781", Some(&s.token)));
        assert_eq!(r.headers()[header::REFERRER_POLICY], "same-origin");
    }

    #[cfg(feature = "upload")]
    #[test]
    fn the_app_comes_from_the_config() {
        let (s, app) = app();
        let starter = crate::config::starter();
        let own = starter.replacen("source_id", "app_url = \"https://qp.example.com\"\nsource_id", 1);
        std::fs::write(&s.config_path, own).unwrap();
        let page = body(call(app.clone(), get("/runs", "127.0.0.1:7781", Some(&s.token))));
        let links = outside_links(&page);
        assert!(links[0].contains(r#"href="https://qp.example.com""#), "{links:?}");
        assert!(
            links[1].contains(r#"href="https://qp.example.com/account/aws""#),
            "{links:?}"
        );
        let none = starter.replacen("source_id", "app_url = \"\"\nsource_id", 1);
        std::fs::write(&s.config_path, none).unwrap();
        let page = body(call(app, get("/runs", "127.0.0.1:7781", Some(&s.token))));
        assert!(outside_links(&page).is_empty(), "{page}");
    }

    #[cfg(not(feature = "upload"))]
    #[test]
    fn a_build_without_the_upload_links_nowhere() {
        let (s, app) = app();
        for path in ["/analyze", "/rules", "/bundles", "/runs"] {
            let page = body(call(app.clone(), get(path, "127.0.0.1:7781", Some(&s.token))));
            assert!(outside_links(&page).is_empty(), "{path}: {page}");
        }
    }

    fn at(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    fn under_run() -> (Arc<Shared>, Router) {
        use super::super::state::RunLog;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("c.toml");
        std::fs::write(&path, crate::config::starter()).unwrap();
        let run = RunLog {
            every_hours: 1,
            since: at("2026-11-26T17:27:00Z"),
            next: None,
            cycles: Default::default(),
        };
        let s = Arc::new(Shared::new(path, "127.0.0.1:7781".parse().unwrap(), vec![], Some(run)));
        (s.clone(), router(s))
    }

    #[test]
    fn the_status_strip_says_where_a_cycle_is_and_how_the_last_went() {
        use super::super::state::{Cycle, lock};
        use crate::run::Phase;
        let (s, app) = under_run();
        let strip = |path: &str| body(call(app.clone(), get(path, "127.0.0.1:7781", Some(&s.token))));
        let run = s.run.as_ref().unwrap();
        lock(run).cycles.push_front(Cycle {
            started: at("2026-11-26T17:27:00Z"),
            phase: Phase::Collecting {
                part: "inventory",
                account: 2,
                accounts: 2,
                region: 3,
                regions: 4,
                region_name: "eu-north-1".into(),
                records: 1284,
            },
            finished: None,
            bundle: None,
            already_collected: false,
            error: None,
            pending: vec![],
        });
        let running = strip("/status");
        assert!(
            running.starts_with(r#"<div id="status" class="status running""#),
            "{running}"
        );
        assert!(running.contains("<b>Cycle running</b>"), "{running}");
        assert!(
            running.contains("collecting inventory: account 2 of 2, region 3 of 4 (eu-north-1), 1284 records so far"),
            "{running}"
        );
        // 4 regions done of the first account, 2 of the second, of two passes over 2 x 4.
        assert!(running.contains(r#"<progress max="16" value="6">"#), "{running}");
        assert!(
            running.contains(r#"<time datetime="2026-11-26T17:27:00Z" data-local data-elapsed>17:27 UTC</time>"#),
            "{running}"
        );
        // The same strip is part of every page.
        for path in ["/analyze", "/rules", "/bundles", "/runs"] {
            assert!(strip(path).contains("account 2 of 2, region 3 of 4"), "{path}");
        }
        assert!(
            strip("/runs").contains("account 2 of 2, region 3 of 4"),
            "the cycle's row says it too"
        );

        {
            let mut log = lock(run);
            let c = log.cycles.front_mut().unwrap();
            c.finished = Some(at("2026-11-26T17:28:10Z"));
            c.bundle = Some("qloudpulse-main-20261126T1600Z.tgz".into());
            log.next = Some(at("2026-11-26T18:05:00Z"));
        }
        let waiting = strip("/status");
        assert!(
            waiting.starts_with(r#"<div id="status" class="status waiting""#),
            "{waiting}"
        );
        assert!(
            waiting.contains(
                r#"next run <time datetime="2026-11-26T18:05:00Z" data-local data-countdown>18:05 UTC</time>"#
            ),
            "{waiting}"
        );
        assert!(waiting.contains(r#"last cycle <b class="ok">ok</b>"#), "{waiting}");
        assert!(
            waiting.contains(r#"href="/bundles/qloudpulse-main-20261126T1600Z.tgz""#),
            "{waiting}"
        );
        assert_ne!(
            running.split("data-rev=").nth(1).and_then(|r| r.split('"').nth(1)),
            waiting.split("data-rev=").nth(1).and_then(|r| r.split('"').nth(1)),
            "the pages that list cycles load again when one ends"
        );

        lock(run).cycles.push_front(Cycle {
            started: at("2026-11-26T18:05:00Z"),
            phase: Phase::Delivering,
            finished: Some(at("2026-11-26T18:05:01Z")),
            bundle: None,
            already_collected: false,
            error: Some("no account could be read".into()),
            pending: vec![],
        });
        let failed = strip("/status");
        assert!(failed.contains(r#"last cycle <b class="bad">failed</b>"#), "{failed}");
        assert!(failed.contains("no account could be read"), "{failed}");
    }

    #[test]
    fn the_run_history_says_run_starts_with_a_cycle() {
        let (s, app) = under_run();
        let page = body(call(app, get("/runs", "127.0.0.1:7781", Some(&s.token))));
        assert!(page.contains("<code>run</code> starts with a cycle"), "{page}");
        assert!(!page.contains("starts at the next run time"));
    }

    #[test]
    fn the_status_strip_needs_the_session_and_says_ui_only_without_a_loop() {
        let (s, app) = app();
        let r = call(app.clone(), get("/status", "127.0.0.1:7781", None));
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let strip = body(call(app, get("/status", "127.0.0.1:7781", Some(&s.token))));
        assert!(strip.contains("<b>UI only</b>"), "{strip}");
    }

    #[test]
    fn through_port_forward_on_another_port() {
        let (s, app) = app();
        let r = call(app, get("/runs", "localhost:8080", Some(&s.token)));
        assert_eq!(r.status(), StatusCode::OK, "kubectl port-forward 8080:7781");
    }

    #[test]
    fn a_read_only_config_is_offered_as_a_download() {
        let (s, app) = app();
        s.config_read_only();
        let r = call(app.clone(), get("/rules", "127.0.0.1:7781", Some(&s.token)));
        let page = body(r);
        assert!(page.contains("The config is read-only here"), "{page}");
        assert!(!page.contains("Save to config"));
        let r = call(
            app,
            axum::http::Request::post("/rules/download")
                .header("host", "127.0.0.1:7781")
                .header("origin", "https://127.0.0.1:7781")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("{COOKIE}={}", s.token))
                .body(Body::from(format!("csrf={}&default=deny&k0=Team&a0=drop", s.csrf)))
                .unwrap(),
        );
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"c.toml\""
        );
        let file = body(r);
        let config: crate::config::Config = toml::from_str(&file).unwrap();
        assert_eq!(config.tag_action("Team"), crate::config::TagAction::Drop);
        assert!(file.contains("# Names this client."), "the file keeps its comments");
    }

    #[test]
    fn a_newer_release_is_said_on_every_page_once_found() {
        let (s, app) = app();
        let text = |r: Response| {
            let bytes = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(axum::body::to_bytes(r.into_body(), usize::MAX))
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        let before = text(call(app.clone(), get("/runs", "127.0.0.1:7781", Some(&s.token))));
        assert!(!before.contains("is out"), "{before}");
        *super::super::state::lock(&s.newer) =
            Some("qloudpulse-collect 9.9.9 is out (this is 0.1.0): `qloudpulse-collect update` installs it".into());
        let after = text(call(app, get("/runs", "127.0.0.1:7781", Some(&s.token))));
        assert!(
            after.contains(r#"<div class="note warn">qloudpulse-collect 9.9.9 is out (this is 0.1.0): "#),
            "{after}"
        );
    }

    #[test]
    fn a_post_needs_the_form_token_and_our_origin() {
        let (s, app) = app();
        let post = |origin: &str, body: String| {
            axum::http::Request::post("/sample/forget")
                .header("host", "127.0.0.1:7781")
                .header("origin", origin)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("{COOKIE}={}", s.token))
                .body(Body::from(body))
                .unwrap()
        };
        let r = call(app.clone(), post("http://evil.example", format!("csrf={}", s.csrf)));
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = call(app.clone(), post("null", format!("csrf={}", s.csrf)));
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "an opaque origin is not ours");
        let r = call(app.clone(), post("http://127.0.0.1:7781", "csrf=nope".into()));
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = call(app, post("http://127.0.0.1:7781", format!("csrf={}", s.csrf)));
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
    }
}
