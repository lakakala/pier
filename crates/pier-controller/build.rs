use std::{env, fs, path::Path};
fn visit(root: &Path, directory: &Path, entries: &mut Vec<String>) {
    let mut files: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for file in files {
        if file.is_dir() {
            visit(root, &file, entries);
        } else {
            let route = format!("/{}", file.strip_prefix(root).unwrap().to_str().unwrap());
            entries.push(format!(
                "{route:?} => Some(include_bytes!({:?})),",
                file.to_str().unwrap()
            ));
        }
    }
}
fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("web/dist");
    assert!(
        root.join("index.html").exists(),
        "Missing controller web assets; run npm ci && npm run build in crates/pier-controller/web"
    );
    let mut entries = Vec::new();
    visit(&root, &root, &mut entries);
    let code = format!(
        "fn asset(path: &str) -> Option<&'static [u8]> {{ match path {{ {} _ => None }} }}",
        entries.join("\n")
    );
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("web_assets.rs"),
        code,
    )
    .unwrap();
}
