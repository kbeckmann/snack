/// Errors returned by the public [`XmppClient`](crate::XmppClient) API.
///
/// Variants distinguish the phase or cause of a failure so callers can react
/// programmatically (e.g. retry on [`Disconnected`](Self::Disconnected), prompt
/// for new credentials on [`Auth`](Self::Auth)) instead of matching on strings.
#[derive(Debug)]
pub enum XmppError
{
    /// An underlying network or TLS read/write failed.
    Io(std::io::Error),
    /// The supplied JID could not be parsed (e.g. it has no `@`).
    InvalidJid(String),
    /// The server's hostname is not a valid TLS server name.
    InvalidServerName(String),
    /// STARTTLS negotiation failed or was refused by the server.
    Tls(String),
    /// The server does not offer the encryption this client requires before
    /// authenticating. Credentials are never sent over a cleartext channel.
    TlsRequired,
    /// SASL authentication failed (bad credentials or a rejected exchange).
    Auth(String),
    /// The server offered no SASL mechanism this client supports.
    NoSaslMechanism,
    /// Resource binding failed after authentication.
    Bind(String),
    /// A stream element or stanza could not be parsed.
    Parse(String),
    /// The peer violated the expected protocol flow or connection state.
    Protocol(String),
    /// The connection was closed.
    Disconnected,
}

impl std::fmt::Display for XmppError
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
    {
        match self
        {
            XmppError::Io(e) => write!(f, "I/O error: {}", e),
            XmppError::InvalidJid(s) => write!(f, "invalid JID: {}", s),
            XmppError::InvalidServerName(s) => write!(f, "invalid server name: {}", s),
            XmppError::Tls(s) => write!(f, "TLS error: {}", s),
            XmppError::TlsRequired => write!(f, "server does not offer required TLS encryption"),
            XmppError::Auth(s) => write!(f, "authentication failed: {}", s),
            XmppError::NoSaslMechanism => write!(f, "no supported SASL mechanism offered by server"),
            XmppError::Bind(s) => write!(f, "resource binding failed: {}", s),
            XmppError::Parse(s) => write!(f, "parse error: {}", s),
            XmppError::Protocol(s) => write!(f, "protocol error: {}", s),
            XmppError::Disconnected => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for XmppError
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)>
    {
        match self
        {
            XmppError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for XmppError
{
    fn from(e: std::io::Error) -> Self
    {
        return XmppError::Io(e);
    }
}
