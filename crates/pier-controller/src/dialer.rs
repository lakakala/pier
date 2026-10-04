use crate::{
    AgentRecord, Controller,
    proxy::{self, Route},
};
use anyhow::{Context, Result, ensure};
use pier_protocol::{
    Message,
    connection::{Connection, ConnectionMode},
    secure::{self, Purpose},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

struct Worker {
    target: Route,
    cancelled: CancellationToken,
    task: JoinHandle<()>,
}
#[derive(Default)]
pub(crate) struct Registry {
    workers: Mutex<BTreeMap<String, Worker>>,
    errors: Mutex<BTreeMap<String, String>>,
}
impl Registry {
    pub fn cancel(&self, id: &str) {
        if let Some(worker) = self.workers.lock().unwrap().remove(id) {
            worker.cancelled.cancel();
            worker.task.abort();
        }
        self.errors.lock().unwrap().remove(id);
    }
    pub fn error(&self, id: &str) -> Option<String> {
        self.errors.lock().unwrap().get(id).cloned()
    }
    pub fn view(
        &self,
        id: &str,
        connection: &Connection,
        proxy_configured: bool,
        online: bool,
    ) -> Value {
        json!({"mode":connection.mode, "endpoint":connection.endpoint, "proxy_configured":proxy_configured,
            "state":if online { "connected" } else if connection.mode == ConnectionMode::ControllerToAgent { "reconnecting" } else { "waiting" },
            "last_error":if online { None } else { self.errors.lock().unwrap().get(id).cloned() }})
    }
}

/// One worker per durable target; reconstructs pending enrollment and control dials on restart.
pub(crate) async fn run(state: Arc<Controller>) {
    let slots = Arc::new(Semaphore::new(64));
    loop {
        if let Err(error) = reconcile(&state, &slots) {
            tracing::warn!(%error, "cannot refresh passive agents");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
fn reconcile(state: &Arc<Controller>, slots: &Arc<Semaphore>) -> Result<()> {
    let _mutation = state.mutation_lock.lock().unwrap();
    let mut targets = BTreeMap::new();
    for agent in state.store.list::<AgentRecord>("agents")? {
        if agent.connection.mode == ConnectionMode::ControllerToAgent {
            targets.insert(
                agent.id,
                Route {
                    endpoint: agent
                        .connection
                        .endpoint
                        .context("agent endpoint missing")?,
                    proxy: agent.proxy,
                },
            );
        }
    }
    for (id, endpoint) in state.passive_enrollments()? {
        targets.insert(format!("enroll:{id}"), endpoint);
    }
    state
        .dialer
        .errors
        .lock()
        .unwrap()
        .retain(|id, _| targets.contains_key(id));
    let mut workers = state.dialer.workers.lock().unwrap();
    workers.retain(|id, worker| {
        let keep = targets.get(id) == Some(&worker.target) && !worker.task.is_finished();
        if !keep {
            worker.cancelled.cancel();
            worker.task.abort();
        }
        keep
    });
    for (id, target) in targets {
        if workers.contains_key(&id) {
            continue;
        }
        let cancelled = CancellationToken::new();
        let worker_state = state.clone();
        let worker_id = id.clone();
        let worker_target = target.clone();
        let stop = cancelled.clone();
        let slots = slots.clone();
        let task = tokio::spawn(async move {
            let _guard = stop.clone().drop_guard();
            let mut delay = 1;
            loop {
                let attempt = async {
                    let permit = slots.acquire().await?;
                    if let Some(grant) = worker_id.strip_prefix("enroll:") {
                        let key = worker_state.enrollment_key(grant)?;
                        let io = proxy::connect(
                            &worker_target.endpoint,
                            worker_target.proxy.as_ref(),
                            Purpose::Enrollment,
                            grant,
                            &key,
                        )
                        .await?;
                        drop(permit);
                        worker_state
                            .dialer
                            .errors
                            .lock()
                            .unwrap()
                            .remove(&worker_id);
                        let mut wire = pier_protocol::framed(io);
                        wire.get_mut().cancel_on(stop.clone());
                        timeout(Duration::from_secs(20), async {
                            let Message::Enroll { request } =
                                pier_protocol::receive(&mut wire).await?
                            else {
                                anyhow::bail!("enrollment request required");
                            };
                            ensure!(
                                request.connection_mode == ConnectionMode::ControllerToAgent,
                                "wrong connection mode"
                            );
                            let credentials =
                                worker_state.issue_credentials(grant, request, &key)?;
                            pier_protocol::send(&mut wire, &Message::Enrolled { credentials }).await
                        })
                        .await??;
                    } else {
                        let agent: AgentRecord = worker_state
                            .store
                            .get("agents", &worker_id)?
                            .context("unknown agent")?;
                        let io = proxy::connect(
                            &worker_target.endpoint,
                            worker_target.proxy.as_ref(),
                            Purpose::Control,
                            &worker_id,
                            &secure::decode_key(&agent.token_hash)?,
                        )
                        .await?;
                        drop(permit);
                        worker_state
                            .dialer
                            .errors
                            .lock()
                            .unwrap()
                            .remove(&worker_id);
                        delay = 1;
                        crate::connection::control(
                            worker_state.clone(),
                            worker_id.clone(),
                            pier_protocol::framed(io),
                            stop.child_token(),
                            true,
                        )
                        .await?;
                    }
                    Ok::<_, anyhow::Error>(())
                };
                tokio::select! {
                    _ = stop.cancelled() => break,
                    result = attempt => {
                        if let Err(error) = result {
                            let detail = error.downcast_ref::<proxy::DialError>()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| "Agent 会话失败，请检查地址、监听端口及网络".into());
                            worker_state.dialer.errors.lock().unwrap().insert(worker_id.clone(), format!("{detail}，正在重试"));
                        }
                    }
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => (),
                }
                delay = (delay * 2).min(60);
            }
        });
        workers.insert(
            id,
            Worker {
                target,
                cancelled,
                task,
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
