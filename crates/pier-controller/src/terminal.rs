use crate::{
    AgentRecord, Controller, Job,
    api::{ApiError, ApiResult, bad, conflict},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message as WsMessage, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use futures_util::SinkExt;
use pier_protocol::{
    Message,
    terminal::{self as protocol, Frame, Wire},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::oneshot,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

struct Ticket {
    owner: String,
    agent: String,
    connection: String,
    instance: String,
    cols: u16,
    rows: u16,
    expires: u64,
    claimed: bool,
    wire: Option<oneshot::Sender<Wire>>,
    cancelled: CancellationToken,
}
#[derive(Default)]
pub(crate) struct Registry(Mutex<BTreeMap<String, Ticket>>);
impl Registry {
    pub fn revoke_owner(&self, owner: &str) {
        for entry in self.0.lock().unwrap().values().filter(|e| e.owner == owner) {
            entry.cancelled.cancel();
        }
    }
    pub fn revoke_all(&self) {
        for entry in self.0.lock().unwrap().values() {
            entry.cancelled.cancel();
        }
    }
    pub fn disconnect_agent(&self, agent: &str, connection: &str) {
        for entry in self
            .0
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.agent == agent && e.connection == connection)
        {
            entry.cancelled.cancel();
        }
    }
    fn remove(&self, id: &str) {
        if let Some(entry) = self.0.lock().unwrap().remove(id) {
            entry.cancelled.cancel();
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Open {
    cols: u16,
    rows: u16,
}
pub(crate) async fn create(
    State(state): State<Arc<Controller>>,
    Path((agent, instance)): Path<(String, String)>,
    headers: HeaderMap,
    Json(open): Json<Open>,
) -> ApiResult<Value> {
    let owner = state.authenticate(&headers)?;
    crate::auth::check_write(&state, &headers, &owner)?;
    protocol::size(open.cols, open.rows).map_err(|_| bad("terminal size must be 1..500"))?;
    if !pier_protocol::safe_id(&agent) || !pier_protocol::safe_id(&instance) {
        return Err(bad("invalid app identity"));
    }
    let record: AgentRecord = state.store.get("agents", &agent)?.ok_or_else(missing)?;
    if !record
        .report
        .apps
        .iter()
        .any(|app| app.instance == instance)
    {
        return Err(missing());
    }
    if state.upgrade_busy(&agent)?
        || state
            .store
            .list::<Job>("jobs")?
            .iter()
            .any(|job| job.agent_id == agent && job.active())
    {
        return Err(conflict("agent is deploying or upgrading"));
    }
    let connection = {
        let sessions = state.sessions.lock().unwrap();
        let session = sessions
            .get(&agent)
            .ok_or_else(|| conflict("agent offline"))?;
        if !session.terminal {
            return Err(conflict("upgrade agent to enable terminals"));
        }
        session.id.clone()
    };
    let id = pier_protocol::new_id();
    let expires = (pier_protocol::now() + 30).min(owner.expires_at);
    let mut registry = state.terminals.0.lock().unwrap();
    registry.retain(|_, t| {
        t.claimed || (!t.cancelled.is_cancelled() && t.expires > pier_protocol::now())
    });
    if registry.len() >= 64 || registry.values().filter(|t| t.agent == agent).count() >= 8 {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "terminal limit reached".into(),
        ));
    }
    registry.insert(
        id.clone(),
        Ticket {
            owner: owner.digest,
            agent,
            connection,
            instance,
            cols: open.cols,
            rows: open.rows,
            expires,
            claimed: false,
            wire: None,
            cancelled: CancellationToken::new(),
        },
    );
    Ok(Json(
        json!({"id":id,"websocket_url":format!("/v1/terminals/{id}/ws"),"expires_at":expires}),
    ))
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "terminal or app not found".into())
}

pub(crate) async fn websocket(
    State(state): State<Arc<Controller>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let owner = state.authenticate(&headers)?;
    crate::auth::check_origin(&state, &headers)?;
    let (sender, receiver) = oneshot::channel();
    let (agent, connection, message, cancelled) = {
        let mut registry = state.terminals.0.lock().unwrap();
        let ticket = registry.get_mut(&id).ok_or_else(missing)?;
        if ticket.owner != owner.digest {
            return Err(missing());
        }
        if ticket.claimed
            || ticket.expires <= pier_protocol::now()
            || ticket.cancelled.is_cancelled()
        {
            return Err(conflict("terminal connection expired or already used"));
        }
        ticket.claimed = true;
        ticket.expires = pier_protocol::now() + 10;
        ticket.wire = Some(sender);
        (
            ticket.agent.clone(),
            ticket.connection.clone(),
            Message::TerminalOpen {
                id: id.clone(),
                instance: ticket.instance.clone(),
                cols: ticket.cols,
                rows: ticket.rows,
            },
            ticket.cancelled.clone(),
        )
    };
    let failed_state = state.clone();
    let failed_id = id.clone();
    Ok(ws.max_message_size(protocol::CHUNK).max_frame_size(protocol::CHUNK)
        .write_buffer_size(0).max_write_buffer_size(protocol::WINDOW)
        .on_failed_upgrade(move |_| failed_state.terminals.remove(&failed_id))
        .on_upgrade(move |mut socket| async move {
            tracing::info!(terminal_id=%id, agent_id=%agent, username=%owner.username, "terminal connecting");
            let result: Result<String> = async {
                let sender = {
                    let sessions = state.sessions.lock().unwrap();
                    let session = sessions.get(&agent).context("agent offline")?;
                    ensure!(session.id == connection && session.terminal, "agent connection changed");
                    session.sender.clone()
                };
                ensure!(!cancelled.is_cancelled() && state.authenticate(&headers).is_ok(), "session revoked");
                timeout(Duration::from_secs(10), sender.send(message)).await??;
                let mut wire = tokio::select! {
                    _ = cancelled.cancelled() => anyhow::bail!("session revoked"),
                    _ = socket.recv() => anyhow::bail!("browser disconnected before terminal ready"),
                    wire = timeout(Duration::from_secs(10), receiver) => wire??,
                };
                bridge(&state, &headers, &mut socket, &mut wire, &cancelled).await
            }.await;
            let reason = result.unwrap_or_else(|_| "connection_closed".into());
            let _ = send_ws(&mut socket, WsMessage::Text(json!({"type":"exit","reason":reason}).to_string().into())).await;
            state.terminals.remove(&id);
            let _ = timeout(Duration::from_secs(2), socket.close()).await;
            tracing::info!(terminal_id=%id, agent_id=%agent, %reason, "terminal closed");
        }))
}

pub(crate) async fn attach(state: &Controller, agent: &str, mut wire: Wire) -> Result<()> {
    wire.codec_mut().set_max_frame_length(protocol::MAX_FRAME);
    let Frame::Attach { id } =
        timeout(Duration::from_secs(10), protocol::receive(&mut wire)).await??
    else {
        anyhow::bail!("terminal attachment required");
    };
    let connection = state
        .sessions
        .lock()
        .unwrap()
        .get(agent)
        .map(|s| s.id.clone())
        .context("agent offline")?;
    let sender = {
        let mut registry = state.terminals.0.lock().unwrap();
        let ticket = registry.get_mut(&id).context("unknown terminal")?;
        ensure!(
            ticket.agent == agent
                && ticket.connection == connection
                && ticket.claimed
                && ticket.expires > pier_protocol::now()
                && !ticket.cancelled.is_cancelled(),
            "invalid terminal attachment"
        );
        ticket.wire.take().context("terminal already attached")?
    };
    sender
        .send(wire)
        .map_err(|_| anyhow::anyhow!("terminal closed"))
}

async fn send_ws(socket: &mut WebSocket, message: WsMessage) -> Result<()> {
    timeout(Duration::from_secs(10), socket.send(message)).await??;
    Ok(())
}
async fn bridge(
    state: &Controller,
    headers: &HeaderMap,
    socket: &mut WebSocket,
    wire: &mut Wire,
    cancelled: &CancellationToken,
) -> Result<String> {
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut auth = tokio::time::interval(Duration::from_secs(1));
    let mut last_browser = Instant::now();
    let mut last_agent = Instant::now();
    let mut outstanding = 0usize;
    let mut ready = false;
    let started = Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => return Ok("session_revoked_or_agent_disconnected".into()),
            _ = tokio::time::sleep_until(last_browser.min(last_agent) + Duration::from_secs(45)) => anyhow::bail!("terminal heartbeat timeout"),
            _ = auth.tick() => {
                if state.authenticate(headers).is_err() { return Ok("login_expired".into()); }
                ensure!(ready || started.elapsed() < Duration::from_secs(10), "terminal startup timeout");
            }
            incoming = socket.recv() => {
                last_browser = Instant::now();
                let Some(incoming) = incoming else { return Ok("browser_closed".into()); };
                match incoming? {
                    WsMessage::Binary(bytes) => {
                        ensure!(ready && !bytes.is_empty() && bytes.len() <= protocol::CHUNK, "invalid input");
                        protocol::send(wire, &Frame::Input { data: protocol::encode(&bytes) }).await?;
                    }
                    WsMessage::Text(text) => {
                        ensure!(ready && text.len() <= 4096, "invalid terminal control");
                        let frame: Frame = serde_json::from_str(&text)?;
                        match &frame {
                            Frame::Resize { cols, rows } => protocol::size(*cols, *rows)?,
                            Frame::Ack { bytes } => { ensure!(*bytes > 0 && *bytes <= outstanding, "invalid acknowledgement"); outstanding -= bytes; }
                            _ => anyhow::bail!("unexpected browser message"),
                        }
                        protocol::send(wire, &frame).await?;
                    }
                    WsMessage::Ping(data) => send_ws(socket, WsMessage::Pong(data)).await?,
                    WsMessage::Pong(_) => (),
                    WsMessage::Close(_) => return Ok("browser_closed".into()),
                }
            }
            incoming = protocol::receive(wire) => {
                last_agent = Instant::now();
                match incoming? {
                    Frame::Ready { user, home } => {
                        ensure!(!ready && user.len() <= 128 && home.len() <= 4096, "invalid terminal identity");
                        ready = true;
                        send_ws(socket, WsMessage::Text(json!({"type":"ready","user":user,"home":home}).to_string().into())).await?;
                    }
                    Frame::Output { data } => {
                        ensure!(ready, "terminal not ready");
                        let bytes = protocol::decode(&data)?;
                        outstanding += bytes.len();
                        ensure!(outstanding <= protocol::WINDOW, "terminal window exceeded");
                        send_ws(socket, WsMessage::Binary(bytes.into())).await?;
                    }
                    Frame::Exit { code, reason } => {
                        ensure!(reason.len() <= 256, "invalid exit reason");
                        send_ws(socket, WsMessage::Text(json!({"type":"exit","code":code,"reason":reason}).to_string().into())).await?;
                        return Ok(reason);
                    }
                    Frame::Ping => protocol::send(wire, &Frame::Pong).await?,
                    Frame::Pong => (),
                    _ => anyhow::bail!("unexpected agent terminal message"),
                }
            }
            _ = heartbeat.tick() => {
                ensure!(last_browser.elapsed() < Duration::from_secs(45) && last_agent.elapsed() < Duration::from_secs(45), "terminal heartbeat timeout");
                send_ws(socket, WsMessage::Ping(Vec::new().into())).await?;
                protocol::send(wire, &Frame::Ping).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests;
