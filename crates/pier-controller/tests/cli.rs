use std::{ffi::OsStr, os::unix::ffi::OsStrExt, process::Command};

#[test]
fn help_and_version_work_without_configuration() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["--config", "missing.yml", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pier-controller"))
            .current_dir(root.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("Usage:") && help.contains("--config <PATH>"));
    }
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pier-controller"))
            .current_dir(root.path())
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            concat!("pier-controller ", env!("CARGO_PKG_VERSION"))
        );
    }
    assert_eq!(root.path().read_dir().unwrap().count(), 0);
}

#[test]
fn config_is_required_and_invalid_arguments_exit_before_loading_it() {
    for args in [
        vec![],
        vec!["--config"],
        vec!["--config", "a.yml", "--config", "b.yml"],
        vec!["--config", "a.yml", "--unknown-option"],
        vec!["run", "--config", "a.yml"],
        vec!["unknown-command"],
        vec!["--unknown-option"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pier-controller"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn config_paths_reach_the_loader_without_utf8_or_whitespace_loss() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        OsStr::new("config with spaces.yml"),
        OsStr::from_bytes(b"config-\xff.yml"),
    ] {
        let path = root.path().join(name);
        std::fs::write(&path, "[").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_pier-controller"))
            .arg("--config")
            .arg(path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid controller config"));
    }
}
