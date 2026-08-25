//! Controllable TCP forwarding for network-chaos integration tests.

use std::{
    net::{SocketAddr, ToSocketAddrs as _},
    num::NonZeroU64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{Notify, watch},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DirectionCondition {
    paused: bool,
    latency: Duration,
    bytes_per_second: Option<NonZeroU64>,
}

#[derive(Debug, Default)]
struct ProxyStats {
    accepted: AtomicU64,
    active: AtomicU64,
    closed: AtomicU64,
    rejected_while_disconnected: AtomicU64,
    client_to_upstream_bytes: AtomicU64,
    upstream_to_client_bytes: AtomicU64,
}

#[derive(Debug)]
struct ProxyInner {
    enabled: AtomicBool,
    generation: Mutex<CancellationToken>,
    shutdown: CancellationToken,
    stats: ProxyStats,
    changed: Notify,
    client_to_upstream: watch::Sender<DirectionCondition>,
    upstream_to_client: watch::Sender<DirectionCondition>,
}

/// A stable local TCP endpoint whose live connections and forwarding behavior are test-controlled.
///
/// `disconnect` closes established sockets as well as rejecting new attempts. `resume` keeps the
/// same listener address, forcing clients to exercise their normal reconnect path.
#[derive(Debug)]
pub(crate) struct TcpChaosProxy {
    listen_addr: SocketAddr,
    inner: Arc<ProxyInner>,
    accept_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
struct DirectedP2pProxy {
    from: usize,
    to: usize,
    proxy: TcpChaosProxy,
}

/// A controllable proxy for every directed link in a three-node P2P cluster.
///
/// Commonware peers can establish a connection in either direction. Giving each node a distinct
/// proxied view of every remote address lets tests isolate a member without depending on which
/// side happened to dial the live connection.
#[derive(Debug)]
pub(crate) struct P2pChaosNetwork {
    links: Vec<DirectedP2pProxy>,
}

impl P2pChaosNetwork {
    /// Start one proxy for each directed peer link.
    ///
    /// Returns the per-node manifest addresses. Row `from`, column `to` is the address node
    /// `from` should use for node `to`; diagonal entries retain the node's real listen address.
    pub(crate) async fn start(
        node_addresses: [SocketAddr; 3],
    ) -> eyre::Result<(Self, [[SocketAddr; 3]; 3])> {
        let mut manifest_addresses = [node_addresses; 3];
        let mut links = Vec::with_capacity(6);
        for (from, manifest_row) in manifest_addresses.iter_mut().enumerate() {
            for (to, (manifest_address, &node_address)) in
                manifest_row.iter_mut().zip(&node_addresses).enumerate()
            {
                if from == to {
                    continue;
                }
                let proxy = TcpChaosProxy::start(node_address).await?;
                *manifest_address = proxy.listen_addr();
                links.push(DirectedP2pProxy { from, to, proxy });
            }
        }
        Ok((Self { links }, manifest_addresses))
    }

    fn links_for<'a>(&'a self, nodes: &'a [usize]) -> impl Iterator<Item = &'a TcpChaosProxy> + 'a {
        self.links
            .iter()
            .filter(|link| nodes.contains(&link.from) || nodes.contains(&link.to))
            .map(|link| &link.proxy)
    }

    pub(crate) fn disconnect_nodes(&self, nodes: &[usize]) {
        for proxy in self.links_for(nodes) {
            proxy.disconnect();
        }
    }

    pub(crate) fn resume_nodes(&self, nodes: &[usize]) {
        for proxy in self.links_for(nodes) {
            proxy.resume();
        }
    }

    pub(crate) async fn wait_for_nodes_disconnected(
        &self,
        nodes: &[usize],
        timeout: Duration,
    ) -> eyre::Result<()> {
        for proxy in self.links_for(nodes) {
            proxy.wait_for_no_connections(timeout).await?;
        }
        Ok(())
    }

    /// Add symmetric latency to every P2P stream involving any of `nodes`.
    pub(crate) fn set_nodes_latency(&self, nodes: &[usize], latency: Duration) {
        for proxy in self.links_for(nodes) {
            proxy.set_client_to_upstream_latency(latency);
            proxy.set_upstream_to_client_latency(latency);
        }
    }
}

impl TcpChaosProxy {
    pub(crate) async fn start(upstream: SocketAddr) -> eyre::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let listen_addr = listener.local_addr()?;
        let (client_to_upstream, _) = watch::channel(DirectionCondition::default());
        let (upstream_to_client, _) = watch::channel(DirectionCondition::default());
        let inner = Arc::new(ProxyInner {
            enabled: AtomicBool::new(true),
            generation: Mutex::new(CancellationToken::new()),
            shutdown: CancellationToken::new(),
            stats: ProxyStats::default(),
            changed: Notify::new(),
            client_to_upstream,
            upstream_to_client,
        });
        let task_inner = Arc::clone(&inner);
        let accept_task = tokio::spawn(async move {
            run_accept_loop(listener, upstream, task_inner).await;
        });
        Ok(Self {
            listen_addr,
            inner,
            accept_task,
        })
    }

    /// Resolve the TCP destination of an HTTP or WebSocket URL.
    pub(crate) fn upstream_addr(url: &url::Url) -> eyre::Result<SocketAddr> {
        let host = url
            .host_str()
            .ok_or_else(|| eyre::eyre!("RPC URL has no host: {url}"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| eyre::eyre!("RPC URL has no port: {url}"))?;
        (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| eyre::eyre!("RPC URL host resolved to no addresses: {url}"))
    }

    /// Rewrite an RPC URL to this proxy while retaining its scheme and path.
    pub(crate) fn proxy_url(&self, upstream_url: &url::Url) -> eyre::Result<url::Url> {
        let mut url = upstream_url.clone();
        url.set_ip_host(self.listen_addr.ip())
            .map_err(|()| eyre::eyre!("failed setting proxy host"))?;
        url.set_port(Some(self.listen_addr.port()))
            .map_err(|()| eyre::eyre!("failed setting proxy port"))?;
        Ok(url)
    }

    pub(crate) const fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub(crate) fn accepted_connections(&self) -> u64 {
        self.inner.stats.accepted.load(Ordering::Acquire)
    }

    pub(crate) fn active_connections(&self) -> u64 {
        self.inner.stats.active.load(Ordering::Acquire)
    }

    pub(crate) fn disconnect(&self) {
        self.inner.enabled.store(false, Ordering::Release);
        let mut generation = self.inner.generation.lock().expect("proxy lock poisoned");
        generation.cancel();
        *generation = CancellationToken::new();
        drop(generation);
        self.inner.changed.notify_waiters();
    }

    pub(crate) fn resume(&self) {
        self.inner.enabled.store(true, Ordering::Release);
        self.inner.changed.notify_waiters();
    }

    pub(crate) fn pause_client_to_upstream(&self, paused: bool) {
        self.inner
            .client_to_upstream
            .send_modify(|condition| condition.paused = paused);
    }

    pub(crate) fn pause_upstream_to_client(&self, paused: bool) {
        self.inner
            .upstream_to_client
            .send_modify(|condition| condition.paused = paused);
    }

    pub(crate) fn set_client_to_upstream_latency(&self, latency: Duration) {
        self.inner
            .client_to_upstream
            .send_modify(|condition| condition.latency = latency);
    }

    pub(crate) fn set_upstream_to_client_latency(&self, latency: Duration) {
        self.inner
            .upstream_to_client
            .send_modify(|condition| condition.latency = latency);
    }

    pub(crate) fn set_client_to_upstream_bandwidth(&self, bytes_per_second: Option<NonZeroU64>) {
        self.inner
            .client_to_upstream
            .send_modify(|condition| condition.bytes_per_second = bytes_per_second);
    }

    pub(crate) fn set_upstream_to_client_bandwidth(&self, bytes_per_second: Option<NonZeroU64>) {
        self.inner
            .upstream_to_client
            .send_modify(|condition| condition.bytes_per_second = bytes_per_second);
    }

    pub(crate) async fn wait_for_no_connections(&self, timeout: Duration) -> eyre::Result<()> {
        self.wait_for(timeout, "all proxied connections to close", || {
            self.active_connections() == 0
        })
        .await
    }

    pub(crate) async fn wait_for_connections_after(
        &self,
        previous_accepted: u64,
        minimum_new_connections: u64,
        timeout: Duration,
    ) -> eyre::Result<u64> {
        assert!(minimum_new_connections > 0);
        let target = previous_accepted.saturating_add(minimum_new_connections);
        self.wait_for(timeout, "fresh proxied connections", || {
            self.accepted_connections() >= target
        })
        .await?;
        Ok(self.accepted_connections())
    }

    async fn wait_for(
        &self,
        timeout: Duration,
        description: &str,
        condition: impl Fn() -> bool,
    ) -> eyre::Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.inner.changed.notified();
                if condition() {
                    return;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| eyre::eyre!("timed out after {timeout:?} waiting for {description}"))
    }
}

