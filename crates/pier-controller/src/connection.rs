use crate::{AgentRecord, Controller, Job, Session};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use pier_protocol::secure::{self, Purpose};
use pier_protocol::{AgentReport, DeploymentResult, Message};
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::{Instant, timeout},
};

pub async fn listen(state: Arc<Controller>, listener: TcpListener) -> Result<()> {
    let slots = Arc::new(tokio::sync::Semaphore::new(256));
    loop {
        let permit = slots.clone().acquire_owned().await?;
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if connection(state, stream).await.is_err() {
                tracing::debug!("agent connection closed");
            }
        });
    }
}
fn result(state: &Controller, agent_id: &str, result: &DeploymentResult) -> Result<()> {
    ensure!(
        matches!(
            result.state.as_str(),
            "succeeded" | "failed" | "rolled_back" | "rollback_failed"
        ),
        "invalid result state"
    );
    let mut job: Job = state
        .store
        .get("jobs", &result.id)?
        .context("unknown deployment")?;
    ensure!(
        job.agent_id == agent_id,
        "deployment belongs to another agent"
    );
    if job.active() || job.state == "rollback_failed" {
        job.state = result.state.clone();
        job.error = result
            .error
            .as_ref()
            .map(|_| "agent reported deployment failure; inspect local agent logs".into());
        state.store.put("jobs", &job.id, &job)?;
    }
    Ok(())
}
fn report(state: &Controller, agent_id: &str, report: AgentReport) -> Result<()> {
    if let Some(result_value) = &report.result {
        result(state, agent_id, result_value)?;
    }
    let mut record: AgentRecord = state
        .store
        .get("agents", agent_id)?
        .context("unknown agent")?;
    record.report = report;
    record.last_seen = Some(pier_protocol::now());
    state.store.put("agents", agent_id, &record)
}
async fn connection(state: Arc<Controller>, mut socket: TcpStream) -> Result<()> {
    let (prelude, key, stream) = timeout(Duration::from_secs(10), async {
        let (prelude, raw) = secure::read_prelude(&mut socket).await?;
        let key = if prelude.purpose == Purpose::Enrollment {
            state.enrollment_key(&prelude.id)?
        } else {
            let agent: AgentRecord = state
                .store
                .get("agents", &prelude.id)?
                .context("unknown agent")?;
            secure::decode_key(&agent.token_hash)?
        };
        let stream = secure::accept(socket, &raw, &key).await?;
        Ok::<_, anyhow::Error>((prelude, key, stream))
    })
    .await??;
    if prelude.purpose != Purpose::Enrollment {
        let current: AgentRecord = state
            .store
            .get("agents", &prelude.id)?
            .context("unknown agent")?;
        ensure!(
            secure::decode_key(&current.token_hash)? == key,
            "credential changed during handshake"
        );
    }
    let mut stream = pier_protocol::framed(stream);
    match prelude.purpose {
        Purpose::Enrollment => {
            let Message::Enroll { request } =
                timeout(Duration::from_secs(10), pier_protocol::receive(&mut stream)).await??
            else {
                anyhow::bail!("enrollment request required");
            };
            let credentials = state.issue_credentials(&prelude.id, request, &key)?;
            timeout(
                Duration::from_secs(10),
                pier_protocol::send(&mut stream, &Message::Enrolled { credentials }),
            )
            .await??;
            return Ok(());
        }
        Purpose::EnrollmentAck => {
            let Message::EnrollmentAck { request_id } =
                timeout(Duration::from_secs(10), pier_protocol::receive(&mut stream)).await??
            else {
                anyhow::bail!("enrollment acknowledgement required");
            };
            state.acknowledge_enrollment(&request_id, &prelude.id)?;
            timeout(
                Duration::from_secs(10),
                pier_protocol::send(&mut stream, &Message::Acked),
            )
            .await??;
            return Ok(());
        }
        Purpose::Artifact => {
            return timeout(
                Duration::from_secs(300),
                artifact(&state, &prelude.id, &mut stream),
            )
            .await?;
        }
        Purpose::Upgrade => {
            return timeout(
                Duration::from_secs(300),
                crate::upgrades::serve(&state, &prelude.id, &mut stream),
            )
            .await?;
        }
        Purpose::Control => (),
        Purpose::Terminal => {
            return crate::terminal::attach(&state, &prelude.id, stream).await;
        }
    }
    let Message::Hello {
        version,
        agent_id,
        info,
        software,
    } = timeout(Duration::from_secs(10), pier_protocol::receive(&mut stream)).await??
    else {
        anyhow::bail!("hello required");
    };
    ensure!(
        version == pier_protocol::VERSION,
        "unsupported protocol version"
    );
    let mut record: AgentRecord = state
        .store
        .get("agents", &agent_id)?
        .context("unknown agent")?;
    ensure!(agent_id == prelude.id, "authenticated identity mismatch");
    ensure!(
        info.hostname.len() <= 256 && info.os_release.len() <= 8192,
        "invalid host information"
    );
    record.info = Some(info);
    record.last_seen = Some(pier_protocol::now());
    state.store.put("agents", &agent_id, &record)?;
    state.record_software(&agent_id, software.as_ref())?;
    pier_protocol::send(
        &mut stream,
        &Message::Welcome {
            version: pier_protocol::VERSION,
            upgrade: if software.is_some() {
                Some(state.upgrade_offer(&agent_id)?)
            } else {
                None
            },
        },
    )
    .await?;
    let Message::Report { report: initial } =
        timeout(Duration::from_secs(10), pier_protocol::receive(&mut stream)).await??
    else {
        anyhow::bail!("initial report required");
    };
    let terminal = initial
        .capabilities
        .iter()
        .any(|v| v == pier_protocol::terminal::CAPABILITY);
    report(&state, &agent_id, initial)?;
    let session_id = pier_protocol::new_id();
    let (sender, mut receiver) = mpsc::channel(8);
    {
        let mut sessions = state.sessions.lock().unwrap();
        ensure!(!sessions.contains_key(&agent_id), "agent already connected");
        sessions.insert(
            agent_id.clone(),
            Session {
                id: session_id.clone(),
                sender,
                terminal,
            },
        );
    }
    let result = async {
        for job in state.store.list::<Job>("jobs")? {
            if job.agent_id == agent_id && job.active() && job.plan.is_some() { state.dispatch(&job.id).await?; }
        }
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        let mut last_received = Instant::now();
        loop {
            tokio::select! {
                message = pier_protocol::receive(&mut stream) => {
                    last_received = Instant::now();
                    match message? {
                        Message::Ping => pier_protocol::send(&mut stream, &Message::Pong).await?,
                        Message::Pong => (),
                        Message::Report { report: value } => report(&state, &agent_id, value)?,
                        Message::Result { result: value } => result(&state, &agent_id, &value)?,
                        Message::Progress { id, phase } => {
                            ensure!(matches!(phase.as_str(), "downloading" | "applying" | "rolling_back"), "invalid progress phase");
                            let mut job: Job = state.store.get("jobs", &id)?.context("unknown job")?;
                            ensure!(job.agent_id == agent_id, "job belongs to another agent");
                            if job.active() { job.state = phase; state.store.put("jobs", &id, &job)?; }
                        }
                        _ => anyhow::bail!("unexpected agent message"),
                    }
                }
                message = receiver.recv() => {
                    let message = message.context("session closed")?;
                    timeout(Duration::from_secs(30), pier_protocol::send(&mut stream, &message)).await??;
                }
                _ = interval.tick() => {
                    ensure!(last_received.elapsed() < Duration::from_secs(45), "heartbeat timeout");
                    timeout(Duration::from_secs(10), pier_protocol::send(&mut stream, &Message::Ping)).await??;
                }
            }
        }
        #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
    }.await;
    state.terminals.disconnect_agent(&agent_id, &session_id);
    let mut sessions = state.sessions.lock().unwrap();
    if sessions.get(&agent_id).is_some_and(|s| s.id == session_id) {
        sessions.remove(&agent_id);
    }
    result
}

