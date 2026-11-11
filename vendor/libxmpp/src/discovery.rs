//! RFC 6120 client SRV discovery; TLS identity always remains the origin domain.

use std::{collections::BTreeMap, net::{IpAddr, SocketAddr}, time::Duration};
use crate::{dns::{self, Record, Resolver}, error::XmppError, tcp_stream::Tcp};

const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    priority: u16,
    weight: u16,
}

pub(crate) async fn connect(domain: &str) -> Result<Tcp, XmppError> {
    let literal = domain.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(domain);
    if literal.parse::<IpAddr>().is_ok() { return Tcp::new().connect(domain.into(), 5222).await; }
    let Some(resolver) = Resolver::system(DNS_TIMEOUT) else {
        log::debug!("No system nameservers for SRV discovery, using address fallback");
        return Tcp::new().connect(domain.into(), 5222).await;
    };
    connect_with_resolver(domain, &resolver).await
}

async fn discover(domain: &str, resolver: &Resolver) -> Result<Option<Vec<Target>>, XmppError> {
    // Fully qualify the query so system search suffixes cannot retarget it.
    let query = format!("_xmpp-client._tcp.{}.", domain.trim_end_matches('.'));
    let response = match tokio::time::timeout(DNS_TIMEOUT, resolver.query(&query, dns::SRV)).await {
        Ok(Ok(response)) if response.code == 0 => response,
        _ => return Ok(None), // RFC6120 3.2.1: no SRV response permits A/AAAA fallback.
    };
    let mut targets = Vec::new();
    for record in response.records {
        let Record::Srv { priority, weight, port, target } = record else { continue; };
        if target == "." {
            return Err(XmppError::Protocol("XMPP service is explicitly unavailable for this domain.".into()));
        }
        targets.push(Target { host: target, port, priority, weight });
    }
    if targets.is_empty() { Ok(None) }
    else { Ok(Some(order_targets(targets, &mut random_below))) }
}

async fn connect_with_resolver(domain: &str, resolver: &Resolver) -> Result<Tcp, XmppError> {
    let Some(targets) = discover(domain, resolver).await? else {
        return Tcp::new().connect(domain.into(), 5222).await;
    };
    let mut last_error = XmppError::Protocol("No usable XMPP server addresses were advertised.".into());
    for target in targets {
        // Resolve the families independently: a combined lookup waits for both
        // replies and can discard or delay a healthy family on timeout.
        // Dropping JoinSet on success/cancellation aborts remaining lookups.
        let mut lookups = crate::connection_attempts::AddressLookups::new();
        for kind in [dns::A, dns::AAAA] {
            let resolver = resolver.clone();
            let host = target.host.clone();
            lookups.spawn(async move {
                match tokio::time::timeout(DNS_TIMEOUT, resolver.query(&host, kind)).await {
                    Ok(Ok(response)) if response.code == 0 => Ok(response.records.into_iter().filter_map(|record| match record {
                        Record::Address(address) => Some(SocketAddr::new(address, target.port)),
                        _ => None,
                    }).collect::<Vec<_>>()),
                    Ok(Ok(response)) => Err(XmppError::Protocol(format!("Cannot resolve advertised XMPP server: DNS error {}", response.code))),
                    Ok(Err(error)) => Err(XmppError::Protocol(format!("Cannot resolve advertised XMPP server: {error}"))),
                    Err(_) => Err(XmppError::Timeout("advertised XMPP server address lookup".into())),
                }
            });
        }
        match crate::connection_attempts::connect(lookups, Some(ADDRESS_TIMEOUT)).await {
            Ok(stream) => return Ok(Tcp::Connected(stream, domain.into())),
            Err(error) => last_error = error,
        }
    }
    // An advertised service that fails must not silently become origin:5222.
    Err(last_error)
}

