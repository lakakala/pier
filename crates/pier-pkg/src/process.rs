use crate::{Error, Result, Stage};
use std::{
    io::{BufRead, BufReader, Read},
    process::{Command, Stdio},
};

#[derive(Clone, Default)]
pub(crate) struct Redactor {
    secrets: Vec<String>,
}
impl Redactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = values
            .into_iter()
            .flat_map(|v| {
                v.lines()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }
    fn redact(&self, text: &str) -> String {
        let mut value = text.to_string();
        for secret in &self.secrets {
            value = value.replace(secret, "[redacted]");
        }
        value
    }
}

/// Drain both pipes concurrently; retain only small stdout results such as a Git commit.
pub(crate) fn run(
    command: &mut Command,
    stage: Stage,
    label: &str,
    redactor: &Redactor,
    capture: bool,
) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| Error::new(stage, format!("cannot start {label}")).cause(e))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (out, err) = std::thread::scope(|scope| {
        let out = scope.spawn(|| drain(stdout, stage, label, redactor, capture));
        let err = scope.spawn(|| drain(stderr, stage, label, redactor, false));
        (out.join(), err.join())
    });
    let status = child
        .wait()
        .map_err(|e| Error::new(stage, format!("cannot wait for {label}")).cause(e))?;
    let output = out.map_err(|_| Error::new(stage, "stdout reader failed"))??;
    err.map_err(|_| Error::new(stage, "stderr reader failed"))??;
    if !status.success() {
        return Err(Error::new(
            stage,
            format!("{label} failed ({status}); see tracing logs"),
        ));
    }
    Ok(output)
}
fn drain(
    input: impl Read,
    stage: Stage,
    label: &str,
    redactor: &Redactor,
    capture: bool,
) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(input);
    let mut output = Vec::new();
    // take() bounds each line so a compiler cannot allocate unbounded log buffers.
    loop {
        let mut line = Vec::new();
        let n = reader
            .by_ref()
            .take(65536)
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::new(stage, "cannot read child output").cause(e))?;
        if n == 0 {
            break;
        }
        if capture {
            if output.len() + n <= 65536 {
                output.extend_from_slice(&line);
            }
        } else {
            let message = redactor.redact(String::from_utf8_lossy(&line).trim_end());
            tracing::debug!(?stage, process=label, %message, "child output");
        }
    }
    Ok(output)
}
