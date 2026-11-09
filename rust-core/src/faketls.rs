use crate::config::{log_debug, log_warn, STATS};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

pub const TLS_RECORD_HANDSHAKE: u8 = 0x16;
pub const TLS_RECORD_CCS: u8 = 0x14;
pub const TLS_RECORD_APPDATA: u8 = 0x17;

const CLIENT_RANDOM_OFFSET: usize = 11;
const CLIENT_RANDOM_LEN: usize = 32;
const SESSION_ID_OFFSET: usize = 44;
const SESSION_ID_LEN: usize = 32;

const TIMESTAMP_TOLERANCE: i64 = 120;
const TLS_APPDATA_MAX: usize = 16384;

const READ_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const READ_RECORD_TIMEOUT: Duration = Duration::from_secs(10);
const MASKING_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const CCS_FRAME: [u8; 6] = [0x14, 0x03, 0x03, 0x00, 0x01, 0x01];

const SH_RANDOM_OFF: usize = 11;
const SH_SESSID_OFF: usize = 44;
const SH_PUBKEY_OFF: usize = 89;

type HmacSha256 = Hmac<Sha256>;

pub struct FakeTlsStream {
    pub read: Box<dyn AsyncRead + Send + Unpin>,
    pub write: Box<dyn AsyncWrite + Send + Unpin>,
}

fn server_hello_template() -> Vec<u8> {
    let mut v = Vec::with_capacity(128);
    v.extend_from_slice(&[0x16, 0x03, 0x03, 0x00, 0x7a]);
    v.extend_from_slice(&[0x02, 0x00, 0x00, 0x76]);
    v.extend_from_slice(&[0x03, 0x03]);
    v.extend_from_slice(&[0u8; 32]);
    v.push(0x20);
    v.extend_from_slice(&[0u8; 32]);
    v.extend_from_slice(&[0x13, 0x01, 0x00]);
    v.extend_from_slice(&[0x00, 0x2e, 0x00]);
    v.extend_from_slice(&[0x00, 0x33, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20]);
    v.extend_from_slice(&[0u8; 32]);
    v.extend_from_slice(&[0x00, 0x2b, 0x00, 0x02, 0x03, 0x04]);
    v
}

fn verify_client_hello(data: &[u8], secret: &[u8]) -> Option<([u8; 32], [u8; 32], i64)> {
    if data.len() < 43 {
        return None;
    }
    if data[0] != TLS_RECORD_HANDSHAKE {
        return None;
    }
    if data[5] != 0x01 {
        return None;
    }

    let mut client_random = [0u8; 32];
    client_random
        .copy_from_slice(&data[CLIENT_RANDOM_OFFSET..CLIENT_RANDOM_OFFSET + CLIENT_RANDOM_LEN]);

    let mut zeroed = data.to_vec();
    for b in &mut zeroed[CLIENT_RANDOM_OFFSET..CLIENT_RANDOM_OFFSET + CLIENT_RANDOM_LEN] {
        *b = 0;
    }

    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(&zeroed);
    let expected = mac.finalize().into_bytes();

    if expected[..28] != client_random[..28] {
        return None;
    }

    let mut ts_bytes = [0u8; 4];
    for i in 0..4 {
        ts_bytes[i] = client_random[28 + i] ^ expected[28 + i];
    }
    let timestamp = u32::from_le_bytes(ts_bytes) as i64;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if (now - timestamp).abs() > TIMESTAMP_TOLERANCE {
        return None;
    }

    let mut session_id = [0u8; 32];
    if data.len() >= SESSION_ID_OFFSET + SESSION_ID_LEN && data[43] == 0x20 {
        session_id
            .copy_from_slice(&data[SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN]);
    }

    Some((client_random, session_id, timestamp))
}

fn build_server_hello(secret: &[u8], client_random: &[u8; 32], session_id: &[u8; 32]) -> Vec<u8> {
    let mut sh = server_hello_template();
    debug_assert_eq!(sh.len(), 128);
    sh[SH_SESSID_OFF..SH_SESSID_OFF + 32].copy_from_slice(session_id);

    let mut pubkey = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut pubkey);
    sh[SH_PUBKEY_OFF..SH_PUBKEY_OFF + 32].copy_from_slice(&pubkey);

    let mut response = Vec::with_capacity(128 + 6 + 5 + 2100);
    response.extend_from_slice(&sh);
    response.extend_from_slice(&CCS_FRAME);

    let encrypted_size = 1900 + (rand::thread_rng().next_u32() % 201) as usize;
    let mut encrypted_data = vec![0u8; encrypted_size];
    rand::thread_rng().fill_bytes(&mut encrypted_data);
    response.push(TLS_RECORD_APPDATA);
    response.push(0x03);
    response.push(0x03);
    response.push((encrypted_size >> 8) as u8);
    response.push(encrypted_size as u8);
    response.extend_from_slice(&encrypted_data);

    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(client_random);
    mac.update(&response);
    let server_random = mac.finalize().into_bytes();

    let mut final_bytes = response;
    final_bytes[SH_RANDOM_OFF..SH_RANDOM_OFF + 32].copy_from_slice(&server_random);
    final_bytes
}