// A uniformly random value in 0..=max from the OS generator.
fn random_below(max: u64) -> u64 {
    if max == u64::MAX { let mut bytes = [0; 8]; getrandom::fill(&mut bytes).expect("OS random generator"); return u64::from_ne_bytes(bytes); }
    let range = max + 1;
    let zone = u64::MAX - (u64::MAX % range);
    loop {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).expect("OS random generator");
        let value = u64::from_ne_bytes(bytes);
        if value < zone { return value % range; }
    }
}

// RFC 2782 ordering: ascending priority, then weighted random selection.
// `random(max)` returns a uniformly random value in 0..=max.
fn order_targets(targets: Vec<Target>, random: &mut impl FnMut(u64) -> u64) -> Vec<Target> {
    let mut priorities: BTreeMap<u16, Vec<Target>> = BTreeMap::new();
    for target in targets { priorities.entry(target.priority).or_default().push(target); }
    let mut result = Vec::new();
    for (_, mut group) in priorities {
        for i in (1..group.len()).rev() { group.swap(i, random(i as u64) as usize); }
        group.sort_by_key(|target| target.weight != 0);
        while !group.is_empty() {
            let sum: u64 = group.iter().map(|target| u64::from(target.weight)).sum();
            let index = weighted_index(&group, random(sum));
            result.push(group.remove(index));
        }
    }
    result
}

