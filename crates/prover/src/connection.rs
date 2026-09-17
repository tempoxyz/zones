use std::io::{self, BufReader, Read, Write};

use futures::{SinkExt as _, StreamExt as _};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_util::{
    bytes::Bytes,
    codec::{Framed, LengthDelimitedCodec, LengthDelimitedCodecError},
};

use crate::{ErrorCode, MAX_FRAME_BYTES, PROTOCOL_VERSION, VerifyResponse};

const CHANNEL_CAPACITY: usize = 2;

/// A typed connection using the prover's chunked, length-delimited JSON protocol.
pub struct ProverConnection<T> {
    inner: Framed<T, LengthDelimitedCodec>,
    maximum: usize,
    last_received_bytes: Option<usize>,
}

impl<IO> ProverConnection<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    /// Wraps an I/O stream with the prover protocol and its maximum message size.
    pub fn new(io: IO, maximum: usize) -> Self {
        Self::with_limits(io, MAX_FRAME_BYTES, maximum)
    }

    /// Wraps an I/O stream with the prover protocol and its maximum message and chunk size.
    fn with_limits(io: IO, frame_maximum: usize, maximum: usize) -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .max_frame_length(frame_maximum)
                .new_framed(io),
            maximum,
            last_received_bytes: None,
        }
    }

    /// Returns the encoded size of the most recently received message.
    pub fn last_received_bytes(&self) -> Option<usize> {
        self.last_received_bytes
    }

    /// Serializes and sends an owned typed message, returning its encoded JSON size.
    pub async fn send<T>(&mut self, message: T) -> Result<usize, ProverConnectionError>
    where
        T: Serialize + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let frame_maximum = self.inner.codec().max_frame_length();
        let maximum = self.maximum;
        let worker = tokio::task::spawn_blocking(move || {
            let mut writer = ChunkWriter::new(tx, frame_maximum, maximum);
            match serde_json::to_writer(&mut writer, &message) {
                Ok(()) => writer.finish().map_err(ProverConnectionError::Io),
                Err(error) if error.io_error_kind() == Some(io::ErrorKind::InvalidData) => {
                    Err(ProverConnectionError::MessageTooLarge { maximum })
                }
                Err(error) => Err(ProverConnectionError::Json(error)),
            }
        });

        while let Some(frame) = rx.recv().await {
            if let Err(error) = self.inner.send(frame).await {
                drop(rx);
                let _ = worker.await;
                return Err(classify_io_error(
                    error,
                    self.inner.codec().max_frame_length(),
                ));
            }
        }
        join_worker(worker.await)?
    }

    /// Receives and deserializes one chunked logical message.
    pub async fn receive<T>(&mut self) -> Result<Option<T>, ProverConnectionError>
    where
        T: DeserializeOwned + Send + 'static,
    {
        self.last_received_bytes = None;
        let Some(first) = self.inner.next().await else {
            return Ok(None);
        };
        let first = first
            .map_err(|error| classify_io_error(error, self.inner.codec().max_frame_length()))?;

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let mut worker = tokio::task::spawn_blocking(move || {
            serde_json::from_reader(BufReader::new(ChunkReader {
                rx,
                current: Bytes::new(),
            }))
            .map_err(ProverConnectionError::Json)
        });
        let (mut total, mut frame) = (0usize, first);
        while !frame.is_empty() {
            total = total.saturating_add(frame.len());
            if total > self.maximum {
                drop(tx);
                let _ = worker.await;
                return Err(ProverConnectionError::MessageTooLarge {
                    maximum: self.maximum,
                });
            }
            if tx.send(frame.freeze()).await.is_err() {
                return Err(worker_error_before_terminator(worker.await));
            }
            frame = tokio::select! {
                biased; // Prefer decoder failure over another ready frame from the peer.
                result = &mut worker => return Err(worker_error_before_terminator(result)),
                frame = self.inner.next() => match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) => {
                        drop(tx);
                        let _ = worker.await;
                        return Err(classify_io_error(
                            error,
                            self.inner.codec().max_frame_length(),
                        ));
                    }
                    None => {
                        drop(tx);
                        let _ = worker.await;
                        return Err(ProverConnectionError::TruncatedFrame);
                    }
                },
            };
        }
        drop(tx);
        self.last_received_bytes = Some(total);
        let value = join_worker(worker.await)??;
        Ok(Some(value))
    }
}

