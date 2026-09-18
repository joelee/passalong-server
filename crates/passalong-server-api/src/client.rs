//! A small HTTP/1.1 client: one request per connection, the whole answer in
//! memory. For the tests, and for `passalong-server check --health`. It is
//! not the passalong client, which is another repository.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Where requests go, and how.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    /// `None` for plain HTTP.
    tls: Option<Arc<rustls::ClientConfig>>,
}

/// An answer, whole.
#[derive(Debug, Clone)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(header, _)| *header == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON; `null` when it is not.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A request being put together.
#[derive(Debug)]
pub struct RequestBuilder {
    client: Client,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Client {
    /// Plain HTTP to `addr`.
    pub fn plain(addr: SocketAddr) -> Self {
        Self { addr, tls: None }
    }

    /// HTTPS to `addr`, to the server whose public key has `pin` and to no
    /// other. There is no way to connect without checking.
    ///
    /// # Errors
    ///
    /// When `pin` is not a pin.
    pub fn pinned(addr: SocketAddr, pin: &str) -> Result<Self, crate::tls::TlsError> {
        Ok(Self {
            addr,
            tls: Some(crate::tls::client_config(pin)?),
        })
    }

    /// A request for `path`, which includes any query.
    pub fn request(&self, method: &str, path: &str) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method: method.to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

impl RequestBuilder {
    /// Adds a header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    /// A JSON body.
    pub fn json(mut self, body: &serde_json::Value) -> Self {
        self.body = body.to_string().into_bytes();
        self.header("Content-Type", "application/json")
    }

    /// A body of bytes.
    pub fn body(mut self, bytes: Vec<u8>) -> Self {
        self.body = bytes;
        self.header("Content-Type", "application/octet-stream")
    }

    /// A body, with whatever `Content-Type` the caller set.
    pub fn body_raw(mut self, bytes: Vec<u8>) -> Self {
        self.body = bytes;
        self
    }

    /// Sends it and reads the whole answer.
    ///
    /// # Errors
    ///
    /// When the connection fails or the answer is not HTTP.
    pub async fn send(self) -> std::io::Result<Response> {
        let stream = TcpStream::connect(self.client.addr).await?;
        match &self.client.tls {
            None => exchange(stream, &self).await,
            Some(config) => {
                // The name is not what is checked; the pin is.
                let name = rustls::pki_types::ServerName::IpAddress(self.client.addr.ip().into());
                let stream = tokio_rustls::TlsConnector::from(config.clone())
                    .connect(name, stream)
                    .await?;
                exchange(stream, &self).await
            }
        }
    }
}

fn invalid(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned())
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &RequestBuilder,
) -> std::io::Result<Response> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        request.method, request.path, request.client.addr
    );
    for (name, value) in &request.headers {
        // A header value with a line break would be another header.
        if value.contains(['\r', '\n']) || name.contains(['\r', '\n', ':']) {
            return Err(invalid("a header that would break the request"));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !request.body.is_empty() || matches!(request.method.as_str(), "POST" | "PUT") {
        head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    // The server may answer, and close, before it has read the body: a
    // refusal needs none of it. That is not this request's failure.
    let sent = stream.write_all(&request.body).await;
    let _ = stream.flush().await;
    let mut raw = Vec::new();
    let read = stream.read_to_end(&mut raw).await;
    if raw.is_empty() {
        sent?;
        read?;
    }
    parse(&raw)
}

fn parse(raw: &[u8]) -> std::io::Result<Response> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("no end of headers"))?;
    let head =
        std::str::from_utf8(&raw[..split]).map_err(|_| invalid("headers that are not text"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid("no status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let rest = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        unchunk(rest)?
    } else {
        rest.to_vec()
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn unchunk(mut rest: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| invalid("a chunk without a size"))?;
        let size_text = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| invalid("a chunk size that is not text"))?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| invalid("a chunk size that is not hex"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size + 2 {
            return Err(invalid("a chunk cut short"));
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_parsed_with_a_length_or_in_chunks() {
        let plain = parse(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Request-Id: abc\r\n\r\n{\"a\":1}").unwrap();
        assert_eq!(
            (plain.status, plain.header("content-type")),
            (200, Some("application/json"))
        );
        assert_eq!(plain.json()["a"], 1);
        assert_eq!(plain.header("X-Request-Id"), Some("abc"));
        let chunked = parse(b"HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!(
            (chunked.status, chunked.body.as_slice()),
            (206, &b"abcde"[..])
        );
        for bad in [
            &b"nonsense"[..],
            b"HTTP/1.1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        assert!(
            parse(b"HTTP/1.1 204 No Content\r\n\r\n")
                .unwrap()
                .body
                .is_empty()
        );
    }
}
