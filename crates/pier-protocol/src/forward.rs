//! Bounded logical streams multiplexed on the authenticated control connection.
use crate::Message;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use pier_pkg::PortProtocol;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    sync::{Semaphore, mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

pub const CAPABILITY: &str = "port_forward_v1";
pub const CHUNK: usize = 32768;
pub const WINDOW: usize = 65536;
pub const MAX_FLOWS: usize = 128;
pub const MAX_DATAGRAM: usize = 65507;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub blueprint: String,
    pub deployment: String,
    pub instance: String,
    pub name: String,
    pub protocol: PortProtocol,
    pub port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub id: String,
    pub protocol: PortProtocol,
    pub port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerStatus {
    pub id: String,
    pub state: String,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ingress { route: String },
    Target { target: Target },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Open { id: String, request: Request },
    Ready { id: String },
    Data { id: String, data: String },
    Credit { id: String, bytes: u32 },
    Fin { id: String },
    Reset { id: String, reason: String },
}

// Budget by bytes, not frame count: a valid 64 KiB window can arrive as
// 65,536 one-byte writes. Coalesce fragments without allocating one queue slot each.
#[derive(Default)]
struct Input {
    state: Mutex<(VecDeque<u8>, bool)>,
    changed: tokio::sync::Notify,
}
impl Input {
    async fn next(&self) -> Option<Vec<u8>> {
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self.state.lock().unwrap();
                if !state.0.is_empty() {
                    let count = state.0.len().min(CHUNK);
                    return Some(state.0.drain(..count).collect());
                }
                if state.1 {
                    return None;
                }
            }
            notified.await;
        }
    }
    fn push(&self, bytes: Vec<u8>) {
        self.state.lock().unwrap().0.extend(bytes);
        self.changed.notify_one();
    }
    fn finish(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_one();
    }
}
struct Entry {
    input: Arc<Input>,
    ready: Option<oneshot::Sender<Result<()>>>,
    credits: Arc<Semaphore>,
    outstanding: Arc<AtomicUsize>,
    allowance: Arc<AtomicUsize>,
    cancelled: CancellationToken,
    received_fin: bool,
}
struct Inner {
    flows: Mutex<BTreeMap<String, Entry>>,
    high: mpsc::Sender<Frame>,
    data: mpsc::Sender<Frame>,
    incoming: mpsc::Sender<Incoming>,
    cancelled: CancellationToken,
}
#[derive(Clone)]
pub struct Mux(Arc<Inner>);

