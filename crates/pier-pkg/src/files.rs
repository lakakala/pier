use crate::{Error, Result, Stage, types::IoResult};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) fn relative(path: &Path, allow_root: bool) -> Result<PathBuf> {
    let s = path
        .to_str()
        .ok_or_else(|| Error::new(Stage::Files, "paths must be UTF-8"))?;
    let mut result = PathBuf::new();
    if path.is_absolute() || s.contains(['\\', '\0', ':']) {
        return Err(Error::new(Stage::Files, "absolute or nonportable path").at(path));
    }
    for part in s.split('/') {
        match part {
            "" | "." => (),
            ".." => return Err(Error::new(Stage::Files, "parent traversal is forbidden").at(path)),
            p => result.push(p),
        }
    }
    if result.as_os_str().is_empty() && !allow_root {
        return Err(Error::new(Stage::Files, "empty path is forbidden").at(path));
    }
    Ok(result)
}

/// Resolve a path without following any symlink inside the trusted root.
pub(crate) fn resolve(root: &Path, path: &Path, stage: Stage) -> Result<PathBuf> {
    let rel = relative(path, true)?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        let m = fs::symlink_metadata(&current).context(stage, &current)?;
        if m.file_type().is_symlink() || !(m.is_dir() || m.is_file()) {
            return Err(Error::new(stage, "links and special files are forbidden").at(&current));
        }
    }
    Ok(current)
}

pub(crate) fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(root: &Path, dir: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
        let mut entries = fs::read_dir(dir)
            .context(Stage::Files, dir)?
            .collect::<std::io::Result<Vec<_>>>()
            .context(Stage::Files, dir)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let m = fs::symlink_metadata(&p).context(Stage::Files, &p)?;
            if m.file_type().is_symlink() || !(m.is_dir() || m.is_file()) {
                return Err(
                    Error::new(Stage::Files, "links and special files are forbidden").at(p),
                );
            }
            let rel = p
                .strip_prefix(root)
                .expect("walk stays inside root")
                .to_path_buf();
            relative(&rel, false)?;
            output.push(rel);
            if m.is_dir() {
                visit(root, &p, output)?;
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries)?;
    Ok(entries)
}

pub(crate) fn copy(source: &Path, dest: &Path, executable: bool) -> Result<()> {
    let m = fs::symlink_metadata(source).context(Stage::Files, source)?;
    if m.file_type().is_symlink() || !(m.is_dir() || m.is_file()) {
        return Err(Error::new(Stage::Files, "links and special files are forbidden").at(source));
    }
    if dest.exists() {
        return Err(Error::new(Stage::Files, "duplicate destination").at(dest));
    }
    if m.is_dir() {
        fs::create_dir_all(dest).context(Stage::Files, dest)?;
        for path in walk(source)? {
            let src = source.join(&path);
            let dst = dest.join(&path);
            if src.is_dir() {
                fs::create_dir_all(&dst).context(Stage::Files, &dst)?;
            } else {
                copy(&src, &dst, executable)?;
            }
        }
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).context(Stage::Files, parent)?;
        }
        let mut reader = fs::File::open(source).context(Stage::Files, source)?;
        let mut writer = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .context(Stage::Files, dest)?;
        std::io::copy(&mut reader, &mut writer).context(Stage::Files, dest)?;
        set_mode(dest, if executable { 0o755 } else { 0o644 })?;
    }
    Ok(())
}
pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).context(Stage::Files, path)?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
pub(crate) fn hash(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path).context(Stage::Files, path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).context(Stage::Files, path)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context(Stage::Files, parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context(Stage::Files, path)?;
    f.write_all(bytes).context(Stage::Files, path)?;
    set_mode(path, 0o644)
}
