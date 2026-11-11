//! A minimal DNS stub resolver for the SRV, A and AAAA queries of RFC 6120
//! server discovery (RFC 1035 messages over UDP, RFC 2782 SRV records).
//!
//! Queries go to the system's configured nameservers. Answers are parsed only
//! for the record types requested; everything else is skipped.
use std::{net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr}, time::Duration};
use tokio::net::UdpSocket;

pub(crate) const A: u16 = 1;
pub(crate) const AAAA: u16 = 28;
pub(crate) const SRV: u16 = 33;
const CLASS_IN: u16 = 1;
const MAX_MESSAGE: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Srv { priority: u16, weight: u16, port: u16, target: String },
    Address(IpAddr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Response {
    /// RFC 1035 RCODE: 0 no error, 3 name error, and so on.
    pub code: u8,
    pub records: Vec<Record>,
}

#[derive(Clone, Debug)]
pub(crate) struct Resolver {
    servers: Vec<SocketAddr>,
    timeout: Duration,
}

impl Resolver {
    pub(crate) fn new(servers: Vec<SocketAddr>, timeout: Duration) -> Self {
        Self { servers, timeout }
    }

    /// The nameservers from /etc/resolv.conf. macOS keeps this file in sync
    /// with the primary network service; other systems have no such file and
    /// get `None`, so callers fall back to the address of the domain itself.
    pub(crate) fn system(timeout: Duration) -> Option<Self> {
        let config = std::fs::read_to_string("/etc/resolv.conf").ok()?;
        let servers = nameservers(&config);
        (!servers.is_empty()).then(|| Self::new(servers, timeout))
    }

    /// Ask each nameserver in turn until one answers within the timeout.
    pub(crate) async fn query(&self, name: &str, kind: u16) -> Result<Response, String> {
        let mut last_error = String::from("no nameservers configured");
        for server in &self.servers {
            match tokio::time::timeout(self.timeout, exchange(*server, name, kind)).await {
                Ok(Ok(response)) => return Ok(response),
                Ok(Err(error)) => last_error = error,
                Err(_) => last_error = format!("{server} did not answer"),
            }
        }
        Err(last_error)
    }
}

fn nameservers(config: &str) -> Vec<SocketAddr> {
    config.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "nameserver").then_some(())?;
        // Zone indices such as fe80::1%en0 are not supported; skip them.
        let address: IpAddr = words.next()?.parse().ok()?;
        Some(SocketAddr::new(address, 53))
    }).collect()
}

async fn exchange(server: SocketAddr, name: &str, kind: u16) -> Result<Response, String> {
    let local: SocketAddr = if server.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let socket = UdpSocket::bind(local).await.map_err(|error| error.to_string())?;
    socket.connect(server).await.map_err(|error| error.to_string())?;
    let mut id = [0u8; 2];
    getrandom::fill(&mut id).map_err(|error| error.to_string())?;
    let id = u16::from_be_bytes(id);
    socket.send(&encode_query(id, name, kind)?).await.map_err(|error| error.to_string())?;
    let mut buffer = vec![0u8; MAX_MESSAGE];
    loop {
        let size = socket.recv(&mut buffer).await.map_err(|error| error.to_string())?;
        // A connected socket only receives from the server; still ignore
        // stray or spoofed replies to other queries.
        if let Some(response) = decode_response(&buffer[..size], id, kind) { return Ok(response); }
    }
}

pub(crate) fn encode_query(id: u16, name: &str, kind: u16) -> Result<Vec<u8>, String> {
    let mut message = Vec::with_capacity(32 + name.len());
    message.extend_from_slice(&id.to_be_bytes());
    message.extend_from_slice(&0x0100u16.to_be_bytes()); // standard query, recursion desired
    message.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // one question
    encode_name(&mut message, name)?;
    message.extend_from_slice(&kind.to_be_bytes());
    message.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(message)
}

pub(crate) fn encode_name(message: &mut Vec<u8>, name: &str) -> Result<(), String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.len() > 253 { return Err(format!("DNS name too long: {name}")); }
    if !name.is_empty() {
        for label in name.split('.') {
            if label.is_empty() || label.len() > 63 || !label.is_ascii() {
                return Err(format!("invalid DNS name: {name}"));
            }
            message.push(label.len() as u8);
            message.extend_from_slice(label.as_bytes());
        }
    }
    message.push(0);
    Ok(())
}

fn u16_at(message: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*message.get(at)?, *message.get(at + 1)?]))
}

/// A possibly compressed name at `at`: the name as an absolute "a.b." string
/// ("." for the root) and the offset just after it in the message.
fn read_name(message: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut end = None;
    for _ in 0..128 {
        let length = *message.get(at)?;
        match length & 0xC0 {
            0x00 if length == 0 => {
                if name.is_empty() { name.push('.'); }
                return Some((name, end.unwrap_or(at + 1)));
            }
            0x00 => {
                let label = message.get(at + 1..at + 1 + usize::from(length))?;
                name.push_str(std::str::from_utf8(label).ok()?);
                name.push('.');
                at += 1 + usize::from(length);
            }
            0xC0 => {
                end.get_or_insert(at + 2);
                at = usize::from(u16_at(message, at)? & 0x3FFF);
            }
            _ => return None,
        }
    }
    None // a compression loop
}

