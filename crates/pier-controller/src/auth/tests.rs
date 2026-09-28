use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

pub(crate) fn free_address() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
pub(crate) fn config(root: &std::path::Path) -> crate::Config {
    crate::Config {
        state_dir: root.join("controller"),
        repository: crate::RepositoryConfig {
            url: "unused".into(),
            reference: "main".into(),
            sync_interval_seconds: 60,
        },
        http_listen: "127.0.0.1:0".parse().unwrap(),
        tcp_listen: free_address(),
        public_url: "https://pier.example.test".into(),
        agent_endpoint: "localhost:7443".into(),
        build_proxy: crate::BuildProxy::default(),
        max_concurrent_builds: 2,
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send(
    router: &Router,
    method: &str,
    path: &str,
    cookie: &str,
    csrf: &str,
    origin: &str,
    body: Option<Value>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if !cookie.is_empty() {
        request = request.header(header::COOKIE, cookie);
    }
    if !csrf.is_empty() {
        request = request.header("x-csrf-token", csrf);
    }
    if !origin.is_empty() {
        request = request.header(header::ORIGIN, origin);
    }
    router
        .clone()
        .oneshot(
            request
                .body(
                    body.map(|v| Body::from(v.to_string()))
                        .unwrap_or_else(Body::empty),
                )
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}
pub(crate) async fn initialize(router: &Router) -> (String, Value) {
    let response = send(
        router,
        "POST",
        "/v1/auth/init",
        "",
        "",
        "https://pier.example.test",
        Some(json!({"username":"admin","password":"test-password-123"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Path=/",
        "Max-Age=28800",
    ] {
        assert!(cookie.contains(attribute));
    }
    assert!(!cookie.contains("Domain="));
    (
        cookie.split(';').next().unwrap().to_string(),
        json_body(response).await,
    )
}
#[tokio::test]
async fn first_initialization_is_atomic_and_sessions_survive_restart() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let state = Controller::open(cfg.clone()).unwrap();
    let router = crate::api::router(state.clone());
    let body = json!({"username":"admin","password":"test-password-123"});
    let (a, b) = tokio::join!(
        send(
            &router,
            "POST",
            "/v1/auth/init",
            "",
            "",
            "https://pier.example.test",
            Some(body.clone())
        ),
        send(
            &router,
            "POST",
            "/v1/auth/init",
            "",
            "",
            "https://pier.example.test",
            Some(body)
        )
    );
    assert!(
        (a.status() == StatusCode::OK && b.status() == StatusCode::CONFLICT)
            || (b.status() == StatusCode::OK && a.status() == StatusCode::CONFLICT)
    );
    let success = if a.status() == StatusCode::OK { a } else { b };
    let cookie = success.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let admin = state.store.get::<Admin>("auth", "admin").unwrap().unwrap();
    assert!(admin.password_hash.starts_with("$argon2id$"));
    let sessions =
        serde_json::to_string(&state.store.list::<WebSession>("web_sessions").unwrap()).unwrap();
    assert!(!sessions.contains(cookie.split_once('=').unwrap().1));
    drop(router);
    drop(state);
    let state = Controller::open(cfg).unwrap();
    let router = crate::api::router(state);
    assert_eq!(
        send(&router, "GET", "/v1/auth/session", &cookie, "", "", None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/auth/init",
            "",
            "",
            "https://pier.example.test",
            Some(json!({"username":"another","password":"other-password-123"}))
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
}
#[tokio::test]
async fn origin_csrf_and_cookie_are_required_and_bearer_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let router = crate::api::router(state);
    for origin in ["", "https://evil.test"] {
        assert_eq!(
            send(
                &router,
                "POST",
                "/v1/auth/init",
                "",
                "",
                origin,
                Some(json!({"username":"admin","password":"test-password-123"}))
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let (cookie, session) = initialize(&router).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    for (c, token, origin, status) in [
        ("", csrf, "https://pier.example.test", 401),
        (&*cookie, "", "https://pier.example.test", 403),
        (&*cookie, csrf, "", 403),
        (&*cookie, csrf, "https://evil.test", 403),
        (&*cookie, csrf, "https://pier.example.test", 200),
    ] {
        assert_eq!(
            send(
                &router,
                "POST",
                "/v1/agents",
                c,
                token,
                origin,
                Some(json!({"name":"server"}))
            )
            .await
            .status()
            .as_u16(),
            status
        );
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/agents")
                .header(header::AUTHORIZATION, "Bearer test-password-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let duplicate = format!("{cookie}; {cookie}");
    assert_eq!(
        send(&router, "GET", "/v1/agents", &duplicate, "", "", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn login_rotates_logout_revokes_and_password_invalidates_all_sessions() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let router = crate::api::router(state);
    let (cookie, session) = initialize(&router).await;
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/auth/login",
            "",
            "",
            "https://pier.example.test",
            Some(json!({"username":"admin","password":"wrong"}))
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = send(
        &router,
        "POST",
        "/v1/auth/login",
        &cookie,
        "",
        "https://pier.example.test",
        Some(json!({"username":"admin","password":"test-password-123"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rotated = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let rotated_session = json_body(response).await;
    assert_ne!(cookie, rotated);
    assert_eq!(
        send(&router, "GET", "/v1/auth/session", &cookie, "", "", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = send(
        &router,
        "POST",
        "/v1/auth/login",
        "",
        "",
        "https://pier.example.test",
        Some(json!({"username":"admin","password":"test-password-123"})),
    )
    .await;
    let other = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let response = send(
        &router,
        "POST",
        "/v1/auth/password",
        &rotated,
        rotated_session["csrf_token"].as_str().unwrap(),
        "https://pier.example.test",
        Some(json!({"current_password":"test-password-123","new_password":"new-password-456"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    for value in [&rotated, &other] {
        assert_eq!(
            send(&router, "GET", "/v1/agents", value, "", "", None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let response = send(
        &router,
        "POST",
        "/v1/auth/login",
        "",
        "",
        "https://pier.example.test",
        Some(json!({"username":"admin","password":"new-password-456"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let current = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let current_session = json_body(response).await;
    assert_ne!(session["csrf_token"], current_session["csrf_token"]);
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/auth/logout",
            &current,
            current_session["csrf_token"].as_str().unwrap(),
            "https://pier.example.test",
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&router, "GET", "/v1/agents", &current, "", "", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn expired_sessions_are_rejected_and_auth_work_is_bounded() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, _) = initialize(&router).await;
    let mut session = state
        .store
        .list::<WebSession>("web_sessions")
        .unwrap()
        .pop()
        .unwrap();
    session.expires_at = 0;
    state
        .store
        .put("web_sessions", &session.digest, &session)
        .unwrap();
    assert_eq!(
        send(&router, "GET", "/v1/agents", &cookie, "", "", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    state.expire_web_sessions().unwrap();
    assert!(
        state
            .store
            .list::<WebSession>("web_sessions")
            .unwrap()
            .is_empty()
    );
    state.auth.attempts.lock().unwrap().1 = 60;
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/auth/login",
            "",
            "",
            "https://pier.example.test",
            Some(json!({"username":"admin","password":"test-password-123"}))
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
#[tokio::test]
async fn binding_patch_preserves_secrets_checks_defaults_and_detects_blueprint_changes() {
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let bp = crate::catalog::Blueprint {
        schema: 1,
        name: "web".into(),
        variables: BTreeMap::from([
            (
                "SECRET".into(),
                pier_pkg::VariableDefinition { default: None },
            ),
            (
                "PORT".into(),
                pier_pkg::VariableDefinition {
                    default: Some("80".into()),
                },
            ),
        ]),
        apps: vec![],
    };
    *state.catalog.write().unwrap() = Some(crate::catalog::Catalog {
        commit: "commit".into(),
        root: root.path().into(),
        apps: BTreeMap::new(),
        blueprints: BTreeMap::from([("web".into(), bp)]),
    });
    state
        .store
        .put(
            "bindings",
            "agent",
            &crate::Binding {
                blueprint: "web".into(),
                variables: BTreeMap::from([
                    ("SECRET".into(), "private-value".into()),
                    ("PORT".into(), "8080".into()),
                ]),
            },
        )
        .unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, session) = initialize(&router).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let response = send(
        &router,
        "PATCH",
        "/v1/agents/agent/binding",
        &cookie,
        csrf,
        "https://pier.example.test",
        Some(json!({"blueprint":"web","variables":{"PORT":null}})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !json_body(response)
            .await
            .to_string()
            .contains("private-value")
    );
    let binding = state
        .store
        .get::<crate::Binding>("bindings", "agent")
        .unwrap()
        .unwrap();
    assert_eq!(binding.variables["SECRET"], "private-value");
    assert!(!binding.variables.contains_key("PORT"));
    for (body, status) in [
        (json!({"blueprint":"other","variables":{}}), 409),
        (json!({"blueprint":"web","variables":{"SECRET":null}}), 400),
        (json!({"blueprint":"web","variables":{"UNKNOWN":"x"}}), 400),
    ] {
        assert_eq!(
            send(
                &router,
                "PATCH",
                "/v1/agents/agent/binding",
                &cookie,
                csrf,
                "https://pier.example.test",
                Some(body)
            )
            .await
            .status()
            .as_u16(),
            status
        );
    }
    assert_eq!(
        state
            .store
            .get::<crate::Binding>("bindings", "agent")
            .unwrap()
            .unwrap()
            .variables,
        binding.variables
    );
}
#[tokio::test]
async fn spa_routes_assets_and_security_headers_do_not_mask_api_errors() {
    let root = tempfile::tempdir().unwrap();
    let router = crate::api::router(Controller::open(config(root.path())).unwrap());
    for path in [
        "/",
        "/login",
        "/init",
        "/agent/init",
        "/agents/test-id",
        "/deployments/test-id",
    ] {
        let response = send(&router, "GET", path, "", "", "", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("nonce-"));
        let html = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!html.contains("__PIER_NONCE__"));
        assert!(html.contains("id=\"root\""));
    }
    for path in [
        "/v1/unknown",
        "/assets/missing.js",
        "/agent/init.js",
        "/src/main.tsx",
        "/unknown",
    ] {
        assert_eq!(
            send(&router, "GET", path, "", "", "", None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn browser_bootstrap_persists_repository_origin_and_ipv6_endpoint_without_fetching() {
    for origin in ["https://pier.example.test:8443", "https://[::1]:8443"] {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        cfg.public_url.clear();
        cfg.agent_endpoint.clear();
        cfg.repository.url.clear();
        let state = Controller::open(cfg.clone()).unwrap();
        let router = crate::api::router(state.clone());
        let body = json!({"username":"admin","password":"test-password-123", "repository":{"url":"https://example.invalid/repo.git"}});
        for (sent_origin, host) in [
            (origin, "evil.test"),
            ("http://pier.example.test", "pier.example.test"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/auth/init")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ORIGIN, sent_origin)
                        .header(header::HOST, host)
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/init")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, origin)
                    .header(header::HOST, origin.strip_prefix("https://").unwrap())
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.public_url().as_deref(), Some(origin));
        assert_eq!(
            state.agent_endpoint().unwrap(),
            if origin.contains("[::1]") {
                format!("[::1]:{}", cfg.tcp_listen.port())
            } else {
                format!("pier.example.test:{}", cfg.tcp_listen.port())
            }
        );
        assert!(!cfg.state_dir.join("snapshots").exists());
        assert!(state.repository_needs_sync());
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        drop(router);
        drop(state);
        let state = Controller::open(cfg.clone()).unwrap();
        assert_eq!(state.public_url().as_deref(), Some(origin));
        assert_eq!(state.repository_view()["repository"]["reference"], "main");
        let router = crate::api::router(state);
        assert_eq!(
            send(&router, "GET", "/v1/auth/session", &cookie, "", "", None)
                .await
                .status(),
            StatusCode::OK
        );
        assert!(!cfg.state_dir.join("snapshots").exists());
    }
}

#[tokio::test]
async fn repository_configuration_requires_auth_and_stale_catalog_blocks_deployments() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, session) = initialize(&router).await;
    let body = json!({"url":"https://example.invalid/new.git", "reference":"release"});
    assert_eq!(
        send(
            &router,
            "PUT",
            "/v1/repository",
            &cookie,
            "",
            "https://pier.example.test",
            Some(body.clone())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &router,
            "PUT",
            "/v1/repository",
            &cookie,
            session["csrf_token"].as_str().unwrap(),
            "https://pier.example.test",
            Some(body)
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(state.repository_needs_sync());
    assert!(!state.config.state_dir.join("snapshots").exists());
    let response = send(
        &router,
        "POST",
        "/v1/deployments",
        &cookie,
        session["csrf_token"].as_str().unwrap(),
        "https://pier.example.test",
        Some(json!({"agent_id":"old", "commit":"old"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        json_body(response).await["error"]
            .as_str()
            .unwrap()
            .contains("sync before deploying")
    );
}
