use std::{ffi::OsStr, os::unix::ffi::OsStrExt, process::Command};

#[test]
fn help_and_version_do_not_load_configuration_or_start_initialization() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["init", "--help"],
        vec!["run", "--help"],
        vec!["run", "--config", "missing.yml", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pier-agent"))
            .current_dir(root.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("Usage:"), "{help}");
        if args == ["run", "--help"] {
            assert!(help.contains("/etc/pier/agent.yml"), "{help}");
        }
    }
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pier-agent"))
            .current_dir(root.path())
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            concat!("pier-agent ", env!("CARGO_PKG_VERSION"))
        );
    }
    assert_eq!(root.path().read_dir().unwrap().count(), 0);
}

#[test]
fn invalid_arguments_and_removed_legacy_entry_exit_before_business_logic() {
    for args in [
        vec![],
        vec!["--config", "missing.yml"],
        vec!["--config", "missing.yml", "run"],
        vec!["init", "--config", "missing.yml"],
        vec!["init", "extra"],
        vec!["run", "--config"],
        vec!["run", "--config", "a.yml", "--config", "b.yml"],
        vec!["run", "extra"],
        vec!["unknown-command"],
        vec!["--unknown-option"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pier-agent"))
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
        // A YAML error proves the file was opened, without starting a service.
        std::fs::write(&path, "[").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_pier-agent"))
            .args(["run", "--config"])
            .arg(path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("无效的 v2 配置"));
    }
}
