use super::*;
use crate::{
    Job, api,
    auth::tests::{config, initialize, send},
    catalog::{BlueprintApp, Catalog},
};
use axum::{Router, body::to_bytes};
use pier_pkg::{Architecture, VariableDefinition};
use pier_protocol::{AgentInfo, AgentReport, Message};
use std::fs;

struct Fixture {
    root: tempfile::TempDir,
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
        let blueprint = Blueprint {
            schema: 1,
            name: "web".into(),
            variables: BTreeMap::from([
                ("INPUT".into(), VariableDefinition { default: None }),
                (
                    "PORT".into(),
                    VariableDefinition {
                        default: Some("8080".into()),
                    },
                ),
            ]),
            apps: vec![BlueprintApp {
                id: "api".into(),
                app: "recipe".into(),
                variables: BTreeMap::from([
                    ("VALUE".into(), "{{ INPUT }}".into()),
                    ("PORT".into(), "{{ PORT }}".into()),
                ]),
            }],
        };
        let mut other = blueprint.clone();
        other.name = "worker".into();
        *state.catalog.write().unwrap() = Some(Catalog {
            root: root.path().into(),
            commit: "commit".into(),
            apps: BTreeMap::new(),
            blueprints: BTreeMap::from([("web".into(), blueprint), ("worker".into(), other)]),
        });
        for id in ["a", "b"] {
            state
                .store
                .put(
                    "agents",
                    id,
                    &AgentRecord {
                        id: id.into(),
                        name: format!("server-{id}"),
                        token_hash: pier_protocol::hash(id),
                        info: Some(AgentInfo {
                            architecture: Architecture::Amd64,
                            hostname: id.into(),
                            os_release: "test".into(),
                        }),
                        last_seen: None,
                        report: AgentReport::default(),
                        connection: Default::default(),
                        proxy: None,
                    },
                )
                .unwrap();
        }
        Self {
            root,
            state,
            router,
            cookie,
            csrf: session["csrf_token"].as_str().unwrap().into(),
        }
    }
    async fn call(&self, method: &str, path: &str, value: Option<Value>, status: u16) -> Value {
        let response = send(
            &self.router,
            method,
            path,
            &self.cookie,
            &self.csrf,
            "https://pier.example.test",
            value,
        )
        .await;
        let actual = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(actual, status, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    }
    fn binding(&self, agent: &str, path: &str) -> Binding {
        self.state
            .bindings(agent)
            .unwrap()
            .remove(&pier_protocol::hash(path))
            .unwrap()
    }
    fn resolved(&self, agent: &str, path: &str) -> BTreeMap<String, Variables> {
        let _guard = self.state.mutation_lock.lock().unwrap();
        self.state
            .resolve_binding(
                &self.binding(agent, path),
                &self
                    .state
                    .catalog
                    .read()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .blueprints[path],
            )
            .unwrap()
            .variables
    }
}

#[tokio::test]
async fn globals_require_auth_validate_requests_and_survive_restart() {
    let fixture = Fixture::new().await;
    for (method, path, body) in [
        ("GET", "/v1/variables", None),
        (
            "POST",
            "/v1/variables",
            Some(json!({"name":"VALUE","value":"x"})),
        ),
        ("PUT", "/v1/variables/VALUE", Some(json!({"value":"x"}))),
        ("DELETE", "/v1/variables/VALUE", None),
    ] {
        assert_eq!(
            send(&fixture.router, method, path, "", "", "", body.clone())
                .await
                .status(),
            401
        );
        if method != "GET" {
            assert_eq!(
                send(
                    &fixture.router,
                    method,
                    path,
                    &fixture.cookie,
                    "",
                    "https://pier.example.test",
                    body.clone()
                )
                .await
                .status(),
                403
            );
            assert_eq!(
                send(
                    &fixture.router,
                    method,
                    path,
                    &fixture.cookie,
                    &fixture.csrf,
                    "https://other.test",
                    body
                )
                .await
                .status(),
                403
            );
        }
    }
    for name in ["", "1VALUE", "BAD-NAME", "PIER_ARCH", "变量"] {
        fixture
            .call(
                "POST",
                "/v1/variables",
                Some(json!({"name":name,"value":"x"})),
                400,
            )
            .await;
    }
    for body in [
        json!({"name":"VALUE","value":1}),
        json!({"name":"VALUE","value":null}),
        json!({"name":"VALUE"}),
        json!({"name":"VALUE","value":"x","secret":true}),
    ] {
        fixture.call("POST", "/v1/variables", Some(body), 422).await;
    }
    fixture
        .call(
            "POST",
            "/v1/variables",
            Some(json!({"name":"VALUE","value":""})),
            200,
        )
        .await;
    fixture
        .call(
            "POST",
            "/v1/variables",
            Some(json!({"name":"VALUE","value":"duplicate"})),
            409,
        )
        .await;
    fixture
        .call(
            "PUT",
            "/v1/variables/MISSING",
            Some(json!({"value":"x"})),
            404,
        )
        .await;
    fixture
        .call(
            "PUT",
            "/v1/variables/VALUE",
            Some(json!({"value":"new","name":"OTHER"})),
            422,
        )
        .await;
    let value = "line one\n{{ LITERAL }}\n第二行";
    fixture
        .call(
            "PUT",
            "/v1/variables/VALUE",
            Some(json!({"value":value})),
            200,
        )
        .await;
    let cfg = fixture.state.config.clone();
    let Fixture {
        root,
        state,
        router,
        cookie,
        csrf,
    } = fixture;
    drop(router);
    drop(state);
    let state = Controller::open(cfg).unwrap();
    let fixture = Fixture {
        root,
        router: api::router(state.clone()),
        state,
        cookie,
        csrf,
    };
    let listing = fixture.call("GET", "/v1/variables", None, 200).await;
    assert_eq!(
        listing["variables"],
        json!([{"name":"VALUE","value":value,"references":[]}])
    );
    fixture
        .call("DELETE", "/v1/variables/VALUE", None, 200)
        .await;
    fixture
        .call("DELETE", "/v1/variables/VALUE", None, 404)
        .await;
}

