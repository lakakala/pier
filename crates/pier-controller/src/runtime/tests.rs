use super::*;
use crate::auth::tests::{config, free_address, initialize, send};
use axum::{body::to_bytes, http::StatusCode, response::Response};

async fn body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}
fn patch(value: Value) -> RuntimePatch {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn first_setup_binds_only_after_validation_and_port_conflict_is_retryable() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.build_proxy.http_proxy = Some("http://user:private-secret@proxy.example:7890".into());
    let state = Controller::open(cfg.clone()).unwrap();
    assert!(state.active_runtime().is_err());
    assert!(!state.agent_ready());
    let occupied = TcpListener::bind(cfg.tcp_listen).unwrap(); // Open must not bind yet.
    let router = crate::api::router(state.clone());
    let status = body(send(&router, "GET", "/v1/auth/status", "", "", "", None).await).await;
    assert_eq!(status["setup_defaults"]["proxy_configured"], true);
    assert!(!status.to_string().contains("private-secret"));
    let input = json!({"username":"admin","password":"test-password-123", "settings":{"max_concurrent_builds":3}});
    let failed = send(
        &router,
        "POST",
        "/v1/auth/init",
        "",
        "",
        &cfg.public_url,
        Some(input.clone()),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::CONFLICT);
    assert!(state.store.get::<Value>("auth", "admin").unwrap().is_none());
    assert!(
        state
            .store
            .list::<Value>("web_sessions")
            .unwrap()
            .is_empty()
    );
    assert!(state.active_runtime().is_err());
    drop(occupied);
    let success = send(
        &router,
        "POST",
        "/v1/auth/init",
        "",
        "",
        &cfg.public_url,
        Some(input),
    )
    .await;
    assert_eq!(success.status(), StatusCode::OK);
    assert!(state.agent_ready());
    assert!(TcpListener::bind(cfg.tcp_listen).is_err());
    assert_eq!(
        state
            .active_runtime()
            .unwrap()
            .build_slots
            .available_permits(),
        3
    );
    assert_eq!(
        state.active_runtime().unwrap().settings.build_proxy,
        cfg.build_proxy
    );
    assert!(!state.runtime_view()["restart_required"].as_bool().unwrap());
    assert!(!cfg.state_dir.join("snapshots").exists());
    assert!(body(send(&router, "GET", "/v1/auth/status", "", "", "", None).await).await["setup_defaults"].is_null());
}

