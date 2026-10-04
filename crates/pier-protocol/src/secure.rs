//! Certificate-free, PSK-authenticated Noise transport. All application bytes
//! pass through encrypted records; plaintext negotiation is bound to the handshake.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::TcpStream,
    time::timeout,
};

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const MAX_RECORD: usize = 65535;
const CHUNK: usize = 32768;
const MAGIC: &[u8; 8] = b"PIERv2\0\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Control,
    Artifact,
    Enrollment,
    EnrollmentAck,
    Upgrade,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prelude {
    pub version: u32,
    pub purpose: Purpose,
    pub id: String,
}

pub fn token_key(token: &str) -> [u8; 32] {
    Sha256::digest(token.trim().as_bytes()).into()
}
pub fn decode_key(hex: &str) -> Result<[u8; 32]> {
    ensure!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid key encoding"
    );
    let mut key = [0; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)?;
    }
    Ok(key)
}
fn prologue(raw: &[u8]) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(raw);
    bytes
}
async fn write_record<S: AsyncWrite + Unpin>(io: &mut S, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_RECORD,
        "invalid record length"
    );
    io.write_u16(bytes.len() as u16).await?;
    io.write_all(bytes).await?;
    io.flush().await?;
    Ok(())
}
async fn read_record<S: AsyncRead + Unpin>(io: &mut S) -> Result<Vec<u8>> {
    let len = io.read_u16().await? as usize;
    ensure!(len > 0, "empty encrypted record");
    let mut data = vec![0; len];
    io.read_exact(&mut data).await?;
    Ok(data)
}

pub async fn read_prelude<S: AsyncRead + Unpin>(io: &mut S) -> Result<(Prelude, Vec<u8>)> {
    let mut magic = [0; 8];
    io.read_exact(&mut magic).await?;
    ensure!(&magic == MAGIC, "protocol v2 required");
    let len = io.read_u16().await? as usize;
    ensure!(len > 0 && len <= 1024, "invalid prelude length");
    let mut raw = vec![0; len];
    io.read_exact(&mut raw).await?;
    let prelude: Prelude = serde_json::from_slice(&raw)?;
    ensure!(
        prelude.version == crate::VERSION && crate::safe_id(&prelude.id),
        "invalid prelude"
    );
    Ok((prelude, raw))
}

pub async fn connect(
    endpoint: &str,
    purpose: Purpose,
    id: &str,
    key: &[u8; 32],
) -> Result<SecureStream> {
    timeout(Duration::from_secs(10), async {
        connect_stream(TcpStream::connect(endpoint).await?, purpose, id, key).await
    })
    .await
    .context("encrypted connection timed out")?
}

/// Authenticate an already connected TCP stream (including a proxy tunnel).
/// The caller must bound connection establishment and this handshake with one timeout.
pub async fn connect_stream(
    mut io: TcpStream,
    purpose: Purpose,
    id: &str,
    key: &[u8; 32],
) -> Result<SecureStream> {
    ensure!(crate::safe_id(id), "invalid peer id");
    io.set_nodelay(true)?;
    let raw = serde_json::to_vec(&Prelude {
        version: crate::VERSION,
        purpose,
        id: id.into(),
    })?;
    io.write_all(MAGIC).await?;
    write_record(&mut io, &raw).await?;
    let binding = prologue(&raw);
    let mut handshake = snow::Builder::new(PATTERN.parse()?)
        .psk(0, key)?
        .prologue(&binding)?
        .build_initiator()?;
    let mut output = vec![0; MAX_RECORD];
    let n = handshake.write_message(&[], &mut output)?;
    write_record(&mut io, &output[..n]).await?;
    let reply = read_record(&mut io).await?;
    ensure!(
        handshake.read_message(&reply, &mut output)? == 0,
        "unexpected handshake payload"
    );
    Ok::<_, anyhow::Error>(SecureStream::new(io, handshake.into_transport_mode()?))
}

/// The caller has already read and validated the bounded prelude and selected
/// a key. Call under a timeout, without holding any controller state locks.
pub async fn accept(mut io: TcpStream, raw: &[u8], key: &[u8; 32]) -> Result<SecureStream> {
    io.set_nodelay(true)?;
    let binding = prologue(raw);
    let mut handshake = snow::Builder::new(PATTERN.parse()?)
        .psk(0, key)?
        .prologue(&binding)?
        .build_responder()?;
    let mut output = vec![0; MAX_RECORD];
    let first = read_record(&mut io).await?;
    ensure!(
        handshake.read_message(&first, &mut output)? == 0,
        "unexpected handshake payload"
    );
    let n = handshake.write_message(&[], &mut output)?;
    write_record(&mut io, &output[..n]).await?;
    Ok(SecureStream::new(io, handshake.into_transport_mode()?))
}