fn wrap_tls_record(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 5 * (data.len() / TLS_APPDATA_MAX + 1));
    let mut offset = 0;
    while offset < data.len() {
        let end = std::cmp::min(offset + TLS_APPDATA_MAX, data.len());
        let chunk = &data[offset..end];
        out.push(TLS_RECORD_APPDATA);
        out.push(0x03);
        out.push(0x03);
        out.push((chunk.len() >> 8) as u8);
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
        offset = end;
    }
    out
}

struct TlsRecordReader<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
    header: [u8; 5],
    header_pos: usize,
    payload_remaining: usize,
    skip_remaining: usize,
    eof: bool,
}

impl<R> TlsRecordReader<R> {
    fn new(inner: R) -> Self {
        TlsRecordReader {
            inner,
            buf: Vec::new(),
            pos: 0,
            header: [0; 5],
            header_pos: 0,
            payload_remaining: 0,
            skip_remaining: 0,
            eof: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for TlsRecordReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            let available = this.buf.len().saturating_sub(this.pos);
            if available > 0 {
                let n = std::cmp::min(available, out.remaining());
                out.put_slice(&this.buf[this.pos..this.pos + n]);
                this.pos += n;
                if this.pos == this.buf.len() {
                    this.buf.clear();
                    this.pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }

            if this.skip_remaining > 0 {
                let want = std::cmp::min(this.skip_remaining, 16 * 1024);
                let mut scratch = [0u8; 16 * 1024];
                let mut read_buf = ReadBuf::new(&mut scratch[..want]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "truncated FakeTLS control record",
                            )));
                        }
                        this.skip_remaining -= n;
                        continue;
                    }
                }
            }

            if this.payload_remaining > 0 {
                let want = std::cmp::min(this.payload_remaining, 16 * 1024);
                let mut scratch = [0u8; 16 * 1024];
                let mut read_buf = ReadBuf::new(&mut scratch[..want]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "truncated FakeTLS application record",
                            )));
                        }
                        this.buf.extend_from_slice(read_buf.filled());
                        this.payload_remaining -= n;
                        continue;
                    }
                }
            }

            let mut header_buf = ReadBuf::new(&mut this.header[this.header_pos..]);
            match Pin::new(&mut this.inner).poll_read(cx, &mut header_buf) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    let n = header_buf.filled().len();
                    drop(header_buf);
                    if n == 0 {
                        if this.header_pos == 0 {
                            this.eof = true;
                            return Poll::Ready(Ok(()));
                        }
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "truncated FakeTLS record header",
                        )));
                    }
                    this.header_pos += n;
                    if this.header_pos < this.header.len() {
                        continue;
                    }

                    let record_type = this.header[0];
                    let record_len = u16::from_be_bytes([this.header[3], this.header[4]]) as usize;
                    this.header_pos = 0;
                    if record_type == TLS_RECORD_CCS {
                        this.skip_remaining = record_len;
                    } else if record_type == TLS_RECORD_APPDATA {
                        this.payload_remaining = record_len;
                    } else {
                        this.eof = true;
                    }
                }
            }
        }
    }
}

struct TlsRecordWriter<W> {
    inner: W,
    pending: Vec<u8>,
    pending_pos: usize,
    pending_plain_len: usize,
}

impl<W> TlsRecordWriter<W> {
    fn new(inner: W) -> Self {
        TlsRecordWriter {
            inner,
            pending: Vec::new(),
            pending_pos: 0,
            pending_plain_len: 0,
        }
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>
    where
        W: AsyncWrite + Unpin,
    {
        while self.pending_pos < self.pending.len() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pending_pos..]) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "FakeTLS record write returned zero",
                    )))
                }
                Poll::Ready(Ok(n)) => self.pending_pos += n,
            }
        }
        self.pending.clear();
        self.pending_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for TlsRecordWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if !this.pending.is_empty() {
            let accepted = this.pending_plain_len;
            match this.poll_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    this.pending_plain_len = 0;
                    return Poll::Ready(Ok(accepted));
                }
            }
        }

        // One plaintext write maps to one TLS application-data record. Retain
        // the complete wrapped frame across Pending/partial socket writes.
        let accepted = std::cmp::min(buf.len(), TLS_APPDATA_MAX);
        this.pending = wrap_tls_record(&buf[..accepted]);
        this.pending_pos = 0;
        this.pending_plain_len = accepted;
        match this.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                this.pending_plain_len = 0;
                Poll::Ready(Ok(accepted))
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(cx),
        }
    }
}