#[tokio::test]
async fn edits_require_auth_are_atomic_and_apply_only_on_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    let state = Controller::open(cfg.clone()).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, session) = initialize(&router).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let initial = state.runtime_view();
    for invalid in [
        json!({"tcp_listen":"127.0.0.1:0"}),
        json!({"max_concurrent_builds":0}),
        json!({"max_concurrent_builds":65}),
        json!({"public_url":"ftp://bad.test"}),
        json!({"agent_endpoint":"missing-port"}),
        json!({"build_proxy":{"http_proxy":"http://proxy.test/path"}}),
    ] {
        let response = send(
            &router,
            "PUT",
            "/v1/settings",
            &cookie,
            csrf,
            &cfg.public_url,
            Some(invalid),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.runtime_view(), initial);
    }
    let next_port = free_address();
    let change = json!({"tcp_listen":next_port, "public_url":"https://new.example.test", "agent_endpoint":"new.example.test:7555", "max_concurrent_builds":4, "build_proxy":{"https_proxy":"http://proxy.example:7890"}});
    assert_eq!(
        send(&router, "GET", "/v1/settings", "", "", "", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(
            &router,
            "PUT",
            "/v1/settings",
            &cookie,
            "",
            &cfg.public_url,
            Some(change.clone())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let response = send(
        &router,
        "PUT",
        "/v1/settings",
        &cookie,
        csrf,
        &cfg.public_url,
        Some(change),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let saved = body(response).await;
    assert_eq!(saved["active"], initial["active"]);
    assert_eq!(saved["restart_required"], true);
    assert_eq!(state.public_url().unwrap(), cfg.public_url);
    assert_eq!(
        state
            .active_runtime()
            .unwrap()
            .build_slots
            .available_permits(),
        2
    );
    assert!(TcpListener::bind(cfg.tcp_listen).is_err());
    assert!(TcpListener::bind(next_port).is_ok());
    drop(router);
    drop(state);
    cfg.public_url = "invalid-legacy-override".into();
    cfg.max_concurrent_builds = 0;
    let restarted = Controller::open(cfg.clone()).unwrap();
    restarted.start_saved_listener();
    assert!(restarted.agent_ready());
    assert_eq!(restarted.runtime_view()["active"], saved["saved"]);
    assert_eq!(restarted.runtime_view()["restart_required"], false);
    assert!(TcpListener::bind(cfg.tcp_listen).is_ok());
    assert!(TcpListener::bind(next_port).is_err());
    assert_eq!(
        restarted
            .active_runtime()
            .unwrap()
            .build_slots
            .available_permits(),
        4
    );
    assert_eq!(
        send(
            &crate::api::router(restarted),
            "GET",
            "/v1/auth/session",
            &cookie,
            "",
            "",
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn occupied_port_after_restart_keeps_settings_accessible_for_repair() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let state = Controller::open(cfg.clone()).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, session) = initialize(&router).await;
    let blocked = TcpListener::bind("127.0.0.1:0").unwrap();
    state
        .save_runtime(patch(json!({"tcp_listen":blocked.local_addr().unwrap()})))
        .unwrap();
    drop(router);
    drop(state);
    let state = Controller::open(cfg.clone()).unwrap();
    state.start_saved_listener();
    assert!(!state.agent_ready());
    let router = crate::api::router(state.clone());
    let response = send(&router, "GET", "/v1/settings", &cookie, "", "", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body(response).await["agent_listener"]["error"].is_string());
    let enroll = json!({"request_id":"a".repeat(64), "name":"test", "public_url":cfg.public_url, "info":{"architecture":"amd64","hostname":"test","os_release":"fixture"}});
    let csrf = session["csrf_token"].as_str().unwrap();
    let response = send(
        &router,
        "POST",
        "/v1/enrollments",
        &cookie,
        csrf,
        &cfg.public_url,
        Some(enroll),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body(response).await["error"]
            .as_str()
            .unwrap()
            .contains("listener")
    );
    let fixed = free_address();
    let response = send(
        &router,
        "PUT",
        "/v1/settings",
        &cookie,
        csrf,
        &cfg.public_url,
        Some(json!({"tcp_listen":fixed})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!state.agent_ready());
    drop(router);
    drop(state);
    let state = Controller::open(cfg).unwrap();
    state.start_saved_listener();
    assert!(state.agent_ready());
    assert!(TcpListener::bind(fixed).is_err());
}

#[tokio::test]
async fn previous_database_and_yaml_overrides_migrate_once_without_losing_web_repository() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    let state = Controller::open(cfg.clone()).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, _) = initialize(&router).await;
    state.store.put("settings", "controller", &json!({
        "repository":{"url":"saved-web-repo","reference":"release"}, "catalog_repository":null,
        "public_url":cfg.public_url,"agent_endpoint":cfg.agent_endpoint, "sync_error":null
    })).unwrap();
    drop(router);
    drop(state);
    cfg.public_url = "https://yaml-override.example.test".into();
    cfg.agent_endpoint = "yaml-override.example.test:7555".into();
    cfg.max_concurrent_builds = 5;
    let state = Controller::open(cfg.clone()).unwrap();
    assert_eq!(state.public_url().unwrap(), cfg.public_url);
    assert_eq!(
        state.repository_view()["repository"]["url"],
        "saved-web-repo"
    );
    assert_eq!(
        state
            .active_runtime()
            .unwrap()
            .build_slots
            .available_permits(),
        5
    );
    assert_eq!(
        send(
            &crate::api::router(state.clone()),
            "GET",
            "/v1/auth/session",
            &cookie,
            "",
            "",
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    state
        .save_runtime(patch(json!({"max_concurrent_builds":6})))
        .unwrap();
    drop(state);
    cfg.max_concurrent_builds = 7;
    cfg.public_url = "https://ignored.example.test".into();
    let state = Controller::open(cfg).unwrap();
    assert_eq!(
        state
            .active_runtime()
            .unwrap()
            .build_slots
            .available_permits(),
        6
    );
    assert_eq!(
        state.public_url().unwrap(),
        "https://yaml-override.example.test"
    );
}
