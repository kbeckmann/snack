//! Staggered TCP attempts shared by SRV discovery and address fallback.

use std::{collections::{HashSet, VecDeque}, future::Future, net::SocketAddr, time::Duration};
use tokio::{net::TcpStream, task::JoinSet, time::Instant};
use crate::error::XmppError;

pub(crate) type AddressLookups = JoinSet<Result<Vec<SocketAddr>, XmppError>>;
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);
const MINIMUM_DELAY: Duration = Duration::from_millis(10);

pub(crate) async fn connect(lookups: AddressLookups, attempt_timeout: Option<Duration>) -> Result<TcpStream, XmppError> {
    race(lookups, |address| TcpStream::connect(address), attempt_timeout).await
}

async fn race<F, Work>(mut lookups: AddressLookups, dial: F, attempt_timeout: Option<Duration>) -> Result<TcpStream, XmppError>
where
    F: Fn(SocketAddr) -> Work,
    Work: Future<Output = std::io::Result<TcpStream>> + Send + 'static,
{
    let mut pending = VecDeque::new();
    let mut seen = HashSet::new();
    let mut attempts = JoinSet::new();
    let mut last_ipv6 = None;
    let mut last_start: Option<Instant> = None;
    let mut next_start = Instant::now();
    let mut last_error = XmppError::Protocol("No usable XMPP server addresses were found.".into());
    while !lookups.is_empty() || !attempts.is_empty() || !pending.is_empty() {
        tokio::select! {
            biased;
            result = attempts.join_next(), if !attempts.is_empty() => {
                match result.expect("nonempty connection set") {
                    Ok(Ok(stream)) => return Ok(stream),
                    Ok(Err(error)) => last_error = error,
                    Err(error) => last_error = XmppError::Protocol(format!("XMPP connection attempt stopped: {error}")),
                }
                // A definite failure can advance sooner, retaining the minimum
                // spacing instead of launching an unbounded burst of SYNs.
                if let Some(start) = last_start {
                    next_start = next_start.min((start + MINIMUM_DELAY).max(Instant::now()));
                }
            }
            result = lookups.join_next(), if !lookups.is_empty() => {
                match result.expect("nonempty lookup set") {
                    Ok(Ok(addresses)) => {
                        for address in addresses {
                            if seen.insert(address) { pending.push_back(address); }
                        }
                    }
                    Ok(Err(error)) => last_error = error,
                    Err(error) => last_error = XmppError::Protocol(format!("XMPP address lookup stopped: {error}")),
                }
            }
            _ = tokio::time::sleep_until(next_start), if !pending.is_empty() => {
                // Preserve each family's order while trying the other family
                // next whenever it has become available, including late DNS.
                let index = last_ipv6.and_then(|ipv6| pending.iter().position(|address: &SocketAddr| address.is_ipv6() != ipv6)).unwrap_or(0);
                let address = pending.remove(index).expect("nonempty address queue");
                last_ipv6 = Some(address.is_ipv6());
                let start = Instant::now();
                last_start = Some(start);
                next_start = start + ATTEMPT_DELAY;
                let work = dial(address);
                attempts.spawn(async move {
                    let Some(timeout) = attempt_timeout else { return work.await.map_err(XmppError::Io); };
                    match tokio::time::timeout(timeout, work).await {
                        Ok(Ok(stream)) => Ok(stream),
                        Ok(Err(error)) => Err(XmppError::Io(error)),
                        Err(_) => Err(XmppError::Timeout("XMPP server connection".into())),
                    }
                });
            }
        }
    }
    // Both JoinSets also abort their tasks when this future is cancelled or
    // returns a successful stream, closing unused sockets and DNS futures.
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::{Ipv4Addr, Ipv6Addr}, sync::{Arc, Mutex}};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn late_ipv6_is_tried_before_more_stalled_ipv4_addresses() {
        let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let healthy = listener.local_addr().unwrap();
        let first = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
        let mut lookups = AddressLookups::new();
        lookups.spawn(async move { Ok(vec![first, SocketAddr::from((Ipv4Addr::LOCALHOST, 2))]) });
        lookups.spawn(async move { tokio::time::sleep(Duration::from_millis(50)).await; Ok(vec![healthy]) });
        let trace = Arc::new(Mutex::new(Vec::new()));
        let captured = trace.clone();
        let dial = move |address: SocketAddr| {
            captured.lock().unwrap().push((address, Instant::now()));
            async move {
                if address.is_ipv4() { std::future::pending().await }
                else { TcpStream::connect(address).await }
            }
        };
        let stream = tokio::time::timeout(Duration::from_secs(1), race(lookups, dial, None)).await.unwrap().unwrap();
        assert!(stream.peer_addr().unwrap().is_ipv6());
        let trace = trace.lock().unwrap();
        assert_eq!(trace[0].0, first);
        assert_eq!(trace[1].0, healthy);
        assert!(trace[1].1.duration_since(trace[0].1) >= Duration::from_millis(200), "attempts must be staggered");
    }

    #[tokio::test]
    async fn duplicate_addresses_are_not_retried_and_definite_failures_advance() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let healthy = listener.local_addr().unwrap();
        let failed = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
        let mut lookups = AddressLookups::new();
        lookups.spawn(async move { Ok(vec![failed, failed, healthy]) });
        let trace = Arc::new(Mutex::new(Vec::new()));
        let captured = trace.clone();
        let dial = move |address| {
            captured.lock().unwrap().push(address);
            async move {
                if address == failed { Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)) }
                else { TcpStream::connect(address).await }
            }
        };
        let _stream = tokio::time::timeout(Duration::from_secs(1), race(lookups, dial, None)).await.unwrap().unwrap();
        assert_eq!(*trace.lock().unwrap(), vec![failed, healthy]);
    }

    struct OnDrop(Arc<Notify>);
    impl Drop for OnDrop { fn drop(&mut self) { self.0.notify_one(); } }

    #[tokio::test]
    async fn success_and_cancellation_abort_unused_dns_and_socket_attempts() {
        for cancel in [false, true] {
            let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
            let healthy = listener.local_addr().unwrap();
            let mut addresses = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 1))];
            if !cancel { addresses.push(healthy); }
            let lookup_started = Arc::new(Notify::new());
            let lookup_dropped = Arc::new(Notify::new());
            let socket_started = Arc::new(Notify::new());
            let socket_dropped = Arc::new(Notify::new());
            let mut lookups = AddressLookups::new();
            lookups.spawn(async move { Ok(addresses) });
            let started = lookup_started.clone();
            let dropped = lookup_dropped.clone();
            lookups.spawn(async move {
                let _guard = OnDrop(dropped);
                started.notify_one();
                std::future::pending().await
            });
            let started = socket_started.clone();
            let dropped = socket_dropped.clone();
            let dial = move |address: SocketAddr| {
                let started = started.clone();
                let dropped = dropped.clone();
                async move {
                    if address.is_ipv4() {
                        let _guard = OnDrop(dropped);
                        started.notify_one();
                        std::future::pending().await
                    } else { TcpStream::connect(address).await }
                }
            };
            let task = tokio::spawn(race(lookups, dial, None));
            tokio::time::timeout(Duration::from_secs(1), async {
                lookup_started.notified().await;
                socket_started.notified().await;
            }).await.unwrap();
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                let stream = tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
                assert!(stream.peer_addr().unwrap().is_ipv6());
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                lookup_dropped.notified().await;
                socket_dropped.notified().await;
            }).await.unwrap();
        }
    }
}
