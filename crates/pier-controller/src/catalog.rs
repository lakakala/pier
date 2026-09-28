use anyhow::{Context, Result, bail, ensure};
use minijinja::{Environment, UndefinedBehavior};
use pier_pkg::{AppMetadata, VariableDefinition};
use pier_protocol::{Variables, relative, safe_id};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueprintApp {
    pub id: String,
    pub app: String,
    #[serde(default, deserialize_with = "pier_protocol::unique_map")]
    pub variables: Variables,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blueprint {
    pub schema: u32,
    pub name: String,
    #[serde(default, deserialize_with = "pier_protocol::unique_map")]
    pub variables: BTreeMap<String, VariableDefinition>,
    pub apps: Vec<BlueprintApp>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub commit: String,
    pub root: PathBuf,
    pub apps: BTreeMap<String, AppMetadata>,
    pub blueprints: BTreeMap<String, Blueprint>,
}

fn environment() -> Environment<'static> {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(UndefinedBehavior::Strict);
    environment
}
fn variable_name(name: &str) -> bool {
    let mut chars = name.bytes();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && name != "PIER_ARCH"
}
impl Blueprint {
    pub fn check(&self, catalog: &BTreeMap<String, AppMetadata>) -> Result<()> {
        ensure!(
            self.schema == 1 && !self.name.is_empty(),
            "invalid blueprint schema or name"
        );
        ensure!(
            self.variables.keys().all(|k| variable_name(k)),
            "invalid blueprint variable name"
        );
        let mut ids = BTreeSet::new();
        let env = environment();
        for app in &self.apps {
            ensure!(
                safe_id(&app.id) && ids.insert(&app.id),
                "invalid or duplicate app instance id"
            );
            relative(&app.app)?;
            let metadata = catalog
                .get(&app.app)
                .context("blueprint references missing app")?;
            for (name, template) in &app.variables {
                ensure!(
                    metadata.variables.contains_key(name),
                    "mapping references undeclared app variable: {name}"
                );
                let compiled = env
                    .template_from_str(template)
                    .map_err(|_| anyhow::anyhow!("invalid variable mapping template"))?;
                for variable in compiled.undeclared_variables(false) {
                    ensure!(
                        self.variables.contains_key(&variable),
                        "mapping references undeclared blueprint variable: {variable}"
                    );
                }
            }
            for (name, definition) in &metadata.variables {
                ensure!(
                    definition.default.is_some() || app.variables.contains_key(name),
                    "required app variable has no mapping: {}.{name}",
                    app.id
                );
            }
        }
        Ok(())
    }
    pub fn resolve(&self, supplied: &Variables) -> Result<BTreeMap<String, Variables>> {
        ensure!(
            supplied.keys().all(|k| self.variables.contains_key(k)),
            "unknown blueprint variable"
        );
        let mut values = Variables::new();
        for (key, declaration) in &self.variables {
            let value = supplied
                .get(key)
                .or(declaration.default.as_ref())
                .with_context(|| format!("missing required blueprint variable: {key}"))?;
            values.insert(key.clone(), value.clone());
        }
        let env = environment();
        self.apps
            .iter()
            .map(|app| {
                let mapped = app
                    .variables
                    .iter()
                    .map(|(key, template)| {
                        let value = env.render_str(template, &values).map_err(|_| {
                            anyhow::anyhow!("variable mapping failed for {}.{key}", app.id)
                        })?;
                        Ok((key.clone(), value))
                    })
                    .collect::<Result<Variables>>()?;
                Ok((app.id.clone(), mapped))
            })
            .collect()
    }
}

