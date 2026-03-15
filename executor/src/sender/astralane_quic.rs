//! Direct QUIC sender to Astralane gateway, bypassing their buggy SDK.
//!
//! Implements the same protocol: TLS with API key as CN, ALPN "astralane-tpu",
//! unidirectional stream per transaction, fire-and-forget.

use std::sync::Arc;
use std::net::ToSocketAddrs;
use anyhow::Result;
use quinn::{ClientConfig, Endpoint, Connection};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::Mutex;

const ALPN: &[u8] = b"astralane-tpu";
const MAX_TX_SIZE: usize = 1232;

#[derive(Clone)]
pub struct AstralaneQuicSender {
    endpoint_addr: String,
    connection: Arc<Mutex<Option<Connection>>>,
}

impl AstralaneQuicSender {
    pub fn new(endpoint_addr: String) -> Self {
        Self {
            endpoint_addr,
            connection: Arc::new(Mutex::new(None)),
        }
    }

    /// Connect to Astralane QUIC gateway.
    pub async fn connect(&self, api_key: &str) -> Result<()> {
        let conn = Self::establish_connection(&self.endpoint_addr, api_key).await?;
        *self.connection.lock().await = Some(conn);
        log::info!("Astralane QUIC direct connected: {}", self.endpoint_addr);
        Ok(())
    }

    /// Send raw transaction bytes. Fire-and-forget.
    pub async fn send(&self, tx_bytes: &[u8]) -> Result<()> {
        if tx_bytes.len() > MAX_TX_SIZE {
            anyhow::bail!("tx too large: {} > {}", tx_bytes.len(), MAX_TX_SIZE);
        }

        let guard = self.connection.lock().await;
        let conn = guard.as_ref().ok_or_else(|| anyhow::anyhow!("not connected"))?;

        // Open unidirectional stream, write bytes, finish
        let mut stream = conn.open_uni().await?;
        stream.write_all(tx_bytes).await?;
        stream.finish()?;

        Ok(())
    }

    async fn establish_connection(addr: &str, api_key: &str) -> Result<Connection> {
        // Ensure rustls crypto provider is installed
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Resolve address
        let socket_addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve {}", addr))?;

        // Generate self-signed cert with API key as CN
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut cert_params = rcgen::CertificateParams::new(vec![api_key.to_string()])?;
        cert_params.distinguished_name.push(rcgen::DnType::CommonName, api_key);
        let cert = cert_params.self_signed(&key_pair)?;

        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| anyhow::anyhow!("key error: {}", e))?;

        // Build rustls config
        let mut tls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_client_auth_cert(vec![cert_der], key_der)?;
        tls_config.alpn_protocols = vec![ALPN.to_vec()];

        // Build quinn config
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into()?));
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(25)));

        let mut client_config = ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?,
        ));
        client_config.transport_config(Arc::new(transport));

        // Create endpoint and connect
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config);

        let host = addr.split(':').next().unwrap_or(addr);
        let conn = endpoint.connect(socket_addr, host)?.await?;

        log::info!("QUIC direct connected to {} ({})", addr, socket_addr);
        Ok(conn)
    }
}

/// Skip server certificate verification (server uses self-signed certs)
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
