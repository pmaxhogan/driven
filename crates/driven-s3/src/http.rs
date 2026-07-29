//! The HTTP layer: client construction, one signed round trip, and the
//! streaming download reader.
//!
//! S3 traffic rides the SAME `reqwest` + `rustls` stack as every other network
//! call Driven makes, with the issue #34 corporate custom-CA and proxy config
//! applied fail-closed - a configured-but-broken CA or proxy errors at client
//! build time rather than silently downgrading to system trust or a direct
//! connection. The timeout profiles mirror `driven_drive::google`'s: a
//! total-capped client for metadata/control requests, and an idle-timeout-only
//! client for body transfers, which must not be killed mid-upload by a total
//! deadline.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use driven_tls::{CustomCaConfig, ProxyConfig};
use futures::Stream;
use tokio::io::{AsyncRead, ReadBuf};

/// Connect timeout shared by both profiles (DESIGN s5.8.4).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total-request timeout for metadata / control calls (HEAD, LIST, DELETE,
/// CreateMultipartUpload, CompleteMultipartUpload).
const META_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle (between-bytes) timeout for a body transfer. No overall cap - a large
/// part upload or download is allowed to take as long as the bytes keep
/// flowing - but a stalled transfer is caught here.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle connections kept per host.
const POOL_MAX_IDLE_PER_HOST: usize = 4;

/// How long an idle pooled connection survives.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Build the metadata/control client (total-capped).
pub fn build_meta_client(
    ca: &CustomCaConfig,
    proxy: &ProxyConfig,
) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(META_TOTAL_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT);
    let builder = driven_tls::apply_custom_ca(builder, ca)?;
    driven_tls::apply_proxy(builder, proxy)?
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build the S3 metadata client: {e}"))
}

/// Build the body-transfer client (idle timeout only, no total cap).
pub fn build_stream_client(
    ca: &CustomCaConfig,
    proxy: &ProxyConfig,
) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT);
    let builder = driven_tls::apply_custom_ca(builder, ca)?;
    driven_tls::apply_proxy(builder, proxy)?
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build the S3 streaming client: {e}"))
}

/// Parse a `Retry-After` header value (delta-seconds form) into milliseconds.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1_000))
}

/// An [`AsyncRead`] over a streaming response body.
///
/// A transport failure that happens WHILE the body is being read is wrapped
/// with `std::io::Error::other(reqwest_error)`, so the real cause stays
/// reachable through the io error's source chain and
/// [`driven_remote::classify_stream_read_error`] reports it as a
/// network/service failure rather than `local.io_error` - the restore sink must
/// never blame the user's disk for the network dropping.
pub struct BodyReader {
    stream: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    /// The remainder of the chunk most recently pulled off the stream.
    pending: bytes::Bytes,
}

impl BodyReader {
    /// Wrap a response's body stream.
    pub fn new(resp: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(resp.bytes_stream()),
            pending: bytes::Bytes::new(),
        }
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.pending.is_empty() {
                let n = self.pending.len().min(buf.remaining());
                let chunk = self.pending.split_to(n);
                buf.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            match self.stream.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                // End of body.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Ok(chunk))) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    self.pending = chunk;
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(std::io::Error::other(e)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn clients_build_with_no_custom_ca_or_proxy() {
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        build_meta_client(&ca, &proxy).expect("meta client");
        build_stream_client(&ca, &proxy).expect("stream client");
    }

    #[test]
    fn a_broken_custom_ca_fails_closed_rather_than_falling_back() {
        // Issue #34 invariant: a configured-but-unreadable CA must NOT silently
        // downgrade to system trust - that would send bucket traffic through a
        // trust store the user deliberately overrode.
        let ca = CustomCaConfig::from_path(Some(std::path::PathBuf::from(
            "/definitely/not/a/real/ca.pem",
        )));
        assert!(build_meta_client(&ca, &ProxyConfig::system()).is_err());
        assert!(build_stream_client(&ca, &ProxyConfig::system()).is_err());
    }

    #[test]
    fn retry_after_parses_delta_seconds_only() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(3_000));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);
        assert_eq!(parse_retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn body_reader_reassembles_chunks_across_small_buffers() {
        // Drive the reader over a synthetic multi-chunk stream: it must hand out
        // every byte in order even when the caller's buffer is smaller than a
        // chunk (the restore sink reads into a fixed buffer).
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = vec![
            Ok(bytes::Bytes::from_static(b"hello ")),
            Ok(bytes::Bytes::new()),
            Ok(bytes::Bytes::from_static(b"world")),
        ];
        let mut reader = BodyReader {
            stream: Box::pin(futures::stream::iter(chunks)),
            pending: bytes::Bytes::new(),
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 4];
        loop {
            let n = reader.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, b"hello world");
    }
}
