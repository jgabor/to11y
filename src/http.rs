//! HTTPS requests over HTTP/1.1, one connection per request.
//!
//! This covers what to11y's four providers need: GET and POST with a small
//! body, `Content-Length` and chunked responses, and no redirects. A
//! redirect comes back as a response like any other, so a token is never
//! sent to a second host.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const USER_AGENT: &str = concat!("to11y/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct Client {
    config: Arc<ClientConfig>,
    timeout: Duration,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// Names are lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

impl Client {
    /// A client that trusts the web's roots, as compiled into to11y.
    pub fn new(timeout: Duration) -> Self {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots, timeout)
    }

    pub fn with_roots(roots: RootCertStore, timeout: Duration) -> Self {
        let provider = Arc::new(rustls_graviola::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("the provider supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Client {
            config: Arc::new(config),
            timeout,
        }
    }

    /// Sends one request. `headers` may set `User-Agent`; to11y's own is
    /// sent otherwise. A JSON `body` makes this a POST.
    pub fn send(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> io::Result<Response> {
        let url = Url::parse(url)?;
        let request = request(&url, headers, body)?;
        let name = ServerName::try_from(url.host.to_owned()).map_err(|_| invalid(url.host))?;
        let connection =
            ClientConnection::new(self.config.clone(), name).map_err(io::Error::other)?;
        let mut stream = StreamOwned::new(connection, connect(url.host, url.port, self.timeout)?);
        stream.write_all(&request)?;
        stream.flush()?;
        read_response(&mut BufReader::new(stream))
    }
}

/// The bytes of a request: a POST when there is a body.
fn request(url: &Url, headers: &[(&str, &str)], body: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let method = if body.is_some() { "POST" } else { "GET" };
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        url.target, url.authority
    );
    let mut agent = false;
    let safe = |text: &str| !text.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0);
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a bad header name",
            ));
        }
        if !safe(value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a bad header value",
            ));
        }
        agent |= name.eq_ignore_ascii_case("user-agent");
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !agent {
        head.push_str(&format!("User-Agent: {USER_AGENT}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body.unwrap_or_default());
    Ok(bytes)
}

/// Reads a whole response, skipping any interim 1xx responses.
pub(crate) fn read_response(stream: &mut impl BufRead) -> io::Result<Response> {
    loop {
        let head = read_head(stream)?;
        let (status, headers) = parse_head(&head)?;
        if (100..200).contains(&status) {
            continue;
        }
        let response = Response {
            status,
            headers,
            body: Vec::new(),
        };
        let body = if status == 204 || status == 304 {
            Vec::new()
        } else if let Some(encoding) = response.header("transfer-encoding") {
            if !encoding.eq_ignore_ascii_case("chunked") {
                return Err(bad_response("a Transfer-Encoding to11y doesn't read"));
            }
            read_chunked(stream)?
        } else if let Some(length) = response.header("content-length") {
            let length: usize = length
                .parse()
                .map_err(|_| bad_response("a malformed Content-Length"))?;
            if length > MAX_BODY_BYTES {
                return Err(bad_response("a body that is too large"));
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body)?;
            body
        } else {
            let mut body = Vec::new();
            stream
                .take(MAX_BODY_BYTES as u64 + 1)
                .read_to_end(&mut body)?;
            if body.len() > MAX_BODY_BYTES {
                return Err(bad_response("a body that is too large"));
            }
            body
        };
        return Ok(Response { body, ..response });
    }
}

/// The response head up to and including the blank line.
fn read_head(stream: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let before = head.len();
        stream
            .take((MAX_HEAD_BYTES + 1 - head.len()) as u64)
            .read_until(b'\n', &mut head)?;
        if head.len() == before {
            return Err(bad_response("a truncated head"));
        }
        if head.len() > MAX_HEAD_BYTES {
            return Err(bad_response("a head that is too large"));
        }
    }
    Ok(head)
}

/// The status and headers of a head that ends with a blank line.
pub(crate) fn parse_head(head: &[u8]) -> io::Result<(u16, Vec<(String, String)>)> {
    let text = std::str::from_utf8(head).map_err(|_| bad_response("a head that isn't UTF-8"))?;
    // Empty lines before the status line are skipped, as httparse does.
    let text = text.trim_start_matches("\r\n");
    let mut lines = text
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| bad_response("a truncated head"))?
        .split("\r\n");
    // `HTTP/1.x DDD`, then an optional reason of visible text.
    let line = lines.next().unwrap_or_default().as_bytes();
    let reason = |b: &u8| *b == b'\t' || *b == b' ' || (0x21..=0x7e).contains(b) || *b >= 0x80;
    if line.len() < 12
        || !line.starts_with(b"HTTP/1.")
        || !matches!(line[7], b'0' | b'1')
        || line[8] != b' '
        || !line[9..12].iter().all(u8::is_ascii_digit)
        || (line.len() > 12 && line[12] != b' ')
        || !line[12..].iter().all(reason)
    {
        return Err(bad_response("a malformed status line"));
    }
    let status = line[9..12]
        .iter()
        .fold(0u16, |status, digit| status * 10 + u16::from(digit - b'0'));
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad_response("a malformed header"))?;
        if name.is_empty() || !name.bytes().all(is_token) {
            return Err(bad_response("a malformed header"));
        }
        if value
            .bytes()
            .any(|b| b == b'\r' || b == b'\n' || (b < 0x20 && b != b'\t') || b == 0x7f)
        {
            return Err(bad_response("a malformed header"));
        }
        headers.push((
            name.to_ascii_lowercase(),
            value.trim_matches([' ', '\t']).to_owned(),
        ));
    }
    Ok((status, headers))
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// A chunked body, with its trailers skipped.
fn read_chunked(stream: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(stream)?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .ok()
            .filter(|_| !size.is_empty() && size.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| bad_response("a malformed chunk size"))?;
        if size == 0 {
            while !read_line(stream)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err(bad_response("a body that is too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        stream.read_exact(&mut body[start..])?;
        if !read_line(stream)?.is_empty() {
            return Err(bad_response("a chunk longer than its size"));
        }
    }
}

/// One line, without its `\r\n`.
fn read_line(stream: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    stream
        .take(MAX_HEAD_BYTES as u64)
        .read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\r\n") {
        return Err(bad_response("a truncated line"));
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).map_err(|_| bad_response("a line that isn't UTF-8"))
}

fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                return Ok(stream);
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

struct Url<'a> {
    authority: &'a str,
    host: &'a str,
    port: u16,
    /// The path and query.
    target: &'a str,
}