impl Drop for TcpChaosProxy {
    fn drop(&mut self) {
        self.inner.shutdown.cancel();
        self.inner
            .generation
            .lock()
            .expect("proxy lock poisoned")
            .cancel();
        self.accept_task.abort();
    }
}

async fn run_accept_loop(listener: TcpListener, upstream: SocketAddr, inner: Arc<ProxyInner>) {
    loop {
        let accepted = tokio::select! {
            _ = inner.shutdown.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((client, _)) = accepted else {
            return;
        };
        if !inner.enabled.load(Ordering::Acquire) {
            inner
                .stats
                .rejected_while_disconnected
                .fetch_add(1, Ordering::AcqRel);
            drop(client);
            inner.changed.notify_waiters();
            continue;
        }

        let generation = inner
            .generation
            .lock()
            .expect("proxy lock poisoned")
            .child_token();
        let connection_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            run_connection(client, upstream, generation, connection_inner).await;
        });
    }
}

async fn run_connection(
    client: TcpStream,
    upstream: SocketAddr,
    generation: CancellationToken,
    inner: Arc<ProxyInner>,
) {
    let upstream_stream = tokio::select! {
        _ = generation.cancelled() => return,
        result = TcpStream::connect(upstream) => match result {
            Ok(stream) => stream,
            Err(_) => return,
        },
    };
    if !inner.enabled.load(Ordering::Acquire) {
        return;
    }

    inner.stats.accepted.fetch_add(1, Ordering::AcqRel);
    inner.stats.active.fetch_add(1, Ordering::AcqRel);
    inner.changed.notify_waiters();

    let connection_cancel = generation.child_token();
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream_stream.into_split();
    let client_to_upstream = pump(
        client_read,
        upstream_write,
        inner.client_to_upstream.subscribe(),
        connection_cancel.clone(),
        &inner.stats.client_to_upstream_bytes,
    );
    let upstream_to_client = pump(
        upstream_read,
        client_write,
        inner.upstream_to_client.subscribe(),
        connection_cancel.clone(),
        &inner.stats.upstream_to_client_bytes,
    );
    tokio::pin!(client_to_upstream, upstream_to_client);
    tokio::select! {
        _ = &mut client_to_upstream => {},
        _ = &mut upstream_to_client => {},
        _ = connection_cancel.cancelled() => {},
    }
    connection_cancel.cancel();
    inner.stats.active.fetch_sub(1, Ordering::AcqRel);
    inner.stats.closed.fetch_add(1, Ordering::AcqRel);
    inner.changed.notify_waiters();
}

