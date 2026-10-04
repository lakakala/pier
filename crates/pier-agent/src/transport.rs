//! Direction-independent business connections; passive agents never dial a socket.
use crate::Config;
use anyhow::{Context, Result, ensure};
use pier_protocol::{
    Message,
    connection::{ConnectionMode, Wire},
    secure::{self, Purpose},
};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

pub(crate) type Accepted = (secure::SecureStream, OwnedSemaphorePermit);
struct Pending {
    purpose: Purpose,
    sender: oneshot::Sender<Wire>,
}
struct Session {
    id: String,
    cancelled: CancellationToken,
    pending: BTreeMap<String, Pending>,
}
pub(crate) struct Transport {
    config: Config,
    events: mpsc::UnboundedSender<Message>,
    session: Mutex<Option<Session>>,
    stopped: CancellationToken,
}
impl Transport {
    pub fn new(config: Config, events: mpsc::UnboundedSender<Message>) -> Arc<Self> {
        Arc::new(Self {
            config,
            events,
            session: Mutex::new(None),
            stopped: CancellationToken::new(),
        })
    }
    pub fn begin(&self, id: String, cancelled: CancellationToken) -> Result<()> {
        ensure!(pier_protocol::safe_id(&id), "invalid control session");
        self.disconnect();
        *self.session.lock().unwrap() = Some(Session {
            id,
            cancelled,
            pending: BTreeMap::new(),
        });
        Ok(())
    }
    pub fn current(&self, id: &str) -> bool {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.id == id && !s.cancelled.is_cancelled())
    }
    pub fn disconnect(&self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            session.cancelled.cancel();
        }
    }
    pub fn stop(&self) {
        self.disconnect();
        self.stopped.cancel();
    }
    pub async fn open(&self, purpose: Purpose) -> Result<Wire> {
        ensure!(
            matches!(
                purpose,
                Purpose::Artifact | Purpose::Upgrade | Purpose::Terminal
            ),
            "invalid business purpose"
        );
        if self.config.connection_mode == ConnectionMode::AgentToController {
            let token = fs::read_to_string(&self.config.token_file)?;
            return Ok(pier_protocol::framed(
                secure::connect(
                    &self.config.controller_tcp,
                    purpose,
                    &self.config.agent_id,
                    &secure::token_key(&token),
                )
                .await?,
            ));
        }
        let id = pier_protocol::new_id();
        let (tx, rx) = oneshot::channel();
        let session_id = {
            let mut locked = self.session.lock().unwrap();
            let session = locked.as_mut().context("controller offline")?;
            ensure!(
                session.pending.len() < 16 && !session.cancelled.is_cancelled(),
                "channel capacity exhausted"
            );
            session.pending.insert(
                id.clone(),
                Pending {
                    purpose,
                    sender: tx,
                },
            );
            session.id.clone()
        };
        // Also removes cancelled futures, including their unclaimed inbound streams.
        struct Cleanup<'a>(&'a Transport, String, String);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                if let Some(session) = self
                    .0
                    .session
                    .lock()
                    .unwrap()
                    .as_mut()
                    .filter(|s| s.id == self.1)
                {
                    session.pending.remove(&self.2);
                }
            }
        }
        let _cleanup = Cleanup(self, session_id.clone(), id.clone());
        self.events.send(Message::OpenChannel {
            session: session_id,
            id,
            purpose,
        })?;
        timeout(Duration::from_secs(10), rx)
            .await?
            .context("business channel closed")
    }
    pub async fn bind(self: &Arc<Self>) -> Result<Option<mpsc::Receiver<Accepted>>> {
        if self.config.connection_mode == ConnectionMode::AgentToController {
            return Ok(None);
        }
        let listener = TcpListener::bind(
            self.config
                .listen
                .context("agent listen address required")?,
        )
        .await
        .context("cannot bind agent listener")?;
        let key = secure::token_key(&fs::read_to_string(&self.config.token_file)?);
        let (tx, rx) = mpsc::channel(1);
        let state = self.clone();
        tokio::spawn(async move {
            let handshakes = Arc::new(Semaphore::new(32));
            let control = Arc::new(Semaphore::new(1));
            loop {
                let accepted = tokio::select! {
                    _ = state.stopped.cancelled() => break,
                    result = listener.accept() => result,
                };
                let Ok((mut socket, _)) = accepted else {
                    break;
                };
                let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                    continue;
                };
                let state = state.clone();
                let tx = tx.clone();
                let control = control.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let result: Result<()> = timeout(Duration::from_secs(10), async {
                        let (prelude, raw) = secure::read_prelude(&mut socket).await?;
                        ensure!(
                            prelude.id == state.config.agent_id,
                            "agent identity mismatch"
                        );
                        ensure!(
                            matches!(
                                prelude.purpose,
                                Purpose::Control
                                    | Purpose::Artifact
                                    | Purpose::Upgrade
                                    | Purpose::Terminal
                            ),
                            "invalid passive connection purpose"
                        );
                        let mut stream = secure::accept(socket, &raw, &key).await?;
                        stream.cancel_on(state.stopped.clone());
                        if prelude.purpose == Purpose::Control {
                            let permit = control
                                .try_acquire_owned()
                                .context("controller already connected")?;
                            tx.try_send((stream, permit))
                                .map_err(|_| anyhow::anyhow!("control queue unavailable"))?;
                        } else {
                            let mut wire = pier_protocol::framed(stream);
                            let Message::Channel {
                                session,
                                id,
                                purpose,
                            } = pier_protocol::receive(&mut wire).await?
                            else {
                                anyhow::bail!("channel binding required");
                            };
                            ensure!(purpose == prelude.purpose, "channel purpose mismatch");
                            let mut locked = state.session.lock().unwrap();
                            let current = locked.as_mut().context("controller offline")?;
                            ensure!(
                                current.id == session && !current.cancelled.is_cancelled(),
                                "stale channel session"
                            );
                            ensure!(
                                current
                                    .pending
                                    .get(&id)
                                    .is_some_and(|p| p.purpose == purpose),
                                "unknown channel request"
                            );
                            let pending = current.pending.remove(&id).unwrap();
                            wire.get_mut().cancel_on(current.cancelled.clone());
                            pending
                                .sender
                                .send(wire)
                                .map_err(|_| anyhow::anyhow!("channel request expired"))?;
                        }
                        Ok(())
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.into()));
                    if result.is_err() {
                        tracing::debug!("passive agent connection rejected");
                    }
                });
            }
        });
        Ok(Some(rx))
    }
}

#[cfg(test)]
mod tests;