fn weighted_index(targets: &[Target], draw: u64) -> usize {
    let mut sum = 0;
    targets.iter().position(|target| { sum += u64::from(target.weight); sum >= draw })
        .expect("draw is within the total SRV weight")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::tests::{question, respond};
    use std::{net::{Ipv4Addr, Ipv6Addr}, sync::{Arc, Mutex}};

    // DNS response codes used by the fixture.
    const NO_ERROR: u8 = 0;
    const SERVFAIL: u8 = 2;
    const NXDOMAIN: u8 = 3;

    fn target(host: &str, port: u16, priority: u16, weight: u16) -> Target {
        Target { host: host.into(), port, priority, weight }
    }

    // A deterministic stand-in for the OS generator (a 64-bit LCG).
    fn seeded(seed: u64) -> impl FnMut(u64) -> u64 {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        move |max| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            if max == u64::MAX { state } else { (state >> 11) % (max + 1) }
        }
    }

    struct DnsFixture {
        resolver: Resolver,
        queries: Arc<Mutex<Vec<(String, u16)>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for DnsFixture { fn drop(&mut self) { self.task.abort(); } }
    impl DnsFixture {
        async fn new(targets: Vec<Target>, response_code: u8) -> Self {
            Self::with_dual_stack(targets, response_code, false).await
        }
        async fn with_dual_stack(targets: Vec<Target>, response_code: u8, dual_stack: bool) -> Self {
            let dropped_family = if targets.iter().any(|target| target.host == "v4-only.test.") { Some(dns::AAAA) }
                else if targets.iter().any(|target| target.host == "v6-only.test.") { Some(dns::A) } else { None };
            let socket = Arc::new(tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
            let address = socket.local_addr().unwrap();
            let queries = Arc::new(Mutex::new(Vec::new()));
            let captured = queries.clone();
            let task = tokio::spawn(async move {
                let mut buffer = [0; 4096];
                let mut replies = tokio::task::JoinSet::new();
                loop {
                    let (size, peer) = socket.recv_from(&mut buffer).await.unwrap();
                    let request = buffer[..size].to_vec();
                    let (name, kind) = question(&request);
                    if Some(kind) == dropped_family { continue; }
                    captured.lock().unwrap().push((name.clone(), kind));
                    let response = match kind {
                        dns::SRV => respond(&request, response_code, &targets.iter().map(|endpoint| Record::Srv {
                            priority: endpoint.priority, weight: endpoint.weight, port: endpoint.port, target: endpoint.host.clone(),
                        }).collect::<Vec<_>>()),
                        dns::A | dns::AAAA if name == "missing.test." => respond(&request, NXDOMAIN, &[]),
                        dns::A => respond(&request, NO_ERROR, &[Record::Address(IpAddr::V4(Ipv4Addr::LOCALHOST))]),
                        dns::AAAA if dual_stack => respond(&request, NO_ERROR, &[Record::Address(IpAddr::V6(Ipv6Addr::LOCALHOST))]),
                        _ => respond(&request, NO_ERROR, &[]),
                    };
                    if kind == dns::AAAA && name == "stalled-v4.test." {
                        let socket = socket.clone();
                        replies.spawn(async move {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            socket.send_to(&response, peer).await.unwrap();
                        });
                    } else { socket.send_to(&response, peer).await.unwrap(); }
                }
            });
            let timeout = if dropped_family.is_some() { Duration::from_secs(2) } else { Duration::from_millis(200) };
            Self { resolver: Resolver::new(vec![address], timeout), queries, task }
        }
    }

    #[test]
    fn priorities_and_zero_weight_selection_follow_rfc2782() {
        let group = vec![target("zero", 1, 0, 0), target("three", 1, 0, 3), target("two", 1, 0, 2)];
        assert_eq!(weighted_index(&group, 0), 0);
        for draw in 1..=3 { assert_eq!(weighted_index(&group, draw), 1); }
        for draw in 4..=5 { assert_eq!(weighted_index(&group, draw), 2); }
        let heavy = vec![target("one", 1, 0, u16::MAX), target("two", 1, 0, u16::MAX)];
        assert_eq!(weighted_index(&heavy, 65535), 0);
        assert_eq!(weighted_index(&heavy, 131070), 1);
        for seed in 0..32 {
            let input = vec![target("later", 2, 9, 65535), target("first", 1, 0, 0), target("peer", 3, 0, 0)];
            let ordered = order_targets(input, &mut seeded(seed));
            assert_eq!(ordered.iter().map(|record| record.priority).collect::<Vec<_>>(), vec![0, 0, 9]);
            assert_ne!(ordered[0].host, ordered[1].host);
            assert_eq!(ordered[2].host, "later");
        }
    }

    #[tokio::test]
    async fn advertised_host_and_port_connect_while_tls_keeps_the_origin_identity() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dns = DnsFixture::new(vec![target("node.test.", listener.local_addr().unwrap().port(), 0, 0)], NO_ERROR).await;
        let connection = tokio::time::timeout(Duration::from_secs(2), connect_with_resolver("origin.test", &dns.resolver)).await.unwrap().unwrap();
        let Tcp::Connected(_, identity) = connection else { panic!("expected TCP transport before STARTTLS"); };
        assert_eq!(identity, "origin.test");
        let _ = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await.unwrap().unwrap();
        let queries = dns.queries.lock().unwrap();
        assert_eq!(queries[0], ("_xmpp-client._tcp.origin.test.".into(), dns::SRV));
        assert!(queries.contains(&("node.test.".into(), dns::A)));
    }

    #[tokio::test]
    async fn unavailable_service_never_falls_back_to_the_origin() {
        for targets in [vec![target(".", 0, 0, 0)], vec![target(".", 0, 0, 0), target("node.test.", 1234, 1, 0)]] {
            let dns = DnsFixture::new(targets, NO_ERROR).await;
            assert!(discover("origin.test", &dns.resolver).await.is_err());
            assert!(connect_with_resolver("origin.test", &dns.resolver).await.is_err());
            assert!(!dns.queries.lock().unwrap().is_empty());
            assert!(dns.queries.lock().unwrap().iter().all(|(_, kind)| *kind == dns::SRV));
        }
    }

    #[tokio::test]
    async fn missing_srv_and_dns_errors_allow_the_address_fallback() {
        for code in [NO_ERROR, NXDOMAIN, SERVFAIL] {
            let dns = DnsFixture::new(Vec::new(), code).await;
            assert!(tokio::time::timeout(Duration::from_secs(2), discover("origin.test.", &dns.resolver)).await.unwrap().unwrap().is_none(), "{code}");
            assert!(!dns.queries.lock().unwrap().is_empty());
            assert!(dns.queries.lock().unwrap().iter().all(|(name, _)| name == "_xmpp-client._tcp.origin.test."));
        }
    }

    #[tokio::test]
    async fn failed_targets_advance_to_the_next_priority() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        // Reserve a non-listening port. Some kernels refuse immediately;
        // others leave the SYN unanswered, exercising the connection timeout.
        let stalled = tokio::net::TcpSocket::new_v4().unwrap();
        stalled.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let stalled_port = stalled.local_addr().unwrap().port();
        let dns = DnsFixture::new(vec![target("missing.test.", 1234, 0, 0), target("stalled.test.", stalled_port, 1, 0),
            target("working.test.", listener.local_addr().unwrap().port(), 2, 0)], NO_ERROR).await;
        let _connection = tokio::time::timeout(Duration::from_secs(8), connect_with_resolver("origin.test", &dns.resolver)).await.unwrap().unwrap();
        let queries = dns.queries.lock().unwrap();
        let hosts: Vec<_> = queries.iter().filter(|(_, kind)| *kind == dns::A).map(|(name, _)| name.as_str()).collect();
        assert_eq!(hosts, vec!["missing.test.", "stalled.test.", "working.test."]);
    }

    #[tokio::test]
    async fn failed_advertised_service_does_not_turn_into_address_fallback() {
        let dns = DnsFixture::new(vec![target("missing.test.", 1234, 0, 0)], NO_ERROR).await;
        let result = connect_with_resolver("origin.test", &dns.resolver).await;
        assert!(matches!(result, Err(XmppError::Protocol(message)) if message.contains("advertised XMPP server")));
        assert!(dns.queries.lock().unwrap().contains(&("missing.test.".into(), dns::A)));
    }

    #[tokio::test]
    async fn advertised_server_can_connect_over_ipv6_when_ipv4_is_unavailable() {
        let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let dns = DnsFixture::with_dual_stack(vec![target("node.test.", listener.local_addr().unwrap().port(), 0, 0)],
            NO_ERROR, true).await;
        let connection = tokio::time::timeout(Duration::from_secs(2), connect_with_resolver("origin.test", &dns.resolver)).await.unwrap().unwrap();
        let Tcp::Connected(stream, identity) = connection else { panic!("TCP before TLS"); };
        assert!(stream.peer_addr().unwrap().is_ipv6());
        assert_eq!(identity, "origin.test");
        let queries = dns.queries.lock().unwrap();
        assert!(queries.contains(&("node.test.".into(), dns::A)));
        assert!(queries.contains(&("node.test.".into(), dns::AAAA)));
    }

    #[tokio::test]
    async fn an_unanswered_address_family_does_not_delay_a_working_connection() {
        for ipv6 in [false, true] {
            let ip = if ipv6 { IpAddr::V6(Ipv6Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
            let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
            let host = if ipv6 { "v6-only.test." } else { "v4-only.test." };
            let dns = DnsFixture::with_dual_stack(vec![target(host, listener.local_addr().unwrap().port(), 0, 0)],
                NO_ERROR, true).await;
            let connection = tokio::time::timeout(Duration::from_secs(1), connect_with_resolver("origin.test", &dns.resolver)).await;
            assert!(connection.is_ok(), "a working address must not wait for the other family's DNS timeout");
            assert!(connection.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn a_stalled_ipv4_connection_does_not_block_a_later_ipv6_result() {
        let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stalled = tokio::net::TcpSocket::new_v4().unwrap();
        stalled.bind((Ipv4Addr::LOCALHOST, port).into()).unwrap();
        let dns = DnsFixture::with_dual_stack(vec![target("stalled-v4.test.", port, 0, 0)],
            NO_ERROR, true).await;
        let connection = tokio::time::timeout(Duration::from_secs(1), connect_with_resolver("origin.test", &dns.resolver)).await;
        assert!(connection.is_ok(), "a later working IPv6 address must not wait for IPv4's connection timeout");
        let Tcp::Connected(stream, _) = connection.unwrap().unwrap() else { panic!("TCP transport"); };
        assert!(stream.peer_addr().unwrap().is_ipv6());
    }
}
