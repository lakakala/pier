use crate::{DurableState, Runtime};
use anyhow::{Context, Result, ensure};
use pier_pkg::PortProtocol;
use pier_protocol::{
    Message,
    forward::{self, Incoming, Listener, ListenerStatus, Mux, Request, Target},
};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::DuplexStream,
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Semaphore, mpsc},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct Manager {
    targets: Mutex<BTreeMap<String, CancellationToken>>,
}
impl Manager {
    pub fn close(&self, blueprint: &str) {
        if let Some(token) = self.targets.lock().unwrap().remove(blueprint) {
            token.cancel();
        }
    }
    pub fn close_all(&self) {
        for (_, token) in std::mem::take(&mut *self.targets.lock().unwrap()) {
            token.cancel();
        }
    }
}
impl Runtime {
    fn forwarding_target(&self, target: &Target) -> Result<(u32, CancellationToken)> {
        self.stopped()?;
        ensure!(!self.maintenance.load(Ordering::SeqCst), "agent upgrading");
        let mut targets = self.forwarding.targets.lock().unwrap();
        let state: DurableState = self.store.get("runtime", "state")?.unwrap_or_default();
        let blueprint = pier_protocol::hash(&target.blueprint);
        ensure!(
            state
                .pending
                .as_ref()
                .is_none_or(|pending| pending.blueprint_id != blueprint),
            "deployment changing"
        );
        let snapshot = state
            .blueprints
            .get(&blueprint)
            .context("blueprint not installed")?;
        ensure!(
            snapshot.deployment_id.as_ref() == Some(&target.deployment),
            "deployment changed"
        );
        let app = snapshot
            .apps
            .iter()
            .find(|app| app.instance == target.instance)
            .context("app not installed")?;
        let port = app
            .manifest
            .ports
            .get(&target.name)
            .context("port not declared")?;
        ensure!(
            port.port == target.port && port.protocol == target.protocol,
            "declared port mismatch"
        );
        let active = self.active.lock().unwrap();
        let supervisor = active.get(&blueprint).context("blueprint stopped")?;
        let statuses = supervisor.status.lock().unwrap();
        let status = statuses
            .iter()
            .find(|status| status.instance == target.instance && status.state == "running")
            .context("app not running")?;
        Ok((
            status.pid.context("app not running")?,
            targets.entry(blueprint).or_default().child_token(),
        ))
    }
}

pub(crate) fn serve(
    runtime: Arc<Runtime>,
    mux: Mux,
    mut incoming: mpsc::Receiver<Incoming>,
    mut configs: mpsc::Receiver<Vec<Listener>>,
    sender: mpsc::Sender<Message>,
    cancelled: CancellationToken,
) {
    tokio::spawn(async move {
        let statuses: Arc<Mutex<BTreeMap<String, ListenerStatus>>> = Arc::default();
        let mut listeners: BTreeMap<
            String,
            (Listener, CancellationToken, tokio::task::JoinHandle<()>),
        > = BTreeMap::new();
        let mut configured = false;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = cancelled.cancelled() => break,
                request = incoming.recv() => {
                    let Some(request) = request else { break; };
                    let runtime = runtime.clone(); let token = cancelled.clone();
                    tokio::spawn(async move {
                        tokio::select! { _ = token.cancelled() => (), _ = backend(runtime, request) => () }
                    });
                }
                config = configs.recv() => {
                    let Some(config) = config else { break; };
                    configured = true;
                    if config.len() > 1024 || config.iter().any(|listener| listener.port == 0 || !pier_protocol::safe_id(&listener.id)) { cancelled.cancel(); break; }
                    let desired: BTreeMap<_, _> = config.into_iter().map(|listener| (listener.id.clone(), listener)).collect();
                    let removed: Vec<_> = listeners.iter().filter(|(id, (old, _, _))| desired.get(*id) != Some(old)).map(|(id, _)| id.clone()).collect();
                    for id in removed {
                        if let Some((_, token, task)) = listeners.remove(&id) { token.cancel(); let _ = task.await; }
                        statuses.lock().unwrap().remove(&id);
                    }
                    for (id, config) in desired {
                        if listeners.contains_key(&id) { continue; }
                        statuses.lock().unwrap().insert(id.clone(), ListenerStatus { id: id.clone(), state: "pending".into(), reason: None });
                        let token = cancelled.child_token();
                        let task = tokio::spawn(entrance(config.clone(), mux.clone(), statuses.clone(), token.clone()));
                        listeners.insert(id, (config, token, task));
                    }
                }
                _ = tick.tick(), if configured => {
                    let statuses = statuses.lock().unwrap().values().cloned().collect();
                    if sender.try_send(Message::PortStatus { statuses }).is_err() { cancelled.cancel(); break; }
                }
            }
        }
        for (_, (_, token, task)) in listeners {
            token.cancel();
            let _ = task.await;
        }
    });
}

