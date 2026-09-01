//! Wire protocol of the sync side-channel (clipboard, file transfer,
//! pairing). Frames are a u32-BE length followed by a bincode-encoded
//! [`SyncMessage`], sent over the mutually-authenticated TLS stream.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const PROTO_VERSION: u16 = 1;
/// chunk size for file transfers
pub(crate) const CHUNK_SIZE: usize = 256 * 1024;
/// upper bound on a single frame; a chunk plus bincode/struct overhead
/// fits comfortably, anything larger is a protocol violation
pub(crate) const MAX_FRAME_SIZE: u32 = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum ProtoError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds limit")]
    FrameTooLarge(u32),
    #[error("malformed message: {0}")]
    Encoding(#[from] bincode::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum SyncMessage {
    /// sent by both sides right after the connection is established
    Hello {
        version: u16,
        name: String,
    },

    /* clipboard */
    ClipText {
        text: String,
    },
    ClipImage {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },

    /* file transfer */
    FileOffer {
        id: u64,
        name: String,
        size: u64,
    },
    FileAccept {
        id: u64,
    },
    FileReject {
        id: u64,
        reason: String,
    },
    FileChunk {
        id: u64,
        offset: u64,
        data: Vec<u8>,
    },
    FileDone {
        id: u64,
        sha256: [u8; 32],
    },
    FileCancel {
        id: u64,
        reason: String,
    },

    /* pairing (only valid on a connection in pairing mode) */
    PairStart {
        spake_msg: Vec<u8>,
        name: String,
    },
    PairResponse {
        spake_msg: Vec<u8>,
        name: String,
    },
    PairConfirmA {
        mac: [u8; 32],
    },
    PairConfirmB {
        mac: [u8; 32],
    },

    Ping,
    Pong,
}

pub(crate) async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<SyncMessage, ProtoError> {
    let len = r.read_u32().await?;
    if len > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(bincode::deserialize(&buf)?)
}

pub(crate) async fn write_msg<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &SyncMessage,
) -> Result<(), ProtoError> {
    let buf = bincode::serialize(msg)?;
    w.write_u32(buf.len() as u32).await?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}
