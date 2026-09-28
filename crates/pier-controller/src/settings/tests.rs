use super::*;
use std::{fs, path::Path, process::Command};

fn repository(root: &Path, name: &str) -> Repository {
    let path = root.join(name);
    fs::create_dir(&path).unwrap();
    fs::write(path.join("README.md"), name).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        );
    }
    Repository {
        url: path.to_string_lossy().into(),
        reference: "main".into(),
    }
}

#[test]
fn defaults_start_without_repository_and_legacy_settings_import_once() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg: Config = serde_yaml_ng::from_str("{}").unwrap();
    assert_eq!(cfg.state_dir, Path::new("/var/lib/pier-controller"));
    assert_eq!(cfg.http_listen.to_string(), "127.0.0.1:8080");
    cfg.state_dir = root.path().join("state");
    cfg.repository.url = repository(root.path(), "first").url;
    cfg.repository.sync_interval_seconds = 0; // Deprecated and ignored.
    let state = Controller::open(cfg.clone()).unwrap();
    assert!(state.catalog.read().unwrap().is_none());
    assert!(!cfg.state_dir.join("snapshots").exists());
    state.sync().unwrap();
    drop(state);
    cfg.repository.url = "https://example.invalid/ignored.git".into();
    let state = Controller::open(cfg).unwrap();
    assert!(
        state.repository_view()["repository"]["url"]
            .as_str()
            .unwrap()
            .ends_with("first")
    );
    assert!(!state.repository_needs_sync());
}

#[test]
fn save_and_restart_never_fetch_and_manual_failures_retain_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg: Config = serde_yaml_ng::from_str("{}").unwrap();
    cfg.state_dir = root.path().join("state");
    let state = Controller::open(cfg.clone()).unwrap();
    let first = repository(root.path(), "first");
    let second = repository(root.path(), "second");
    state.save_repository(first.clone()).unwrap();
    assert!(!cfg.state_dir.join("snapshots").exists());
    let commit = state.sync().unwrap();
    let original = state.catalog.read().unwrap().clone().unwrap();
    // Failure for the same settings leaves the last successful catalog usable.
    fs::rename(&first.url, root.path().join("temporarily-offline")).unwrap();
    assert!(state.sync().is_err());
    assert!(!state.repository_needs_sync());
    state.save_repository(second.clone()).unwrap();
    assert!(state.repository_needs_sync());
    assert_eq!(state.repository_view()["commit"], commit);
    drop(state);
    let state = Controller::open(cfg).unwrap();
    assert!(state.repository_needs_sync());
    assert_eq!(state.repository_view()["commit"], commit);
    let invalid = Repository {
        reference: "missing".into(),
        ..second.clone()
    };
    state.save_repository(invalid.clone()).unwrap();
    assert!(state.sync().is_err());
    assert!(state.repository_needs_sync());
    assert_eq!(
        state.repository_view()["repository"],
        serde_json::to_value(invalid).unwrap()
    );
    assert_eq!(state.repository_view()["commit"], commit);
    state.save_repository(second).unwrap();
    assert_ne!(state.sync().unwrap(), commit);
    assert!(!state.repository_needs_sync());
    assert!(
        original.root.join("README.md").exists(),
        "active deployments retain immutable snapshots"
    );
}

#[test]
fn repository_edits_wait_for_sync_and_invalid_input_is_atomic() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg: Config = serde_yaml_ng::from_str("{}").unwrap();
    cfg.state_dir = root.path().join("state");
    let state = Controller::open(cfg).unwrap();
    let repo = repository(root.path(), "first");
    let guard = state.sync_lock.lock().unwrap();
    let worker = state.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        worker.save_repository(repo).unwrap();
        sent.send(()).unwrap();
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err()
    );
    assert!(state.settings.read().unwrap().repository.is_none());
    drop(guard);
    received
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    thread.join().unwrap();
    let before = state.repository_view();
    assert!(
        state
            .save_repository(Repository {
                url: "--upload-pack=bad".into(),
                reference: "main".into()
            })
            .is_err()
    );
    assert_eq!(state.repository_view(), before);
}