pub struct Incoming {
    pub request: Request,
    pub stream: DuplexStream,
    id: String,
    mux: Mux,
}
impl Incoming {
    pub async fn accept(&self) -> Result<()> {
        self.mux
            .high(Frame::Ready {
                id: self.id.clone(),
            })
            .await
    }
    pub fn reject(&self, reason: &str) {
        self.mux.reset(&self.id, reason);
    }
}
impl Mux {
    async fn high(&self, frame: Frame) -> Result<()> {
        tokio::select! {
            _ = self.0.cancelled.cancelled() => anyhow::bail!("control disconnected"),
            result = self.0.high.send(frame) => result.context("control disconnected"),
        }
    }
    fn reset(&self, id: &str, reason: &str) {
        if let Some(flow) = self.0.flows.lock().unwrap().remove(id) {
            flow.cancelled.cancel();
        }
        // A peer sending faster than bounded control processing permits is disconnected.
        if self
            .0
            .high
            .try_send(Frame::Reset {
                id: id.into(),
                reason: reason.into(),
            })
            .is_err()
        {
            self.0.cancelled.cancel();
        }
    }
    fn allocate(
        &self,
        id: String,
        ready: Option<oneshot::Sender<Result<()>>>,
    ) -> Result<DuplexStream> {
        let mut flows = self.0.flows.lock().unwrap();
        ensure!(
            flows.len() < MAX_FLOWS && !flows.contains_key(&id),
            "forwarding connection limit or duplicate stream"
        );
        let (external, internal) = tokio::io::duplex(WINDOW);
        let input = Arc::new(Input::default());
        let output = input.clone();
        let credits = Arc::new(Semaphore::new(WINDOW));
        let outstanding = Arc::new(AtomicUsize::new(0));
        let allowance = Arc::new(AtomicUsize::new(WINDOW));
        let cancelled = self.0.cancelled.child_token();
        flows.insert(
            id.clone(),
            Entry {
                input,
                ready,
                credits: credits.clone(),
                outstanding: outstanding.clone(),
                allowance: allowance.clone(),
                cancelled: cancelled.clone(),
                received_fin: false,
            },
        );
        let mux = self.clone();
        tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(internal);
            let send = async {
                let mut buffer = vec![0; CHUNK];
                loop {
                    let count = read.read(&mut buffer).await?;
                    if count == 0 {
                        // FIN must stay behind previously queued data.
                        mux.0.data.send(Frame::Fin { id: id.clone() }).await?;
                        break;
                    }
                    credits.acquire_many(count as u32).await?.forget();
                    outstanding.fetch_add(count, Ordering::SeqCst);
                    mux.0
                        .data
                        .send(Frame::Data {
                            id: id.clone(),
                            data: STANDARD.encode(&buffer[..count]),
                        })
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let receive = async {
                while let Some(bytes) = output.next().await {
                    write.write_all(&bytes).await?;
                    allowance.fetch_add(bytes.len(), Ordering::SeqCst);
                    mux.high(Frame::Credit {
                        id: id.clone(),
                        bytes: bytes.len() as u32,
                    })
                    .await?;
                }
                write.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            };
            let result = tokio::select! {
                _ = cancelled.cancelled() => Ok(((), ())),
                result = async { tokio::try_join!(send, receive) } => result,
            };
            if result.is_err() {
                mux.reset(&id, "stream closed");
            }
            mux.0.flows.lock().unwrap().remove(&id);
        });
        Ok(external)
    }
    pub async fn open(&self, request: Request) -> Result<DuplexStream> {
        let id = crate::new_id();
        let (tx, rx) = oneshot::channel();
        let stream = self.allocate(id.clone(), Some(tx))?;
        struct PendingOpen<'a> {
            mux: &'a Mux,
            id: String,
            completed: bool,
        }
        impl Drop for PendingOpen<'_> {
            fn drop(&mut self) {
                if !self.completed {
                    self.mux.reset(&self.id, "open cancelled");
                }
            }
        }
        let mut guard = PendingOpen {
            mux: self,
            id: id.clone(),
            completed: false,
        };
        let result = async {
            self.high(Frame::Open { id: id.clone(), request }).await?;
            tokio::select! {
                _ = self.0.cancelled.cancelled() => anyhow::bail!("control disconnected"),
                result = timeout(Duration::from_secs(10), rx) => result.context("forwarding open timeout")?.context("forwarding rejected")?,
            }
        }.await;
        if let Err(error) = result {
            self.reset(&id, "open failed");
            return Err(error);
        }
        guard.completed = true;
        Ok(stream)
    }
    fn handle(&self, frame: Frame) -> Result<()> {
        if let Frame::Open { id, request } = frame {
            ensure!(crate::safe_id(&id), "invalid forwarding stream id");
            match self.allocate(id.clone(), None) {
                Ok(stream) => {
                    if self
                        .0
                        .incoming
                        .try_send(Incoming {
                            request,
                            stream,
                            id: id.clone(),
                            mux: self.clone(),
                        })
                        .is_err()
                    {
                        self.reset(&id, "forwarding connection limit");
                    }
                }
                Err(_) => self.reset(&id, "forwarding connection limit or duplicate stream"),
            }
            return Ok(());
        }
        let id = match &frame {
            Frame::Ready { id }
            | Frame::Data { id, .. }
            | Frame::Credit { id, .. }
            | Frame::Fin { id }
            | Frame::Reset { id, .. } => id.clone(),
            Frame::Open { .. } => unreachable!(),
        };
        let mut flows = self.0.flows.lock().unwrap();
        let Some(flow) = flows.get_mut(&id) else {
            return Ok(());
        };
        let valid = match frame {
            Frame::Ready { .. } => {
                if let Some(ready) = flow.ready.take() {
                    let _ = ready.send(Ok(()));
                    true
                } else {
                    false
                }
            }
            Frame::Reset { reason, .. } => {
                if let Some(ready) = flow.ready.take() {
                    let _ = ready.send(Err(anyhow::anyhow!(
                        "{}",
                        reason.chars().take(200).collect::<String>()
                    )));
                }
                flow.cancelled.cancel();
                flows.remove(&id);
                return Ok(());
            }
            Frame::Data { data, .. } => {
                let bytes = if data.len() <= CHUNK.div_ceil(3) * 4 {
                    STANDARD.decode(data).ok()
                } else {
                    None
                };
                if let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty() && bytes.len() <= CHUNK)
                {
                    let valid = !flow.received_fin
                        && flow.ready.is_none()
                        && flow
                            .allowance
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                n.checked_sub(bytes.len())
                            })
                            .is_ok();
                    if valid {
                        flow.input.push(bytes);
                    }
                    valid
                } else {
                    false
                }
            }
            Frame::Credit { bytes, .. } => {
                if bytes > 0
                    && flow
                        .outstanding
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                            n.checked_sub(bytes as usize)
                        })
                        .is_ok()
                {
                    flow.credits.add_permits(bytes as usize);
                    true
                } else {
                    false
                }
            }
            Frame::Fin { .. } => {
                let valid = !flow.received_fin;
                flow.received_fin = true;
                flow.input.finish();
                valid
            }
            Frame::Open { .. } => unreachable!(),
        };
        drop(flows);
        if !valid {
            self.reset(&id, "invalid stream frame or receive window exceeded");
        }
        Ok(())
    }
}

