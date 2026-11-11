use tokio::io::{ AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf };
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ ClientConfig, RootCertStore };
use tokio_rustls::TlsConnector;

use crate::error::XmppError;

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

                let config = ClientConfig::builder()
                    .with_root_certificates(root_cert_store)
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
                let stream = TcpStream::connect((connection_host(&server)?, port)).await?;

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
}
