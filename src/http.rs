use serde_json::Value as Json;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn cookies(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .filter_map(|(_, v)| v.split(';').next()?.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect()
    }

    pub fn json(&self) -> Json {
        serde_json::from_slice(&self.body).unwrap_or(Json::Null)
    }
}

pub fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<Response> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "header contains a line break",
            ));
        }
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    let written = stream.write_all(&bytes);
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    let read_err = loop {
        match stream.read(&mut buf) {
            Ok(0) => break None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) => break Some(e),
        }
    };
    match parse(&raw) {
        Ok(r) if complete(&r) => Ok(r),
        parsed => Err(read_err.or(written.err()).unwrap_or_else(|| match parsed {
            Err(e) => e,
            Ok(_) => io::Error::new(io::ErrorKind::UnexpectedEof, "truncated HTTP response"),
        })),
    }
}

fn complete(r: &Response) -> bool {
    r.header("content-length")
        .and_then(|l| l.parse::<usize>().ok())
        .is_none_or(|n| r.body.len() >= n)
}

fn parse(raw: &[u8]) -> io::Result<Response> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("incomplete HTTP response"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad("non-UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("bad status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        body = dechunk(&body).ok_or_else(|| bad("bad chunked body"))?;
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&b[..eol])
                .ok()?
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        b = &b[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(..size)?);
        b = b.get(size + 2..)?;
    }
}
