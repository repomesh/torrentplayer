// SPDX-License-Identifier: MIT OR Apache-2.0

//! The loopback HTTP range server.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::io::{self, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr};
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::TryStreamExt as _;
use futures_util::future::BoxFuture;
use http::header::{
    ACCEPT_RANGES, ACCESS_CONTROL_ALLOW_ORIGIN, ALLOW, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, HOST, ORIGIN, RANGE, REFERRER_POLICY, VARY,
};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Empty, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeek, AsyncSeekExt as _, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::range::{RangeRequest, parse_range};
use crate::token::SessionToken;

/// A seekable byte source the server can stream from, such as a torrent file stream.
pub trait MediaReader: AsyncRead + AsyncSeek + Unpin + Send {}
impl<T: AsyncRead + AsyncSeek + Unpin + Send> MediaReader for T {}

/// Something that can be published on the server: one video file.
pub trait MediaSource: Send + Sync {
    /// Total length in bytes.
    fn byte_len(&self) -> u64;
    /// MIME type sent as `Content-Type`.
    fn content_type(&self) -> &str;
    /// Opens a new independent reader positioned at the start.
    fn open(&self) -> BoxFuture<'static, io::Result<Box<dyn MediaReader>>>;
}

/// A one-way stream of unknown length, such as the output of an audio conversion. It is served
/// once, from the start, without `Range` support.
pub trait LiveSource: Send + Sync {
    /// MIME type sent as `Content-Type`.
    fn content_type(&self) -> &str;
    /// Takes the stream for a request; `None` once it has been taken.
    fn take(&self) -> Option<Box<dyn AsyncRead + Unpin + Send>>;
}

/// Receives timing information about requests, for streaming metrics.
pub trait RequestObserver: Send + Sync {
    /// A request for `range` of stream `stream` has arrived and passed authentication.
    fn on_request(&self, stream: StreamId, range: RangeInclusive<u64>);
    /// The first body bytes of a request starting at `offset` were ready after `waited`.
    fn on_first_byte(&self, stream: StreamId, offset: u64, waited: Duration);
}

/// Identifies a published stream.
pub type StreamId = u64;

/// Server limits.
#[derive(Clone)]
pub struct ServerConfig {
    /// Maximum simultaneous authenticated connections. A connection takes a slot with its
    /// first authenticated request; when none is free, it is answered `503 Service
    /// Unavailable`, unless a connection that has been stuck for
    /// [`ServerConfig::stall_timeout`] can be closed to make room.
    pub max_connections: usize,
    /// Connections that have not authenticated yet, kept apart so that other local programs
    /// cannot take the slots: when there are more, the oldest is closed.
    pub max_unauthenticated: usize,
    /// How long a client may take to send request headers, and to authenticate after
    /// connecting.
    pub header_read_timeout: Duration,
    /// An authenticated connection that has read or written nothing for this long (a client
    /// that stopped reading a response, or an idle keep-alive connection) is closed when its
    /// slot is needed. A player paused with its buffer full stalls like that, so it keeps its
    /// connection until then.
    pub stall_timeout: Duration,
    /// Optional metrics hook.
    pub observer: Option<Arc<dyn RequestObserver>>,
    /// Origins whose scripts may read live streams with `fetch` (answered with
    /// `Access-Control-Allow-Origin`): the app's own pages. Media elements need no such header.
    pub allowed_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 16,
            max_unauthenticated: 8,
            header_read_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_secs(30),
            observer: None,
            allowed_origins: Vec::new(),
        }
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("max_connections", &self.max_connections)
            .field("max_unauthenticated", &self.max_unauthenticated)
            .field("header_read_timeout", &self.header_read_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .field("observer", &self.observer.is_some())
            .field("allowed_origins", &self.allowed_origins)
            .finish()
    }
}

/// A stream that has been published, with the URL a player can open.
#[derive(Clone)]
pub struct PublishedStream {
    /// The stream's identifier, used to unpublish it.
    pub id: StreamId,
    /// The full URL, including the session token. Treat it as a secret: do not log it.
    pub url: String,
}