/// Split the existing wire after its initial handshake. Only the writer owns the
/// sink; data tasks never await socket I/O inside the message receive loop.
pub fn session<S>(
    wire: tokio_util::codec::Framed<S, tokio_util::codec::LengthDelimitedCodec>,
    cancelled: CancellationToken,
    enabled: bool,
) -> (
    mpsc::Sender<Message>,
    mpsc::Receiver<Message>,
    Mux,
    mpsc::Receiver<Incoming>,
)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (control, mut commands) = mpsc::channel::<Message>(32);
    let (received, messages) = mpsc::channel(32);
    let (high, mut priority) = mpsc::channel(256);
    let (data, mut payloads) = mpsc::channel(64);
    let (incoming, requests) = mpsc::channel(32);
    let mux = Mux(Arc::new(Inner {
        flows: Mutex::new(BTreeMap::new()),
        high,
        data,
        incoming,
        cancelled: cancelled.clone(),
    }));
    let reader_mux = mux.clone();
    tokio::spawn(async move {
        let _guard = cancelled.clone().drop_guard();
        let (mut sink, mut source) = wire.split();
        let read = async {
            while let Some(bytes) = source.next().await {
                let message: Message = serde_json::from_slice(&bytes?)?;
                if let Message::Forward { frame } = message {
                    ensure!(enabled, "port forwarding capability required");
                    reader_mux.handle(frame)?;
                } else {
                    // Never let an overloaded control consumer block stream demultiplexing.
                    received
                        .try_send(message)
                        .context("control receiver overloaded")?;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let write = async {
            let mut priority_count = 0;
            loop {
                let message = if let Ok(command) = commands.try_recv() {
                    command
                } else if priority_count >= 8 {
                    priority_count = 0;
                    match payloads.try_recv() {
                        Ok(frame) => Message::Forward { frame },
                        Err(_) => tokio::select! {
                            biased;
                            Some(message) = commands.recv() => message,
                            Some(frame) = priority.recv() => Message::Forward { frame },
                            Some(frame) = payloads.recv() => Message::Forward { frame },
                            else => break,
                        },
                    }
                } else {
                    priority_count += 1;
                    tokio::select! {
                        biased;
                        Some(message) = commands.recv() => message,
                        Some(frame) = priority.recv() => Message::Forward { frame },
                        Some(frame) = payloads.recv() => { priority_count = 0; Message::Forward { frame } },
                        else => break,
                    }
                };
                let bytes = serde_json::to_vec(&message)?;
                ensure!(
                    bytes.len() <= crate::MAX_FRAME,
                    "message exceeds maximum frame length"
                );
                timeout(Duration::from_secs(10), sink.send(bytes.into())).await??;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            _ = cancelled.cancelled() => (),
            _ = read => (),
            _ = write => (),
        }
    });
    (control, messages, mux, requests)
}

pub async fn read_datagram<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let len = reader.read_u16().await? as usize;
    ensure!(len <= MAX_DATAGRAM, "datagram too large");
    let mut data = vec![0; len];
    reader.read_exact(&mut data).await?;
    Ok(data)
}
pub async fn write_datagram<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> Result<()> {
    ensure!(data.len() <= MAX_DATAGRAM, "datagram too large");
    writer.write_u16(data.len() as u16).await?;
    writer.write_all(data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (
        Mux,
        mpsc::Receiver<Incoming>,
        mpsc::Sender<Message>,
        mpsc::Receiver<Message>,
        CancellationToken,
    ) {
        let cancel = CancellationToken::new();
        let (a, b) = tokio::io::duplex(4096);
        let (tx, _, left, _) = session(crate::framed(a), cancel.clone(), true);
        let (_, rx, _, incoming) = session(crate::framed(b), cancel.clone(), true);
        (left, incoming, tx, rx, cancel)
    }
    #[tokio::test]
    async fn large_stream_half_close_and_control_share_one_wire() {
        let (mux, mut incoming, control, mut received, cancel) = pair();
        let server = tokio::spawn(async move {
            let mut stream = incoming.recv().await.unwrap();
            stream.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.stream.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes.len(), 2 * 1024 * 1024);
            stream.stream.write_all(&bytes).await.unwrap();
            stream.stream.shutdown().await.unwrap();
        });
        timeout(Duration::from_secs(5), async {
            let mut stream = mux
                .open(Request::Ingress {
                    route: "route".into(),
                })
                .await
                .unwrap();
            let client = tokio::spawn(async move {
                let payload = vec![0xa5; 2 * 1024 * 1024];
                stream.write_all(&payload).await.unwrap();
                stream.shutdown().await.unwrap();
                let mut reply = Vec::new();
                stream.read_to_end(&mut reply).await.unwrap();
                assert_eq!(reply, payload);
            });
            for _ in 0..20 {
                control.send(Message::Ping).await.unwrap();
                assert!(matches!(received.recv().await, Some(Message::Ping)));
            }
            client.await.unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
        cancel.cancel();
    }
    #[tokio::test]
    async fn slow_stream_is_bounded_and_does_not_block_other_streams_or_heartbeat() {
        let (mux, mut incoming, control, mut received, cancel) = pair();
        let m = mux.clone();
        let open = tokio::spawn(async move {
            m.open(Request::Ingress {
                route: "slow".into(),
            })
            .await
            .unwrap()
        });
        let slow = incoming.recv().await.unwrap();
        slow.accept().await.unwrap();
        let mut client = open.await.unwrap();
        let stalled = tokio::spawn(async move { client.write_all(&vec![1; 1024 * 1024]).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!stalled.is_finished());
        timeout(Duration::from_secs(2), async {
            control.send(Message::Ping).await.unwrap();
            assert!(matches!(received.recv().await, Some(Message::Ping)));
            let m = mux.clone();
            let open = tokio::spawn(async move {
                m.open(Request::Ingress {
                    route: "fast".into(),
                })
                .await
                .unwrap()
            });
            let mut fast = incoming.recv().await.unwrap();
            fast.accept().await.unwrap();
            let mut stream = open.await.unwrap();
            stream.write_all(b"fast").await.unwrap();
            let mut data = [0; 4];
            fast.stream.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"fast");
        })
        .await
        .unwrap();
        {
            let flows = mux.0.flows.lock().unwrap();
            assert!(
                flows
                    .values()
                    .all(|entry| entry.outstanding.load(Ordering::SeqCst) <= WINDOW)
            );
        }
        cancel.cancel();
        assert!(
            timeout(Duration::from_secs(1), stalled)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    #[tokio::test]
    async fn datagram_boundaries_empty_and_maximum_payload_survive_chunking() {
        let (mux, mut incoming, _, _, cancel) = pair();
        let server = tokio::spawn(async move {
            let mut peer = incoming.recv().await.unwrap();
            peer.accept().await.unwrap();
            for expected in [vec![], vec![3; MAX_DATAGRAM], vec![7; 17]] {
                assert_eq!(read_datagram(&mut peer.stream).await.unwrap(), expected);
                write_datagram(&mut peer.stream, &expected).await.unwrap();
            }
        });
        timeout(Duration::from_secs(3), async {
            let mut stream = mux
                .open(Request::Ingress {
                    route: "udp".into(),
                })
                .await
                .unwrap();
            for data in [vec![], vec![3; MAX_DATAGRAM], vec![7; 17]] {
                write_datagram(&mut stream, &data).await.unwrap();
                assert_eq!(read_datagram(&mut stream).await.unwrap(), data);
            }
            server.await.unwrap();
        })
        .await
        .unwrap();
        cancel.cancel();
    }
    #[tokio::test]
    async fn advertised_window_accepts_tiny_fragments_and_rejects_overrun() {
        let (mux, _, _, _, cancel) = pair();
        let mut stream = mux.allocate("tiny".into(), None).unwrap();
        // Do not yield: exhaust the entire advertised byte window before the
        // consumer runs. Small TCP writes must not hit an unrelated frame limit.
        for _ in 0..WINDOW {
            mux.handle(Frame::Data {
                id: "tiny".into(),
                data: "YQ==".into(),
            })
            .unwrap();
        }
        assert!(mux.0.flows.lock().unwrap().contains_key("tiny"));
        mux.handle(Frame::Fin { id: "tiny".into() }).unwrap();
        let mut received = Vec::new();
        timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, vec![b'a'; WINDOW]);
        let _overrun = mux.allocate("overrun".into(), None).unwrap();
        for _ in 0..2 {
            mux.handle(Frame::Data {
                id: "overrun".into(),
                data: STANDARD.encode(vec![0; CHUNK]),
            })
            .unwrap();
        }
        mux.handle(Frame::Data {
            id: "overrun".into(),
            data: "YQ==".into(),
        })
        .unwrap();
        assert!(!mux.0.flows.lock().unwrap().contains_key("overrun"));
        cancel.cancel();
    }
    #[tokio::test]
    async fn cancelled_open_and_connection_release_streams() {
        let (mux, mut incoming, _, _, cancel) = pair();
        let m = mux.clone();
        let task = tokio::spawn(async move {
            m.open(Request::Ingress {
                route: "pending".into(),
            })
            .await
        });
        let pending = incoming.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(mux.0.flows.lock().unwrap().is_empty());
        drop(pending);
        cancel.cancel();
    }
}