pub fn scan(root: &Path, commit: String) -> Result<Catalog> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "repository contains symlink");
            if kind.is_dir() {
                walk(root, &entry.path(), found)?;
            } else if entry.file_name() == "pier-pkg.yml"
                || entry.file_name() == "pier-blueprint.yml"
            {
                let mut id = dir
                    .strip_prefix(root)?
                    .to_str()
                    .context("non UTF-8 repository path")?
                    .to_string();
                if id.is_empty() {
                    id = ".".into();
                }
                relative(&id)?;
                found.push((id, entry.path()));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    let mut result = Catalog {
        root: root.to_path_buf(),
        commit,
        apps: BTreeMap::new(),
        blueprints: BTreeMap::new(),
    };
    for (id, path) in files {
        if path.file_name().is_some_and(|v| v == "pier-pkg.yml") {
            result
                .apps
                .insert(id, pier_pkg::inspect(path.parent().unwrap())?);
        } else {
            let blueprint = serde_yaml_ng::from_slice(&fs::read(&path)?)
                .with_context(|| format!("invalid blueprint: {id}"))?;
            result.blueprints.insert(id, blueprint);
        }
    }
    for (id, blueprint) in &result.blueprints {
        blueprint
            .check(&result.apps)
            .with_context(|| format!("invalid blueprint: {id}"))?;
    }
    Ok(result)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !result.status.success() {
        bail!("repository git command failed ({})", result.status);
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_string())
}
pub fn sync(url: &str, reference: &str, state_dir: &Path) -> Result<Catalog> {
    ensure!(
        !url.is_empty() && !url.starts_with('-') && !url.contains(['\0', '\n']),
        "invalid repository URL"
    );
    ensure!(
        !reference.is_empty() && !reference.starts_with('-') && !reference.contains(['\0', '\n']),
        "invalid repository reference"
    );
    let snapshots = state_dir.join("snapshots");
    fs::create_dir_all(&snapshots)?;
    let temporary = tempfile::tempdir_in(&snapshots)?;
    git(temporary.path(), &["init", "--quiet"])?;
    git(temporary.path(), &["remote", "add", "origin", url])?;
    git(
        temporary.path(),
        &["fetch", "--quiet", "--depth=1", "origin", reference],
    )?;
    let commit = git(temporary.path(), &["rev-parse", "FETCH_HEAD"])?;
    ensure!(
        commit.len() == 40 || commit.len() == 64,
        "invalid git commit"
    );
    ensure!(
        commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid git commit"
    );
    git(
        temporary.path(),
        &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
    )?;
    let mut catalog = scan(temporary.path(), commit.clone())?;
    let destination = snapshots.join(&commit);
    if !destination.exists() {
        fs::rename(temporary.path(), &destination)?;
    }
    catalog.root = destination;
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pier_pkg::SourceKind;
    #[test]
    fn rejects_duplicate_mappings_and_invalid_instance_ids() {
        assert!(
            serde_yaml_ng::from_str::<Blueprint>(
                "schema: 1\nname: test\nvariables:\n  A: {}\n  A: {}\napps: []"
            )
            .is_err()
        );
        assert!(serde_yaml_ng::from_str::<Blueprint>("schema: 1\nname: test\napps:\n- id: app\n  app: apps/test\n  variables: {A: one, A: two}\n").is_err());
        let blueprint: Blueprint = serde_yaml_ng::from_str(
            "schema: 1\nname: test\napps:\n- id: '../escape'\n  app: apps/test\n",
        )
        .unwrap();
        assert!(blueprint.check(&BTreeMap::new()).is_err());
    }
    #[test]
    fn explicit_mapping_defaults_and_shared_variables() {
        let metadata = AppMetadata {
            name: "demo".into(),
            version: "1".into(),
            source: SourceKind::Binary,
            variables: BTreeMap::from([
                ("HOST".into(), VariableDefinition { default: None }),
                (
                    "PORT".into(),
                    VariableDefinition {
                        default: Some("80".into()),
                    },
                ),
            ]),
        };
        let apps = BTreeMap::from([("apps/demo/1".into(), metadata)]);
        let bp: Blueprint = serde_yaml_ng::from_str("schema: 1\nname: test\nvariables:\n  HOST: {}\n  PORT: {default: '8080'}\napps:\n- id: a\n  app: apps/demo/1\n  variables: {HOST: '{{ HOST }}', PORT: '{{ PORT }}'}\n- id: b\n  app: apps/demo/1\n  variables: {HOST: '{{ HOST }}'}\n").unwrap();
        bp.check(&apps).unwrap();
        assert!(bp.resolve(&Variables::new()).is_err());
        let resolved = bp
            .resolve(&BTreeMap::from([(
                "HOST".into(),
                "{{ literal }}:\nsecret".into(),
            )]))
            .unwrap();
        assert_eq!(resolved["a"]["HOST"], "{{ literal }}:\nsecret");
        assert_eq!(resolved["a"]["PORT"], "8080");
        assert!(!resolved["b"].contains_key("PORT"));
        let mut invalid = bp.clone();
        invalid.apps[0]
            .variables
            .insert("HOST".into(), "{{ UNKNOWN }}".into());
        assert!(invalid.check(&apps).is_err());
        invalid = bp.clone();
        invalid.apps[0].variables.remove("HOST");
        assert!(invalid.check(&apps).is_err());
    }
}
