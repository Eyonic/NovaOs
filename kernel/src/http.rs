//! Minimal HTTP/1.1 client over TCP or TLS.
//!
//! One request per connection (`Connection: close`). Supports Content-Length,
//! chunked transfer encoding, and close-delimited bodies. [`Response::lines`]
//! streams the body line by line, which the Claude client uses for server-sent events.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::net::{self, TcpStream};
use crate::tls::TlsStream;

/// A byte stream: plain TCP or TLS.
pub trait Stream {
    fn write_all(&mut self, data: &[u8]) -> Result<(), String>;
    /// Ok(0) = end of stream.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String>;
}

impl Stream for TcpStream {
    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        TcpStream::write_all(self, data)
    }
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        TcpStream::read(self, buf)
    }
}

pub struct Url<'a> {
    pub https: bool,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
}

pub fn parse_url(url: &str) -> Result<Url<'_>, String> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("URL must start with http:// or https://".into());
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {url}"))?),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("no host in {url}"));
    }
    Ok(Url { https, host, port, path })
}

pub fn connect(url: &Url) -> Result<Box<dyn Stream>, String> {
    let ip = net::resolve(url.host)?;
    let tcp = TcpStream::connect(ip, url.port)?;
    Ok(if url.https { Box::new(TlsStream::connect(tcp, url.host)?) } else { Box::new(tcp) })
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    reader: BodyReader,
}

pub struct FullResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

enum Framing {
    Length(usize),
    Chunked { remaining_in_chunk: usize, done: bool },
    UntilClose,
}

struct BodyReader {
    stream: Box<dyn Stream>,
    buf: Vec<u8>,
    pos: usize,
    framing: Framing,
    eof: bool,
}

impl BodyReader {
    /// Ensure at least one unread byte is buffered; false at end of stream.
    fn fill(&mut self) -> Result<bool, String> {
        if self.pos < self.buf.len() {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        self.buf.resize(16 * 1024, 0);
        let n = self.stream.read(&mut self.buf)?;
        self.buf.truncate(n);
        self.pos = 0;
        if n == 0 {
            self.eof = true;
        }
        Ok(n > 0)
    }

    fn read_raw_line(&mut self) -> Result<Option<String>, String> {
        let mut line = Vec::new();
        loop {
            if !self.fill()? {
                return Ok(if line.is_empty() { None } else { Some(String::from_utf8_lossy(&line).into_owned()) });
            }
            let available = &self.buf[self.pos..];
            if let Some(i) = available.iter().position(|&b| b == b'\n') {
                line.extend_from_slice(&available[..i]);
                self.pos += i + 1;
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            line.extend_from_slice(available);
            self.pos = self.buf.len();
        }
    }

    /// Read body bytes, honouring the framing; Ok(0) at the end of the body.
    fn read_body(&mut self, out: &mut [u8]) -> Result<usize, String> {
        loop {
            let limit = match &mut self.framing {
                Framing::Length(0) => return Ok(0),
                Framing::Length(n) => *n,
                Framing::Chunked { done: true, .. } => return Ok(0),
                Framing::Chunked { remaining_in_chunk: 0, .. } => {
                    let size_line = self.read_raw_line()?.ok_or("connection closed inside chunked body")?;
                    let size_line = if size_line.is_empty() { self.read_raw_line()?.unwrap_or_default() } else { size_line };
                    let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                        .map_err(|_| format!("bad chunk size line {size_line:?}"))?;
                    self.framing = if size == 0 { Framing::Chunked { remaining_in_chunk: 0, done: true } } else { Framing::Chunked { remaining_in_chunk: size, done: false } };
                    continue;
                }
                Framing::Chunked { remaining_in_chunk, .. } => *remaining_in_chunk,
                Framing::UntilClose => usize::MAX,
            };
            if !self.fill()? {
                return match self.framing {
                    Framing::UntilClose => Ok(0),
                    _ => Err("connection closed before the end of the body".into()),
                };
            }
            let n = out.len().min(limit).min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            match &mut self.framing {
                Framing::Length(left) => *left -= n,
                Framing::Chunked { remaining_in_chunk, .. } => *remaining_in_chunk -= n,
                Framing::UntilClose => {}
            }
            return Ok(n);
        }
    }
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn read_all(mut self) -> Result<FullResponse, String> {
        let mut body = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = self.reader.read_body(&mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Ok(FullResponse { status: self.status, headers: self.headers, body })
    }

    /// Next line of the body (without the line ending), or None at the end.
    pub fn next_line(&mut self) -> Result<Option<String>, String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            // Byte-at-a-time over the already-buffered body; cheap enough for SSE.
            if self.reader.read_body(&mut byte)? == 0 {
                return Ok(if line.is_empty() { None } else { Some(String::from_utf8_lossy(&line).into_owned()) });
            }
            if byte[0] == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            line.push(byte[0]);
        }
    }
}

/// Send a request and read the status line and headers. The body is read from the
/// returned [`Response`].
pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    let u = parse_url(url)?;
    let mut stream = connect(&u)?;
    let mut req = format!("{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: NovaOS/0.4\r\nConnection: close\r\n", u.path, u.host);
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    stream.write_all(&out)?;

    let mut reader = BodyReader { stream, buf: Vec::new(), pos: 0, framing: Framing::UntilClose, eof: false };
    let status_line = reader.read_raw_line()?.ok_or("server closed the connection without a response")?;
    let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| format!("bad status line {status_line:?}"))?;
    let mut headers = Vec::new();
    loop {
        let line = reader.read_raw_line()?.ok_or("connection closed inside headers")?;
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let find = |name: &str| headers.iter().find(|(k, _): &&(String, String)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    reader.framing = if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        Framing::Chunked { remaining_in_chunk: 0, done: false }
    } else if let Some(len) = find("content-length").and_then(|v| v.parse().ok()) {
        Framing::Length(len)
    } else if method == "HEAD" || status == 204 || status == 304 {
        Framing::Length(0)
    } else {
        Framing::UntilClose
    };
    Ok(Response { status, headers, reader })
}

pub fn get(url: &str) -> Result<FullResponse, String> {
    request("GET", url, &[], &[])?.read_all()
}
