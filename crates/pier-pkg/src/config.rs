use crate::{Error, PackOptions, Result, Stage, types::IoResult};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};
use std::{
    collections::BTreeMap,
    fmt,
    marker::PhantomData,
    path::{Path, PathBuf},
};

pub(crate) fn unique_map<'de, D, K, V>(
    deserializer: D,
) -> std::result::Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct MapVisitor<K, V>(PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> Visitor<'de> for MapVisitor<K, V> {
        type Value = BTreeMap<K, V>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a mapping without duplicate keys")
        }
        fn visit_map<M: MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some((k, v)) = map.next_entry()? {
                if values.insert(k, v).is_some() {
                    return Err(de::Error::custom("duplicate mapping key"));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(MapVisitor(PhantomData))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipe {
    pub schema: u32,
    pub name: String,
    pub version: String,
    #[serde(default, deserialize_with = "unique_map")]
    pub variables: BTreeMap<String, Variable>,
    #[serde(default)]
    pub proxy: ProxySwitch,
    pub source: Source,
    pub build: Option<Build>,
    pub files: Vec<FileMapping>,
    pub service: Service,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Variable {
    pub default: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProxySwitch {
    #[serde(default)]
    pub enabled: bool,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Source {
    Git {
        repo: String,
        r#ref: String,
    },
    Binary {
        url: String,
        format: DownloadFormat,
        sha256: Option<String>,
    },
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Language {
    Rust,
    Go,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Build {
    pub language: Language,
    pub commands: Vec<String>,
    #[serde(default, deserialize_with = "unique_map")]
    pub env: BTreeMap<String, String>,
    #[serde(default = "dot")]
    pub workdir: PathBuf,
}
fn dot() -> PathBuf {
    ".".into()
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) enum DownloadFormat {
    #[serde(rename = "raw")]
    Raw,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "zip")]
    Zip,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FileBase {
    #[default]
    Artifact,
    Recipe,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileMapping {
    #[serde(default)]
    pub base: FileBase,
    pub from: PathBuf,
    pub to: PathBuf,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub command: Vec<String>,
    #[serde(default, deserialize_with = "unique_map")]
    pub env: BTreeMap<String, String>,
    #[serde(default = "dot")]
    pub working_dir: PathBuf,
}

pub(crate) fn load(
    root: &Path,
    options: &PackOptions,
) -> Result<(Recipe, BTreeMap<String, String>)> {
    let mut recipe = read(root)?;
    let path = root.join("pier-pkg.yml");
    let variables = recipe.variables(options)?;
    crate::templates::render_recipe(&mut recipe, &variables, &path)?;
    recipe.check(options)?;
    Ok((recipe, variables))
}

pub(crate) fn read(root: &Path) -> Result<Recipe> {
    let path = crate::files::resolve(root, Path::new("pier-pkg.yml"), Stage::Configuration)?;
    let bytes = std::fs::read(&path).context(Stage::Configuration, &path)?;
    // Read the version first so valid schema-1 recipes get a useful migration error
    // before schema-2 field validation rejects the old targets structure.
    #[derive(Deserialize)]
    struct Version {
        schema: u32,
    }
    let version: Version = serde_yaml_ng::from_slice(&bytes)
        .map_err(|e| Error::new(Stage::Configuration, e.to_string()).at(&path))?;
    if version.schema != 2 {
        let message = if version.schema == 1 {
            "schema 1 is no longer supported; migrate to schema 2: remove targets, move files to the recipe root and download fields to source, and pass architecture/image through PackOptions"
        } else {
            "only schema 2 is supported"
        };
        return Err(Error::new(Stage::Configuration, message).at(&path));
    }
    let recipe: Recipe = serde_yaml_ng::from_slice(&bytes)
        .map_err(|e| Error::new(Stage::Configuration, e.to_string()).at(&path))?;
    Ok(recipe)
}

fn valid_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}
pub(crate) fn valid_env(s: &str) -> bool {
    let mut chars = s.bytes();
    matches!(chars.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && chars.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Recipe {
    fn check(&self, options: &PackOptions) -> Result<()> {
        let bad = |s| Error::new(Stage::Configuration, s);
        if self.schema != 2 {
            return Err(bad("only schema 2 is supported"));
        }
        if !valid_component(&self.name) || !valid_component(&self.version) {
            return Err(bad(
                "name and version must be nonempty safe filename components",
            ));
        }
        for name in self.variables.keys() {
            if !valid_env(name) {
                return Err(bad("invalid variable name"));
            }
        }
        if self.service.command.is_empty() || self.service.command.iter().any(|s| s.contains('\0'))
        {
            return Err(bad(
                "service.command requires an executable and valid arguments",
            ));
        }
        crate::files::relative(Path::new(&self.service.command[0]), false)?;
        crate::files::relative(&self.service.working_dir, true)?;
        check_env(&self.service.env)?;
        match &self.source {
            Source::Git { repo, r#ref } => {
                if options.image.as_ref().is_none_or(|image| {
                    image.is_empty()
                        || image.starts_with('-')
                        || image.contains(char::is_whitespace)
                        || image.contains('\0')
                }) {
                    return Err(bad("git source requires a valid image in PackOptions"));
                }
                if repo.is_empty()
                    || repo.starts_with('-')
                    || repo.contains(['\0', '\n'])
                    || r#ref.is_empty()
                    || r#ref.starts_with('-')
                    || r#ref.contains(['\0', '\n'])
                {
                    return Err(bad(
                        "git repo and ref must be nonempty and cannot start with '-'",
                    ));
                }
                let b = self
                    .build
                    .as_ref()
                    .ok_or_else(|| bad("git source requires build"))?;
                if b.commands.is_empty()
                    || b.commands
                        .iter()
                        .any(|c| c.trim().is_empty() || c.contains('\0'))
                {
                    return Err(bad("build.commands must contain nonempty commands"));
                }
                crate::files::relative(&b.workdir, true)?;
                check_env(&b.env)?;
                for key in b.env.keys() {
                    if crate::proxy::is_proxy_key(key)
                        || [
                            "CARGO_BUILD_TARGET",
                            "GOOS",
                            "GOARCH",
                            "PIER_TARGET",
                            "PIER_ARCH",
                            "PIER_RUST_TARGET",
                            "PIER_OUTPUT",
                        ]
                        .contains(&key.as_str())
                    {
                        return Err(Error::new(
                            Stage::Configuration,
                            format!("reserved build environment key: {key}"),
                        ));
                    }
                }
            }
            Source::Binary { url, sha256, .. } => {
                if self.build.is_some() || options.image.is_some() {
                    return Err(bad(
                        "binary source cannot have build settings or a PackOptions image",
                    ));
                }
                crate::proxy::http_url(url)
                    .map_err(|_| bad("binary URL must be a valid HTTP(S) URL"))?;
                if sha256
                    .as_ref()
                    .is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    return Err(bad("sha256 must contain 64 hex digits"));
                }
            }
        }
        if self.files.is_empty() {
            return Err(bad("files must contain at least one mapping"));
        }
        let mut destinations = Vec::new();
        for file in &self.files {
            crate::files::relative(&file.from, true)?;
            let to = crate::files::relative(&file.to, false)?;
            if to.starts_with("configs") || to.starts_with("manifest.yml") {
                return Err(bad("configs/ and manifest.yml are reserved package paths"));
            }
            if destinations
                .iter()
                .any(|p: &PathBuf| to.starts_with(p) || p.starts_with(&to))
            {
                return Err(bad("overlapping file mapping destinations"));
            }
            destinations.push(to);
        }
        Ok(())
    }

    pub fn variables(&self, options: &PackOptions) -> Result<BTreeMap<String, String>> {
        if self.variables.contains_key("PIER_ARCH") || options.variables.contains_key("PIER_ARCH") {
            return Err(Error::new(
                Stage::Variables,
                "PIER_ARCH is a read-only built-in variable; select it through PackOptions.architecture",
            ));
        }
        let unknown: Vec<_> = options
            .variables
            .keys()
            .filter(|k| !self.variables.contains_key(*k))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(Error::new(
                Stage::Variables,
                format!("undeclared variables: {}", unknown.join(", ")),
            ));
        }
        let missing: Vec<_> = self
            .variables
            .iter()
            .filter(|(k, v)| v.default.is_none() && !options.variables.contains_key(*k))
            .map(|(k, _)| k.clone())
            .collect();
        if !missing.is_empty() {
            return Err(Error::new(
                Stage::Variables,
                format!("missing required variables: {}", missing.join(", ")),
            ));
        }
        let mut variables: BTreeMap<String, String> = self
            .variables
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    options
                        .variables
                        .get(k)
                        .or(v.default.as_ref())
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
        variables.insert("PIER_ARCH".into(), options.architecture.to_string());
        Ok(variables)
    }
}
fn check_env(env: &BTreeMap<String, String>) -> Result<()> {
    if env.iter().any(|(k, v)| !valid_env(k) || v.contains('\0')) {
        return Err(Error::new(
            Stage::Configuration,
            "invalid environment name or value",
        ));
    }
    Ok(())
}
