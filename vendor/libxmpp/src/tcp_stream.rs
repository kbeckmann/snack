use tokio::io::{ AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf };
use tokio::net::TcpStream;
use tokio_rustls::rustls::{ ClientConfig, RootCertStore };
use tokio_rustls::TlsConnector;
use webpki_roots;

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

impl Tcp
{
    pub fn new() -> Self
    {
        return Self::Disconnected;
    }

    pub async fn add_tls(self) -> Result<Self, String>
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
                let tls_stream = connector.connect(domain.try_into().unwrap(), stream).await.map_err(|e| e.to_string())?;

                return Ok(Self::ConnectedTls(tls_stream));
            }
            _ => return Err("Stream is not connected".to_string()),
        }
    }

    pub async fn connect(self, server: String, port: u16) -> Result<Self, String>
    {
        match self
        {
            Self::Disconnected =>
            {
                let addr = format!("{}:{}", server, port);
                let stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;

                return Ok(Self::Connected(stream, server));
            }
            _ => return Err("Stream is already connected".to_string()),
        }
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<(), String>
    {
        match self
        {
            Self::Connected(stream, _) =>
            {
                stream.write_all(data).await.map_err(|e| e.to_string())?;
                stream.flush().await.map_err(|e| e.to_string())?;
                return Ok(());
            }
            Self::ConnectedTls(stream) =>
            {
                stream.write_all(data).await.map_err(|e| e.to_string())?;
                stream.flush().await.map_err(|e| e.to_string())?;
                return Ok(());
            }
            Self::Disconnected => return Err("Not connected".to_string()),
        }
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>, String>
    {
        let mut buf = vec![0u8; 4096];
        let n = match self
        {
            Self::Connected(stream, _) =>
            {
                stream.read(&mut buf).await.map_err(|e| e.to_string())?
            }
            Self::ConnectedTls(stream) =>
            {
                stream.read(&mut buf).await.map_err(|e| e.to_string())?
            }
            Self::Disconnected => return Err("Not connected".to_string()),
        };

        if n == 0
        {
            return Err("Connection closed".to_string());
        }

        buf.truncate(n);

        return Ok(buf);
    }

    pub fn split(self) -> Result<(TcpReader, TcpWriter), String>
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
            Self::Disconnected => return Err("Stream is not connected".to_string()),
        }
    }
}

impl TcpReader
{
    pub async fn read(&mut self) -> Result<Vec<u8>, String>
    {
        let mut buf = vec![0u8; 4096];
        let n = match self
        {
            Self::Plain(reader) =>
            {
                reader.read(&mut buf).await.map_err(|e| e.to_string())?
            }
            Self::Tls(reader) =>
            {
                reader.read(&mut buf).await.map_err(|e| e.to_string())?
            }
        };

        if n == 0
        {
            return Err("Connection closed".to_string());
        }

        buf.truncate(n);

        return Ok(buf);
    }
}

impl TcpWriter
{
    pub async fn write(&mut self, data: &[u8]) -> Result<(), String>
    {
        match self
        {
            Self::Plain(writer) =>
            {
                writer.write_all(data).await.map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?;
            }
            Self::Tls(writer) =>
            {
                writer.write_all(data).await.map_err(|e| e.to_string())?;
                writer.flush().await.map_err(|e| e.to_string())?;
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
