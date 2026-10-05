use super::*;

#[test]
fn checks_blueprint_username_before_acquiring_sources() {
    for (blueprint_name, expected) in [
        ("bad+name", "name must be a system username"),
        ("a".repeat(33).as_str(), "name must be a system username"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let recipe = root.path().join("recipe");
        fs::create_dir(&recipe).unwrap();
        // Valid packaging metadata, but acquiring this source would fail.
        fs::write(
            recipe.join("pier-pkg.yml"),
            r#"
schema: 2
name: '{{ APP_NAME }}'
version: '1'
variables:
  APP_NAME: {}
source: {type: binary, url: 'http://127.0.0.1:1/unreachable', format: raw}
files: [{from: download, to: bin/demo, executable: true}]
service: {command: [bin/demo]}
"#,
        )
        .unwrap();
        let state = Controller::open(auth::tests::config(root.path())).unwrap();
        let settings = state.settings.read().unwrap().runtime.clone().unwrap();
        let runtime = Arc::new(runtime::Runtime::new(settings).unwrap());
        let blueprint = catalog::Blueprint {
            schema: 1,
            name: blueprint_name.into(),
            variables: BTreeMap::new(),
            apps: [("first", "Demo.Api"), ("second", "Demo.Api")]
                .into_iter()
                .map(|(id, name)| catalog::BlueprintApp {
                    id: id.into(),
                    app: "recipe".into(),
                    variables: BTreeMap::from([("APP_NAME".into(), name.into())]),
                })
                .collect(),
        };
        let catalog = Catalog {
            root: root.path().into(),
            commit: "commit".into(),
            apps: BTreeMap::from([("recipe".into(), pier_pkg::inspect(&recipe).unwrap())]),
            blueprints: BTreeMap::from([("blueprint".into(), blueprint)]),
        };
        let job = Job {
            action: pier_protocol::DeploymentAction::Deploy,
            id: "job".into(),
            agent_id: "agent".into(),
            blueprint: "blueprint".into(),
            commit: "commit".into(),
            state: "building".into(),
            error: None,
            created_at: 0,
            plan: None,
            artifacts: BTreeMap::new(),
        };
        state.store.put("jobs", "job", &job).unwrap();
        let error = state
            .build(
                "job",
                catalog,
                Binding {
                    blueprint: "blueprint".into(),
                    variables: BTreeMap::new(),
                },
                BTreeMap::new(),
                pier_pkg::Architecture::Amd64,
                runtime,
            )
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        assert!(!state.config.state_dir.join("artifacts").exists());
    }
}
