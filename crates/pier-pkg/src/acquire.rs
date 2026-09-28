use crate::{
    Error, Result, Stage,
    config::DownloadFormat,
    files,
    process::{self, Redactor},
    proxy::Proxy,
    types::IoResult,
};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::Command,
};

pub(crate) fn checkout(
    repo: &str,
    git_ref: &str,
    root: &Path,
    dest: &Path,
    proxy: &Proxy,
    redactor: &Redactor,
) -> Result<String> {
    fs::create_dir_all(dest).context(Stage::Git, dest)?;
    // Local repository paths are relative to the recipe, not the caller's cwd.
    let resolved;
    let repo = if !repo.contains(':') && !Path::new(repo).is_absolute() {
        resolved = root.join(repo).to_string_lossy().into_owned();
        &resolved
    } else {
        repo
    };
    let command = || {
        let mut c = Command::new("git");
        c.current_dir(dest);
        proxy.command(&mut c);
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_PARAMETERS",
        ] {
            c.env_remove(key);
        }
        c.env("GIT_TERMINAL_PROMPT", "0");
        // Explicit command-scope config wins over system/global proxy settings.
        c.env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_0", "http.proxy")
            .env("GIT_CONFIG_VALUE_0", proxy.git_proxy(repo))
            .env(
                "GIT_CONFIG_KEY_1",
                if repo.starts_with("http://") || repo.starts_with("https://") {
                    format!("http.{repo}.proxy")
                } else {
                    "remote.origin.proxy".into()
                },
            )
            .env("GIT_CONFIG_VALUE_1", proxy.git_proxy(repo))
            .env("GIT_CONFIG_KEY_2", "credential.interactive")
            .env("GIT_CONFIG_VALUE_2", "false");
        c
    };
    process::run(
        command().args(["init", "--quiet"]),
        Stage::Git,
        "git init",
        redactor,
        false,
    )?;
    process::run(
        command().args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--",
            repo,
            git_ref,
        ]),
        Stage::Git,
        "git fetch",
        redactor,
        false,
    )?;
    let hash = process::run(
        command().args(["rev-parse", "--verify", "FETCH_HEAD^{commit}"]),
        Stage::Git,
        "git rev-parse",
        redactor,
        true,
    )?;
    let hash = String::from_utf8_lossy(&hash).trim().to_string();
    if ![40, 64].contains(&hash.len()) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(Stage::Git, "invalid resolved Git commit"));
    }
    process::run(
        command().args(["checkout", "--quiet", "--detach", "--force", &hash]),
        Stage::Git,
        "git checkout",
        redactor,
        false,
    )?;
    Ok(hash)
}

pub(crate) fn download(
    url: &str,
    format: DownloadFormat,
    sha256: Option<&str>,
    dest: &Path,
    proxy: &Proxy,
) -> Result<()> {
    fs::create_dir_all(dest).context(Stage::Download, dest)?;
    let client = proxy.client()?;
    let mut response = client.get(url).send().map_err(|e| {
        Error::new(
            Stage::Download,
            if e.is_timeout() {
                "download timed out"
            } else if e.is_connect() {
                "download connection failed"
            } else {
                "download request failed"
            },
        )
    })?;
    if !response.status().is_success() {
        return Err(Error::new(
            Stage::Download,
            format!("download returned HTTP {}", response.status()),
        ));
    }
    let mut archive = tempfile::NamedTempFile::new().context(Stage::Download, dest)?;
    std::io::copy(&mut response, &mut archive).context(Stage::Download, dest)?;
    archive.flush().context(Stage::Download, dest)?;
    if let Some(expected) = sha256 {
        if !files::hash(archive.path())?.eq_ignore_ascii_case(expected) {
            return Err(Error::new(Stage::Download, "SHA-256 mismatch"));
        }
    }
    match format {
        DownloadFormat::Raw => files::copy(archive.path(), &dest.join("download"), false),
        DownloadFormat::TarGz => extract_tar(archive.path(), dest),
        DownloadFormat::Zip => extract_zip(archive.path(), dest),
    }
}

fn create_entry(dest: &Path, relative: &Path, directory: bool) -> Result<Option<fs::File>> {
    let relative = files::relative(relative, true)?;
    if relative.as_os_str().is_empty() {
        if directory {
            return Ok(None);
        }
        return Err(Error::new(
            Stage::Download,
            "archive file has an empty path",
        ));
    }
    let out = dest.join(relative);
    if directory {
        fs::create_dir_all(&out).context(Stage::Download, &out)?;
        return Ok(None);
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).context(Stage::Download, parent)?;
    }
    let f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out)
        .context(Stage::Download, &out)?;
    files::set_mode(&out, 0o644)?;
    Ok(Some(f))
}
fn extract_tar(path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(path).context(Stage::Download, path)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive.entries().context(Stage::Download, path)? {
        let mut entry = entry.context(Stage::Download, path)?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(Error::new(
                Stage::Download,
                "archive links and special files are forbidden",
            ));
        }
        let relative = entry.path().context(Stage::Download, path)?.into_owned();
        if let Some(mut f) = create_entry(dest, &relative, kind.is_dir())? {
            std::io::copy(&mut entry, &mut f).context(Stage::Download, path)?;
        }
    }
    Ok(())
}
fn extract_zip(path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(path).context(Stage::Download, path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|_| Error::new(Stage::Download, "invalid zip archive"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| Error::new(Stage::Download, "cannot read zip entry"))?;
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if ![0, 0o100000, 0o040000].contains(&kind) {
                return Err(Error::new(
                    Stage::Download,
                    "zip links and special files are forbidden",
                ));
            }
        }
        let name = entry.name().to_string();
        if let Some(mut f) = create_entry(dest, Path::new(&name), entry.is_dir())? {
            std::io::copy(&mut entry, &mut f).context(Stage::Download, path)?;
            // Reading to EOF above validates CRC through ZipFile's reader.
            let mut tail = [0];
            let _ = entry.read(&mut tail).context(Stage::Download, path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_checks_out_branch_tag_and_commit_without_changing_source() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(repo.join("value"), "first").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "first",
        ]);
        let first = git(&["rev-parse", "HEAD"]);
        git(&["tag", "v1"]);
        fs::write(repo.join("value"), "second").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "second",
        ]);
        for (reference, expected) in [
            ("v1", "first"),
            (first.as_str(), "first"),
            ("main", "second"),
        ] {
            let dest = tempfile::tempdir().unwrap();
            let resolved = checkout(
                "repo",
                reference,
                root.path(),
                dest.path(),
                &Proxy::default(),
                &Redactor::default(),
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(dest.path().join("value")).unwrap(),
                expected
            );
            if expected == "first" {
                assert_eq!(resolved, first);
            }
        }
        assert_eq!(fs::read_to_string(repo.join("value")).unwrap(), "second");
    }
}
