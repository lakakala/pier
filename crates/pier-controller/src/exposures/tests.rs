use super::*;
use crate::{
    api,
    auth::tests::{config, initialize, send},
};
use axum::{Router, body::to_bytes};
use pier_pkg::{Port, PortProtocol};
use pier_protocol::{AgentReport, AppStatus, BlueprintStatus, DeploymentAction};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    _root: tempfile::TempDir,
    state: Arc<Controller>,
    router: Router,
    cookie: String,
    csrf: String,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = Controller::open(config(root.path())).unwrap();
        let router = api::router(state.clone());
        let (cookie, session) = initialize(&router).await;
        for id in ["backend", "edge1", "edge2", "other"] {
            let record = AgentRecord {
                id: id.into(),
                name: id.into(),
                tags: if id.starts_with("edge") {
                    vec!["edge".into()]
                } else {
                    vec![]
                },
                proxy: None,
                connection: Default::default(),
                token_hash: pier_protocol::hash(id),
                info: None,
                last_seen: None,
                report: AgentReport::default(),
            };
            state.store.put("agents", id, &record).unwrap();
        }
        Self {
            _root: root,
            state,
            router,
            cookie,
            csrf: session["csrf_token"].as_str().unwrap().into(),
        }
    }
    async fn api(&self, method: &str, path: &str, body: Option<Value>, expected: u16) -> Value {
        let response = send(
            &self.router,
            method,
            path,
            &self.cookie,
            &self.csrf,
            "https://pier.example.test",
            body,
        )
        .await;
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(status, expected, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).unwrap()
    }
    fn install(&self, id: &str, blueprint: &str, network: Deployment, state: &str) -> AgentReport {
        let job = Job {
            id: id.into(),
            agent_id: "backend".into(),
            blueprint: blueprint.into(),
            network,
            action: DeploymentAction::Deploy,
            commit: "commit".into(),
            state: "succeeded".into(),
            error: None,
            created_at: 1,
            plan: None,
            artifacts: BTreeMap::new(),
        };
        self.state.store.put("jobs", id, &job).unwrap();
        let report = AgentReport {
            capabilities: vec![pier_protocol::forward::CAPABILITY.into()],
            blueprints: vec![BlueprintStatus {
                account_reserved: true,
                id: pier_protocol::hash(blueprint),
                blueprint: blueprint.into(),
                name: "app".into(),
                deployment_id: Some(id.into()),
                state: state.into(),
                apps: vec![AppStatus {
                    instance: pier_protocol::hash(format!("{blueprint}\0api")),
                    id: "api".into(),
                    state: state.into(),
                    pid: Some(1),
                    ..Default::default()
                }],
                result: None,
            }],
            ..Default::default()
        };
        self.report(report.clone());
        report
    }
    fn report(&self, value: AgentReport) {
        let mut record: AgentRecord = self.state.store.get("agents", "backend").unwrap().unwrap();
        super::report(&self.state, "backend", &value).unwrap();
        record.report = value;
        self.state.store.put("agents", "backend", &record).unwrap();
    }
    fn connection(
        &self,
        agent: &str,
    ) -> (
        pier_protocol::forward::Mux,
        tokio::sync::mpsc::Receiver<Incoming>,
        CancellationToken,
    ) {
        let token = CancellationToken::new();
        let (left, right) = tokio::io::duplex(4096);
        let (sender, mut messages, mux, incoming) =
            pier_protocol::forward::session(pier_protocol::framed(left), token.clone(), true);
        let (_, mut remote_messages, remote, remote_incoming) =
            pier_protocol::forward::session(pier_protocol::framed(right), token.clone(), true);
        tokio::spawn(async move { while messages.recv().await.is_some() {} });
        tokio::spawn(async move { while remote_messages.recv().await.is_some() {} });
        self.state.sessions.lock().unwrap().insert(
            agent.into(),
            crate::Session {
                id: pier_protocol::new_id(),
                forward: Some(mux),
                sender,
                terminal: false,
                multi_blueprint: true,
                cancelled: token.clone(),
            },
        );
        serve(self.state.clone(), agent.into(), incoming, token.clone());
        (remote, remote_incoming, token)
    }
}
fn network(public: u16) -> Deployment {
    Deployment {
        ports: BTreeMap::from([(
            "api".into(),
            BTreeMap::from([(
                "http".into(),
                Port {
                    protocol: PortProtocol::Tcp,
                    port: 8080,
                },
            )]),
        )]),
        exposures: BTreeMap::from([(
            "api".into(),
            BTreeMap::from([(
                "http".into(),
                Exposure {
                    tag: "edge".into(),
                    port: public,
                },
            )]),
        )]),
    }
}