async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    mut conditions: watch::Receiver<DirectionCondition>,
    cancel: CancellationToken,
    bytes: &AtomicU64,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        if !wait_until_resumed(&mut conditions, &cancel).await {
            return Ok(());
        }
        let read = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = reader.read(&mut buffer) => result?,
        };
        if read == 0 {
            return writer.shutdown().await;
        }

        // `paused` may change while `read` is pending. Recheck it before forwarding the bytes so
        // enabling a pause cannot leak the chunk that wakes the read.
        if !wait_until_resumed(&mut conditions, &cancel).await {
            return Ok(());
        }

        let condition = *conditions.borrow();
        let throttle = condition.bytes_per_second.map_or(Duration::ZERO, |rate| {
            Duration::from_secs_f64(read as f64 / rate.get() as f64)
        });
        let delay = condition.latency.saturating_add(throttle);
        if !delay.is_zero() {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(delay) => {},
            }
        }

        // The pause can also be enabled while the latency or bandwidth delay is pending.
        // Recheck immediately before forwarding so that delayed chunks cannot leak through.
        if !wait_until_resumed(&mut conditions, &cancel).await {
            return Ok(());
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = writer.write_all(&buffer[..read]) => result?,
        }
        bytes.fetch_add(read as u64, Ordering::Relaxed);
    }
}