pub async fn server_handshake(
    mut conn: TcpStream,
    first: u8,
    secret: &[u8],
    masking: &str,
    label: &str,
) -> Option<FakeTlsStream> {
    let mut hdr_rest = [0u8; 4];
    match tokio::time::timeout(READ_HANDSHAKE_TIMEOUT, conn.read_exact(&mut hdr_rest)).await {
        Ok(Ok(_)) => {}
        _ => {
            log_debug(&format!("[{}] incomplete TLS record header", label));
            return None;
        }
    }

    let mut tls_header = [0u8; 5];
    tls_header[0] = first;
    tls_header[1..5].copy_from_slice(&hdr_rest);
    let record_len = u16::from_be_bytes([tls_header[3], tls_header[4]]) as usize;

    let mut body = vec![0u8; record_len];
    match tokio::time::timeout(READ_RECORD_TIMEOUT, conn.read_exact(&mut body)).await {
        Ok(Ok(_)) => {}
        _ => {
            log_debug(&format!("[{}] incomplete TLS record body", label));
            return None;
        }
    }

    let mut client_hello = Vec::with_capacity(5 + record_len);
    client_hello.extend_from_slice(&tls_header);
    client_hello.extend_from_slice(&body);

    match verify_client_hello(&client_hello, secret) {
        Some((client_random, session_id, ts)) => {
            log_debug(&format!("[{}] Fake TLS handshake ok (ts={})", label, ts));
            let server_hello = build_server_hello(secret, &client_random, &session_id);
            if conn.write_all(&server_hello).await.is_err() {
                return None;
            }
            let _ = conn.flush().await;

            let (r, w) = tokio::io::split(conn);
            Some(FakeTlsStream {
                read: Box::new(TlsRecordReader::new(r)),
                write: Box::new(TlsRecordWriter::new(w)),
            })
        }
        None => {
            log_debug(&format!(
                "[{}] Fake TLS verify failed (size={} rec={}) -> masking",
                label,
                client_hello.len(),
                record_len
            ));
            proxy_to_masking_domain(conn, client_hello, masking, label).await;
            None
        }
    }
}

async fn proxy_to_masking_domain(conn: TcpStream, initial: Vec<u8>, domain: &str, label: &str) {
    let upstream = match tokio::time::timeout(
        MASKING_CONNECT_TIMEOUT,
        TcpStream::connect((domain, 443u16)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            log_warn(&format!(
                "[{}] masking: cannot connect to {}:443",
                label, domain
            ));
            return;
        }
    };

    log_debug(&format!("[{}] masking -> {}:443", label, domain));
    STATS
        .connections_masked
        .fetch_add(1, Ordering::Relaxed);

    let mut upstream = upstream;
    if upstream.write_all(&initial).await.is_err() {
        return;
    }

    // Keep both directions in this future. Spawning detached copy tasks here
    // meant dropping the FakeTLS handshake on service cancellation left both
    // sockets alive outside the client JoinSet.
    let mut client = conn;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}


#[cfg(test)]
mod io_tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn reader_preserves_fragmented_headers_and_skips_ccs() {
        let (mut peer, stream) = duplex(2);
        let mut reader = TlsRecordReader::new(stream);
        let payload = b"fragmented FakeTLS payload";
        let mut wire = vec![TLS_RECORD_CCS, 0x03, 0x03, 0x00, 0x01, 0x01];
        wire.extend_from_slice(&[TLS_RECORD_APPDATA, 0x03, 0x03, 0x00, payload.len() as u8]);
        wire.extend_from_slice(payload);

        let sender = tokio::spawn(async move {
            for byte in wire {
                peer.write_all(&[byte]).await.unwrap();
            }
        });

        let mut actual = vec![0u8; payload.len()];
        reader.read_exact(&mut actual).await.unwrap();
        sender.await.unwrap();
        assert_eq!(actual, payload);
    }

    #[tokio::test]
    async fn writer_retains_tls_frame_across_partial_socket_writes() {
        let (mut peer, stream) = duplex(3);
        let mut writer = TlsRecordWriter::new(stream);
        let payload: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();

        let sender = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
            writer.flush().await.unwrap();
        });

        let frame_count = (expected.len() + TLS_APPDATA_MAX - 1) / TLS_APPDATA_MAX;
        let mut wire = vec![0u8; expected.len() + frame_count * 5];
        peer.read_exact(&mut wire).await.unwrap();
        sender.await.unwrap();

        let mut offset = 0;
        let mut recovered = Vec::with_capacity(expected.len());
        while offset < wire.len() {
            assert_eq!(wire[offset], TLS_RECORD_APPDATA);
            assert_eq!(wire[offset + 1], 0x03);
            assert_eq!(wire[offset + 2], 0x03);
            let len = u16::from_be_bytes([wire[offset + 3], wire[offset + 4]]) as usize;
            offset += 5;
            recovered.extend_from_slice(&wire[offset..offset + len]);
            offset += len;
        }
        assert_eq!(recovered, expected);
    }
}
