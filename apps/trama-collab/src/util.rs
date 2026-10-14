//! Process-level helpers: secrets, blocking bridge, clock and health probes.

use super::*;

/// Reads a single secret (password) from `reader`, refusing input longer
/// than `max_bytes` so callers cannot be forced to buffer unbounded stdin.
/// One trailing newline (Unix or Windows) is stripped. The secret itself is
/// never included in returned error text.
pub fn read_secret(reader: impl Read, max_bytes: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "read secret from stdin".to_owned())?;
    if bytes.len() > max_bytes {
        return Err("secret exceeds maximum accepted length".into());
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| "secret must be valid UTF-8".to_owned())
}

pub(crate) async fn tokio_blocking<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| AuthError::Database)?
}

pub(crate) fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Direct (no proxy) serving is loopback only: the service speaks plain HTTP
/// and bearer tokens must not cross a network in clear text. Binding anything
/// else requires the explicit reverse-proxy opt-in in the `serve` command
/// (`--behind-proxy`), which skips this check and enables [`ProxyTrust`].
pub fn validate_bind(address: SocketAddr) -> Result<(), String> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {address}: the service speaks plain HTTP and only loopback \
             addresses are allowed by default. Behind a TLS reverse proxy on a private \
             network, opt in explicitly with `serve --behind-proxy` \
             (or TRAMA_COLLAB_BEHIND_PROXY=1)"
        ))
    }
}

/// One-shot `GET /healthz` over plain TCP, for container health checks where
/// no `curl` exists. `Ok` only for an HTTP 200 reply.
pub fn probe_health(address: SocketAddr, timeout: Duration) -> Result<(), String> {
    use std::io::Write;
    use std::net::TcpStream;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|e| format!("connect {address}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("send request: {e}"))?;
    let mut head = Vec::new();
    stream
        .take(64)
        .read_to_end(&mut head)
        .map_err(|e| format!("read reply: {e}"))?;
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("unhealthy reply: {status:?}"))
    }
}
