//! Opt-in real Docker/QEMU tests. Build the supplied images first; see README.
use pier_pkg::{Architecture, PackOptions, Stage, pack};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn run(command: &mut Command) -> String {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).trim().into()
}
fn repository(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"),"fn main() { println!(\"{}\", std::fs::read_to_string(\"configs/message.txt\").unwrap().trim()); }\n").unwrap();
    fs::write(root.join("go.mod"), "module fixture\n\ngo 1.24\n").unwrap();
    fs::write(root.join("main.go"),"package main\nimport (\"fmt\"; \"os\"; \"strings\")\nfunc main() { b,e:=os.ReadFile(\"configs/message.txt\"); if e!=nil {panic(e)}; fmt.Println(strings.TrimSpace(string(b))) }\n").unwrap();
    run(Command::new("git")
        .current_dir(root)
        .args(["init", "-q", "-b", "main"]));
    run(Command::new("git").current_dir(root).args(["add", "."]));
    run(Command::new("git").current_dir(root).args([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]));
}
fn recipe(root: &Path, repo: &Path, lang: &str, commands: &[&str]) {
    fs::create_dir_all(root.join("configs")).unwrap();
    fs::write(
        root.join("configs/message.txt"),
        "{{ MESSAGE }} {{ PIER_ARCH }}\n",
    )
    .unwrap();
    let yaml = serde_json::json!({
        "schema":2,"name":"fixture","version":"1.0.0",
        "variables":{"MESSAGE":{}},
        "source":{"type":"git","repo":repo.to_str().unwrap(),"ref":"main"},
        "build":{"language":lang,"commands":commands,"env":{"SELECTED_ARCH":"{{ PIER_ARCH }}"}},
        "files":[{"from":"fixture-{{ PIER_ARCH }}","to":"bin/fixture","executable":true}],
        "service":{"command":["bin/fixture"]}
    });
    fs::write(
        root.join("pier-pkg.yml"),
        serde_yaml_ng::to_string(&yaml).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "requires Docker, the builder images, and QEMU/binfmt for non-native targets"]
fn source_build_and_run_matrix() {
    let workspace = tempfile::tempdir().unwrap();
    let repo = workspace.path().join("repo");
    repository(&repo);
    let targets = std::env::var("PIER_TEST_TARGETS")
        .unwrap_or("almalinux8/amd64,almalinux8/arm64,ubuntu24.04/amd64,ubuntu24.04/arm64".into());
    let languages = std::env::var("PIER_TEST_LANGUAGES").unwrap_or("rust,go".into());
    for lang in languages.split(',') {
        // One recipe is reused unchanged across both architectures and both builder images.
        let root = workspace.path().join(lang);
        let commands = if lang == "rust" {
            vec![
                "test \"$SELECTED_ARCH\" = \"$PIER_ARCH\"",
                "cargo build --release --locked",
                "cp target/$PIER_RUST_TARGET/release/fixture /output/fixture-{{ PIER_ARCH }}",
            ]
        } else {
            vec![
                "test \"$SELECTED_ARCH\" = \"$PIER_ARCH\"",
                "go build -o /output/fixture-{{ PIER_ARCH }} .",
            ]
        };
        recipe(&root, &repo, lang, &commands);
        for target in targets.split(',') {
            let (distro, arch) = target.split_once('/').unwrap();
            let options = PackOptions {
                output_dir: root.join("dist").join(distro),
                image: Some(format!("pier-builder-{lang}:{distro}")),
                variables: BTreeMap::from([(
                    "MESSAGE".into(),
                    "hello from rendered config".into(),
                )]),
                ..PackOptions::new(arch.parse().unwrap())
            };
            let report = pack(&root, &options).unwrap();
            let unpacked = root.join(format!("unpacked-{distro}-{arch}"));
            tar::Archive::new(flate2::read::GzDecoder::new(
                fs::File::open(&report.path).unwrap(),
            ))
            .unpack(&unpacked)
            .unwrap();
            let runtime = if distro == "almalinux8" {
                "almalinux:8.10"
            } else {
                "ubuntu:24.04"
            };
            let output = run(Command::new("docker").args([
                "run",
                "--rm",
                "--pull=never",
                "--platform",
                &format!("linux/{arch}"),
                "--volume",
                &format!("{}:/package:ro", unpacked.display()),
                "--workdir",
                "/package",
                runtime,
                "/package/bin/fixture",
            ]));
            assert_eq!(output, format!("hello from rendered config {arch}"));
            let manifest: serde_yaml_ng::Value = serde_yaml_ng::from_str(
                &fs::read_to_string(unpacked.join("manifest.yml")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["image"].as_str(), options.image.as_deref());
            assert_eq!(manifest["architecture"].as_str(), Some(arch));
            assert_eq!(manifest["os"].as_str(), Some("linux"));
            assert!(manifest["source_commit"].as_str().is_some());
            assert!(manifest.get("target").is_none());
            if lang == "rust" {
                let elf = &manifest["files"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .find(|file| file["path"].as_str() == Some("bin/fixture"))
                    .unwrap()["elf"];
                assert!(!elf["required_glibc"].as_sequence().unwrap().is_empty());
            }
            eprintln!("verified {lang} {target}: {}", report.sha256);
        }
    }
}

#[test]
#[ignore = "requires pier-builder-rust:ubuntu24.04 for linux/amd64"]
fn failed_build_returns_no_archive() {
    let workspace = tempfile::tempdir().unwrap();
    let repo = workspace.path().join("repo");
    repository(&repo);
    let root = workspace.path().join("recipe");
    recipe(
        &root,
        &repo,
        "rust",
        &["echo intentional-build-failure >&2", "exit 42"],
    );
    let options = PackOptions {
        output_dir: root.join("dist"),
        variables: BTreeMap::from([("MESSAGE".into(), "test".into())]),
        image: Some("pier-builder-rust:ubuntu24.04".into()),
        ..PackOptions::new(Architecture::Amd64)
    };
    let error = pack(&root, &options).unwrap_err();
    assert_eq!(error.stage, Stage::Build);
    assert!(error.message.contains("42"), "{error}");
    assert_eq!(error.architecture, Some(Architecture::Amd64));
    assert!(!options.output_dir.exists());
}
