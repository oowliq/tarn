//! Минимальный HTTP/1.1-сервер для MCP: только 127.0.0.1, одно соединение —
//! один запрос (`Connection: close`). Новых зависимостей не тянет.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use super::Hub;
use super::mcp::{self, Request};

const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY: usize = 4 * 1024 * 1024;

pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// `port` 0 — любой свободный (для тестов).
    pub fn start(port: u16, hub: Hub) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("mcp-server".into())
                .spawn(move || {
                    for conn in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        match conn {
                            Ok(stream) => {
                                let hub = hub.clone();
                                let port = addr.port();
                                let _ = std::thread::Builder::new().name("mcp-conn".into()).spawn(
                                    move || {
                                        if let Err(e) = serve(stream, &hub, port) {
                                            tracing::debug!(%e, "mcp: соединение");
                                        }
                                    },
                                );
                            }
                            Err(e) => tracing::warn!(%e, "mcp: accept"),
                        }
                    }
                })?
        };
        tracing::info!(%addr, "MCP-сервер для Claude запущен");
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Разбудить accept().
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(300));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        tracing::info!(addr = %self.addr, "MCP-сервер для Claude остановлен");
    }
}

fn read_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    r.take(MAX_LINE).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "строка слишком длинная",
        ));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn read_chunked(r: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line(r)?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk size"))?;
        if size == 0 {
            // Трейлеры до пустой строки.
            while !read_line(r)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "тело слишком большое",
            ));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])?;
        read_line(r)?;
    }
}

fn serve(stream: TcpStream, hub: &Hub, port: u16) -> io::Result<()> {
    // Действия вроде drag/wait занимают секунды — на запись даём с запасом.
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let mut writer = stream.try_clone()?;
    let mut r = BufReader::new(stream);

    let request_line = read_line(&mut r)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();

    let mut headers = Vec::new();
    loop {
        let line = read_line(&mut r)?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return write_status(&mut writer, 431);
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    if header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let body = if header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        read_chunked(&mut r)?
    } else {
        let len: usize = header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if len > MAX_BODY {
            return write_status(&mut writer, 413);
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        body
    };

    let resp = mcp::handle(
        &Request {
            method: &method,
            path: &path,
            headers: &headers,
            body: &body,
            port,
        },
        hub,
    );
    let body = resp.body.map(|v| v.to_string()).unwrap_or_default();
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    if !body.is_empty() {
        head += "Content-Type: application/json\r\n";
    }
    if resp.status == 405 {
        head += "Allow: POST\r\n";
    }
    head += &format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

fn write_status(w: &mut impl Write, status: u16) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {status} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        reason(status)
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(port: u16, raw: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_over_tcp_and_stops() {
        let hub = Hub::new(|| {});
        hub.set_token("t0k");
        let server = Server::start(0, hub).unwrap();
        let port = server.port();

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let ok = roundtrip(
            port,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer t0k\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
        assert!(ok.contains(r#""id":1"#), "{ok}");

        // Chunked-тело тоже понимаем.
        let chunked = roundtrip(
            port,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer t0k\r\n\
                 Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            ),
        );
        assert!(chunked.starts_with("HTTP/1.1 200 OK"), "{chunked}");

        let unauthorized = roundtrip(
            port,
            &format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\n\r\n"),
        );
        assert!(unauthorized.starts_with("HTTP/1.1 401"), "{unauthorized}");

        drop(server);
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }
}