async fn artifact(
    state: &Controller,
    agent_id: &str,
    stream: &mut tokio_util::codec::Framed<
        secure::SecureStream,
        tokio_util::codec::LengthDelimitedCodec,
    >,
) -> Result<()> {
    let Message::ArtifactRequest { deployment, app } = pier_protocol::receive(stream).await? else {
        anyhow::bail!("artifact request required");
    };
    let job: Job = state
        .store
        .get("jobs", &deployment)?
        .context("unknown deployment")?;
    ensure!(
        job.agent_id == agent_id,
        "artifact belongs to another agent"
    );
    let path = job.artifacts.get(&app).context("unknown artifact")?;
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    ensure!(size <= 10 * 1024 * 1024 * 1024, "artifact too large");
    pier_protocol::send(stream, &Message::ArtifactBegin { size }).await?;
    let mut buffer = vec![0; 32768];
    let mut sent = 0;
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        sent += n as u64;
        ensure!(sent <= size, "artifact changed during transfer");
        pier_protocol::send(
            stream,
            &Message::ArtifactChunk {
                data: STANDARD.encode(&buffer[..n]),
            },
        )
        .await?;
    }
    ensure!(sent == size, "artifact changed during transfer");
    pier_protocol::send(stream, &Message::ArtifactEnd { size }).await?;
    Ok(())
}
