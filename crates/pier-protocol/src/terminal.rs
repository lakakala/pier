//! Dedicated, bounded terminal records over an authenticated Noise connection.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub const CAPABILITY: &str = "app_terminal_v1";
pub const CHUNK: usize = 32 * 1024;
pub const WINDOW: usize = 256 * 1024;
pub const MAX_FRAME: usize = 64 * 1024;
pub type Wire = Framed<crate::secure::SecureStream, LengthDelimitedCodec>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Attach { id: String },
    Ready { user: String, home: String },
    Input { data: String },
    Output { data: String },
    Resize { cols: u16, rows: u16 },
    Ack { bytes: usize },
    Exit { code: Option<i32>, reason: String },
    Ping,
    Pong,
}

pub fn size(cols: u16, rows: u16) -> Result<()> {
    ensure!(
        (1..=500).contains(&cols) && (1..=500).contains(&rows),
        "terminal size must be 1..500"
    );
    Ok(())
}
pub fn decode(data: &str) -> Result<Vec<u8>> {
    ensure!(
        data.len() <= CHUNK.div_ceil(3) * 4,
        "terminal chunk too large"
    );
    let bytes = STANDARD.decode(data)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= CHUNK,
        "invalid terminal chunk"
    );
    Ok(bytes)
}
pub fn encode(data: &[u8]) -> String {
    STANDARD.encode(data)
}
pub async fn send(wire: &mut Wire, frame: &Frame) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    ensure!(bytes.len() <= MAX_FRAME, "terminal record too large");
    tokio::time::timeout(std::time::Duration::from_secs(10), wire.send(bytes.into())).await??;
    Ok(())
}
pub async fn receive(wire: &mut Wire) -> Result<Frame> {
    let bytes = wire.next().await.context("terminal disconnected")??;
    ensure!(bytes.len() <= MAX_FRAME, "terminal record too large");
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_binary_data_and_dimensions() {
        let bytes = vec![0xff; CHUNK];
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        assert!(decode(&encode(&vec![0; CHUNK + 1])).is_err());
        assert!(decode("").is_err());
        assert!(size(80, 24).is_ok());
        assert!(size(0, 24).is_err());
        assert!(size(501, 24).is_err());
    }
    #[test]
    fn old_reports_have_no_terminal_capability() {
        let report: crate::AgentReport =
            serde_json::from_str(r#"{"deployment_id":null,"apps":[],"result":null}"#).unwrap();
        assert!(report.capabilities.is_empty());
    }
}