struct ChunkWriter {
    tx: mpsc::Sender<Bytes>,
    buffer: Vec<u8>,
    max_frame_bytes: usize,
    max_message_bytes: usize,
    total: usize,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<Bytes>, max_frame_bytes: usize, max_message_bytes: usize) -> Self {
        Self {
            tx,
            buffer: Vec::with_capacity(max_frame_bytes),
            max_frame_bytes,
            max_message_bytes,
            total: 0,
        }
    }

    fn push(&self, frame: Bytes) -> io::Result<()> {
        self.tx
            .blocking_send(frame)
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    fn finish(mut self) -> io::Result<usize> {
        if !self.buffer.is_empty() {
            let frame = Bytes::from(std::mem::take(&mut self.buffer));
            self.push(frame)?;
        }
        self.push(Bytes::new())?;
        Ok(self.total)
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, mut input: &[u8]) -> io::Result<usize> {
        let written = input.len();
        if self.total.saturating_add(written) > self.max_message_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "message exceeds the maximum of {} bytes",
                    self.max_message_bytes
                ),
            ));
        }
        while !input.is_empty() {
            let space = self.max_frame_bytes - self.buffer.len();
            let take = space.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.buffer.len() == self.max_frame_bytes {
                let frame = Bytes::from(std::mem::take(&mut self.buffer));
                self.push(frame)?;
                self.buffer = Vec::with_capacity(self.max_frame_bytes);
            }
        }
        self.total += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ChunkReader {
    rx: mpsc::Receiver<Bytes>,
    current: Bytes,
}

