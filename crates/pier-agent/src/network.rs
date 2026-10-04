use crate::{Config, Runtime};
use anyhow::{Context, Result, ensure};
use pier_protocol::{AgentInfo, Message};
use std::{fs, sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};

pub async fn connect(
    config: Config,
    runtime: Arc<Runtime>,
    mut events: mpsc::UnboundedReceiver<Message>,
    upgrades: Arc<crate::upgrade::Manager>,
    mut accepted: Option<mpsc::Receiver<crate::transport::Accepted>>,
) -> Result<()> {
    let token = fs::read_to_string(&config.token_file)?.trim().to_string();
    let info = host_info()?;
    let (jobs_tx, mut jobs_rx) = mpsc::channel::<pier_protocol::DeploymentPlan>(1);
    let worker_runtime = runtime.clone();
    tokio::spawn(async move {
        while let Some(plan) = jobs_rx.recv().await {
            let id = plan.id.clone();
            let worker = worker_runtime.clone();
            let result = tokio::task::spawn_blocking(move || worker.apply(plan)).await;
            let result = match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    tracing::error!(%error, deployment_id=%id, "deployment operation failed");
                    pier_protocol::DeploymentResult {
                        id,
                        state: "failed".into(),
                        error: Some("agent rejected deployment or local operation failed".into()),
                    }
                }
                _ => pier_protocol::DeploymentResult {
                    id,
                    state: "failed".into(),
                    error: Some("agent rejected deployment or local operation failed".into()),
                },
            };
            let _ = worker_runtime.events.send(Message::Result { result });
        }
    });
    let mut delay = 1;
    loop {
        let terminals = tokio_util::sync::CancellationToken::new();
        let _terminal_guard = terminals.clone().drop_guard();
        let session: Result<()> = async {
            let (stream, _permit) = if let Some(receiver) = accepted.as_mut() {
                let (stream, permit) = receiver.recv().await.context("agent listener stopped")?;
                (stream, Some(permit))
            } else {
                (pier_protocol::secure::connect(&config.controller_tcp, pier_protocol::secure::Purpose::Control, &config.agent_id, &pier_protocol::secure::token_key(&token)).await?, None)
            };
            let mut stream = pier_protocol::framed(stream);
            pier_protocol::send(&mut stream, &Message::Hello { version: pier_protocol::VERSION, agent_id: config.agent_id.clone(), info: info.clone(), software: Some(upgrades.software()) }).await?;
            if config.connection_mode == pier_protocol::connection::ConnectionMode::ControllerToAgent {
                let Message::Session { id } = timeout(Duration::from_secs(10), pier_protocol::receive(&mut stream)).await?? else { anyhow::bail!("control session required"); };
                runtime.transport.begin(id, terminals.clone())?;
            }
            let welcome = timeout(Duration::from_secs(10), pier_protocol::receive(&mut stream)).await??;
            let Message::Welcome { version: pier_protocol::VERSION, upgrade } = welcome else { anyhow::bail!("invalid welcome or protocol version"); };
            upgrades.offer(upgrade);
            pier_protocol::send(&mut stream, &Message::Report { report: runtime.report()? }).await?;
            tracing::info!("connected to controller");
            delay = 1;
            let mut interval = tokio::time::interval(Duration::from_secs(config.heartbeat_seconds));
            let mut received = Instant::now();
            loop {
                tokio::select! {
                    message = pier_protocol::receive(&mut stream) => {
                        received = Instant::now();
                        match message? {
                            Message::Ping => timeout(Duration::from_secs(10), pier_protocol::send(&mut stream, &Message::Pong)).await??,
                            Message::Pong => (),
                            Message::Deploy { plan } => {
                                // Bounded queue avoids blocking heartbeat processing on builds.
                                jobs_tx.try_send(plan).context("deployment queue full")?;
                            }
                            Message::TerminalOpen { id, instance, cols, rows } => {
                                let runtime = runtime.clone();
                                let cancelled = terminals.child_token();
                                tokio::spawn(async move {
                                    if crate::terminal::serve(runtime, crate::terminal::Request { id, instance, cols, rows }, cancelled).await.is_err() {
                                        tracing::debug!("app terminal closed");
                                    }
                                });
                            }
                            _ => anyhow::bail!("unexpected controller message"),
                        }
                    }
                    event = events.recv() => {
                        let event = event.context("event channel closed")?;
                        if matches!(&event, Message::OpenChannel { session, .. } if !runtime.transport.current(session)) { continue; }
                        timeout(Duration::from_secs(30), pier_protocol::send(&mut stream, &event)).await??;
                        if matches!(event, Message::Result { .. }) {
                            timeout(Duration::from_secs(10), pier_protocol::send(&mut stream, &Message::Report { report: runtime.report()? })).await??;
                        }
                    }
                    _ = interval.tick() => {
                        upgrades.poll();
                        ensure!(received.elapsed() < Duration::from_secs(45), "controller heartbeat timeout");
                        timeout(Duration::from_secs(10), pier_protocol::send(&mut stream, &Message::Report { report: runtime.report()? })).await??;
                    }
                }
            }
        }.await;
        terminals.cancel();
        runtime.transport.disconnect();
        if session.is_err() {
            tracing::warn!(
                retry_seconds = delay,
                "controller connection lost; local services continue"
            );
        }
        if let Some(receiver) = &accepted {
            ensure!(!receiver.is_closed(), "agent listener stopped");
            continue;
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(60);
    }
}

