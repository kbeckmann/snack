use std::sync::Arc;

use tokio::io::{ AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf };
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{ HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier };
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::pki_types::{ CertificateDer, ServerName, UnixTime };
use tokio_rustls::rustls::{ CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme };
use tokio_rustls::TlsConnector;

use crate::error::XmppError;

/// A rustls certificate verifier that performs the normal WebPKI checks
/// (hostname match, chain of trust, signature) but tolerates a server
/// certificate that has **expired**. The full verification is repeated at the expiry boundary; every
/// other validation failure is still rejected, and a warning is logged whenever
/// an expired cert is accepted so it is never silent. A non-expired (valid) cert
/// passes the inner verifier and never reaches the relaxed branch, so normal
/// connections keep full strict validation.
#[derive(Debug)]
struct IgnoreExpiredCerts
{
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for IgnoreExpiredCerts
{
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error>
    {
        match self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(Error::InvalidCertificate(CertificateError::ExpiredContext { not_after, .. })) =>
            {
                // Verification stops at the first error. Expiry does NOT prove
                // the chain or hostname passed. Re-run the entire verifier at
                // the expiry boundary so all remaining checks must succeed.
                let verified = self.inner.verify_server_cert(
                    end_entity, intermediates, server_name, ocsp_response, not_after,
                )?;
                log::warn!("Accepting expired server TLS certificate after full chain and hostname verification");
                return Ok(verified);
            }
            other => return other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error>
    {
        return self.inner.verify_tls12_signature(message, cert, dss);
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error>
    {
        return self.inner.verify_tls13_signature(message, cert, dss);
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme>
    {
        return self.inner.supported_verify_schemes();
    }
}

pub enum Tcp
{
    Disconnected,
    Connected(TcpStream, String),
    ConnectedTls(tokio_rustls::client::TlsStream<TcpStream>),
}

pub enum TcpReader
{
    Plain(ReadHalf<TcpStream>),
    Tls(ReadHalf<tokio_rustls::client::TlsStream<TcpStream>>),
}

pub enum TcpWriter
{
    Plain(WriteHalf<TcpStream>),
    Tls(WriteHalf<tokio_rustls::client::TlsStream<TcpStream>>),
}

// Brackets belong to the JID's IPv6 domain representation, not the socket
// hostname or TLS IP identity. Preserve the original domain for XML routing.
fn connection_host(server: &str) -> Result<&str, XmppError>
{
    if server.contains(['[', ']']) {
        let address = server.strip_prefix('[').and_then(|value| value.strip_suffix(']'))
            .filter(|value| value.parse::<std::net::Ipv6Addr>().is_ok())
            .ok_or_else(|| XmppError::InvalidServerName("Expected a bracketed IPv6 address.".into()))?;
        Ok(address)
    } else { Ok(server) }
}

fn tls_server_name(server: &str) -> Result<ServerName<'static>, XmppError>
{
    ServerName::try_from(connection_host(server)?.to_string())
        .map_err(|error| XmppError::InvalidServerName(format!("{server}: {error}")))
}

impl Tcp
{
    pub fn new() -> Self
    {
        return Self::Disconnected;
    }

    /// Whether the connection is protected by TLS. Used to refuse sending
    /// credentials over a cleartext channel.
    pub fn is_encrypted(&self) -> bool
    {
        return matches!(self, Self::ConnectedTls(_));
    }

    pub async fn add_tls(self) -> Result<Self, XmppError>
    {
        match self
        {
            Self::Connected(stream, domain) =>
            {
                let mut root_cert_store = RootCertStore::empty();
                root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

                // Verify normally but tolerate an expired server cert (see
                // IgnoreExpiredCerts) — hostname/chain/signature are still checked.
                let webpki = WebPkiServerVerifier::builder(Arc::new(root_cert_store))
                    .build()
                    .map_err(|e| XmppError::Tls(format!("building cert verifier: {}", e)))?;

                let config = ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(IgnoreExpiredCerts { inner: webpki }))
                    .with_no_client_auth();

                let connector = TlsConnector::from(std::sync::Arc::new(config));
                let server_name = tls_server_name(&domain)?;
                let tls_stream = connector.connect(server_name, stream).await
                    .map_err(|e| XmppError::Tls(e.to_string()))?;

                return Ok(Self::ConnectedTls(tls_stream));
            }
            _ => return Err(XmppError::Protocol("stream is not connected".to_string())),
        }
    }

    pub async fn connect(self, server: String, port: u16) -> Result<Self, XmppError>
    {
        match self
        {
            Self::Disconnected =>
            {
                let host = connection_host(&server)?.to_string();
                let mut lookups = crate::connection_attempts::AddressLookups::new();
                lookups.spawn(async move {
                    tokio::net::lookup_host((host, port)).await
                        .map(|addresses| addresses.collect()).map_err(XmppError::Io)
                });
                // Retain OS connection deadlines for address fallback; the
                // caller may also cancel its overall connection setup future.
                let stream = crate::connection_attempts::connect(lookups, None).await?;

                return Ok(Self::Connected(stream, server));
            }
            _ => return Err(XmppError::Protocol("stream is already connected".to_string())),
        }
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<(), XmppError>
    {
        match self
        {
            Self::Connected(stream, _) =>
            {
                stream.write_all(data).await?;
                stream.flush().await?;
                return Ok(());
            }
            Self::ConnectedTls(stream) =>
            {
                stream.write_all(data).await?;
                stream.flush().await?;
                return Ok(());
            }
            Self::Disconnected => return Err(XmppError::Protocol("not connected".to_string())),
        }
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>, XmppError>
    {
        let mut buf = vec![0u8; 4096];
        let n = match self
        {
            Self::Connected(stream, _) =>
            {
                stream.read(&mut buf).await?
            }
            Self::ConnectedTls(stream) =>
            {
                stream.read(&mut buf).await?
            }
            Self::Disconnected => return Err(XmppError::Protocol("not connected".to_string())),
        };

        if n == 0
        {
            return Err(XmppError::Disconnected);
        }

        buf.truncate(n);

        return Ok(buf);
    }

    pub fn split(self) -> Result<(TcpReader, TcpWriter), XmppError>
    {
        match self
        {
            Self::Connected(stream, _) =>
            {
                let (reader, writer) = tokio::io::split(stream);
                return Ok((TcpReader::Plain(reader), TcpWriter::Plain(writer)));
            }
            Self::ConnectedTls(tls_stream) =>
            {
                let (reader, writer) = tokio::io::split(tls_stream);
                return Ok((TcpReader::Tls(reader), TcpWriter::Tls(writer)));
            }
            Self::Disconnected => return Err(XmppError::Protocol("stream is not connected".to_string())),
        }
    }
}

impl TcpReader
{
    pub async fn read(&mut self) -> Result<Vec<u8>, XmppError>
    {
        let mut buf = vec![0u8; 4096];
        let n = match self
        {
            Self::Plain(reader) =>
            {
                reader.read(&mut buf).await?
            }
            Self::Tls(reader) =>
            {
                reader.read(&mut buf).await?
            }
        };

        if n == 0
        {
            return Err(XmppError::Disconnected);
        }

        buf.truncate(n);

        return Ok(buf);
    }
}

impl TcpWriter
{
    pub async fn write(&mut self, data: &[u8]) -> Result<(), XmppError>
    {
        match self
        {
            Self::Plain(writer) =>
            {
                writer.write_all(data).await?;
                writer.flush().await?;
            }
            Self::Tls(writer) =>
            {
                writer.write_all(data).await?;
                writer.flush().await?;
            }
        }

        return Ok(());
    }

    pub async fn shutdown(&mut self)
    {
        match self
        {
            Self::Plain(writer) =>
            {
                let _ = writer.shutdown().await;
            }
            Self::Tls(writer) =>
            {
                let _ = writer.shutdown().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn transport_and_tls_use_ip_literals_without_changing_jid_representation() {
        for domain in ["[::1]", "[2001:db8::1]", "[::ffff:192.0.2.1]"] {
            assert!(matches!(super::tls_server_name(domain).unwrap(), tokio_rustls::rustls::pki_types::ServerName::IpAddress(_)));
            assert_eq!(super::connection_host(domain).unwrap(), &domain[1..domain.len() - 1]);
        }
        assert!(matches!(super::tls_server_name("127.0.0.1").unwrap(), tokio_rustls::rustls::pki_types::ServerName::IpAddress(_)));
        assert!(matches!(super::tls_server_name("example.com").unwrap(), tokio_rustls::rustls::pki_types::ServerName::DnsName(_)));
        for domain in ["[example.com]", "[::1]:5222", "[::1", "::1]", "[]"] {
            assert!(super::connection_host(domain).is_err(), "{domain}");
        }
    }

    #[tokio::test]
    async fn bracketed_ipv6_reaches_tls_negotiation_instead_of_failing_name_conversion() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let tcp = super::Tcp::new().connect("[::1]".into(), port).await.unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        drop(peer);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), tcp.add_tls()).await.unwrap();
        assert!(matches!(result, Err(crate::XmppError::Tls(_))), "a valid IP identity must reach the TLS handshake");
    }

    use super::*;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    fn certificates() -> (IgnoreExpiredCerts, Vec<CertificateDer<'static>>) {
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(include_bytes!("test-data/root.pem")) { roots.add(cert.unwrap()).unwrap(); }
        let inner = WebPkiServerVerifier::builder(Arc::new(roots)).build().unwrap();
        let chain = CertificateDer::pem_slice_iter(include_bytes!("test-data/chain.pem")).map(Result::unwrap).collect();
        (IgnoreExpiredCerts { inner }, chain)
    }
    #[test]
    fn expired_certificate_still_requires_matching_hostname() {
        let (verifier, chain) = certificates();
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(100_000_000_000));
        let valid_name = ServerName::try_from("foobar.com").unwrap();
        assert!(matches!(verifier.inner.verify_server_cert(&chain[0], &chain[1..], &valid_name, &[], now),
            Err(Error::InvalidCertificate(CertificateError::ExpiredContext { .. }))));
        assert!(verifier.verify_server_cert(&chain[0], &chain[1..], &valid_name, &[], now).is_ok());
        assert!(verifier.verify_server_cert(&chain[0], &chain[1..], &ServerName::try_from("wrong.example").unwrap(), &[], now).is_err());
    }
}