impl Read for ChunkReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.current.is_empty() {
            let Some(chunk) = self.rx.blocking_recv() else {
                return Ok(0);
            };
            self.current = chunk;
        }
        let count = output.len().min(self.current.len());
        output[..count].copy_from_slice(&self.current[..count]);
        self.current = self.current.slice(count..);
        Ok(count)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProverConnectionError {
    #[error("message or frame exceeds the maximum of {maximum} bytes")]
    MessageTooLarge { maximum: usize },
    #[error("message frame is truncated")]
    TruncatedFrame,
    #[error("message JSON is invalid: {0}")]
    Json(#[source] serde_json::Error),
    #[error("connection I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("JSON worker panicked")]
    WorkerPanic,
}

fn join_worker<T>(result: Result<T, tokio::task::JoinError>) -> Result<T, ProverConnectionError> {
    result.map_err(|_| ProverConnectionError::WorkerPanic)
}

fn worker_error_before_terminator<T>(
    result: Result<Result<T, ProverConnectionError>, tokio::task::JoinError>,
) -> ProverConnectionError {
    match join_worker(result) {
        Err(error) | Ok(Err(error)) => error,
        Ok(Ok(_)) => ProverConnectionError::TruncatedFrame,
    }
}

fn classify_io_error(error: io::Error, maximum: usize) -> ProverConnectionError {
    if error
        .get_ref()
        .is_some_and(|source| source.is::<LengthDelimitedCodecError>())
    {
        ProverConnectionError::MessageTooLarge { maximum }
    } else if error.kind() == io::ErrorKind::Other
        && error.to_string() == "bytes remaining on stream"
    {
        ProverConnectionError::TruncatedFrame
    } else {
        ProverConnectionError::Io(error)
    }
}

/// Converts a request receive error into a prover protocol response.
pub fn request_error_response(error: &ProverConnectionError) -> VerifyResponse {
    let (code, message) = match error {
        ProverConnectionError::MessageTooLarge { maximum } => (
            ErrorCode::RequestTooLarge,
            format!("request or frame exceeds the maximum of {maximum} bytes"),
        ),
        ProverConnectionError::TruncatedFrame => (
            ErrorCode::TruncatedFrame,
            "request frame is truncated".into(),
        ),
        ProverConnectionError::Json(error) => (
            ErrorCode::MalformedRequest,
            format!("invalid request JSON: {error}"),
        ),
        ProverConnectionError::Io(error) => (
            ErrorCode::InternalError,
            format!("frame I/O failed: {error}"),
        ),
        ProverConnectionError::WorkerPanic => {
            (ErrorCode::InternalError, "JSON worker panicked".into())
        }
    };
    VerifyResponse::Error {
        version: PROTOCOL_VERSION,
        request_id: None,
        code,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn chunked_round_trip_and_metrics() {
        let (client, server) = tokio::io::duplex(7);
        let mut client = ProverConnection::with_limits(client, 5, usize::MAX);
        let mut server = ProverConnection::with_limits(server, 5, usize::MAX);
        let value = "a long hexadecimal-ish 0123456789abcdef".to_string();
        let expected = serde_json::to_vec(&value).unwrap().len();
        let send = tokio::spawn(async move { client.send(value).await.map(|n| (n, client)) });
        let received: String = server.receive().await.unwrap().unwrap();
        let (sent, _) = send.await.unwrap().unwrap();
        assert_eq!(received, "a long hexadecimal-ish 0123456789abcdef");
        assert_eq!(sent, expected);
        assert_eq!(server.last_received_bytes(), Some(expected));
    }

    #[tokio::test]
    async fn exact_boundary_has_terminator() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut connection = ProverConnection::with_limits(client, 3, usize::MAX);
        connection.send(123_u32).await.unwrap();
        let mut header = [0; 4];
        server.read_exact(&mut header).await.unwrap();
        assert_eq!(u32::from_be_bytes(header), 3);
        let mut payload = [0; 3];
        server.read_exact(&mut payload).await.unwrap();
        server.read_exact(&mut header).await.unwrap();
        assert_eq!(u32::from_be_bytes(header), 0);
    }

    #[tokio::test]
    async fn rejects_truncation_oversize_and_missing_terminator() {
        for raw in [&[0, 0, 0, 2, 1][..], &[0, 0, 0, 1, b'1'][..]] {
            let (mut writer, reader) = tokio::io::duplex(32);
            writer.write_all(raw).await.unwrap();
            writer.shutdown().await.unwrap();
            let error = ProverConnection::with_limits(reader, 4, usize::MAX)
                .receive::<u32>()
                .await
                .unwrap_err();
            assert!(matches!(error, ProverConnectionError::TruncatedFrame));
        }
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(&10_u32.to_be_bytes()).await.unwrap();
        let error = ProverConnection::with_limits(reader, 4, usize::MAX)
            .receive::<u32>()
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProverConnectionError::MessageTooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn malformed_json_fails_without_a_terminator() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer
            .write_all(&[0, 0, 0, 4, b'n', b'o', b't', b'!'])
            .await
            .unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ProverConnection::with_limits(reader, 4, usize::MAX).receive::<u32>(),
        )
        .await
        .expect("malformed JSON must not wait for a terminator")
        .unwrap_err();
        assert!(matches!(
            request_error_response(&error),
            VerifyResponse::Error {
                code: ErrorCode::MalformedRequest,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn logical_oversize_fails_without_a_terminator() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer
            .write_all(&[0, 0, 0, 3, b'1', b'2', b'3', 0, 0, 0, 3, b'4', b'5', b'6'])
            .await
            .unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ProverConnection::with_limits(reader, 3, 5).receive::<u32>(),
        )
        .await
        .expect("oversized JSON must not wait for a terminator")
        .unwrap_err();
        assert!(matches!(
            error,
            ProverConnectionError::MessageTooLarge { maximum: 5 }
        ));
    }

    #[tokio::test]
    async fn sending_logical_oversize_reports_the_size_limit() {
        let (client, _server) = tokio::io::duplex(32);
        let error = ProverConnection::with_limits(client, 16, 3)
            .send("abcd")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProverConnectionError::MessageTooLarge { maximum: 3 }
        ));
    }
}
