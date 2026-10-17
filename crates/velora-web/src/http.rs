use std::collections::BTreeMap;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// As much of a request head as will be read before giving up.
///
/// A local page sends a few hundred bytes. Anything approaching this is either
/// a mistake or someone probing, and neither should be allowed to grow a buffer
/// until the process dies.
const MAX_HEAD: usize = 16 * 1024;

/// As much of a body as will be read.
///
/// The one body this server accepts is a message from the operator, and a message
/// longer than this is not a message.
const MAX_BODY: usize = 256 * 1024;

/// What was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

impl Request {
    /// The method, uppercased as it was sent.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The path, without its query string.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The last segment of the path, if there is one.
    pub fn last_segment(&self) -> Option<&str> {
        self.path.rsplit('/').next().filter(|part| !part.is_empty())
    }

    /// The body as text, refusing anything that is not UTF-8.
    pub fn text(&self) -> Result<&str, HttpError> {
        std::str::from_utf8(&self.body).map_err(|_| HttpError::bad_request("body is not UTF-8"))
    }
}

/// Why a request could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The connection ended or failed before a request arrived.
    Closed,
    /// The request was not something this server will handle.
    BadRequest(String),
}

impl HttpError {
    fn bad_request(reason: impl Into<String>) -> Self {
        HttpError::BadRequest(reason.into())
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Closed => f.write_str("the connection closed before a request arrived"),
            HttpError::BadRequest(reason) => write!(f, "bad request: {reason}"),
        }
    }
}

/// Reads one request.
///
/// Hand-written rather than pulled in: this serves four routes to a local
/// browser, and a full HTTP stack would be more machinery than the whole rest of
/// the runtime. What it does implement, it implements strictly — a request it
/// does not fully understand is refused rather than guessed at.
pub async fn read_request<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Request, HttpError> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];

    // Read until the blank line that ends the head.
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(HttpError::bad_request("request head is too long"));
        }
        match stream.read(&mut byte).await {
            Ok(0) => return Err(HttpError::Closed),
            Ok(_) => head.push(byte[0]),
            Err(err) => return Err(HttpError::bad_request(err.to_string())),
        }
    }

    let head = String::from_utf8(head)
        .map_err(|_| HttpError::bad_request("request head is not UTF-8"))?;

    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpError::bad_request("no request line"))?;

    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpError::bad_request("no method"))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| HttpError::bad_request("no path"))?;

    // The query string is not used, and dropping it here means no route has to
    // think about it.
    let path = target.split('?').next().unwrap_or(target).to_owned();

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }

    let length: usize = headers
        .get("content-length")
        .map_or(Ok(0), |value| {
            value
                .parse()
                .map_err(|_| HttpError::bad_request("content-length is not a number"))
        })?;

    if length > MAX_BODY {
        return Err(HttpError::bad_request("body is too long"));
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        stream
            .read_exact(&mut body)
            .await
            .map_err(|err| HttpError::bad_request(err.to_string()))?;
    }

    Ok(Request { method, path, body })
}

/// Writes a complete response.
pub async fn write_response<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        reason(status),
        body.len(),
    );

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// Writes the head of a response that stays open, for a stream of events.
///
/// No `Content-Length`, because there is no end to it, and no `Connection:
/// close`, because the whole point is that it does not.
pub async fn write_stream_head<W: AsyncWrite + Unpin>(
    stream: &mut W,
    content_type: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {content_type}\r\n\
         Cache-Control: no-store\r\n\
         Connection: keep-alive\r\n\
         \r\n"
    );

    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

/// The reason phrase for a status, for the handful this server sends.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn parse(raw: &str) -> Result<Request, HttpError> {
        let mut bytes = raw.as_bytes().to_vec();
        let mut cursor = std::io::Cursor::new(&mut bytes);
        read_request(&mut cursor).await
    }

    #[tokio::test]
    async fn a_get_is_read() {
        let request = parse("GET /api/snapshot HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        assert_eq!(request.method(), "GET");
        assert_eq!(request.path(), "/api/snapshot");
        assert_eq!(request.text().unwrap(), "");
    }

    #[tokio::test]
    async fn a_query_string_is_not_part_of_the_path() {
        let request = parse("GET /?since=12 HTTP/1.1\r\n\r\n").await.unwrap();
        assert_eq!(request.path(), "/");
    }

    #[tokio::test]
    async fn a_body_is_read_by_its_declared_length() {
        let request = parse(
            "POST /api/say HTTP/1.1\r\nContent-Length: 17\r\n\r\n{\"text\":\"hello!\"}",
        )
        .await
        .unwrap();

        assert_eq!(request.method(), "POST");
        assert_eq!(request.text().unwrap(), "{\"text\":\"hello!\"}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_utf8_is_refused() {
        let mut cursor = std::io::Cursor::new(
            b"POST /api/say HTTP/1.1\r\nContent-Length: 2\r\n\r\n\xff\xfe".to_vec(),
        );
        let request = read_request(&mut cursor).await.unwrap();

        assert!(request.text().is_err(), "a message has to be text");
    }

    #[tokio::test]
    async fn the_last_segment_is_the_thing_asked_for() {
        let request = parse("GET /api/record/431 HTTP/1.1\r\n\r\n").await.unwrap();
        assert_eq!(request.last_segment(), Some("431"));
    }

    #[tokio::test]
    async fn a_connection_that_ends_before_a_request_is_not_an_error() {
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(
            read_request(&mut empty).await.unwrap_err(),
            HttpError::Closed
        );
    }

    #[tokio::test]
    async fn a_head_that_never_ends_is_refused_rather_than_buffered_forever() {
        // No blank line, so the head never terminates.
        let noise = "X".repeat(MAX_HEAD + 1);
        let error = parse(&noise).await.unwrap_err();

        assert!(
            matches!(&error, HttpError::BadRequest(reason) if reason.contains("too long")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_body_longer_than_a_message_is_refused_before_it_is_read() {
        let raw = format!(
            "POST /api/say HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let error = parse(&raw).await.unwrap_err();

        assert!(
            matches!(&error, HttpError::BadRequest(reason) if reason.contains("body is too long")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_response_is_well_formed() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            write_response(&mut server, 200, "text/plain", b"hi").await.unwrap();
        });

        let mut got = String::new();
        client.read_to_string(&mut got).await.unwrap();

        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "{got}");
        assert!(got.contains("Content-Length: 2\r\n"), "{got}");
        assert!(got.ends_with("\r\n\r\nhi"), "{got}");
    }

    #[tokio::test]
    async fn a_streamed_response_declares_no_length() {
        // A response with a length that never ends is a response a browser waits
        // on forever.
        let (mut client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            write_stream_head(&mut server, "text/event-stream").await.unwrap();
            server.write_all(b"\n").await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut got = String::new();
        client.read_to_string(&mut got).await.unwrap();

        assert!(got.contains("text/event-stream"), "{got}");
        assert!(!got.to_lowercase().contains("content-length"), "{got}");
    }
}
