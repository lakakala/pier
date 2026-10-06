use crate::{Error, Result, Stage, files, types::IoResult};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn environment<'a>() -> Environment<'a> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_keep_trailing_newline(true);
    env
}

/// Render string values after YAML parsing; injected text cannot become YAML structure.
/// Declaration names/defaults, mapping keys, enum selectors and booleans stay literal.
pub(crate) fn render_recipe(
    recipe: &mut crate::config::Recipe,
    variables: &BTreeMap<String, String>,
    path: &Path,
) -> Result<()> {
    let env = environment();
    let render = |value: &mut String| -> Result<()> {
        *value = env
            .render_str(value, variables)
            .map_err(|e| template_error(path, e))?;
        Ok(())
    };
    let render_path = |value: &mut PathBuf| -> Result<()> {
        let mut text = value
            .to_str()
            .ok_or_else(|| Error::new(Stage::Template, "recipe paths must be UTF-8").at(path))?
            .to_string();
        render(&mut text)?;
        *value = text.into();
        Ok(())
    };
    let ports = crate::resolve_ports(&recipe.ports, variables)?;
    for (name, value) in ports {
        recipe.ports.get_mut(&name).unwrap().port = crate::PortValue::Number(value.port);
    }
    render(&mut recipe.name)?;
    render(&mut recipe.version)?;
    match &mut recipe.source {
        crate::config::Source::Git { repo, r#ref } => {
            render(repo)?;
            render(r#ref)?;
        }
        crate::config::Source::Binary { url, sha256, .. } => {
            render(url)?;
            if let Some(hash) = sha256 {
                render(hash)?;
            }
        }
    }
    if let Some(build) = &mut recipe.build {
        render_path(&mut build.workdir)?;
        for value in build.commands.iter_mut().chain(build.env.values_mut()) {
            render(value)?;
        }
    }
    for file in &mut recipe.files {
        render_path(&mut file.from)?;
        render_path(&mut file.to)?;
    }
    render_path(&mut recipe.service.working_dir)?;
    for value in recipe
        .service
        .command
        .iter_mut()
        .chain(recipe.service.env.values_mut())
    {
        render(value)?;
    }
    Ok(())
}

pub(crate) fn render(
    root: &Path,
    variables: &BTreeMap<String, String>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut rendered = BTreeMap::new();
    let dir = root.join("configs");
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(rendered),
        Err(e) => {
            return Err(
                Error::new(Stage::Template, "cannot inspect configs directory")
                    .at(dir)
                    .cause(e),
            );
        }
        Ok(m) if !m.is_dir() || m.file_type().is_symlink() => {
            return Err(Error::new(Stage::Template, "configs must be a real directory").at(dir));
        }
        _ => (),
    }
    let mut env = environment();
    let mut paths = Vec::new();
    for path in files::walk(&dir)? {
        let full = dir.join(&path);
        if full.is_dir() {
            continue;
        }
        let name = path.to_str().expect("walk validates UTF-8").to_string();
        let text = fs::read_to_string(&full).context(Stage::Template, &full)?;
        env.add_template_owned(name.clone(), text)
            .map_err(|e| template_error(&full, e))?;
        paths.push((path, name));
    }
    for (path, name) in paths {
        let template = env
            .get_template(&name)
            .map_err(|e| template_error(&dir.join(&path), e))?;
        let text = template
            .render(variables)
            .map_err(|e| template_error(&dir.join(&path), e))?;
        rendered.insert(Path::new("configs").join(path), text.into_bytes());
    }
    Ok(rendered)
}
fn template_error(path: &Path, error: minijinja::Error) -> Error {
    Error::new(
        Stage::Template,
        format!(
            "{} at template {} line {}",
            error.kind(),
            error.name().unwrap_or("unknown"),
            error.line().unwrap_or(0)
        ),
    )
    .at(path)
}
