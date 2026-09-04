//! A hardened newline-delimited JSON-RPC transport for MCP servers reached
//! over a byte stream (a child process' stdio, or the in-process duplex pipe
//! Android uses instead).
//!
//! This exists because `rmcp` 0.9's own `AsyncRwTransport` has three
//! properties that are fine for a well-behaved server and fatal for a
//! third-party one:
//!
//! 1. **Any decode error kills the connection permanently.** `FramedRead`
//!    yields `Some(Err(..))`, rmcp maps it to `None`, and the serve loop
//!    treats `None` as EOF and breaks `Closed`. A server that logs one
//!    non-JSON line to stdout therefore takes its entire tool set offline
//!    until someone reconnects it by hand. Here a *decode* error (bad JSON,
//!    overlong line) is logged and skipped; only a real I/O error or EOF
//!    ends the stream.
//! 2. **The read buffer is unbounded** (`JsonRpcMessageCodec`'s default
//!    `max_length` is `usize::MAX`, which also makes its own
//!    discard-and-resync path unreachable). A server emitting bytes with no
//!    newline grows that buffer until the daemon is OOM-killed — on a phone,
//!    that's the whole app. We build the codec with an explicit frame cap so
//!    the codec's discard-to-next-newline resync actually engages.
//! 3. **The write mutex is held across `send().await`.** If the child is
//!    alive but not reading its stdin, the pipe fills (~4 KB on Windows),
//!    the in-flight `send` blocks forever holding the lock, and every later
//!    send queues behind it in a growing `JoinSet` — a task leak plus total,
//!    permanent loss of that server even though the caller's own timeout
//!    fires. Here every send is bounded by `WRITE_TIMEOUT`; a send that
//!    times out marks the transport closed so subsequent sends fail fast
//!    (which surfaces to the manager as a dead server it can evict and
//!    reconnect) instead of piling up behind the wedged one.
//!
//! The read side drives `rmcp`'s line codec by hand rather than wrapping it
//! in a `tokio_util::codec::FramedRead`, because `FramedRead` latches itself
//! off after yielding one error: every later poll returns `None`, which the
//! serve loop reads as EOF. That is the same failure as (1) by another route,
//! so skip-and-continue has to live below it.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use futures::SinkExt;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError};
use rmcp::transport::Transport;
use rmcp::RoleClient;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::{Decoder, FramedWrite};

/// Maximum bytes for one JSON-RPC line. Past this the codec discards to the
/// next newline and resyncs (see `JsonRpcMessageCodec`'s `is_discarding`
/// branch, which is dead code at the default `usize::MAX`). Generous enough
/// that no legitimate tool result comes near it — the tool-output path
/// truncates at 100 KB — while still bounding a hostile server's ability to
/// grow our heap.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// How long a single outbound write may block before the transport is
/// declared dead. A child that isn't draining its stdin is wedged; waiting
/// longer only grows the queue behind it.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

/// Spare capacity ensured before each read.
const READ_CHUNK_BYTES: usize = 16 * 1024;

type Codec = JsonRpcMessageCodec<RxJsonRpcMessage<RoleClient>>;
type Writer<W> = FramedWrite<W, JsonRpcMessageCodec<TxJsonRpcMessage<RoleClient>>>;

/// The read half. Deliberately *not* a `tokio_util` `FramedRead`: that latches
/// itself off after yielding a single error (every later poll returns `None`,
/// which rmcp's serve loop reads as EOF), which is precisely the "one bad line
/// kills the server" failure this transport exists to prevent. Driving the
/// codec by hand lets a decode error be logged and stepped over.
///
/// Cancellation-safe, which matters because rmcp polls `receive()` inside a
/// `select!`: the only await is `read_buf`, which either appends bytes to
/// `buf` or does nothing, and `buf` lives here rather than on the stack.
struct Reader<R> {
    io: R,
    codec: Codec,
    buf: BytesMut,
    eof: bool,
    /// Latched on a real I/O failure — distinct from `eof`, which is a clean
    /// end of stream.
    failed: bool,
}

impl<R: AsyncRead + Send + Unpin> Reader<R> {
    fn new(io: R) -> Self {
        Self {
            io,
            codec: JsonRpcMessageCodec::new_with_max_length(MAX_FRAME_BYTES),
            buf: BytesMut::new(),
            eof: false,
            failed: false,
        }
    }