/// Wait until forwarding is enabled again. Returns false if the connection or condition stream
/// has been cancelled, in which case the caller should stop forwarding.
async fn wait_until_resumed(
    conditions: &mut watch::Receiver<DirectionCondition>,
    cancel: &CancellationToken,
) -> bool {
    while conditions.borrow().paused {
        tokio::select! {
            _ = cancel.cancelled() => return false,
            changed = conditions.changed() => if changed.is_err() { return false },
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start_echo_server() -> eyre::Result<(SocketAddr, CancellationToken)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = task_shutdown.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = accepted else {
                    return;
                };
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.split();
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                });
            }
        });
        Ok((address, shutdown))
    }

    #[tokio::test]
    async fn disconnect_closes_live_socket_and_resume_accepts_a_fresh_connection()
    -> eyre::Result<()> {
        let (upstream, shutdown) = start_echo_server().await?;
        let proxy = TcpChaosProxy::start(upstream).await?;
        let mut first = TcpStream::connect(proxy.listen_addr()).await?;
        first.write_all(b"before").await?;
        let mut echoed = [0_u8; 6];
        first.read_exact(&mut echoed).await?;
        assert_eq!(&echoed, b"before");

        let accepted = proxy.accepted_connections();
        proxy.disconnect();
        proxy
            .wait_for_no_connections(Duration::from_secs(2))
            .await?;
        let closed = tokio::time::timeout(Duration::from_secs(2), first.read(&mut echoed)).await?;
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "old connection still carried data after disconnect: {closed:?}"
        );

        proxy.resume();
        let mut second = TcpStream::connect(proxy.listen_addr()).await?;
        second.write_all(b"after!").await?;
        second.read_exact(&mut echoed).await?;
        assert_eq!(&echoed, b"after!");
        proxy
            .wait_for_connections_after(accepted, 1, Duration::from_secs(2))
            .await?;
        shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn directional_pause_latency_and_bandwidth_preserve_the_stream() -> eyre::Result<()> {
        let (upstream, shutdown) = start_echo_server().await?;
        let proxy = TcpChaosProxy::start(upstream).await?;
        let mut client = TcpStream::connect(proxy.listen_addr()).await?;
        client.write_all(b"ready!").await?;
        let mut warmup = [0_u8; 6];
        client.read_exact(&mut warmup).await?;
        assert_eq!(&warmup, b"ready!");

        // Pause an established stream after its forwarding loop has returned to `read`. This
        // covers the race where the paused state changes while that read is pending.
        proxy.pause_client_to_upstream(true);
        proxy.pause_upstream_to_client(false);
        proxy.set_upstream_to_client_latency(Duration::from_millis(100));
        proxy.set_client_to_upstream_bandwidth(NonZeroU64::new(8 * 1024));
        proxy.set_upstream_to_client_bandwidth(None);

        let payload = vec![0x5a; 4 * 1024];
        client.write_all(&payload).await?;
        let mut echoed = vec![0_u8; payload.len()];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.read_exact(&mut echoed))
                .await
                .is_err(),
            "paused direction unexpectedly forwarded the payload"
        );

        let started = tokio::time::Instant::now();
        proxy.pause_client_to_upstream(false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        proxy.pause_client_to_upstream(true);
        assert!(
            tokio::time::timeout(Duration::from_millis(800), client.read_exact(&mut echoed))
                .await
                .is_err(),
            "pause enabled during the bandwidth delay leaked the pending payload"
        );
        proxy.pause_client_to_upstream(false);
        client.read_exact(&mut echoed).await?;
        assert_eq!(echoed, payload);
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "configured latency and bandwidth limit were not applied"
        );

        proxy.set_client_to_upstream_latency(Duration::ZERO);
        proxy.set_upstream_to_client_latency(Duration::ZERO);
        shutdown.cancel();
        Ok(())
    }
}