impl<'a> Url<'a> {
    fn parse(url: &'a str) -> io::Result<Self> {
        let rest = url.strip_prefix("https://").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{url} doesn't use https"),
            )
        })?;
        let slash = rest.find('/').ok_or_else(|| invalid(url))?;
        let (authority, target) = rest.split_at(slash);
        if authority.is_empty() || !target.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid(url));
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| invalid(url))?),
            None => (authority, 443),
        };
        Ok(Url {
            authority,
            host,
            port,
            target,
        })
    }
}

fn invalid(url: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{url} isn't a valid URL"),
    )
}

fn bad_response(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the server sent {what}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(bytes: &[u8]) -> io::Result<Response> {
        read_response(&mut BufReader::new(bytes))
    }

    #[test]
    fn a_request_over_real_tls_reaches_a_localhost_server() {
        let (port, server) = crate::testing::https_server(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
              Transfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n"
                .to_vec(),
        );
        let client = crate::testing::https_client();
        let url = format!("https://localhost:{port}/v1:x");
        let reply = client
            .send(&url, &[("Authorization", "Bearer t")], Some(b"{}"))
            .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"{\"a\":1}");
        let request = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(request.starts_with("POST /v1:x HTTP/1.1\r\n"), "{request}");
        assert!(request.ends_with("\r\n\r\n{}"), "{request}");
    }

    #[test]
    fn a_server_the_roots_do_not_trust_is_refused() {
        let (port, _server) = crate::testing::https_server(b"HTTP/1.1 200 OK\r\n\r\n".to_vec());
        let client = Client::new(Duration::from_secs(5));
        let error = client
            .send(&format!("https://localhost:{port}/"), &[], None)
            .unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("certificate"),
            "{error}"
        );
    }

    #[test]
    fn a_get_request_has_the_fixed_headers_and_our_user_agent() {
        let url = Url::parse("https://api.example.com/api/oauth/usage?x=1").unwrap();
        let bytes = request(&url, &[("Authorization", "Bearer t")], None).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            format!(
                "GET /api/oauth/usage?x=1 HTTP/1.1\r\nHost: api.example.com\r\n\
                 Accept-Encoding: identity\r\nConnection: close\r\n\
                 Authorization: Bearer t\r\nUser-Agent: {USER_AGENT}\r\n\r\n"
            )
        );
    }

    #[test]
    fn a_post_carries_its_json_body_and_a_given_user_agent() {
        let url = Url::parse("https://h.example:8443/v1:x").unwrap();
        let bytes = request(&url, &[("User-Agent", "other/1")], Some(b"{}")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("POST /v1:x HTTP/1.1\r\nHost: h.example:8443\r\n"));
        assert!(text.contains("User-Agent: other/1\r\n"));
        assert!(!text.contains(USER_AGENT));
        assert!(text.ends_with("Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"));
    }

    #[test]
    fn a_header_value_with_a_line_break_is_refused() {
        let url = Url::parse("https://h.example/").unwrap();
        assert!(request(&url, &[("Authorization", "a\r\nX-Evil: 1")], None).is_err());
        assert!(request(&url, &[("Bad Name", "a")], None).is_err());
    }

    #[test]
    fn a_url_must_be_https_with_a_path() {
        assert!(Url::parse("http://h.example/").is_err());
        assert!(Url::parse("https://h.example").is_err());
        assert!(Url::parse("https:///x").is_err());
        let url = Url::parse("https://h.example:444/a").unwrap();
        assert_eq!((url.host, url.port, url.target), ("h.example", 444, "/a"));
    }

    #[test]
    fn a_content_length_body_is_read_exactly() {
        let reply = response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}trailing").unwrap();
        assert_eq!((reply.status, reply.body.as_slice()), (200, &b"{}"[..]));
    }

    #[test]
    fn a_chunked_body_is_joined_and_its_trailers_skipped() {
        let reply = response(
            b"HTTP/1.1 429 Too Many\r\nTransfer-Encoding: chunked\r\nRetry-After: 30\r\n\r\n\
              3;x=y\r\nabc\r\n2\r\nde\r\n0\r\nX-Trailer: 1\r\n\r\n",
        )
        .unwrap();
        assert_eq!(reply.status, 429);
        assert_eq!(reply.header("retry-after"), Some("30"));
        assert_eq!(reply.body, b"abcde");
    }

    #[test]
    fn a_body_with_no_length_runs_to_the_end() {
        let reply = response(b"HTTP/1.0 200 OK\r\n\r\nall of it").unwrap();
        assert_eq!(reply.body, b"all of it");
    }

    #[test]
    fn an_interim_response_is_skipped() {
        let reply =
            response(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert_eq!((reply.status, reply.body.len()), (204, 0));
    }

    #[test]
    fn a_redirect_is_returned_not_followed() {
        let reply = response(
            b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere/\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(reply.status, 302);
        assert_eq!(reply.header("location"), Some("https://elsewhere/"));
    }

    #[test]
    fn malformed_responses_are_errors() {
        for bytes in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabc\r\n0\r\n\r\n",
            b"HTTP/2 200\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nNo-Colon\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n",
        ] {
            assert!(
                response(bytes).is_err(),
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Our head parser agrees with httparse on generated heads: both accept
    /// with the same status and headers, or both refuse.
    #[test]
    fn head_parsing_matches_httparse() {
        let mut state = 0x6a09_e667_f3bc_c908u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let pieces: [&str; 24] = [
            "HTTP/1.1 ",
            "HTTP/1.0 ",
            "HTTP/1.2 ",
            "HTTP/2 ",
            "200",
            "429",
            "20",
            "2000",
            " OK",
            " Too Many Requests",
            "",
            "\r\n",
            "Content-Length: 0",
            "X-A:b",
            "Retry-After:\t30 ",
            "Bad Name: x",
            ":empty",
            "x-\u{e9}: y",
            "Key: va\u{1}l",
            "Key: v\tal",
            "\r\n",
            "\r\n",
            " ",
            "Set-Cookie: a=b; Path=/",
        ];
        let mut agreed = 0;
        for _ in 0..50_000 {
            let mut head = String::new();
            let count = 1 + next() % 8;
            for _ in 0..count {
                head.push_str(pieces[(next() as usize) % pieces.len()]);
            }
            head.push_str("\r\n\r\n");
            let bytes = head.as_bytes();

            let mut parsed = [httparse::EMPTY_HEADER; 32];
            let mut oracle = httparse::Response::new(&mut parsed);
            let expected = match oracle.parse(bytes) {
                Ok(httparse::Status::Complete(length)) if length == bytes.len() => Some((
                    oracle.code.unwrap(),
                    oracle
                        .headers
                        .iter()
                        .map(|h| {
                            (
                                h.name.to_ascii_lowercase(),
                                String::from_utf8(h.value.to_vec()).unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            };
            let actual = parse_head(bytes).ok();
            // httparse accepts non-UTF-8 header values; to11y doesn't.
            if expected.is_some() && actual.is_none() && !head.is_ascii() {
                continue;
            }
            assert_eq!(actual, expected, "{head:?}");
            agreed += 1;
        }
        assert!(agreed > 40_000);
    }
}