pub(crate) fn host_info() -> Result<AgentInfo> {
    Ok(AgentInfo {
        architecture: crate::architecture()?,
        hostname: fs::read_to_string("/etc/hostname")
            .unwrap_or_else(|_| "unknown".into())
            .trim()
            .chars()
            .take(256)
            .collect(),
        os_release: fs::read_to_string("/etc/os-release")
            .unwrap_or_default()
            .chars()
            .take(8192)
            .collect(),
    })
}
/// Used from the existing blocking deployment worker, never from a Tokio I/O task.
pub(crate) fn download(
    transport: &crate::transport::Transport,
    deployment: &str,
    app: &pier_protocol::DeploymentApp,
    path: &std::path::Path,
    stopping: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::{io::Write, sync::atomic::Ordering};
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            timeout(Duration::from_secs(300), async {
                let mut stream = transport
                    .open(pier_protocol::secure::Purpose::Artifact)
                    .await?;
                pier_protocol::send(
                    &mut stream,
                    &Message::ArtifactRequest {
                        deployment: deployment.into(),
                        app: app.id.clone(),
                    },
                )
                .await?;
                let Message::ArtifactBegin { size } = pier_protocol::receive(&mut stream).await?
                else {
                    anyhow::bail!("artifact header required");
                };
                ensure!(size == app.size, "artifact size mismatch");
                let mut file = fs::File::create(path)?;
                let mut received = 0;
                loop {
                    ensure!(!stopping.load(Ordering::SeqCst), "agent shutting down");
                    match pier_protocol::receive(&mut stream).await? {
                        Message::ArtifactChunk { data } => {
                            ensure!(data.len() <= 43692, "artifact chunk too large");
                            let bytes = STANDARD.decode(data)?;
                            ensure!(
                                !bytes.is_empty() && bytes.len() <= 32768,
                                "invalid artifact chunk"
                            );
                            received += bytes.len() as u64;
                            ensure!(received <= size, "artifact exceeds declared size");
                            file.write_all(&bytes)?;
                        }
                        Message::ArtifactEnd { size: end_size } => {
                            ensure!(received == size && end_size == size, "artifact truncated");
                            file.sync_all()?;
                            return Ok::<_, anyhow::Error>(());
                        }
                        _ => anyhow::bail!("unexpected artifact message"),
                    }
                }
            })
            .await?
        })
}
