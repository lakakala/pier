use super::*;
use crate::auth::tests::{config, initialize, send};
use axum::{body::to_bytes, http::StatusCode};
use pier_protocol::{AgentReport, enrollment::InitRequest};

#[tokio::test]
async fn passive_enrollment_and_address_edits_use_admin_auth_and_survive_restart() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let state = Controller::open(cfg.clone()).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, login) = initialize(&router).await;
    // Passive enrollment and deployment must not depend on inbound controller readiness.
    state.listener_status.write().unwrap().listening = false;
    let request = InitRequest {
        request_id: pier_protocol::new_token(),
        name: "passive".into(),
        public_url: "https://pier.example.test".into(),
        connection_mode: ConnectionMode::ControllerToAgent,
        listen: Some("0.0.0.0:7444".parse().unwrap()),
        info: pier_protocol::AgentInfo {
            architecture: pier_pkg::Architecture::Amd64,
            hostname: "passive".into(),
            os_release: "test".into(),
        },
    };
    let mut body = serde_json::to_value(&request).unwrap();
    let csrf = login["csrf_token"].as_str().unwrap();
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/enrollments",
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(body.clone())
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    body["agent_endpoint"] = json!("agent.example.test:7444");
    body["agent_proxy"] = json!("socks5://user:secret@proxy.example.test:1080");
    let response = send(
        &router,
        "POST",
        "/v1/enrollments",
        &cookie,
        csrf,
        "https://pier.example.test",
        Some(body.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let grant: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    let pairing =
        pier_protocol::enrollment::Pairing::decode(grant["pairing"].as_str().unwrap(), &request)
            .unwrap();
    let key = secure::decode_key(&pairing.secret).unwrap();
    let credentials = state
        .issue_credentials(&pairing.grant_id, request.clone(), &key)
        .unwrap();
    assert_eq!(
        credentials.agent_id,
        state
            .issue_credentials(&pairing.grant_id, request.clone(), &key)
            .unwrap()
            .agent_id
    );
    assert!(credentials.controller_tcp.is_empty());
    assert!(
        !serde_json::to_string(&credentials)
            .unwrap()
            .contains("proxy")
    );
    assert!(!serde_json::to_string(&pairing).unwrap().contains("proxy"));
    // Correcting a pending proxy keeps the grant lifetime and delivered identity.
    for proxy in [
        Some(json!("socks5://next:password@proxy2.test:1080")),
        None,
        Some(Value::Null),
    ] {
        body.as_object_mut().unwrap().remove("agent_proxy");
        if let Some(proxy) = proxy {
            body["agent_proxy"] = proxy;
        }
        let response = send(
            &router,
            "POST",
            "/v1/enrollments",
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(body.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let updated: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(updated, grant);
        let retry = state
            .issue_credentials(&pairing.grant_id, request.clone(), &key)
            .unwrap();
        assert_eq!(retry.token, credentials.token);
        assert_eq!(retry.agent_id, credentials.agent_id);
        let record: AgentRecord = state
            .store
            .get("agents", &credentials.agent_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            record.proxy.is_some(),
            body.get("agent_proxy") != Some(&Value::Null)
        );
    }
    state
        .complete_passive_enrollment(&credentials.agent_id)
        .unwrap();
    assert!(state.enrollment_key(&pairing.grant_id).is_err());
    let path = format!("/v1/agents/{}/connection", credentials.agent_id);
    // A connected agent is required to have completed first contact, not to be online during edit.
    let mut record: AgentRecord = state
        .store
        .get("agents", &credentials.agent_id)
        .unwrap()
        .unwrap();
    record.last_seen = Some(pier_protocol::now());
    state.store.put("agents", &record.id, &record).unwrap();
    for (c, token, origin, status) in [
        (
            "",
            "",
            "https://pier.example.test",
            StatusCode::UNAUTHORIZED,
        ),
        (
            cookie.as_str(),
            "",
            "https://pier.example.test",
            StatusCode::FORBIDDEN,
        ),
        (
            cookie.as_str(),
            csrf,
            "https://wrong.test",
            StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(
            send(
                &router,
                "PUT",
                &path,
                c,
                token,
                origin,
                Some(json!({"endpoint":"next.example.test:7444"}))
            )
            .await
            .status(),
            status
        );
    }
    for endpoint in ["0.0.0.0:7444", "[::]:7444", "host:0", "host:7444/path"] {
        assert_eq!(
            send(
                &router,
                "PUT",
                &path,
                &cookie,
                csrf,
                "https://pier.example.test",
                Some(json!({"endpoint":endpoint}))
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(
            &router,
            "PUT",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(json!({"endpoint":"next.example.test:7444"}))
        )
        .await
        .status(),
        StatusCode::OK
    );
    for value in [
        "http://user:private-password@host:80",
        "socks5://user:%zz@host:1080",
    ] {
        let response = send(
            &router,
            "PUT",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(json!({"endpoint":"next.example.test:7444","proxy":value})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let text = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!text.contains(value) && !text.contains("private-password"));
    }
    let job = crate::Job {
        id: "busy".into(),
        agent_id: credentials.agent_id.clone(),
        blueprint: "test".into(),
        commit: "test".into(),
        state: "building".into(),
        error: None,
        created_at: pier_protocol::now(),
        plan: None,
        artifacts: Default::default(),
    };
    let upgrading = json!({"status":{"release":{"package":{"version":"1.0.0","revision":1},"format":"deb","architecture":"amd64",
        "system":"ubuntu24.04","sha256":pier_protocol::hash("test"),"size":1},"phase":"downloading","grant":null,"error":null,"updated_at":pier_protocol::now()},
        "expires_at":pier_protocol::now()+600});
    for (table, key, value) in [
        ("jobs", "busy", serde_json::to_value(job).unwrap()),
        ("agent_upgrades", credentials.agent_id.as_str(), upgrading),
    ] {
        state.store.put(table, key, &value).unwrap();
        let response = send(
            &router,
            "PUT",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(json!({"endpoint":"next.example.test:7444","proxy":"socks5://host:1080"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let unchanged: AgentRecord = state
            .store
            .get("agents", &credentials.agent_id)
            .unwrap()
            .unwrap();
        assert!(unchanged.proxy.is_none());
        state.store.delete(table, key).unwrap();
    }
    // Endpoint-only updates preserve the secret, null clears it, string replaces it.
    for (patch, configured) in [
        (
            json!({"endpoint":"next.example.test:7444","proxy":"socks5://user:private-password@proxy.test:1080"}),
            true,
        ),
        (json!({"endpoint":"next.example.test:7444"}), true),
        (
            json!({"endpoint":"next.example.test:7444","proxy":null}),
            false,
        ),
        (
            json!({"endpoint":"next.example.test:7444","proxy":"socks5://user:private-password@proxy.test:1080"}),
            true,
        ),
    ] {
        let response = send(
            &router,
            "PUT",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(patch),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let public: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(public["connection"]["proxy_configured"], configured);
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !text.contains("private-password")
                && !text.contains("proxy.test")
                && !text.contains("socks5://")
        );
    }
    for uri in [
        format!("/v1/agents/{}", credentials.agent_id),
        "/v1/agents".into(),
        format!("/v1/enrollments/{}", request.request_id),
    ] {
        let response = send(&router, "GET", &uri, &cookie, "", "", None).await;
        let text = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!text.contains("private-password") && !text.contains("proxy.test"));
    }
    drop(router);
    drop(state);
    let reopened = Controller::open(cfg).unwrap();
    let record: AgentRecord = reopened
        .store
        .get("agents", &credentials.agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record.connection.endpoint.as_deref(),
        Some("next.example.test:7444")
    );
    assert!(record.proxy.is_some());
    assert_eq!(record.token_hash, pier_protocol::hash(credentials.token));
}

#[tokio::test]
async fn controller_initiates_noise_control_and_cleans_up_on_disconnect() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    initialize(&crate::api::router(state.clone())).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let record = AgentRecord {
        proxy: None,
        id: "agent".into(),
        name: "passive".into(),
        token_hash: pier_protocol::hash("token"),
        connection: Connection {
            mode: ConnectionMode::ControllerToAgent,
            endpoint: Some(listener.local_addr().unwrap().to_string()),
        },
        info: None,
        last_seen: None,
        report: AgentReport::default(),
    };
    state.store.put("agents", &record.id, &record).unwrap();
    reconcile(&state, &Arc::new(Semaphore::new(2))).unwrap();
    let (mut socket, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let (prelude, raw) = secure::read_prelude(&mut socket).await.unwrap();
    assert_eq!(prelude.purpose, Purpose::Control);
    assert_eq!(prelude.id, "agent");
    let mut wire = pier_protocol::framed(
        secure::accept(socket, &raw, &secure::token_key("token"))
            .await
            .unwrap(),
    );
    pier_protocol::send(
        &mut wire,
        &Message::Hello {
            version: pier_protocol::VERSION,
            agent_id: "agent".into(),
            info: pier_protocol::AgentInfo {
                architecture: pier_pkg::Architecture::Amd64,
                hostname: "passive".into(),
                os_release: String::new(),
            },
            software: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        pier_protocol::receive(&mut wire).await.unwrap(),
        Message::Session { .. }
    ));
    assert!(matches!(
        pier_protocol::receive(&mut wire).await.unwrap(),
        Message::Welcome { .. }
    ));
    pier_protocol::send(
        &mut wire,
        &Message::Report {
            report: AgentReport::default(),
        },
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(2), async {
        while !state.sessions.lock().unwrap().contains_key("agent") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old_cancelled = state
        .dialer
        .workers
        .lock()
        .unwrap()
        .get("agent")
        .unwrap()
        .cancelled
        .clone();
    let mut record = record;
    record.proxy = Some(crate::proxy::Proxy::try_from("socks5://127.0.0.1:1".to_owned()).unwrap());
    state.store.put("agents", &record.id, &record).unwrap();
    // Changing only the proxy replaces the worker and closes its control session.
    reconcile(&state, &Arc::new(Semaphore::new(2))).unwrap();
    assert!(old_cancelled.is_cancelled());
    state.dialer.cancel("agent");
    timeout(Duration::from_secs(2), async {
        while state.sessions.lock().unwrap().contains_key("agent") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