async fn backend(runtime: Arc<Runtime>, mut incoming: Incoming) -> Result<()> {
    let result = async {
        let Request::Target { target } = incoming.request.clone() else { anyhow::bail!("target request required"); };
        let (pid, cancelled) = runtime.forwarding_target(&target)?;
        let validate = async {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if !runtime.forwarding_target(&target).is_ok_and(|(current, _)| current == pid) { break; }
            }
        };
        tokio::select! {
            _ = cancelled.cancelled() => anyhow::bail!("deployment stopped"),
            _ = validate => anyhow::bail!("app process changed"),
            result = async {
                match target.protocol {
                    PortProtocol::Tcp => {
                        let mut socket = timeout(Duration::from_secs(5), TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, target.port))).await.context("backend connect timeout")?.context("backend port unavailable")?;
                        incoming.accept().await?;
                        tokio::io::copy_bidirectional(&mut incoming.stream, &mut socket).await?;
                    }
                    PortProtocol::Udp => {
                        let socket = UdpSocket::bind("127.0.0.1:0").await?;
                        socket.connect((std::net::Ipv4Addr::LOCALHOST, target.port)).await?;
                        incoming.accept().await?;
                        udp_backend(&mut incoming.stream, socket).await?;
                    }
                }
                Ok(())
            } => result,
        }
    }.await;
    if let Err(error) = &result {
        incoming.reject(&error.to_string());
    }
    result
}