    /// Decode as far as the buffered bytes allow. `Some(msg)` on a message,
    /// `None` when more input is needed, `Err(())` when the stream is over.
    #[allow(clippy::result_unit_err)]
    fn drain_buffer(&mut self) -> Result<Option<RxJsonRpcMessage<RoleClient>>, ()> {
        loop {
            let before = self.buf.len();
            let decoded = if self.eof {
                self.codec.decode_eof(&mut self.buf)
            } else {
                self.codec.decode(&mut self.buf)
            };
            match decoded {
                Ok(Some(msg)) => return Ok(Some(msg)),
                Ok(None) => {
                    // The codec also answers `Ok(None)` after *consuming* a
                    // line it chose to ignore (a non-standard notification).
                    // Only a decode that consumed nothing genuinely needs
                    // more input — otherwise a complete message already
                    // sitting behind it would wait for a read that never
                    // comes.
                    if self.buf.len() == before {
                        return Ok(None);
                    }
                }
                // Recoverable: the codec has already consumed the bad line,
                // or is discarding to the next newline, so decoding resumes
                // at a frame boundary. Keep the connection.
                Err(JsonRpcMessageCodecError::Serde(e)) => {
                    tracing::warn!("skipping unparsable MCP line: {e}");
                }
                Err(JsonRpcMessageCodecError::MaxLineLengthExceeded) => {
                    tracing::warn!(
                        "skipping MCP line over the {MAX_FRAME_BYTES}-byte frame cap; resyncing at the next newline"
                    );
                }
                // Not recoverable: the pipe itself is gone.
                Err(JsonRpcMessageCodecError::Io(e)) => {
                    tracing::error!("MCP transport decode error: {e}");
                    self.failed = true;
                    return Err(());
                }
            }
        }
    }

    async fn next_message(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            if self.failed {
                return None;
            }
            match self.drain_buffer() {
                Ok(Some(msg)) => return Some(msg),
                Ok(None) => {}
                Err(()) => return None,
            }
            if self.eof {
                return None;
            }
            // `read_buf` fills spare capacity only; with none left it would
            // return `Ok(0)` and be misread as EOF.
            self.buf.reserve(READ_CHUNK_BYTES);
            match self.io.read_buf(&mut self.buf).await {
                Ok(0) => self.eof = true,
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("MCP transport read error: {e}");
                    self.failed = true;
                    return None;
                }
            }
        }
    }
}

/// The client half of a newline-delimited JSON-RPC session over any
/// `AsyncRead`/`AsyncWrite` pair. See the module docs for what it hardens.
pub struct HardenedRwTransport<R, W> {
    read: Reader<R>,
    write: Arc<Mutex<Option<Writer<W>>>>,
    /// Set once a write times out, so later sends fail immediately rather
    /// than each burning their own `WRITE_TIMEOUT` behind the same wedge.
    wedged: Arc<AtomicBool>,
}

impl<R, W> HardenedRwTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    pub fn new(read: R, write: W) -> Self {
        Self {
            read: Reader::new(read),
            write: Arc::new(Mutex::new(Some(FramedWrite::new(
                write,
                JsonRpcMessageCodec::new_with_max_length(MAX_FRAME_BYTES),
            )))),
            wedged: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Read the next protocol message, skipping past anything that isn't one.
    /// `None` means the stream is genuinely over (EOF or I/O error).
    pub async fn next_message(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.read.next_message().await
    }

    fn send_bounded(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>> + Send + 'static {
        let lock = self.write.clone();
        let wedged = self.wedged.clone();
        async move {
            if wedged.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "MCP transport is wedged (a previous write timed out)",
                ));
            }
            let write_fut = async {
                let mut guard = lock.lock().await;
                match guard.as_mut() {
                    Some(write) => write.send(item).await.map_err(std::io::Error::from),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "MCP transport is closed",
                    )),
                }
            };
            match tokio::time::timeout(WRITE_TIMEOUT, write_fut).await {
                Ok(result) => result,
                Err(_) => {
                    // The peer is alive but not draining its input. Mark the
                    // transport dead so the queue behind this write drains
                    // as errors instead of blocking; the manager's health
                    // watcher then evicts and reconnects the server.
                    wedged.store(true, Ordering::Relaxed);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "MCP transport write blocked for {}s; treating the peer as dead",
                            WRITE_TIMEOUT.as_secs()
                        ),
                    ))
                }
            }
        }
    }

    /// Drop the writer, closing the peer's input (for a child process this is
    /// what asks it to exit).
    pub async fn close_write(&mut self) {
        let mut guard = self.write.lock().await;
        drop(guard.take());
    }
}

