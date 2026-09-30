use super::*;
use crate::{
    Session,
    auth::tests::{config, initialize, send},
};
use axum::{Router, body::to_bytes};
use pier_protocol::{AgentReport, AppStatus};

async fn setup() -> (tempfile::TempDir, Arc<Controller>, Router, String, String) {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let router = crate::api::router(state.clone());
    let (cookie, session) = initialize(&router).await;
    state
        .store
        .put(
            "agents",
            "agent",
            &AgentRecord {
                id: "agent".into(),
                name: "demo".into(),
                token_hash: pier_protocol::hash("token"),
                info: None,
                last_seen: None,
                report: AgentReport {
                    apps: vec![AppStatus {
                        instance: "instance".into(),
                        id: "demo".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let (sender, _) = tokio::sync::mpsc::channel(8);
    state.sessions.lock().unwrap().insert(
        "agent".into(),
        Session {
            id: "control".into(),
            sender,
            terminal: true,
        },
    );
    (
        root,
        state,
        router,
        cookie,
        session["csrf_token"].as_str().unwrap().into(),
    )
}

#[tokio::test]
async fn create_requires_authentication_origin_csrf_and_current_capability() {
    let (_root, state, router, cookie, csrf) = setup().await;
    let path = "/v1/agents/agent/apps/instance/terminals";
    for (session, token, origin, expected) in [
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
            csrf.as_str(),
            "https://other.test",
            StatusCode::FORBIDDEN,
        ),
        (
            cookie.as_str(),
            csrf.as_str(),
            "https://pier.example.test",
            StatusCode::OK,
        ),
    ] {
        assert_eq!(
            send(
                &router,
                "POST",
                path,
                session,
                token,
                origin,
                Some(json!({"cols":80,"rows":24}))
            )
            .await
            .status(),
            expected
        );
    }
    state
        .sessions
        .lock()
        .unwrap()
        .get_mut("agent")
        .unwrap()
        .terminal = false;
    assert_eq!(
        send(
            &router,
            "POST",
            path,
            &cookie,
            &csrf,
            "https://pier.example.test",
            Some(json!({"cols":80,"rows":24}))
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    state.sessions.lock().unwrap().clear();
    assert_eq!(
        send(
            &router,
            "POST",
            path,
            &cookie,
            &csrf,
            "https://pier.example.test",
            Some(json!({"cols":80,"rows":24}))
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn tickets_are_bounded_expire_and_logout_revokes_them() {
    let (_root, state, router, cookie, csrf) = setup().await;
    let path = "/v1/agents/agent/apps/instance/terminals";
    for body in [json!({"cols":0,"rows":24}), json!({"cols":501,"rows":24})] {
        assert_eq!(
            send(
                &router,
                "POST",
                path,
                &cookie,
                &csrf,
                "https://pier.example.test",
                Some(body)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/agents/agent/apps/missing/terminals",
            &cookie,
            &csrf,
            "https://pier.example.test",
            Some(json!({"cols":80,"rows":24}))
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let mut ticket_id = String::new();
    for _ in 0..8 {
        let response = send(
            &router,
            "POST",
            path,
            &cookie,
            &csrf,
            "https://pier.example.test",
            Some(json!({"cols":80,"rows":24})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert!(body["expires_at"].as_u64().unwrap() <= pier_protocol::now() + 30);
        ticket_id = body["id"].as_str().unwrap().into();
    }
    assert_eq!(
        send(
            &router,
            "POST",
            path,
            &cookie,
            &csrf,
            "https://pier.example.test",
            Some(json!({"cols":80,"rows":24}))
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    state
        .terminals
        .0
        .lock()
        .unwrap()
        .get_mut(&ticket_id)
        .unwrap()
        .expires = 0;
    assert_eq!(
        send(
            &router,
            "POST",
            path,
            &cookie,
            &csrf,
            "https://pier.example.test",
            Some(json!({"cols":80,"rows":24}))
        )
        .await
        .status(),
        StatusCode::OK
    );
    let tokens: Vec<_> = state
        .terminals
        .0
        .lock()
        .unwrap()
        .values()
        .map(|t| t.cancelled.clone())
        .collect();
    assert_eq!(
        send(
            &router,
            "POST",
            "/v1/auth/logout",
            &cookie,
            &csrf,
            "https://pier.example.test",
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert!(tokens.iter().all(CancellationToken::is_cancelled));
}
