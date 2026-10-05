use super::*;
use crate::{Controller, auth::tests as auth_tests, settings::Repository};
use serde_json::{Value, json};

#[test]
fn repository_proxy_transport_and_restart() {
    let status = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/catalog/proxy_fixture.py"
        ))
        .arg(std::env::current_exe().unwrap())
        .status()
        .expect("repository proxy tests require Python 3 and OpenSSL");
    assert!(
        status.success(),
        "repository proxy integration tests failed"
    );
}

// A child test process isolates environment/Git configuration from parallel
// tests without changing process-global environment variables in Rust.
#[tokio::test]
#[ignore = "invoked by repository_proxy_transport_and_restart with local servers"]
async fn sync_proxy_child() {
    let fixture: Value = serde_json::from_str(
        &std::env::var("PIER_REPOSITORY_PROXY_FIXTURE").expect("run the parent integration test"),
    )
    .unwrap();
    let value = |key: &str| fixture[key].as_str().unwrap();
    let root = tempfile::tempdir_in(value("root")).unwrap();
    let proxy = |http: Option<&str>, https: Option<&str>, bypass: Option<&str>| BuildProxy {
        http_proxy: http.map(str::to_owned),
        https_proxy: https.map(str::to_owned),
        no_proxy: bypass.map(str::to_owned),
    };
    let fetch = |url: &str, proxy: &BuildProxy| {
        sync_with_proxy(url, "main", &root.path().join("fetches"), proxy)
    };
    let requests = || -> Vec<Value> {
        fs::read_to_string(value("log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let proxy_count = || {
        requests()
            .iter()
            .filter(|event| event["kind"].as_str().unwrap().starts_with("proxy-"))
            .count()
    };

    // Both authenticated proxy transports can forward HTTP and tunnel HTTPS.
    // Only the appropriate field is set; the other points at a refused port.
    for endpoint in ["proxy_http", "proxy_https"] {
        for scheme in ["http", "https"] {
            let before = proxy_count();
            let settings = if scheme == "http" {
                proxy(Some(value(endpoint)), Some(value("dead")), None)
            } else {
                proxy(Some(value("dead")), Some(value(endpoint)), None)
            };
            let catalog = fetch(value(scheme), &settings).unwrap();
            assert!(proxy_count() > before);
            assert_eq!(
                fs::read_to_string(catalog.root.join("README.md")).unwrap(),
                "repository proxy fixture\n"
            );
            let git_config = fs::read_to_string(catalog.root.join(".git/config")).unwrap();
            assert!(!git_config.contains("proxy-user"));
            assert!(!git_config.contains("repo-proxy-secret"));
            assert!(!git_config.contains("proxy ="));
        }
    }
    assert!(requests().iter().any(|event| event["method"] == "CONNECT"));

    // No corresponding proxy means direct, even with the other field configured
    // and hostile environment/global/URL-specific Git proxy configuration.
    let before = proxy_count();
    fetch(
        value("direct_http"),
        &proxy(None, Some(value("dead")), None),
    )
    .unwrap();
    fetch(
        value("direct_https"),
        &proxy(Some(value("dead")), None, None),
    )
    .unwrap();
    sync(
        value("direct_http"),
        "main",
        &root.path().join("public-api"),
    )
    .unwrap();
    assert_eq!(proxy_count(), before);

    // An explicit bypass works for both schemes, overriding ambient NO_PROXY.
    let bypass = proxy(
        Some(value("dead")),
        Some(value("dead")),
        Some("127.0.0.1,localhost"),
    );
    fetch(value("direct_http"), &bypass).unwrap();
    fetch(value("direct_https"), &bypass).unwrap();

    // A reachable origin must not hide a proxy failure by falling back to direct.
    let before = requests().len();
    let error = fetch(
        value("direct_http"),
        &proxy(Some(value("dead")), None, None),
    )
    .unwrap_err();
    assert_eq!(requests().len(), before);
    assert!(!error.to_string().contains(value("dead")));
    let incorrect = value("proxy_http").replace("repo-proxy-secret", "incorrect-secret");
    let error = fetch(value("direct_http"), &proxy(Some(&incorrect), None, None)).unwrap_err();
    assert!(!error.to_string().contains("secret"));
    assert!(!error.to_string().contains("proxy-user"));

    // Local paths and the configured SSH transport still work with HTTP proxies.
    let bad = proxy(Some(value("dead")), Some(value("dead")), None);
    let before = proxy_count();
    fetch(value("repo"), &bad).unwrap();
    fetch("ssh://git@repository.invalid/repo", &bad).unwrap();
    assert_eq!(proxy_count(), before);

    // Exercise persisted settings and the real active-runtime lifecycle.
    let mut config = auth_tests::config(&root.path().join("lifecycle"));
    config.build_proxy = bad;
    let state = Controller::open(config.clone()).unwrap();
    state
        .save_repository(Repository {
            url: value("direct_http").into(),
            reference: "main".into(),
        })
        .unwrap();
    let commit = state.sync().unwrap(); // No active runtime before initialization: direct.
    let router = crate::api::router(state.clone());
    auth_tests::initialize(&router).await;
    assert!(state.sync().is_err()); // Initialized with the refused proxy.
    assert_eq!(state.repository_view()["commit"], commit);
    assert!(!state.repository_needs_sync());
    state
        .save_runtime(
            serde_json::from_value(json!({
                "build_proxy": {"http_proxy": value("proxy_http")}
            }))
            .unwrap(),
        )
        .unwrap();
    assert!(state.sync().is_err()); // Saved replacement is still pending.
    assert_eq!(state.repository_view()["commit"], commit);
    drop(router);
    drop(state);

    let state = Controller::open(config.clone()).unwrap();
    let before = proxy_count();
    assert_eq!(state.sync().unwrap(), commit);
    assert!(proxy_count() > before);
    assert!(!state.repository_needs_sync());
    assert!(state.repository_view()["error"].is_null());
    state
        .save_runtime(serde_json::from_value(json!({"build_proxy": {}})).unwrap())
        .unwrap();
    let before = proxy_count();
    assert_eq!(state.sync().unwrap(), commit);
    assert!(proxy_count() > before); // Clearing also waits for restart.
    drop(state);

    let state = Controller::open(config).unwrap();
    let before = proxy_count();
    assert_eq!(state.sync().unwrap(), commit);
    assert_eq!(proxy_count(), before);
    assert!(!state.repository_needs_sync());
}
