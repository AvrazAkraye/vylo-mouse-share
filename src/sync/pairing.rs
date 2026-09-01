//! PIN pairing over an (as yet untrusted) TLS connection.
//!
//! Flow: the machine that shows the PIN opens a pairing window and
//! waits; the machine where the user types the PIN connects and starts
//! the exchange. Both run SPAKE2 with the PIN as password, yielding a
//! shared key only if the PINs match. Each side then proves knowledge
//! of that key with an HMAC over keying material exported from the TLS
//! session itself, so the proof cannot be relayed between two sessions
//! by a machine-in-the-middle. Once both MACs verify, each side pins
//! the peer certificate it saw during the TLS handshake.
//!
//! SPAKE2 gives an attacker no offline PIN oracle: every guess costs a
//! visible, failed online pairing attempt, and the window closes after
//! the first failure.

use super::proto::{ProtoError, SyncMessage, read_msg, write_msg};
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpStream,
};
use tokio_rustls::TlsStream;

const PAIRING_IDENTITY: &[u8] = b"vylo-mouse-share pairing";
const CONFIRM_A: &[u8] = b"vylo pairing confirm A";
const CONFIRM_B: &[u8] = b"vylo pairing confirm B";

#[derive(Debug, Error)]
pub(crate) enum PairingError {
    #[error("connection error during pairing: {0}")]
    Proto(#[from] ProtoError),
    #[error("peer sent an unexpected message")]
    UnexpectedMessage,
    #[error("wrong PIN")]
    WrongPin,
    #[error("pairing rejected by peer (wrong PIN?)")]
    PeerRejected,
}

pub(crate) fn generate_pin() -> String {
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:06}")
}

fn confirm_mac(key: &[u8], label: &[u8], exporter: &[u8; 32]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(label);
    mac.update(exporter);
    mac.finalize().into_bytes().into()
}

fn mac_matches(expected: &[u8; 32], received: &[u8; 32]) -> bool {
    // constant-time comparison via hmac's verify machinery
    let mut mac = Hmac::<Sha256>::new_from_slice(expected).expect("hmac accepts any key length");
    mac.update(b"eq");
    let a = mac.finalize().into_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(received).expect("hmac accepts any key length");
    mac.update(b"eq");
    let b = mac.finalize().into_bytes();
    a == b
}

/// Side that dialed after the user typed the PIN ("A").
/// Returns the peer's device name.
pub(crate) async fn run_initiator(
    r: &mut ReadHalf<TlsStream<TcpStream>>,
    w: &mut WriteHalf<TlsStream<TcpStream>>,
    exporter: &[u8; 32],
    pin: &str,
    device_name: &str,
) -> Result<String, PairingError> {
    let (spake, outbound_msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(pin.as_bytes()),
        &Identity::new(PAIRING_IDENTITY),
    );
    write_msg(
        w,
        &SyncMessage::PairStart {
            spake_msg: outbound_msg,
            name: device_name.to_string(),
        },
    )
    .await?;

    let (peer_spake_msg, peer_name) = match read_msg(r).await? {
        SyncMessage::PairResponse { spake_msg, name } => (spake_msg, name),
        _ => return Err(PairingError::UnexpectedMessage),
    };
    let key = spake
        .finish(&peer_spake_msg)
        .map_err(|_| PairingError::WrongPin)?;

    write_msg(
        w,
        &SyncMessage::PairConfirmA {
            mac: confirm_mac(&key, CONFIRM_A, exporter),
        },
    )
    .await?;

    let peer_mac = match read_msg(r).await? {
        SyncMessage::PairConfirmB { mac } => mac,
        _ => return Err(PairingError::PeerRejected),
    };
    if !mac_matches(&confirm_mac(&key, CONFIRM_B, exporter), &peer_mac) {
        return Err(PairingError::WrongPin);
    }
    Ok(peer_name)
}

/// Side that displays the PIN and accepted the connection ("B").
/// Returns the peer's device name.
pub(crate) async fn run_responder(
    r: &mut ReadHalf<TlsStream<TcpStream>>,
    w: &mut WriteHalf<TlsStream<TcpStream>>,
    exporter: &[u8; 32],
    pin: &str,
    device_name: &str,
) -> Result<String, PairingError> {
    let (peer_spake_msg, peer_name) = match read_msg(r).await? {
        SyncMessage::PairStart { spake_msg, name } => (spake_msg, name),
        _ => return Err(PairingError::UnexpectedMessage),
    };

    let (spake, outbound_msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(pin.as_bytes()),
        &Identity::new(PAIRING_IDENTITY),
    );
    write_msg(
        w,
        &SyncMessage::PairResponse {
            spake_msg: outbound_msg,
            name: device_name.to_string(),
        },
    )
    .await?;
    let key = spake
        .finish(&peer_spake_msg)
        .map_err(|_| PairingError::WrongPin)?;

    let peer_mac = match read_msg(r).await? {
        SyncMessage::PairConfirmA { mac } => mac,
        _ => return Err(PairingError::UnexpectedMessage),
    };
    if !mac_matches(&confirm_mac(&key, CONFIRM_A, exporter), &peer_mac) {
        return Err(PairingError::WrongPin);
    }

    write_msg(
        w,
        &SyncMessage::PairConfirmB {
            mac: confirm_mac(&key, CONFIRM_B, exporter),
        },
    )
    .await?;
    Ok(peer_name)
}