async fn entrance(
    listener: Listener,
    mux: Mux,
    statuses: Arc<Mutex<BTreeMap<String, ListenerStatus>>>,
    cancelled: CancellationToken,
) {
    loop {
        let result = tokio::select! {
            _ = cancelled.cancelled() => break,
            result = async {
                match listener.protocol {
                    PortProtocol::Tcp => {
                        let socket = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, listener.port)).await?;
                        ready(&statuses, &listener.id);
                        tcp_entrance(socket, &listener.id, mux.clone(), cancelled.clone()).await
                    }
                    PortProtocol::Udp => {
                        let socket = Arc::new(UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, listener.port)).await?);
                        ready(&statuses, &listener.id);
                        udp_entrance(socket, &listener.id, mux.clone(), cancelled.clone()).await
                    }
                }
            } => result,
        };
        let reason = match result {
            Ok(()) => "入口已关闭".into(),
            Err(error) => format!(
                "监听 {} {:?} 失败：{error}",
                listener.port, listener.protocol
            ),
        };
        statuses.lock().unwrap().insert(
            listener.id.clone(),
            ListenerStatus {
                id: listener.id.clone(),
                state: "error".into(),
                reason: Some(reason),
            },
        );
        tokio::select! { _ = cancelled.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(5)) => () }
    }
}
fn ready(statuses: &Mutex<BTreeMap<String, ListenerStatus>>, id: &str) {
    statuses.lock().unwrap().insert(
        id.into(),
        ListenerStatus {
            id: id.into(),
            state: "ready".into(),
            reason: None,
        },
    );
}
async fn tcp_entrance(
    listener: TcpListener,
    route: &str,
    mux: Mux,
    cancelled: CancellationToken,
) -> Result<()> {
    let slots = Arc::new(Semaphore::new(64));
    loop {
        let (mut socket, _) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::debug!(%route, "port entrance connection limit reached");
            continue;
        };
        let mux = mux.clone();
        let route = route.to_string();
        let cancelled = cancelled.clone();
        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = cancelled.cancelled() => (),
                _ = async {
                    let mut stream = mux.open(Request::Ingress { route: route.clone() }).await.map_err(|error| {
                        tracing::debug!(%route, %error, "port forwarding open rejected");
                        error
                    })?;
                    tokio::io::copy_bidirectional(&mut socket, &mut stream).await?;
                    Ok::<_, anyhow::Error>(())
                } => (),
            }
        });
    }
}
async fn udp_entrance(
    socket: Arc<UdpSocket>,
    route: &str,
    mux: Mux,
    cancelled: CancellationToken,
) -> Result<()> {
    let peers: Arc<Mutex<BTreeMap<SocketAddr, mpsc::Sender<Vec<u8>>>>> = Arc::default();
    let mut buffer = vec![0; forward::MAX_DATAGRAM + 1];
    loop {
        let (size, peer) = socket.recv_from(&mut buffer).await?;
        if size > forward::MAX_DATAGRAM {
            continue;
        }
        let mut peers_guard = peers.lock().unwrap();
        if let Some(sender) = peers_guard.get(&peer) {
            let _ = sender.try_send(buffer[..size].to_vec());
            continue;
        }
        if peers_guard.len() >= 64 {
            tracing::debug!(%route, "UDP entrance session limit reached");
            continue;
        }
        let (sender, receiver) = mpsc::channel(8);
        let _ = sender.try_send(buffer[..size].to_vec());
        peers_guard.insert(peer, sender);
        let peers = peers.clone();
        let mux = mux.clone();
        let socket = socket.clone();
        let route = route.to_string();
        let cancelled = cancelled.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancelled.cancelled() => (),
                _ = async {
                    let mut stream = mux.open(Request::Ingress { route: route.clone() }).await.map_err(|error| {
                        tracing::debug!(%route, %error, "port forwarding open rejected");
                        error
                    })?;
                    udp_peer(&mut stream, socket, peer, receiver).await
                } => (),
            }
            peers.lock().unwrap().remove(&peer);
        });
    }
}
async fn idle(last: &AtomicU64) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if pier_protocol::now().saturating_sub(last.load(Ordering::Relaxed)) >= 60 {
            return;
        }
    }
}
async fn udp_peer(
    stream: &mut DuplexStream,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    mut datagrams: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let (mut read, mut write) = tokio::io::split(stream);
    let last = AtomicU64::new(pier_protocol::now());
    let send = async {
        while let Some(data) = datagrams.recv().await {
            forward::write_datagram(&mut write, &data).await?;
            last.store(pier_protocol::now(), Ordering::Relaxed);
        }
        Ok::<_, anyhow::Error>(())
    };
    let receive = async {
        loop {
            let data = forward::read_datagram(&mut read).await?;
            socket.send_to(&data, peer).await?;
            last.store(pier_protocol::now(), Ordering::Relaxed);
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! { result = send => result, result = receive => result, _ = idle(&last) => Ok(()) }
}
async fn udp_backend(stream: &mut DuplexStream, socket: UdpSocket) -> Result<()> {
    let (mut read, mut write) = tokio::io::split(stream);
    let last = AtomicU64::new(pier_protocol::now());
    let send = async {
        loop {
            let data = forward::read_datagram(&mut read).await?;
            socket.send(&data).await?;
            last.store(pier_protocol::now(), Ordering::Relaxed);
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    let receive = async {
        let mut buffer = vec![0; forward::MAX_DATAGRAM + 1];
        loop {
            let size = socket.recv(&mut buffer).await?;
            if size <= forward::MAX_DATAGRAM {
                forward::write_datagram(&mut write, &buffer[..size]).await?;
            }
            last.store(pier_protocol::now(), Ordering::Relaxed);
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! { result = send => result, result = receive => result, _ = idle(&last) => Ok(()) }
}