impl fmt::Debug for PublishedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishedStream")
            .field("id", &self.id)
            .field("url", &"<redacted>")
            .finish()
    }
}

/// Errors starting the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The OS random number generator failed, so no token could be created.
    #[error("could not generate a session token: {0}")]
    Token(getrandom::Error),
    /// Binding the loopback socket failed.
    #[error("could not bind the loopback stream server: {0}")]
    Bind(#[source] io::Error),
}

/// An HTTP server on `127.0.0.1` that serves published media with `Range` support.
///
/// Every request must carry the per-session token and the exact `Host` the server is bound
/// to, which blocks other local programs and DNS-rebinding attacks from websites. Only
/// explicitly published streams can be read. The server stops when this value is dropped.
#[derive(Debug)]
pub struct StreamServer {
    inner: Arc<Inner>,
    shutdown: CancellationToken,
}

struct Inner {
    addr: SocketAddr,
    host: String,
    token: SessionToken,
    sources: RwLock<HashMap<StreamId, Published>>,
    live: RwLock<HashMap<StreamId, Arc<dyn LiveSource>>>,
    next_id: AtomicU64,
    config: ServerConfig,
    /// The connection slots (see [`ServerConfig::max_connections`]).
    slots: Arc<Semaphore>,
    /// Connections holding a slot, by connection id.
    serving: Mutex<HashMap<u64, Arc<Conn>>>,
    /// Connections that have not authenticated yet, oldest first.
    pending: Mutex<VecDeque<Arc<Conn>>>,
    /// The time base of [`Conn::last_io`].
    epoch: Instant,
}

/// One client connection, shared by its task, its requests, and the server.
struct Conn {
    id: u64,
    /// Closes the connection; cancelled once it has ended, too.
    close: CancellationToken,
    /// When it last read or wrote anything, in milliseconds after [`Inner::epoch`].
    last_io: AtomicU64,
    /// The slot its first authenticated request took.
    slot: Mutex<Option<OwnedSemaphorePermit>>,
}

impl Conn {
    fn authenticated(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// Milliseconds since `epoch`.
fn millis_since(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A published source, and the token of its own that also opens it, if any.
struct Published {
    source: Arc<dyn MediaSource>,
    token: Option<SessionToken>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("addr", &self.addr)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl StreamServer {
    /// Binds to `127.0.0.1` on a random port and starts serving on the current Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the token cannot be generated or the socket cannot be bound.
    pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
        let token = SessionToken::generate().map_err(ServerError::Token)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(ServerError::Bind)?;
        let addr = listener.local_addr().map_err(ServerError::Bind)?;
        let inner = Arc::new(Inner {
            addr,
            host: addr.to_string(),
            token,
            sources: RwLock::new(HashMap::new()),
            live: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            slots: Arc::new(Semaphore::new(config.max_connections.max(1))),
            serving: Mutex::new(HashMap::new()),
            pending: Mutex::new(VecDeque::new()),
            epoch: Instant::now(),
            config,
        });
        let shutdown = CancellationToken::new();
        tokio::spawn(accept_loop(listener, Arc::clone(&inner), shutdown.clone()));
        debug!(%addr, "stream server listening");
        Ok(Self { inner, shutdown })
    }

    /// The address the server is bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.addr
    }

    /// Makes `source` available and returns its URL.
    pub fn publish(&self, source: Arc<dyn MediaSource>) -> PublishedStream {
        let url_token = self.inner.token.as_str().to_owned();
        self.insert(source, None, &url_token)
    }

    /// Makes `source` available under a token of its own, which opens nothing else, and
    /// returns its URL. For URLs handed to other programs (on a command line, where other
    /// local programs can read them): the session token stays private, and the stream's token
    /// stops working when it is unpublished.
    ///
    /// # Errors
    ///
    /// Returns an error if the token cannot be generated.
    pub fn publish_private(
        &self,
        source: Arc<dyn MediaSource>,
    ) -> Result<PublishedStream, ServerError> {
        let token = SessionToken::generate().map_err(ServerError::Token)?;
        let url_token = token.as_str().to_owned();
        Ok(self.insert(source, Some(token), &url_token))
    }

    fn insert(
        &self,
        source: Arc<dyn MediaSource>,
        token: Option<SessionToken>,
        url_token: &str,
    ) -> PublishedStream {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, Published { source, token });
        PublishedStream {
            id,
            url: format!("http://{}/s/{id}?t={url_token}", self.inner.host),
        }
    }

    /// Makes a live stream available and returns its URL. Its first request takes it.
    pub fn publish_live(&self, source: Arc<dyn LiveSource>) -> PublishedStream {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, source);
        PublishedStream {
            id,
            url: format!(
                "http://{}/l/{id}?t={}",
                self.inner.host,
                self.inner.token.as_str()
            ),
        }
    }