#[tokio::test]
async fn references_are_explicit_shared_and_block_deletion_until_removed() {
    let fixture = Fixture::new().await;
    fixture
        .call(
            "POST",
            "/v1/variables",
            Some(json!({"name":"SHARED","value":"first"})),
            200,
        )
        .await;
    fixture
        .call(
            "POST",
            "/v1/variables",
            Some(json!({"name":"PORT","value":"9090"})),
            200,
        )
        .await;
    for (agent, blueprint) in [("a", "web"), ("a", "worker"), ("b", "web")] {
        let body = fixture
            .call(
                "POST",
                &format!("/v1/agents/{agent}/bindings"),
                Some(json!({"blueprint":blueprint,"variables":{"INPUT":{"ref":"SHARED"}}})),
                200,
            )
            .await;
        assert_eq!(body["variable_refs"], json!({"INPUT":"SHARED"}));
        assert!(!body.to_string().contains("first"));
        assert_eq!(fixture.resolved(agent, blueprint)["api"]["VALUE"], "first");
        assert_eq!(fixture.resolved(agent, blueprint)["api"]["PORT"], "8080");
    }
    fixture
        .call(
            "PUT",
            "/v1/variables/SHARED",
            Some(json!({"value":"{{ stays_literal }}\nnext"})),
            200,
        )
        .await;
    assert_eq!(
        fixture.resolved("a", "web")["api"]["VALUE"],
        "{{ stays_literal }}\nnext"
    );
    let listing = fixture.call("GET", "/v1/variables", None, 200).await;
    let refs = &listing["variables"][1]["references"];
    assert_eq!(refs.as_array().unwrap().len(), 3);
    assert_eq!(refs[0]["agent_name"], "server-a");
    fixture
        .call("DELETE", "/v1/variables/SHARED", None, 409)
        .await;
    let path = format!("/v1/agents/a/bindings/{}", pier_protocol::hash("web"));
    for variables in [
        json!({"INPUT":{"ref":"MISSING"}}),
        json!({"INPUT":{"ref":"bad-name"}}),
        json!({"INPUT":null}),
        json!({"UNKNOWN":{"ref":"SHARED"}}),
    ] {
        fixture
            .call(
                "PATCH",
                &path,
                Some(json!({"blueprint":"web","variables":variables})),
                400,
            )
            .await;
        assert_eq!(
            fixture.binding("a", "web").variables["INPUT"].reference(),
            Some("SHARED")
        );
    }
    fixture
        .call(
            "PATCH",
            &path,
            Some(json!({"blueprint":"web","variables":{"PORT":{"ref":"PORT"}}})),
            200,
        )
        .await;
    assert_eq!(fixture.resolved("a", "web")["api"]["PORT"], "9090");
    fixture
        .call(
            "PATCH",
            &path,
            Some(json!({"blueprint":"web","variables":{"PORT":null}})),
            200,
        )
        .await;
    assert_eq!(fixture.resolved("a", "web")["api"]["PORT"], "8080");
    fixture
        .call("DELETE", "/v1/variables/PORT", None, 200)
        .await;
    for (agent, blueprint) in [("a", "web"), ("a", "worker"), ("b", "web")] {
        let path = format!(
            "/v1/agents/{agent}/bindings/{}",
            pier_protocol::hash(blueprint)
        );
        fixture
            .call(
                "PUT",
                &path,
                Some(json!({"blueprint":blueprint,"variables":{"INPUT":"literal"}})),
                200,
            )
            .await;
    }
    fixture
        .call("DELETE", "/v1/variables/SHARED", None, 200)
        .await;
    assert_eq!(fixture.resolved("a", "web")["api"]["VALUE"], "literal");
}

