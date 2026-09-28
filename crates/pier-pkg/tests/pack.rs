use pier_pkg::{Architecture, PackOptions, ProxyOptions, Stage, pack, validate};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(routes: BTreeMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut bytes = Vec::new();
                        let mut byte = [0; 1];
                        while bytes.len() < 65536 && !bytes.ends_with(b"\r\n\r\n") {
                            if stream.read(&mut byte).unwrap_or(0) == 0 {
                                break;
                            }
                            bytes.push(byte[0]);
                        }
                        let request = String::from_utf8_lossy(&bytes).to_string();
                        let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                        seen.lock().unwrap().push(request);
                        let (status, body) = routes
                            .get(&path)
                            .map_or((404, b"missing".as_slice()), |v| (200, v.as_slice()));
                        let header = format!(
                            "HTTP/1.1 {status} response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn binary() -> Self {
        Self::new(BTreeMap::from([(
            "/app".into(),
            b"#!/bin/sh\nprintf 'ok\\n'\n".to_vec(),
        )]))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn recipe(root: &Path, url: &str, format: &str, from: &str) {
    fs::write(root.join("pier-pkg.yml"), format!(
        "schema: 2\nname: demo\nversion: '1.0.0'\nsource:\n  type: binary\n  url: '{url}'\n  format: {format}\nfiles:\n  - from: {from}\n    to: bin/demo\n    executable: true\nservice:\n  command: [bin/demo]\n"
    )).unwrap();
}
fn update(root: &Path, old: &str, new: &str) {
    let p = root.join("pier-pkg.yml");
    let data = fs::read_to_string(&p).unwrap();
    fs::write(p, data.replace(old, new)).unwrap();
}
fn options(root: &Path) -> PackOptions {
    PackOptions {
        output_dir: root.join("dist"),
        ..PackOptions::new(Architecture::Amd64)
    }
}
fn unpack(path: &Path, dest: &Path) {
    tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(path).unwrap()))
        .unpack(dest)
        .unwrap();
}