    /// Stops serving a stream, live or not. Requests already in progress finish normally.
    pub fn unpublish(&self, id: StreamId) {
        self.inner
            .sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        self.inner
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn accept_loop(listener: TcpListener, inner: Arc<Inner>, shutdown: CancellationToken) {
    let mut next_id: u64 = 0;
    loop {
        let (tcp, peer) = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    warn!(%error, "stream server accept failed");
                    // Avoid a hot loop if accepting keeps failing (for example, out of fds).
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        // Bound to 127.0.0.1, so this cannot fail; checked anyway as defense in depth.
        if !peer.ip().is_loopback() {
            continue;
        }
        next_id += 1;
        let conn = Arc::new(Conn {
            id: next_id,
            close: CancellationToken::new(),
            last_io: AtomicU64::new(millis_since(inner.epoch)),
            slot: Mutex::new(None),
        });
        inner.add_pending(&conn);
        let _ = tcp.set_nodelay(true);
        tokio::spawn(serve_connection(
            Arc::clone(&inner),
            conn,
            tcp,
            shutdown.clone(),
        ));
    }
}

/// Serves one connection until it ends, the server stops, or the connection is closed to make
/// room (see [`Inner::add_pending`] and [`Inner::admit`]).
async fn serve_connection(
    inner: Arc<Inner>,
    conn: Arc<Conn>,
    tcp: TcpStream,
    shutdown: CancellationToken,
) {
    let header_read_timeout = inner.config.header_read_timeout;
    let io = Watched {
        io: tcp,
        conn: Arc::clone(&conn),
        epoch: inner.epoch,
    };
    let service = {
        let inner = Arc::clone(&inner);
        let conn = Arc::clone(&conn);
        service_fn(move |req| handle(Arc::clone(&inner), Arc::clone(&conn), req))
    };
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout)
        .max_buf_size(16 * 1024)
        .serve_connection(TokioIo::new(io), service);
    // A connection must authenticate as promptly as it must send headers.
    let unauthenticated = async {
        tokio::time::sleep(header_read_timeout).await;
        if conn.authenticated() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = shutdown.cancelled() => {}
        () = conn.close.cancelled() => {}
        () = unauthenticated => debug!("stream connection did not authenticate in time"),
        result = served => if let Err(error) = result {
            debug!(%error, "stream connection ended with an error");
        },
    }
    conn.close.cancel();
    inner
        .serving
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&conn.id);
    conn.slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
}