pub(crate) fn decode_response(message: &[u8], id: u16, kind: u16) -> Option<Response> {
    if u16_at(message, 0)? != id { return None; }
    let flags = u16_at(message, 2)?;
    if flags & 0x8000 == 0 { return None; } // not a response
    let questions = u16_at(message, 4)?;
    let answers = u16_at(message, 6)?;
    let mut at = 12;
    for _ in 0..questions {
        at = read_name(message, at)?.1 + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        let (_, after) = read_name(message, at)?;
        let record_type = u16_at(message, after)?;
        let class = u16_at(message, after + 2)?;
        let length = usize::from(u16_at(message, after + 8)?);
        let data = after + 10;
        let rdata = message.get(data..data + length)?;
        at = data + length;
        if class != CLASS_IN || record_type != kind { continue; }
        match record_type {
            A if length == 4 => records.push(Record::Address(IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])))),
            AAAA if length == 16 => records.push(Record::Address(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(rdata).ok()?)))),
            SRV if length >= 7 => records.push(Record::Srv {
                priority: u16_at(rdata, 0)?, weight: u16_at(rdata, 2)?, port: u16_at(rdata, 4)?,
                // The target may use compression into the whole message.
                target: read_name(message, data + 6)?.0,
            }),
            _ => {}
        }
    }
    Some(Response { code: (flags & 0x000F) as u8, records })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a response to `query` with the given answers, for test servers.
    pub(crate) fn respond(query: &[u8], code: u8, answers: &[Record]) -> Vec<u8> {
        let (_, question_end) = read_name(query, 12).unwrap();
        let question_end = question_end + 4;
        let mut message = query[..question_end].to_vec();
        message[2] = 0x81; // response, recursion desired
        message[3] = 0x80 | code; // recursion available
        message[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
        for answer in answers {
            message.extend_from_slice(&[0xC0, 12]); // owner: the question name
            let (kind, rdata) = match answer {
                Record::Address(IpAddr::V4(ip)) => (A, ip.octets().to_vec()),
                Record::Address(IpAddr::V6(ip)) => (AAAA, ip.octets().to_vec()),
                Record::Srv { priority, weight, port, target } => {
                    let mut rdata = [priority.to_be_bytes(), weight.to_be_bytes(), port.to_be_bytes()].concat();
                    encode_name(&mut rdata, target).unwrap();
                    (SRV, rdata)
                }
            };
            message.extend_from_slice(&kind.to_be_bytes());
            message.extend_from_slice(&CLASS_IN.to_be_bytes());
            message.extend_from_slice(&60u32.to_be_bytes());
            message.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            message.extend_from_slice(&rdata);
        }
        message
    }

    /// The name and type a query asks for.
    pub(crate) fn question(query: &[u8]) -> (String, u16) {
        let (name, end) = read_name(query, 12).unwrap();
        (name, u16_at(query, end).unwrap())
    }

    #[test]
    fn queries_and_answers_round_trip_including_compressed_srv_targets() {
        let query = encode_query(0x1234, "_xmpp-client._tcp.example.org", SRV).unwrap();
        assert_eq!(question(&query), ("_xmpp-client._tcp.example.org.".to_string(), SRV));
        let srv = Record::Srv { priority: 5, weight: 10, port: 5222, target: "xmpp.example.org.".into() };
        let response = respond(&query, 0, &[srv.clone()]);
        assert_eq!(decode_response(&response, 0x1234, SRV), Some(Response { code: 0, records: vec![srv] }));
        assert_eq!(decode_response(&response, 0x4321, SRV), None, "a reply to another query is ignored");

        // A target compressed as a pointer to the question name.
        // Replace the answer's RDLENGTH and RDATA (6 fixed bytes plus the
        // 18-byte encoded target) with a compressed target.
        let mut compressed = response[..response.len() - 2 - 24].to_vec();
        let rdata = [0, 1, 0, 2, 0x14, 0x66, 0xC0, 12];
        compressed.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        compressed.extend_from_slice(&rdata);
        let Some(Response { records, .. }) = decode_response(&compressed, 0x1234, SRV) else { panic!("decodes") };
        assert_eq!(records, [Record::Srv { priority: 1, weight: 2, port: 5222, target: "_xmpp-client._tcp.example.org.".into() }]);

        let root = respond(&query, 0, &[Record::Srv { priority: 0, weight: 0, port: 0, target: ".".into() }]);
        assert!(matches!(&decode_response(&root, 0x1234, SRV).unwrap().records[..], [Record::Srv { target, .. }] if target == "."));
        assert_eq!(decode_response(&respond(&query, 3, &[]), 0x1234, SRV).unwrap().code, 3);
    }

    #[test]
    fn malformed_messages_and_names_are_rejected() {
        let query = encode_query(1, "example.org", A).unwrap();
        let mut looping = respond(&query, 0, &[Record::Address(IpAddr::V4(Ipv4Addr::LOCALHOST))]);
        let owner = looping.len() - 16;
        looping[owner..owner + 2].copy_from_slice(&[0xC0, owner as u8]);
        assert_eq!(decode_response(&looping, 1, A), None);
        assert_eq!(decode_response(&query, 1, A), None, "a query is not a response");
        assert_eq!(decode_response(&query[..5], 1, A), None);
        assert!(encode_query(1, "bad..name", A).is_err());
        assert!(encode_query(1, &"x".repeat(64), A).is_err());
        assert!(encode_query(1, "ünicode.example", A).is_err());
    }

    #[test]
    fn nameservers_come_from_resolv_conf() {
        let config = "# comment\nnameserver 192.0.2.53\nnameserver 2001:db8::53\nnameserver fe80::1%en0\nsearch example.org\n";
        assert_eq!(nameservers(config), [SocketAddr::from(([192, 0, 2, 53], 53)), "[2001:db8::53]:53".parse().unwrap()]);
    }
}