#[test]
fn raw_pack_templates_metadata_and_reproducibility() {
    let server = Server::binary();
    let root = tempfile::tempdir().unwrap();
    recipe(
        root.path(),
        &format!("{}/app", server.url),
        "raw",
        "download",
    );
    update(
        root.path(),
        "source:",
        "variables:\n  HOST: {}\n  PORT: {default: '8080'}\nsource:",
    );
    fs::create_dir_all(root.path().join("configs/nested")).unwrap();
    fs::write(root.path().join("configs/nested/app.toml"),"host={{ HOST | tojson }}\nport={{ PORT }}\n{% if HOST %}enabled=true\n{% endif %}{% for x in [1,2] %}# {{ x }}\n{% endfor %}").unwrap();
    let mut opts = options(root.path());
    opts.variables.insert("HOST".into(), "db.internal".into());
    let report = validate(root.path(), &opts).unwrap();
    assert_eq!(report.configuration_files.len(), 1);
    assert!(server.requests.lock().unwrap().is_empty());
    let first = pack(root.path(), &opts).unwrap();
    let out = &first.path;
    assert!(out.ends_with("demo-1.0.0-linux-amd64.tar.gz"));
    let dest = root.path().join("unpacked");
    unpack(out, &dest);
    assert_eq!(
        fs::read_to_string(dest.join("configs/nested/app.toml")).unwrap(),
        "host=\"db.internal\"\nport=8080\nenabled=true\n# 1\n# 2\n"
    );
    let manifest: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(dest.join("manifest.yml")).unwrap()).unwrap();
    assert_eq!(manifest["schema"].as_u64(), Some(2));
    assert_eq!(manifest["os"].as_str(), Some("linux"));
    assert_eq!(manifest["architecture"].as_str(), Some("amd64"));
    assert!(manifest.get("target").is_none());
    assert!(manifest.get("image").is_none());
    assert_eq!(first.architecture, Architecture::Amd64);
    assert_eq!(report.package.path, first.path);
    assert_eq!(report.package.architecture, first.architecture);
    assert!(manifest.get("variables").is_none());
    assert!(manifest.get("proxy").is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dest.join("bin/demo"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    assert_eq!(
        pack(root.path(), &opts).unwrap_err().stage,
        Stage::Configuration
    );
    opts.overwrite = true;
    let again = pack(root.path(), &opts).unwrap();
    assert_eq!(first.sha256, again.sha256);
}

#[test]
fn inspect_declarations_and_verified_unpack_are_usable_by_services() {
    let server = Server::binary();
    let root = tempfile::tempdir().unwrap();
    recipe(
        root.path(),
        &format!("{}/app", server.url),
        "raw",
        "download",
    );
    update(
        root.path(),
        "source:",
        "variables:\n  REQUIRED: {}\n  OPTIONAL: {default: 'value'}\nsource:",
    );
    let metadata = pier_pkg::inspect(root.path()).unwrap();
    assert_eq!(metadata.source, pier_pkg::SourceKind::Binary);
    assert!(metadata.variables["REQUIRED"].default.is_none());
    assert_eq!(
        metadata.variables["OPTIONAL"].default.as_deref(),
        Some("value")
    );
    assert!(server.requests.lock().unwrap().is_empty());
    let mut opts = options(root.path());
    opts.variables.insert("REQUIRED".into(), "secret".into());
    let artifact = pack(root.path(), &opts).unwrap();
    let dest = root.path().join("verified");
    let manifest =
        pier_pkg::unpack(&artifact.path, &dest, &artifact.sha256, Architecture::Amd64).unwrap();
    assert_eq!(manifest.service.command, ["bin/demo"]);
    assert!(dest.join("bin/demo").is_file());
    assert!(
        pier_pkg::unpack(&artifact.path, &dest, &artifact.sha256, Architecture::Amd64).is_err()
    );
    let bad = root.path().join("bad");
    assert!(pier_pkg::unpack(&artifact.path, &bad, &"0".repeat(64), Architecture::Amd64).is_err());
    assert!(!bad.exists());
    assert!(pier_pkg::unpack(&artifact.path, &bad, &artifact.sha256, Architecture::Arm64).is_err());
    assert!(!bad.exists());
    // Re-archive an injected file with an otherwise valid manifest and outer hash.
    fs::write(dest.join("injected"), "not declared").unwrap();
    let poisoned = root.path().join("poisoned.tar.gz");
    let gzip = flate2::write::GzEncoder::new(
        fs::File::create(&poisoned).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(gzip);
    for name in ["manifest.yml", "bin/demo", "injected"] {
        let data = fs::read(dest.join(name)).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(if name == "bin/demo" { 0o755 } else { 0o644 });
        header.set_size(data.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, name, data.as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(fs::read(&poisoned).unwrap()));
    assert!(
        pier_pkg::unpack(&poisoned, &bad, &digest, Architecture::Amd64)
            .unwrap_err()
            .message
            .contains("inventory")
    );
    assert!(!bad.exists());
}

#[test]
fn verified_unpack_rejects_links_duplicate_entries_and_traversal() {
    use sha2::{Digest, Sha256};
    for kind in ["symlink", "hardlink", "duplicate", "traversal", "device"] {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("bad.tar.gz");
        let gzip = flate2::write::GzEncoder::new(
            fs::File::create(&package).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_path("entry").unwrap();
        header.set_mode(0o644);
        header.set_size(0);
        match kind {
            "symlink" => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name("../outside").unwrap();
            }
            "hardlink" => {
                header.set_entry_type(tar::EntryType::Link);
                header.set_link_name("../outside").unwrap();
            }
            "traversal" => {
                header.as_mut_bytes()[..100].fill(0);
                header.as_mut_bytes()[..10].copy_from_slice(b"../outside");
            }
            "device" => header.set_entry_type(tar::EntryType::Char),
            _ => (),
        }
        header.set_cksum();
        tar.append(&header, std::io::empty()).unwrap();
        if kind == "duplicate" {
            tar.append(&header, std::io::empty()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let hash = format!("{:x}", Sha256::digest(fs::read(&package).unwrap()));
        let destination = root.path().join("destination");
        assert!(
            pier_pkg::unpack(&package, &destination, &hash, Architecture::Amd64).is_err(),
            "{kind}"
        );
        assert!(!destination.exists());
        assert!(!root.path().join("outside").exists());
    }
}

#[test]
fn required_default_empty_and_unknown_variables() {
    let root = tempfile::tempdir().unwrap();
    recipe(root.path(), "http://unused.invalid/app", "raw", "download");
    update(
        root.path(),
        "source:",
        "variables:\n  B: {}\n  A: {}\n  PORT: {default: '8080'}\nsource:",
    );
    update(
        root.path(),
        "command: [bin/demo]",
        "command: [bin/demo, '{{ A }}']",
    );
    fs::create_dir(root.path().join("configs")).unwrap();
    fs::write(root.path().join("configs/app"), "{{ A }}{{ B }}{{ PORT }}").unwrap();
    let mut opts = options(root.path());
    let err = validate(root.path(), &opts).unwrap_err();
    assert_eq!(err.stage, Stage::Variables);
    assert!(err.message.contains("A, B"));
    opts.variables.insert("A".into(), String::new());
    opts.variables.insert("B".into(), "{{ untouched }}".into());
    opts.variables.insert("PORT".into(), "9090".into());
    validate(root.path(), &opts).unwrap();
    opts.variables.insert("UNKNOWN".into(), "secret".into());
    assert_eq!(
        validate(root.path(), &opts).unwrap_err().stage,
        Stage::Variables
    );
}

#[test]
fn yaml_variables_render_values_without_changing_structure_or_evaluating_twice() {
    let server = Server::binary();
    let root = tempfile::tempdir().unwrap();
    recipe(root.path(), "{{ BASE }}/app", "raw", "download");
    update(root.path(), "version: '1.0.0'", "version: '{{ VERSION }}'");
    update(
        root.path(),
        "source:",
        "variables:\n  BASE: {}\n  ARG: {}\n  VERSION: {default: '2.0.0'}\nsource:",
    );
    update(
        root.path(),
        "command: [bin/demo]",
        "command: [bin/demo, '{{ ARG }}']\n  env:\n    MESSAGE: '{{ ARG }}'",
    );
    fs::create_dir(root.path().join("configs")).unwrap();
    fs::write(root.path().join("configs/value"), "{{ ARG }}").unwrap();
    let arg = "quote: \"\nproxy: {enabled: true}\n{{ NEVER_EVALUATE }}";
    let mut opts = options(root.path());
    opts.variables.insert("BASE".into(), server.url.clone());
    opts.variables.insert("ARG".into(), arg.into());
    let report = pack(root.path(), &opts).unwrap();
    assert!(report.path.ends_with("demo-2.0.0-linux-amd64.tar.gz"));
    let unpacked = root.path().join("unpacked");
    unpack(&report.path, &unpacked);
    let manifest: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(unpacked.join("manifest.yml")).unwrap())
            .unwrap();
    assert_eq!(manifest["service"]["command"][1].as_str(), Some(arg));
    assert_eq!(manifest["service"]["env"]["MESSAGE"].as_str(), Some(arg));
    assert_eq!(
        fs::read_to_string(unpacked.join("configs/value")).unwrap(),
        arg
    );
    opts.output_dir = root.path().join("other-dist");
    update(root.path(), "{{ BASE }}/app", "{{ UNKNOWN }}/app");
    assert_eq!(
        validate(root.path(), &opts).unwrap_err().stage,
        Stage::Template
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[test]
fn template_failures_and_duplicate_keys_are_preflight_errors() {
    let root = tempfile::tempdir().unwrap();
    recipe(root.path(), "http://unused.invalid/app", "raw", "download");
    fs::create_dir(root.path().join("configs")).unwrap();
    let p = root.path().join("configs/app");
    for bytes in [b"{{ UNDEFINED }}".as_slice(), b"{% broken %}", b"\xff\xfe"] {
        fs::write(&p, bytes).unwrap();
        assert_eq!(
            validate(root.path(), &options(root.path()))
                .unwrap_err()
                .stage,
            Stage::Template
        );
    }
    fs::write(&p, "unchanged\n").unwrap();
    update(
        root.path(),
        "source:",
        "variables:\n  PORT: {}\n  PORT: {}\nsource:",
    );
    assert_eq!(
        validate(root.path(), &options(root.path()))
            .unwrap_err()
            .stage,
        Stage::Configuration
    );
}

#[test]
fn proxy_enable_disable_bypass_and_isolation() {
    let direct = Server::binary();
    let proxy = Server::new(BTreeMap::from([(
        "http://only-via-proxy.invalid/app".into(),
        b"#!/bin/sh\nexit 0\n".to_vec(),
    )]));
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    recipe(
        a.path(),
        "http://only-via-proxy.invalid/app",
        "raw",
        "download",
    );
    update(a.path(), "source:", "proxy: {enabled: true}\nsource:");
    recipe(b.path(), &format!("{}/app", direct.url), "raw", "download");
    let mut proxied = options(a.path());
    assert_eq!(
        validate(a.path(), &proxied).unwrap_err().stage,
        Stage::Proxy
    );
    proxied.proxy.http_proxy = Some(proxy.url.clone());
    let mut unproxied = options(b.path());
    unproxied.proxy.http_proxy = Some("not even a URL".into());
    let before: BTreeMap<_, _> = std::env::vars_os().collect();
    thread::scope(|scope| {
        let one = scope.spawn(|| pack(a.path(), &proxied));
        let two = scope.spawn(|| pack(b.path(), &unproxied));
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
    });
    assert!(before == std::env::vars_os().collect());
    assert_eq!(proxy.requests.lock().unwrap().len(), 1);
    assert_eq!(direct.requests.lock().unwrap().len(), 1);
    update(b.path(), "source:", "proxy: {enabled: true}\nsource:");
    unproxied.overwrite = true;
    unproxied.proxy = ProxyOptions {
        http_proxy: Some(proxy.url.clone()),
        no_proxy: Some("127.0.0.1".into()),
        ..Default::default()
    };
    pack(b.path(), &unproxied).unwrap();
    assert_eq!(proxy.requests.lock().unwrap().len(), 1);
    assert_eq!(direct.requests.lock().unwrap().len(), 2);
}

#[test]
fn checksum_and_http_failures_do_not_publish_or_replace_archives() {
    let server = Server::binary();
    let root = tempfile::tempdir().unwrap();
    recipe(
        root.path(),
        &format!("{}/app", server.url),
        "raw",
        "download",
    );
    update(
        root.path(),
        "  format: raw",
        &format!("  sha256: '{}'\n  format: raw", "0".repeat(64)),
    );
    let mut opts = options(root.path());
    let error = pack(root.path(), &opts).unwrap_err();
    assert_eq!(error.stage, Stage::Download);
    assert_eq!(error.architecture, Some(Architecture::Amd64));
    assert!(!opts.output_dir.exists());
    recipe(
        root.path(),
        &format!("{}/app", server.url),
        "raw",
        "download",
    );
    let artifact = pack(root.path(), &opts).unwrap();
    let original = fs::read(&artifact.path).unwrap();
    opts.overwrite = true;
    update(
        root.path(),
        &format!("{}/app", server.url),
        &format!("{}/missing", server.url),
    );
    assert_eq!(pack(root.path(), &opts).unwrap_err().stage, Stage::Download);
    assert_eq!(fs::read(&artifact.path).unwrap(), original);
    assert_eq!(fs::read_dir(&opts.output_dir).unwrap().count(), 1);
}

fn tar_bytes(name: &str, kind: tar::EntryType) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let content = b"#!/bin/sh\nexit 0\n";
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(0o755);
    header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
    if kind.is_symlink() {
        header.set_size(0);
        header.set_link_name("/etc/passwd").unwrap();
    } else {
        header.set_size(content.len() as u64);
    }
    header.set_cksum();
    builder
        .append(
            &header,
            if kind.is_symlink() {
                b"".as_slice()
            } else {
                content.as_slice()
            },
        )
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}
fn zip_bytes(name: &str, symlink: bool) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    if symlink {
        z.add_symlink(name, "/etc/passwd", opts).unwrap();
    } else {
        z.start_file(name, opts).unwrap();
        z.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
    }
    z.finish().unwrap().into_inner()
}

#[test]
fn archives_extract_and_reject_traversal_and_links() {
    for (format, bytes, success) in [
        (
            "tar.gz",
            tar_bytes("release/demo", tar::EntryType::Regular),
            true,
        ),
        ("zip", zip_bytes("release/demo", false), true),
        (
            "tar.gz",
            tar_bytes("../escape", tar::EntryType::Regular),
            false,
        ),
        (
            "tar.gz",
            tar_bytes("release/demo", tar::EntryType::Symlink),
            false,
        ),
        ("zip", zip_bytes("../escape", false), false),
        ("zip", zip_bytes("release/demo", true), false),
    ] {
        let server = Server::new(BTreeMap::from([("/app".into(), bytes)]));
        let root = tempfile::tempdir().unwrap();
        recipe(
            root.path(),
            &format!("{}/app", server.url),
            format,
            "release/demo",
        );
        let result = pack(root.path(), &options(root.path()));
        assert_eq!(result.is_ok(), success, "{format}: {result:?}");
    }
}

#[test]
fn file_mapping_and_source_rules() {
    let root = tempfile::tempdir().unwrap();
    recipe(root.path(), "http://unused.invalid/app", "raw", "download");
    for destination in ["../escape", "/absolute", "configs/app", "manifest.yml"] {
        recipe(root.path(), "http://unused.invalid/app", "raw", "download");
        update(root.path(), "to: bin/demo", &format!("to: {destination}"));
        assert!(validate(root.path(), &options(root.path())).is_err());
    }
    recipe(root.path(), "http://unused.invalid/app", "raw", "download");
    update(
        root.path(),
        "source:\n  type: binary",
        "source:\n  type: binary\n  repo: unexpected",
    );
    assert_eq!(
        validate(root.path(), &options(root.path()))
            .unwrap_err()
            .stage,
        Stage::Configuration
    );
}

#[test]
fn rejects_wrong_elf_architecture() {
    let mut elf = vec![0u8; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&183u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    let server = Server::new(BTreeMap::from([("/app".into(), elf)]));
    let root = tempfile::tempdir().unwrap();
    recipe(
        root.path(),
        &format!("{}/app", server.url),
        "raw",
        "download",
    );
    assert_eq!(
        pack(root.path(), &options(root.path())).unwrap_err().stage,
        Stage::Compatibility
    );
}

#[test]
fn documented_recipes_validate_without_network_or_docker() {
    let output = tempfile::tempdir().unwrap();
    let mut opts = options(output.path());
    opts.variables
        .insert("DB_HOST".into(), "db.internal".into());
    for recipe in ["rust/1.0.0", "rust/1.1.0", "go/1.0.0", "binary/1.0.0"] {
        opts.image = if recipe.starts_with("binary/") {
            None
        } else {
            Some("custom-builder:latest".into())
        };
        let report = validate(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/recipes")
                .join(recipe),
            &opts,
        )
        .unwrap();
        assert_eq!(report.package.architecture, Architecture::Amd64);
        assert_eq!(report.version, recipe.split_once('/').unwrap().1);
    }
    assert!(!opts.output_dir.exists());
}

#[test]
fn one_recipe_selects_url_checksum_paths_and_config_from_call_architecture() {
    use sha2::{Digest, Sha256};
    let amd64 = tar_bytes("release-amd64/demo", tar::EntryType::Regular);
    let arm64 = tar_bytes("release-arm64/demo", tar::EntryType::Regular);
    let amd64_sha = format!("{:x}", Sha256::digest(&amd64));
    let arm64_sha = format!("{:x}", Sha256::digest(&arm64));
    let server = Server::new(BTreeMap::from([
        ("/amd64".into(), amd64),
        ("/arm64".into(), arm64),
    ]));
    let root = tempfile::tempdir().unwrap();
    recipe(
        root.path(),
        &format!("{}/{{{{ PIER_ARCH }}}}", server.url),
        "tar.gz",
        "release-{{ PIER_ARCH }}/demo",
    );
    update(
        root.path(),
        "source:",
        &format!(
            "variables:\n  SHA_AMD64: {{default: '{amd64_sha}'}}\n  SHA_ARM64: {{default: '{arm64_sha}'}}\nsource:"
        ),
    );
    update(
        root.path(),
        "  format: tar.gz",
        "  format: tar.gz\n  sha256: \"{{ SHA_AMD64 if PIER_ARCH == 'amd64' else SHA_ARM64 }}\"",
    );
    update(root.path(), "bin/demo", "bin/{{ PIER_ARCH }}/demo");
    // A scalar starting with a template must be quoted in a YAML flow sequence.
    update(
        root.path(),
        "command: [bin/{{ PIER_ARCH }}/demo]",
        "command: ['bin/{{ PIER_ARCH }}/demo']",
    );
    fs::create_dir(root.path().join("configs")).unwrap();
    fs::write(root.path().join("configs/arch.txt"), "{{ PIER_ARCH }}\n").unwrap();
    let original = fs::read(root.path().join("pier-pkg.yml")).unwrap();
    for architecture in [Architecture::Amd64, Architecture::Arm64] {
        let opts = PackOptions {
            output_dir: root.path().join("dist"),
            ..PackOptions::new(architecture)
        };
        let report = validate(root.path(), &opts).unwrap();
        let artifact = pack(root.path(), &opts).unwrap();
        assert_eq!(artifact.path, report.package.path);
        assert_eq!(artifact.architecture, architecture);
        assert!(
            artifact
                .path
                .ends_with(format!("demo-1.0.0-linux-{architecture}.tar.gz"))
        );
        let unpacked = root.path().join(architecture.as_str());
        unpack(&artifact.path, &unpacked);
        assert!(unpacked.join(format!("bin/{architecture}/demo")).is_file());
        assert_eq!(
            fs::read_to_string(unpacked.join("configs/arch.txt")).unwrap(),
            format!("{architecture}\n")
        );
        let manifest: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&fs::read_to_string(unpacked.join("manifest.yml")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["architecture"].as_str(),
            Some(architecture.as_str())
        );
    }
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /amd64 "));
    assert!(requests[1].starts_with("GET /arm64 "));
    assert_eq!(fs::read_dir(root.path().join("dist")).unwrap().count(), 2);
    assert_eq!(
        fs::read(root.path().join("pier-pkg.yml")).unwrap(),
        original
    );
}

#[test]
fn architecture_and_image_are_caller_options_and_builtin_is_read_only() {
    assert_eq!(
        "amd64".parse::<Architecture>().unwrap(),
        Architecture::Amd64
    );
    assert_eq!(
        "arm64".parse::<Architecture>().unwrap(),
        Architecture::Arm64
    );
    for invalid in ["", "x86", "linux/amd64", "almalinux8/arm64"] {
        assert!(invalid.parse::<Architecture>().is_err());
    }
    let root = tempfile::tempdir().unwrap();
    recipe(root.path(), "http://unused.invalid/app", "raw", "download");
    let mut opts = options(root.path());
    opts.image = Some("builder:latest".into());
    assert_eq!(
        validate(root.path(), &opts).unwrap_err().stage,
        Stage::Configuration
    );
    opts.image = None;
    opts.variables.insert("PIER_ARCH".into(), "arm64".into());
    assert_eq!(
        validate(root.path(), &opts).unwrap_err().stage,
        Stage::Variables
    );
    opts.variables.clear();
    update(
        root.path(),
        "source:",
        "variables:\n  PIER_ARCH: {default: arm64}\nsource:",
    );
    assert_eq!(
        validate(root.path(), &opts).unwrap_err().stage,
        Stage::Variables
    );

    let source_recipe = "schema: 2\nname: demo\nversion: '1'\nsource: {type: git, repo: missing-repository, ref: main}\nbuild:\n  language: rust\n  commands: ['echo {{ PIER_ARCH }}']\nfiles: [{from: demo, to: bin/demo, executable: true}]\nservice: {command: [bin/demo]}\n";
    fs::write(root.path().join("pier-pkg.yml"), source_recipe).unwrap();
    for image in [
        None,
        Some(""),
        Some("--bad"),
        Some("bad image"),
        Some("bad\0image"),
    ] {
        opts.image = image.map(String::from);
        let error = pack(root.path(), &opts).unwrap_err();
        assert_eq!(error.stage, Stage::Configuration);
        assert!(error.message.contains("PackOptions"));
    }
    opts.image = Some(format!("registry.example/custom@sha256:{}", "a".repeat(64)));
    // validate is offline: the repository and custom image need not exist yet.
    validate(root.path(), &opts).unwrap();
    update(
        root.path(),
        "  language: rust",
        "  language: rust\n  env: {PIER_ARCH: arm64}",
    );
    assert_eq!(
        validate(root.path(), &opts).unwrap_err().stage,
        Stage::Configuration
    );
    assert!(!opts.output_dir.exists());
}

#[test]
fn old_schema_has_migration_error_and_schema_two_rejects_old_target_fields() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pier-pkg.yml"), "schema: 1\nname: old\nversion: '1'\nsource: {type: binary}\ntargets:\n  ubuntu24.04/amd64: {url: 'http://unused.invalid/app', format: raw, files: [{from: download, to: bin/demo, executable: true}]}\nservice: {command: [bin/demo]}\n").unwrap();
    let error = validate(root.path(), &options(root.path())).unwrap_err();
    assert_eq!(error.stage, Stage::Configuration);
    assert!(error.message.contains("migrate to schema 2"));
    assert!(error.message.contains("PackOptions"));
    for field in [
        "targets: {}",
        "image: builder:latest",
        "architecture: arm64",
    ] {
        recipe(root.path(), "http://unused.invalid/app", "raw", "download");
        update(root.path(), "source:", &format!("{field}\nsource:"));
        assert_eq!(
            validate(root.path(), &options(root.path()))
                .unwrap_err()
                .stage,
            Stage::Configuration
        );
    }
    assert!(!options(root.path()).output_dir.exists());
}

#[cfg(unix)]
#[test]
fn rejects_local_symlinks() {
    let root = tempfile::tempdir().unwrap();
    recipe(root.path(), "http://unused.invalid/app", "raw", "download");
    fs::create_dir(root.path().join("configs")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", root.path().join("configs/leak")).unwrap();
    assert_eq!(
        validate(root.path(), &options(root.path()))
            .unwrap_err()
            .stage,
        Stage::Files
    );
}