impl Inner {
    /// Registers a connection that has not authenticated yet. Beyond
    /// [`ServerConfig::max_unauthenticated`] the oldest such connection is closed, so other
    /// local programs that open connections and send nothing cannot lock the player out: they
    /// would have to outpace it between its connecting and its sending a request.
    fn add_pending(&self, conn: &Arc<Conn>) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|c| !c.close.is_cancelled());
        if pending.len() >= self.config.max_unauthenticated.max(1)
            && let Some(oldest) = pending.pop_front()
        {
            debug!("too many unauthenticated stream connections; closing the oldest");
            oldest.close.cancel();
        }
        pending.push_back(Arc::clone(conn));
    }

    /// Gives an authenticated connection a slot, unless it has one. When none is free, the
    /// connection stuck the longest (for at least [`ServerConfig::stall_timeout`]) is closed to
    /// make room; failing that, the request is refused.
    async fn admit(&self, conn: &Arc<Conn>) -> Result<(), StatusCode> {
        if conn.authenticated() {
            return Ok(());
        }
        let permit = if let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() {
            permit
        } else {
            if let Some(stuck) = self.stuck() {
                debug!("stream connection limit reached; closing a stalled connection");
                stuck.close.cancel();
            }
            tokio::time::timeout(
                Duration::from_secs(1),
                Arc::clone(&self.slots).acquire_owned(),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                debug!("stream connection limit reached");
                StatusCode::SERVICE_UNAVAILABLE
            })?
        };
        *conn.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(permit);
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|c| c.id != conn.id);
        self.serving
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(conn.id, Arc::clone(conn));
        Ok(())
    }

    /// The connection holding a slot that has read and written nothing for the longest, if
    /// that is at least [`ServerConfig::stall_timeout`].
    fn stuck(&self) -> Option<Arc<Conn>> {
        let now = millis_since(self.epoch);
        let limit = u64::try_from(self.config.stall_timeout.as_millis()).unwrap_or(u64::MAX);
        self.serving
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|c| (now.saturating_sub(c.last_io.load(Ordering::Relaxed)), c))
            .filter(|(idle, c)| *idle >= limit && !c.close.is_cancelled())
            .max_by_key(|(idle, _)| *idle)
            .map(|(_, c)| Arc::clone(c))
    }
}

/// A connection's socket, recording when it last made progress (see [`Inner::stuck`]).
struct Watched {
    io: TcpStream,
    conn: Arc<Conn>,
    epoch: Instant,
}

impl Watched {
    fn progressed(&self) {
        self.conn
            .last_io
            .store(millis_since(self.epoch), Ordering::Relaxed);
    }
}

impl AsyncRead for Watched {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.io).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.progressed();
        }
        polled
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write(cx, buf);
        if matches!(polled, Poll::Ready(Ok(n)) if n > 0) {
            this.progressed();
        }
        polled
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        if matches!(polled, Poll::Ready(Ok(n)) if n > 0) {
            this.progressed();
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

type Body = UnsyncBoxBody<Bytes, io::Error>;

async fn handle(
    inner: Arc<Inner>,
    conn: Arc<Conn>,
    req: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let mut response = respond(&inner, &conn, &req).await.unwrap_or_else(simple);
    // Refused before authenticating: the connection has nothing more to do here.
    if !conn.authenticated() {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    Ok(response)
}

async fn respond(
    inner: &Inner,
    conn: &Arc<Conn>,
    req: &Request<Incoming>,
) -> Result<Response<Body>, StatusCode> {
    // Authenticate before revealing anything, including whether a stream exists.
    let stream = authenticate(inner, req)?;
    inner.admit(conn).await?;

    let head = match *req.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => {
            let mut response = simple(StatusCode::METHOD_NOT_ALLOWED);
            response
                .headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
            return Ok(response);
        }
    };

    if let Some(live) = req.uri().path().strip_prefix("/l/") {
        return respond_live(inner, req, live, head);
    }

    let (id, source) = stream.ok_or(StatusCode::NOT_FOUND)?;
    let len = source.byte_len();
    let range = parse_range(req.headers().get(RANGE).map(HeaderValue::as_bytes), len);
    let (status, range) = match range {
        RangeRequest::Unsatisfiable => {
            let mut response = simple(StatusCode::RANGE_NOT_SATISFIABLE);
            if let Ok(v) = HeaderValue::try_from(format!("bytes */{len}")) {
                response.headers_mut().insert(CONTENT_RANGE, v);
            }
            return Ok(response);
        }
        RangeRequest::Full if len == 0 => (StatusCode::OK, None),
        RangeRequest::Full => (StatusCode::OK, Some(0..=len - 1)),
        RangeRequest::Partial(r) => (StatusCode::PARTIAL_CONTENT, Some(r)),
    };
    let count = range.as_ref().map_or(0, |r| r.end() - r.start() + 1);

    let mut builder = Response::builder()
        .status(status)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, count)
        .header(CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .header(REFERRER_POLICY, "no-referrer")
        .header(
            CONTENT_TYPE,
            HeaderValue::from_str(source.content_type())
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
        );
    if status == StatusCode::PARTIAL_CONTENT
        && let Some(r) = &range
    {
        builder = builder.header(
            CONTENT_RANGE,
            format!("bytes {}-{}/{len}", r.start(), r.end()),
        );
    }

    let body = match range {
        Some(range) if !head => {
            if let Some(observer) = &inner.config.observer {
                observer.on_request(id, range.clone());
            }
            stream_body(inner, &*source, id, *range.start(), count)
                .await
                .map_err(|error| {
                    warn!(%error, stream = id, "could not open stream");
                    StatusCode::SERVICE_UNAVAILABLE
                })?
        }
        _ => empty(),
    };
    builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// A published stream and its id.
type Found = (StreamId, Arc<dyn MediaSource>);

/// Checks the `Host` header and the token. Returns the stream asked for, if it is published,
/// with its id.
fn authenticate(inner: &Inner, req: &Request<Incoming>) -> Result<Option<Found>, StatusCode> {
    let host_ok = req
        .headers()
        .get(HOST)
        .is_some_and(|h| h.as_bytes() == inner.host.as_bytes());
    let presented = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("t=")))
        .map(str::as_bytes);
    let session_ok = presented.is_some_and(|t| inner.token.matches(t));
    let id: Option<StreamId> = req
        .uri()
        .path()
        .strip_prefix("/s/")
        .and_then(|s| s.parse().ok());
    let published = id.and_then(|id| {
        inner
            .sources
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .map(|p| (Arc::clone(&p.source), p.token.clone()))
    });
    // A stream's own token opens only that stream.
    let stream_ok = presented
        .zip(published.as_ref().and_then(|(_, token)| token.as_ref()))
        .is_some_and(|(presented, token)| token.matches(presented));
    if !host_ok || !(session_ok || stream_ok) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(id.zip(published.map(|(source, _)| source)))
}

