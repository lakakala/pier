use crate::{Architecture, Error, Result, Service, Stage, files, types::IoResult};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub path: PathBuf,
    pub sha256: String,
    pub mode: u32,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elf: Option<ElfRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElfRecord {
    pub libraries: Vec<String>,
    pub required_glibc: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct Manifest<'a> {
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub ports: crate::Ports,
    pub schema: u32,
    pub name: &'a str,
    pub version: &'a str,
    pub os: &'static str,
    pub architecture: Architecture,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<&'a str>,
    pub service: &'a Service,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<&'a str>,
    pub files: Vec<FileRecord>,
}

pub(crate) fn inventory(root: &Path, architecture: Architecture) -> Result<Vec<FileRecord>> {
    let mut records = Vec::new();
    for path in files::walk(root)? {
        let full = root.join(&path);
        let meta = fs::metadata(&full).context(Stage::Files, &full)?;
        if meta.is_dir() {
            continue;
        }
        let elf = inspect_elf(&full, architecture)?;
        records.push(FileRecord {
            path,
            sha256: files::hash(&full)?,
            mode: mode(&meta),
            size: meta.len(),
            elf,
        });
    }
    Ok(records)
}
fn mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            0o755
        } else {
            0o644
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}
fn inspect_elf(path: &Path, architecture: Architecture) -> Result<Option<ElfRecord>> {
    let mut f = fs::File::open(path).context(Stage::Compatibility, path)?;
    let mut magic = [0; 4];
    let n = f.read(&mut magic).context(Stage::Compatibility, path)?;
    if n != 4 || magic != *b"\x7fELF" {
        return Ok(None);
    }
    let bytes = fs::read(path).context(Stage::Compatibility, path)?;
    let elf = goblin::elf::Elf::parse(&bytes)
        .map_err(|_| Error::new(Stage::Compatibility, "invalid ELF binary").at(path))?;
    let machine = match architecture {
        Architecture::Amd64 => goblin::elf::header::EM_X86_64,
        Architecture::Arm64 => goblin::elf::header::EM_AARCH64,
    };
    if elf.header.e_machine != machine || !elf.is_64 || !elf.little_endian {
        return Err(Error::new(
            Stage::Compatibility,
            "ELF architecture does not match package target",
        )
        .at(path));
    }
    let mut versions = Vec::new();
    if let Some(needs) = &elf.verneed {
        for need in needs.iter() {
            for aux in need.iter() {
                let name = elf.dynstrtab.get_at(aux.vna_name).ok_or_else(|| {
                    Error::new(Stage::Compatibility, "invalid ELF version string").at(path)
                })?;
                if name.starts_with("GLIBC_") {
                    versions.push(name.into());
                }
            }
        }
    }
    versions.sort();
    versions.dedup();
    Ok(Some(ElfRecord {
        libraries: elf.libraries.into_iter().map(String::from).collect(),
        required_glibc: versions,
    }))
}

pub(crate) fn check_service(root: &Path, service: &Service) -> Result<()> {
    let program = files::resolve(root, Path::new(&service.command[0]), Stage::Files)?;
    let metadata = fs::metadata(&program).context(Stage::Files, &program)?;
    if !metadata.is_file() || mode(&metadata) != 0o755 {
        return Err(Error::new(
            Stage::Files,
            "service executable must exist and have executable: true",
        )
        .at(program));
    }
    let working = files::resolve(root, &service.working_dir, Stage::Files)?;
    if !working.is_dir() {
        return Err(Error::new(
            Stage::Files,
            "service working_dir must be a packaged directory",
        )
        .at(working));
    }
    Ok(())
}

pub(crate) fn publish(staging: &Path, output: &Path, overwrite: bool) -> Result<String> {
    let parent = output
        .parent()
        .ok_or_else(|| Error::new(Stage::Archive, "output has no parent"))?;
    fs::create_dir_all(parent).context(Stage::Archive, parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context(Stage::Archive, parent)?;
    {
        let gzip = flate2::GzBuilder::new()
            .mtime(0)
            .write(&mut temporary, flate2::Compression::default());
        let mut tar = tar::Builder::new(gzip);
        for path in files::walk(staging)? {
            let full = staging.join(&path);
            let metadata = fs::metadata(&full).context(Stage::Archive, &full)?;
            let mut header = tar::Header::new_gnu();
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            if metadata.is_dir() {
                header.set_mode(0o755);
                header.set_size(0);
                header.set_entry_type(tar::EntryType::Directory);
                tar.append_data(&mut header, &path, std::io::empty())
                    .context(Stage::Archive, output)?;
            } else {
                header.set_mode(mode(&metadata));
                header.set_size(metadata.len());
                header.set_entry_type(tar::EntryType::Regular);
                let file = fs::File::open(&full).context(Stage::Archive, &full)?;
                tar.append_data(&mut header, &path, file)
                    .context(Stage::Archive, output)?;
            }
        }
        tar.into_inner()
            .context(Stage::Archive, output)?
            .finish()
            .context(Stage::Archive, output)?;
    }
    temporary.flush().context(Stage::Archive, output)?;
    temporary
        .as_file()
        .sync_all()
        .context(Stage::Archive, output)?;
    let hash = files::hash(temporary.path())?;
    let result = if overwrite {
        temporary.persist(output)
    } else {
        temporary.persist_noclobber(output)
    };
    result.map_err(|e| {
        Error::new(
            Stage::Archive,
            "cannot publish archive (destination may already exist)",
        )
        .at(output)
        .cause(e.error)
    })?;
    Ok(hash)
}