#[test]
fn binding_wire_format_preserves_literals_and_rejects_ambiguous_references() {
    let legacy: Binding =
        serde_json::from_str(r#"{"blueprint":"web","variables":{"INPUT":"{{ GLOBAL }}"}}"#)
            .unwrap();
    assert_eq!(legacy.variables["INPUT"], "{{ GLOBAL }}".into());
    for invalid in [
        r#"{"INPUT":{"ref":"A","extra":true}}"#,
        r#"{"INPUT":{"ref":"A","ref":"B"}}"#,
        r#"{"INPUT":{"ref":null}}"#,
        r#"{"INPUT":1}"#,
        r#"{"INPUT":"x","INPUT":{"ref":"A"}}"#,
    ] {
        assert!(
            serde_json::from_str::<Binding>(&format!(
                r#"{{"blueprint":"web","variables":{invalid}}}"#
            ))
            .is_err(),
            "{invalid}"
        );
    }
}

#[tokio::test]
async fn queued_deployment_builds_with_captured_values_and_next_deployment_uses_edits() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/app",
                axum::routing::get(|| async { "#!/bin/sh\nexit 0\n" }),
            ),
        )
        .await
        .unwrap();
    });
    let recipe = fixture.root.path().join("recipe");
    fs::create_dir(&recipe).unwrap();
    fs::write(recipe.join("pier-pkg.yml"), format!("schema: 2\nname: demo\nversion: '1.0.0'\nvariables:\n  VALUE: {{}}\n  PORT: {{}}\nsource: {{type: binary, url: 'http://{address}/app', format: raw}}\nfiles: [{{from: download, to: bin/demo, executable: true}}]\nservice:\n  command: [bin/demo]\n  env: {{VALUE: '{{{{ VALUE }}}}', PORT: '{{{{ PORT }}}}'}}\n")).unwrap();
    fixture
        .state
        .catalog
        .write()
        .unwrap()
        .as_mut()
        .unwrap()
        .apps
        .insert("recipe".into(), pier_pkg::inspect(&recipe).unwrap());
    {
        let mut settings = fixture.state.settings.write().unwrap();
        settings.catalog_repository = settings.repository.clone();
    }
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    fixture.state.sessions.lock().unwrap().insert(
        "a".into(),
        crate::Session {
            id: "test".into(),
            sender,
            terminal: false,
            multi_blueprint: true,
            cancelled: tokio_util::sync::CancellationToken::new(),
        },
    );
    fixture
        .call(
            "POST",
            "/v1/variables",
            Some(json!({"name":"SHARED","value":"old\n{{ literal }}"})),
            200,
        )
        .await;
    fixture
        .call(
            "POST",
            "/v1/agents/a/bindings",
            Some(json!({"blueprint":"web","variables":{"INPUT":{"ref":"SHARED"}}})),
            200,
        )
        .await;
    let runtime = fixture.state.active_runtime().unwrap();
    let permits = runtime
        .build_slots
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    let request = json!({"agent_id":"a","blueprint":"web","commit":"commit","images":{}});
    let first = fixture
        .call("POST", "/v1/deployments", Some(request.clone()), 200)
        .await;
    fixture
        .call(
            "PUT",
            "/v1/variables/SHARED",
            Some(json!({"value":"new"})),
            200,
        )
        .await;
    assert_eq!(
        fixture
            .state
            .store
            .get::<Job>("jobs", first["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .state,
        "building"
    );
    drop(permits);
    for (index, expected) in ["old\n{{ literal }}", "new"].into_iter().enumerate() {
        if index == 1 {
            fixture
                .call("POST", "/v1/deployments", Some(request.clone()), 200)
                .await;
        }
        let message = tokio::time::timeout(std::time::Duration::from_secs(15), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let Message::Deploy { plan } = message else {
            panic!("expected deployment")
        };
        let mut job = fixture
            .state
            .store
            .get::<Job>("jobs", &plan.id)
            .unwrap()
            .unwrap();
        let manifest = pier_pkg::unpack(
            &job.artifacts["api"],
            fixture.root.path().join(format!("unpacked-{index}")),
            &plan.apps[0].sha256,
            Architecture::Amd64,
        )
        .unwrap();
        assert_eq!(manifest.service.env["VALUE"], expected);
        assert_eq!(manifest.service.env["PORT"], "8080");
        job.state = "succeeded".into();
        fixture.state.store.put("jobs", &plan.id, &job).unwrap();
    }
    // Corrupt/stale stored references also fail before creating a job.
    fixture.state.store.delete(NAMESPACE, "SHARED").unwrap();
    let count = fixture.state.store.list::<Job>("jobs").unwrap().len();
    let error = fixture
        .call("POST", "/v1/deployments", Some(request), 400)
        .await;
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("INPUT references missing global variable: SHARED")
    );
    assert_eq!(
        fixture.state.store.list::<Job>("jobs").unwrap().len(),
        count
    );
    server.abort();
}