/// Serves a live stream: once, whole, with a CORS header for the app's own origins.
fn respond_live(
    inner: &Inner,
    req: &Request<Incoming>,
    id: &str,
    head: bool,
) -> Result<Response<Body>, StatusCode> {
    let id: StreamId = id.parse().map_err(|_| StatusCode::NOT_FOUND)?;
    let source = inner
        .live
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .header(REFERRER_POLICY, "no-referrer")
        .header(VARY, "Origin")
        .header(
            CONTENT_TYPE,
            HeaderValue::from_str(source.content_type())
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
        );
    if let Some(origin) = req.headers().get(ORIGIN)
        && inner
            .config
            .allowed_origins
            .iter()
            .any(|allowed| allowed.as_bytes() == origin.as_bytes())
    {
        builder = builder.header(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    let body = if head {
        empty()
    } else {
        let reader = source.take().ok_or(StatusCode::GONE)?;
        let stream = ReaderStream::with_capacity(reader, 256 * 1024);
        StreamBody::new(stream.map_ok(Frame::data)).boxed_unsync()
    };
    builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn stream_body(
    inner: &Inner,
    source: &dyn MediaSource,
    id: StreamId,
    start: u64,
    count: u64,
) -> io::Result<Body> {
    let requested = Instant::now();
    let mut reader = source.open().await?;
    reader.seek(SeekFrom::Start(start)).await?;
    let reader = reader.take(count);

    let observer = inner.config.observer.clone();
    let mut first = true;
    let stream = ReaderStream::with_capacity(reader, 256 * 1024).inspect_ok(move |_| {
        if std::mem::take(&mut first)
            && let Some(observer) = &observer
        {
            observer.on_first_byte(id, start, requested.elapsed());
        }
    });
    Ok(StreamBody::new(stream.map_ok(Frame::data)).boxed_unsync())
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

fn simple(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(empty());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