impl<R, W> Transport<RoleClient> for HardenedRwTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn name() -> Cow<'static, str> {
        "bigtiny-hardened-rw".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.send_bounded(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.next_message()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.close_write().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// A well-formed `ServerJsonRpcMessage` — a server-initiated `ping`
    /// request, which needs no prior state to be valid.
    const VALID_SERVER_LINE: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    type DuplexTransport = HardenedRwTransport<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >;

    fn transport(stream: tokio::io::DuplexStream) -> DuplexTransport {
        let (r, w) = tokio::io::split(stream);
        HardenedRwTransport::new(r, w)
    }

    /// Bounded so a regression that stops skipping (and therefore waits for a
    /// message that never comes) fails the test instead of hanging the suite.
    async fn next(t: &mut DuplexTransport) -> Option<RxJsonRpcMessage<RoleClient>> {
        tokio::time::timeout(Duration::from_secs(5), t.next_message())
            .await
            .expect("next_message must not block waiting for more input")
    }

    /// The headline fix: a server that writes a plain log line to stdout must
    /// not take its whole tool set offline. rmcp 0.9's own transport returns
    /// `None` here, which the serve loop reads as EOF.
    #[tokio::test]
    async fn a_non_json_line_is_skipped_not_fatal() {
        let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
        let mut t = transport(client_side);

        server_side
            .write_all(b"[info] starting up, definitely not JSON\n")
            .await
            .unwrap();
        server_side.write_all(VALID_SERVER_LINE).await.unwrap();
        server_side.write_all(b"\n").await.unwrap();

        assert!(
            next(&mut t).await.is_some(),
            "the valid message after the junk must arrive"
        );
    }

    /// Well-formed JSON that isn't a JSON-RPC message is also a decode error,
    /// and must likewise be skipped rather than closing the connection.
    #[tokio::test]
    async fn a_json_line_that_is_not_a_jsonrpc_message_is_skipped() {
        let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
        let mut t = transport(client_side);

        server_side
            .write_all(b"{\"hello\":\"world\"}\n")
            .await
            .unwrap();
        server_side.write_all(VALID_SERVER_LINE).await.unwrap();
        server_side.write_all(b"\n").await.unwrap();

        assert!(next(&mut t).await.is_some());
    }

    /// A line past the frame cap must be discarded up to the next newline —
    /// bounding the read buffer — and the connection must survive it. With
    /// rmcp's default `usize::MAX` cap this path is unreachable and the
    /// buffer just grows.
    #[tokio::test]
    async fn an_overlong_line_is_discarded_and_the_stream_resyncs() {
        let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_side);
        // Small cap so the test doesn't have to push 32 MB through the pipe.
        let mut t = HardenedRwTransport::new(r, w);
        t.read.codec = JsonRpcMessageCodec::new_with_max_length(512);

        let writer = tokio::spawn(async move {
            server_side.write_all(&vec![b'x'; 4096]).await.unwrap();
            server_side.write_all(b"\n").await.unwrap();
            server_side.write_all(VALID_SERVER_LINE).await.unwrap();
            server_side.write_all(b"\n").await.unwrap();
            server_side
        });

        assert!(
            next(&mut t).await.is_some(),
            "the message after an over-cap line must still arrive"
        );
        let _keep_open = writer.await.unwrap();
    }

    #[tokio::test]
    async fn eof_ends_the_stream() {
        let (client_side, server_side) = tokio::io::duplex(1024);
        let mut t = transport(client_side);
        drop(server_side);
        assert!(t.next_message().await.is_none());
    }

    /// A peer that never reads must not wedge the transport forever. The
    /// write is bounded, and the transport latches closed so the sends queued
    /// behind it fail immediately instead of each waiting their own timeout.
    #[tokio::test(start_paused = true)]
    async fn a_write_to_a_peer_that_never_reads_times_out_and_latches_closed() {
        // Tiny buffer, and we never read the server side, so the pipe fills.
        let (client_side, _server_side) = tokio::io::duplex(16);
        let (r, w) = tokio::io::split(client_side);
        let mut t = HardenedRwTransport::new(r, w);

        let msg: TxJsonRpcMessage<RoleClient> = rmcp::model::JsonRpcMessage::notification(
            rmcp::model::ClientNotification::InitializedNotification(
                rmcp::model::InitializedNotification {
                    method: Default::default(),
                    extensions: Default::default(),
                },
            ),
        );

        let err = t
            .send(msg.clone())
            .await
            .expect_err("a wedged pipe must not succeed");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

        // Latched: the next send fails immediately rather than waiting again.
        let err = t
            .send(msg)
            .await
            .expect_err("a wedged transport must fail fast");
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    }
}
