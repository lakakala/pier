use super::*;
use crate::auth::tests::{config, initialize, send};
use pier_protocol::{BlueprintStatus, DeploymentAction};

#[tokio::test]
async fn bindings_are_independent_and_only_stopped_online_blueprints_can_be_unbound() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let router = router(state.clone());
    let (cookie, session) = initialize(&router).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let mut agent = AgentRecord {
        proxy: None,
        connection: Default::default(),
        id: "agent".into(),
        name: "server".into(),
        token_hash: pier_protocol::hash("token"),
        info: None,
        last_seen: None,
        report: AgentReport::default(),
    };
    state.store.put("agents", "agent", &agent).unwrap();
    let definition = |name: &str| crate::catalog::Blueprint {
        schema: 1,
        name: name.into(),
        apps: vec![],
        variables: BTreeMap::from([(
            "SECRET".into(),
            pier_pkg::VariableDefinition { default: None },
        )]),
    };
    *state.catalog.write().unwrap() = Some(crate::catalog::Catalog {
        root: root.path().into(),
        commit: "commit".into(),
        apps: BTreeMap::new(),
        blueprints: BTreeMap::from([
            ("web".into(), definition("Web.Site")),
            ("other".into(), definition("Other.Site")),
            ("duplicate".into(), definition("Web.Site")),
        ]),
    });
    for blueprint in ["web", "other"] {
        let response = send(
            &router,
            "POST",
            "/v1/agents/agent/bindings",
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(
                json!({"blueprint":blueprint,"variables":{"SECRET":format!("secret-{blueprint}")}}),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("secret-"));
    }
    for (path, expected) in [
        ("web", StatusCode::CONFLICT),
        ("duplicate", StatusCode::BAD_REQUEST),
    ] {
        let response = send(
            &router,
            "POST",
            "/v1/agents/agent/bindings",
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(json!({"blueprint":path,"variables":{"SECRET":"new"}})),
        )
        .await;
        assert_eq!(response.status(), expected);
    }
    let web = pier_protocol::hash("web");
    let path = format!("/v1/agents/agent/bindings/{web}");
    assert_eq!(
        send(
            &router,
            "PATCH",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            Some(json!({"blueprint":"web","variables":{"SECRET":"updated"}}))
        )
        .await
        .status(),
        StatusCode::OK
    );
    let bindings = state.bindings("agent").unwrap();
    assert_eq!(bindings[&web].variables["SECRET"], "updated".into());
    assert_eq!(
        bindings[&pier_protocol::hash("other")].variables["SECRET"],
        "secret-other".into()
    );
    // No online session means even an unused binding cannot be removed.
    assert_eq!(
        send(
            &router,
            "DELETE",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            None
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let (sender, _receiver) = tokio::sync::mpsc::channel(8);
    state.sessions.lock().unwrap().insert(
        "agent".into(),
        crate::Session {
            id: "session".into(),
            sender,
            terminal: true,
            multi_blueprint: true,
            cancelled: tokio_util::sync::CancellationToken::new(),
        },
    );
    agent.report.blueprints.push(BlueprintStatus {
        account_reserved: true,
        id: web.clone(),
        blueprint: "web".into(),
        name: "Web.Site".into(),
        deployment_id: Some("installed".into()),
        state: "running".into(),
        apps: vec![],
        result: None,
    });
    state.store.put("agents", "agent", &agent).unwrap();
    assert_eq!(
        send(
            &router,
            "DELETE",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            None
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    agent.report.blueprints[0].state = "stopped".into();
    state.store.put("agents", "agent", &agent).unwrap();
    state
        .store
        .put(
            "jobs",
            "busy",
            &Job {
                action: DeploymentAction::Stop,
                id: "busy".into(),
                agent_id: "agent".into(),
                blueprint: "web".into(),
                commit: String::new(),
                state: "ready".into(),
                error: None,
                created_at: 0,
                plan: None,
                artifacts: BTreeMap::new(),
            },
        )
        .unwrap();
    assert_eq!(
        send(
            &router,
            "DELETE",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            None
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    state.store.delete("jobs", "busy").unwrap();
    assert_eq!(
        send(
            &router,
            "DELETE",
            &path,
            &cookie,
            csrf,
            "https://pier.example.test",
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(state.bindings("agent").unwrap().len(), 1);
    assert!(
        state
            .bindings("agent")
            .unwrap()
            .contains_key(&pier_protocol::hash("other"))
    );
}

#[tokio::test]
async fn new_plans_are_never_dispatched_to_old_agents() {
    let root = tempfile::tempdir().unwrap();
    let state = Controller::open(config(root.path())).unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    state.sessions.lock().unwrap().insert(
        "agent".into(),
        crate::Session {
            id: "session".into(),
            sender,
            terminal: true,
            multi_blueprint: false,
            cancelled: tokio_util::sync::CancellationToken::new(),
        },
    );
    state
        .store
        .put(
            "jobs",
            "job",
            &Job {
                action: DeploymentAction::Deploy,
                id: "job".into(),
                agent_id: "agent".into(),
                blueprint: "web".into(),
                commit: "commit".into(),
                state: "ready".into(),
                error: None,
                created_at: 0,
                artifacts: BTreeMap::new(),
                plan: Some(pier_protocol::DeploymentPlan {
                    id: "job".into(),
                    agent_id: "agent".into(),
                    blueprint: "web".into(),
                    blueprint_name: "Web.Site".into(),
                    action: DeploymentAction::Deploy,
                    commit: "commit".into(),
                    architecture: pier_pkg::Architecture::Amd64,
                    apps: vec![],
                }),
            },
        )
        .unwrap();
    state.dispatch("job").await.unwrap();
    assert!(receiver.try_recv().is_err());
    state
        .sessions
        .lock()
        .unwrap()
        .get_mut("agent")
        .unwrap()
        .multi_blueprint = true;
    state.dispatch("job").await.unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        pier_protocol::Message::Deploy { .. }
    ));
}
