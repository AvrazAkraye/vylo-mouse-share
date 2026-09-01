//! TLS setup for the sync side-channel.
//!
//! Both machines use the same self-signed certificate that already
//! identifies them on the DTLS input channel. Instead of web-PKI chain
//! verification, each side pins the peer's certificate to the shared
//! `authorized_fingerprints` allowlist. While a pairing window is open,
//! unauthorized peers are admitted at the TLS layer and then have to
//! prove knowledge of the PIN (see [`super::pairing`]) before their
//! fingerprint is added to the allowlist.

use crate::crypto;
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use thiserror::Error;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream, rustls};
use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub(crate) enum TlsSetupError {
    #[error("certificate has no private key material")]
    MissingKey,
    #[error(transparent)]
    Rustls(#[from] rustls::Error),
}

/// Accepts a peer certificate iff its sha256 fingerprint is in the
/// allowlist — or, when `allow_pairing` is set, while a pairing window
/// is open. Only the *acceptor* and the dedicated pairing *dialer* set
/// `allow_pairing`: an unauthorized inbound peer during a pairing
/// window still has to complete the PIN exchange (see
/// [`super::pairing`]) before its fingerprint is trusted, and the
/// pairing dialer likewise proves the PIN before treating the peer as
/// paired. Ordinary outbound sync dials use a policy with
/// `allow_pairing = false`, so they can never be redirected to an
/// unpinned attacker while a window happens to be open.
#[derive(Debug)]
struct FingerprintPolicy {
    authorized: Arc<RwLock<HashMap<String, String>>>,
    pairing_open: Arc<AtomicBool>,
    allow_pairing: bool,
    provider: Arc<CryptoProvider>,
}

impl FingerprintPolicy {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let fingerprint = crypto::generate_fingerprint(end_entity);
        if self
            .authorized
            .read()
            .expect("lock")
            .contains_key(&fingerprint)
            || (self.allow_pairing && self.pairing_open.load(Ordering::SeqCst))
        {
            Ok(())
        } else {
            log::warn!("sync channel: rejecting unauthorized peer {fingerprint}");
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn tls12(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedServerVerifier(FingerprintPolicy);

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.0.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.schemes()
    }
}

#[derive(Debug)]
struct PinnedClientVerifier {
    policy: FingerprintPolicy,
    root_hints: Vec<DistinguishedName>,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.root_hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.policy.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.policy.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.policy.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.policy.schemes()
    }
}

pub(crate) struct TlsConfigs {
    /// accepts inbound connections; admits unauthorized peers only
    /// while a pairing window is open (they still must pass the PIN)
    pub(crate) acceptor: TlsAcceptor,
    /// dials known, already-authorized peers; NEVER admits an unpinned
    /// server certificate, regardless of pairing state
    pub(crate) connector: TlsConnector,
    /// dials a specific peer for pairing; admits its (as yet unpinned)
    /// certificate so the PIN exchange can run over the session
    pub(crate) pairing_connector: TlsConnector,
}

fn build_connector(
    provider: &Arc<CryptoProvider>,
    certs: &[CertificateDer<'static>],
    key: &PrivateKeyDer<'static>,
    policy: FingerprintPolicy,
) -> Result<TlsConnector, TlsSetupError> {
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier(policy)))
        .with_client_auth_cert(certs.to_vec(), key.clone_key())
        .map_err(|_| TlsSetupError::MissingKey)?;
    Ok(TlsConnector::from(Arc::new(config)))
}

pub(crate) fn build_tls(
    cert: &Certificate,
    authorized: Arc<RwLock<HashMap<String, String>>>,
    pairing_open: Arc<AtomicBool>,
) -> Result<TlsConfigs, TlsSetupError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let certs: Vec<CertificateDer<'static>> = cert.certificate.clone();
    let key = PrivateKeyDer::Pkcs8(cert.private_key.serialized_der.clone().into());

    let policy = |allow_pairing: bool| FingerprintPolicy {
        authorized: authorized.clone(),
        pairing_open: pairing_open.clone(),
        allow_pairing,
        provider: provider.clone(),
    };

    // strict outbound: allowlist only, pairing window is irrelevant
    let connector = build_connector(&provider, &certs, &key, policy(false))?;
    // pairing outbound: admit the unpinned peer, PIN proof follows
    let pairing_connector = build_connector(&provider, &certs, &key, policy(true))?;

    // acceptor honors the pairing window (inbound pairing is legit);
    // an admitted unauthorized peer is still routed through the PIN
    // exchange before it is trusted
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(PinnedClientVerifier {
            policy: policy(true),
            root_hints: Vec::new(),
        }))
        .with_single_cert(certs, key)
        .map_err(|_| TlsSetupError::MissingKey)?;

    Ok(TlsConfigs {
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        connector,
        pairing_connector,
    })
}

/// sha256 fingerprint of the peer's end-entity certificate
pub(crate) fn peer_fingerprint(stream: &TlsStream<tokio::net::TcpStream>) -> Option<String> {
    let (_, common) = stream.get_ref();
    let certs = common.peer_certificates()?;
    Some(crypto::generate_fingerprint(certs.first()?))
}

/// Keying material exported from this exact TLS session. Used to bind
/// the PIN proof during pairing to the session, which defeats an
/// attacker relaying the pairing between two separate TLS sessions.
pub(crate) fn exporter(
    stream: &TlsStream<tokio::net::TcpStream>,
) -> Result<[u8; 32], rustls::Error> {
    let buf = [0u8; 32];
    match stream {
        TlsStream::Client(s) => s
            .get_ref()
            .1
            .export_keying_material(buf, b"vylo pairing v1", None),
        TlsStream::Server(s) => s
            .get_ref()
            .1
            .export_keying_material(buf, b"vylo pairing v1", None),
    }
}