#[tokio::test]
async fn tags_are_authorized_atomic_and_conflicts_include_offline_and_pending_agents() {
    let f = Fixture::new().await;
    f.install("v1", "web", network(80), "running");
    assert!(check_conflicts(&f.state, Some(("other", "web", &network(80))), None).is_err());
    let mut other = network(80);
    other
        .exposures
        .get_mut("api")
        .unwrap()
        .get_mut("http")
        .unwrap()
        .tag = "other".into();
    assert!(check_conflicts(&f.state, Some(("other", "web", &other)), None).is_ok());
    f.state
        .store
        .put(
            "jobs",
            "pending",
            &Job {
                id: "pending".into(),
                agent_id: "other".into(),
                blueprint: "web".into(),
                network: other,
                action: DeploymentAction::Deploy,
                commit: "commit".into(),
                state: "building".into(),
                error: None,
                created_at: 2,
                plan: None,
                artifacts: BTreeMap::new(),
            },
        )
        .unwrap();
    f.api(
        "PUT",
        "/v1/agents/edge1/tags",
        Some(json!({"tags":["edge","other"]})),
        409,
    )
    .await;
    assert_eq!(
        f.state
            .store
            .get::<AgentRecord>("agents", "edge1")
            .unwrap()
            .unwrap()
            .tags,
        ["edge"]
    );
    f.api(
        "PUT",
        "/v1/agents/edge1/tags",
        Some(json!({"tags":[" edge ","edge","public"]})),
        200,
    )
    .await;
    assert_eq!(
        f.state
            .store
            .get::<AgentRecord>("agents", "edge1")
            .unwrap()
            .unwrap()
            .tags,
        ["edge", "public"]
    );
    f.api(
        "PUT",
        "/v1/agents/edge1/tags",
        Some(json!({"tags":[""]})),
        400,
    )
    .await;
    let response = send(
        &f.router,
        "PUT",
        "/v1/agents/edge1/tags",
        "",
        "",
        "https://pier.example.test",
        Some(json!({"tags":[]})),
    )
    .await;
    assert_eq!(response.status().as_u16(), 401);
    // An ingress cannot steal a declared backend port on the same machine.
    f.api(
        "PUT",
        "/v1/agents/backend/tags",
        Some(json!({"tags":["edge"]})),
        200,
    )
    .await;
    assert!(check_conflicts(&f.state, Some(("backend", "web", &network(8080))), None).is_err());
}

#[tokio::test]
async fn two_ingresses_relay_on_existing_connections_and_tag_removal_revokes_streams() {
    let f = Fixture::new().await;
    f.install("v1", "web", network(80), "running");
    let (_, mut backend, backend_token) = f.connection("backend");
    let (first, _, first_token) = f.connection("edge1");
    let (second, _, second_token) = f.connection("edge2");
    let echo = tokio::spawn(async move {
        while let Some(mut request) = backend.recv().await {
            tokio::spawn(async move {
                assert!(
                    matches!(&request.request, Request::Target { target } if target.deployment == "v1" && target.port == 8080)
                );
                request.accept().await.unwrap();
                let (mut read, mut write) = tokio::io::split(&mut request.stream);
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    reconcile(&f.state).unwrap();
    let route = |agent: &str| {
        f.state
            .forwarding
            .data
            .lock()
            .unwrap()
            .routes
            .values()
            .find(|r| r.view.ingress_id.as_deref() == Some(agent))
            .unwrap()
            .view
            .id
            .clone()
    };
    let first_id = route("edge1");
    let second_id = route("edge2");
    let mut one = first
        .open(Request::Ingress {
            route: first_id.clone(),
        })
        .await
        .unwrap();
    let mut two = second
        .open(Request::Ingress {
            route: second_id.clone(),
        })
        .await
        .unwrap();
    for stream in [&mut one, &mut two] {
        stream.write_all(b"roundtrip").await.unwrap();
        let mut response = [0; 9];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&response, b"roundtrip");
    }
    assert!(
        first
            .open(Request::Ingress { route: second_id })
            .await
            .is_err()
    );
    f.api(
        "PUT",
        "/v1/agents/edge1/tags",
        Some(json!({"tags":[]})),
        200,
    )
    .await;
    reconcile(&f.state).unwrap();
    assert!(
        first
            .open(Request::Ingress { route: first_id })
            .await
            .is_err()
    );
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), one.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    two.write_all(b"x").await.unwrap();
    two.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, [b'x']);
    backend_token.cancel();
    first_token.cancel();
    second_token.cancel();
    echo.abort();
}

#[tokio::test]
async fn snapshots_follow_installed_deployment_rollback_stop_and_missing_catalog() {
    let f = Fixture::new().await;
    let old = f.install("v1", "web", network(80), "running");
    let rows = f
        .api("GET", "/v1/agents/backend/exposures", None, 200)
        .await;
    assert_eq!(rows["exposures"].as_array().unwrap().len(), 2);
    assert!(
        rows["exposures"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["state"] == "pending")
    );
    f.install("v2", "web", network(81), "running");
    f.report(old.clone());
    assert_eq!(
        f.state.store.list::<Active>("port_deployments").unwrap()[0]
            .network
            .exposures["api"]["http"]
            .port,
        80
    );
    let mut stop: Job = f.state.store.get("jobs", "v2").unwrap().unwrap();
    stop.id = "stop".into();
    stop.action = DeploymentAction::Stop;
    stop.network = Deployment::default();
    f.state.store.put("jobs", "stop", &stop).unwrap();
    let mut stopped = old;
    stopped.blueprints[0].deployment_id = Some("stop".into());
    stopped.blueprints[0].state = "stopped".into();
    stopped.blueprints[0].apps.clear();
    f.report(stopped);
    assert!(
        f.state
            .store
            .list::<Active>("port_deployments")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.api("GET", "/v1/agents/backend/exposures", None, 200)
            .await["exposures"],
        json!([])
    );
}