/// A bounded, cancellation-safe encrypted stream. Flush completes the network
/// write, so dropping a framed stream after `send` cannot lose its final reply.
pub struct SecureStream {
    cancelled: Option<Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
    io: TcpStream,
    transport: snow::TransportState,
    header: [u8; 2],
    header_read: usize,
    ciphertext: Vec<u8>,
    cipher_read: usize,
    plaintext: Vec<u8>,
    plain_read: usize,
    outgoing: Vec<u8>,
    written: usize,
    failed: bool,
}
impl SecureStream {
    pub fn cancel_on(&mut self, token: tokio_util::sync::CancellationToken) {
        self.cancelled = Some(Box::pin(token.cancelled_owned()));
    }
    fn check_cancelled(&mut self, cx: &mut TaskContext<'_>) -> std::io::Result<()> {
        if self.failed {
            return Err(std::io::ErrorKind::ConnectionAborted.into());
        }
        if self
            .cancelled
            .as_mut()
            .is_some_and(|f| f.as_mut().poll(cx).is_ready())
        {
            self.failed = true;
            return Err(std::io::ErrorKind::ConnectionAborted.into());
        }
        Ok(())
    }

    fn new(io: TcpStream, transport: snow::TransportState) -> Self {
        Self {
            cancelled: None,
            io,
            transport,
            header: [0; 2],
            header_read: 0,
            ciphertext: Vec::new(),
            cipher_read: 0,
            plaintext: Vec::new(),
            plain_read: 0,
            outgoing: Vec::new(),
            written: 0,
            failed: false,
        }
    }
    fn flush_pending(&mut self, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        while self.written < self.outgoing.len() {
            let n = std::task::ready!(
                Pin::new(&mut self.io).poll_write(cx, &self.outgoing[self.written..])
            )?;
            if n == 0 {
                return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
            }
            self.written += n;
        }
        self.outgoing.clear();
        self.written = 0;
        Poll::Ready(Ok(()))
    }
    fn read_record_part(&mut self, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<bool>> {
        while self.header_read < 2 {
            let mut buf = ReadBuf::new(&mut self.header[self.header_read..]);
            std::task::ready!(Pin::new(&mut self.io).poll_read(cx, &mut buf))?;
            let n = buf.filled().len();
            if n == 0 {
                if self.header_read == 0 {
                    return Poll::Ready(Ok(false));
                }
                return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()));
            }
            self.header_read += n;
        }
        let len = u16::from_be_bytes(self.header) as usize;
        if len <= 16 {
            return Poll::Ready(Err(std::io::ErrorKind::InvalidData.into()));
        }
        self.ciphertext.resize(len, 0);
        while self.cipher_read < len {
            let mut buf = ReadBuf::new(&mut self.ciphertext[self.cipher_read..]);
            std::task::ready!(Pin::new(&mut self.io).poll_read(cx, &mut buf))?;
            let n = buf.filled().len();
            if n == 0 {
                return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()));
            }
            self.cipher_read += n;
        }
        self.plaintext.resize(MAX_RECORD, 0);
        let n = self
            .transport
            .read_message(&self.ciphertext, &mut self.plaintext)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "encrypted record authentication failed",
                )
            })?;
        self.plaintext.truncate(n);
        self.plain_read = 0;
        self.header_read = 0;
        self.cipher_read = 0;
        Poll::Ready(Ok(true))
    }
}
impl AsyncRead for SecureStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        if self.failed {
            return Poll::Ready(Err(std::io::ErrorKind::InvalidData.into()));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.plain_read == self.plaintext.len() {
            match std::task::ready!(self.read_record_part(cx)) {
                Ok(false) => return Poll::Ready(Ok(())),
                Ok(true) => (),
                Err(e) => {
                    self.failed = true;
                    return Poll::Ready(Err(e));
                }
            }
        }
        let n = buf.remaining().min(self.plaintext.len() - self.plain_read);
        buf.put_slice(&self.plaintext[self.plain_read..self.plain_read + n]);
        self.plain_read += n;
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for SecureStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Err(error) = self.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        if self.failed {
            return Poll::Ready(Err(std::io::ErrorKind::InvalidData.into()));
        }
        if let Err(error) = self.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        std::task::ready!(self.flush_pending(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = CHUNK.min(buf.len());
        let this = &mut *self;
        this.outgoing.resize(n + 18, 0);
        let count = this
            .transport
            .write_message(&buf[..n], &mut this.outgoing[2..])
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "encryption failed")
            })?;
        this.outgoing[..2].copy_from_slice(&(count as u16).to_be_bytes());
        this.outgoing.truncate(count + 2);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        std::task::ready!(self.flush_pending(cx))?;
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        std::task::ready!(self.flush_pending(cx))?;
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    #[tokio::test]
    async fn encrypted_stream_roundtrip_large_messages_and_wrong_key() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let key = token_key("legacy-token-with-at-least-32-characters");
        assert_eq!(
            decode_key(&crate::hash("legacy-token-with-at-least-32-characters")).unwrap(),
            key
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_, raw) = read_prelude(&mut socket).await.unwrap();
            assert!(accept(socket, &raw, &key).await.is_err());
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_, raw) = read_prelude(&mut socket).await.unwrap();
            let mut stream = crate::framed(accept(socket, &raw, &key).await.unwrap());
            let message = crate::receive(&mut stream).await.unwrap();
            crate::send(&mut stream, &message).await.unwrap();
            // send() must actually flush the last message before drop.
        });
        assert!(
            connect(&address, Purpose::Control, "test", &[42; 32])
                .await
                .is_err()
        );
        let mut stream = crate::framed(
            connect(&address, Purpose::Control, "test", &key)
                .await
                .unwrap(),
        );
        let data = "x".repeat(1024 * 1024);
        crate::send(
            &mut stream,
            &crate::Message::ArtifactChunk { data: data.clone() },
        )
        .await
        .unwrap();
        let crate::Message::ArtifactChunk { data: actual } =
            crate::receive(&mut stream).await.unwrap()
        else {
            panic!("wrong message");
        };
        assert_eq!(actual, data);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn rejects_replay_tampering_and_changed_handshake_binding() {
        for mode in ["replay", "tamper", "binding", "truncated"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let key = [7; 32];
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (_, raw) = read_prelude(&mut socket).await.unwrap();
                let accepted = accept(socket, &raw, &key).await;
                if mode == "binding" {
                    assert!(accepted.is_err());
                    return;
                }
                let mut stream = crate::framed(accepted.unwrap());
                if mode == "replay" {
                    assert!(matches!(
                        crate::receive(&mut stream).await.unwrap(),
                        crate::Message::Ping
                    ));
                }
                assert!(crate::receive(&mut stream).await.is_err());
            });
            let mut io = TcpStream::connect(address).await.unwrap();
            let raw = serde_json::to_vec(&Prelude {
                version: crate::VERSION,
                purpose: Purpose::Control,
                id: "test".into(),
            })
            .unwrap();
            io.write_all(MAGIC).await.unwrap();
            write_record(&mut io, &raw).await.unwrap();
            let binding = if mode == "binding" {
                b"changed".to_vec()
            } else {
                prologue(&raw)
            };
            let mut handshake = snow::Builder::new(PATTERN.parse().unwrap())
                .psk(0, &key)
                .unwrap()
                .prologue(&binding)
                .unwrap()
                .build_initiator()
                .unwrap();
            let mut output = vec![0; MAX_RECORD];
            let n = handshake.write_message(&[], &mut output).unwrap();
            write_record(&mut io, &output[..n]).await.unwrap();
            if mode != "binding" {
                let reply = read_record(&mut io).await.unwrap();
                handshake.read_message(&reply, &mut output).unwrap();
                let mut state = handshake.into_transport_mode().unwrap();
                let json = serde_json::to_vec(&crate::Message::Ping).unwrap();
                let mut payload = (json.len() as u32).to_be_bytes().to_vec();
                payload.extend(json);
                let n = state.write_message(&payload, &mut output).unwrap();
                if mode == "tamper" {
                    output[n - 1] ^= 1;
                }
                if mode == "truncated" {
                    io.write_u16(n as u16).await.unwrap();
                    io.write_all(&output[..n - 1]).await.unwrap();
                } else {
                    write_record(&mut io, &output[..n]).await.unwrap();
                    if mode == "replay" {
                        write_record(&mut io, &output[..n]).await.unwrap();
                    }
                }
                io.shutdown().await.unwrap();
            }
            server.await.unwrap();
        }
    }
}
