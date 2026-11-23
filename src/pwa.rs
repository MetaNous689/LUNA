#[cfg(test)]
use std::io::Read;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

pub(super) const HTTP_PORT: u16 = 4782;
const MAX_HEADER_BYTES: usize = 16 * 1024;

const INDEX_HTML: &str = include_str!("pwa_assets/index.html");
const STYLE_CSS: &str = include_str!("pwa_assets/app.css");
const APP_JS: &str = include_str!("pwa_assets/app.js");
const MANIFEST: &str = include_str!("pwa_assets/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("pwa_assets/service-worker.js");
const APP_ICON: &[u8] = include_bytes!("pwa_assets/icon.png");
const APP_ICON_192: &[u8] = include_bytes!("pwa_assets/icon-192.png");
const APP_ICON_512: &[u8] = include_bytes!("pwa_assets/icon-512.png");

#[derive(Clone)]
struct Shared {
    state: Arc<RwLock<Value>>,
    stop: Arc<AtomicBool>,
    port: u16,
}

pub(super) struct PwaServer {
    shared: Shared,
    accept_thread: Option<JoinHandle<()>>,
}

impl PwaServer {
    pub(super) fn bind(initial_state: Value) -> io::Result<Self> {
        Self::bind_at(initial_state, HTTP_PORT)
    }

    fn bind_at(initial_state: Value, requested_port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", requested_port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shared = Shared {
            state: Arc::new(RwLock::new(initial_state)),
            stop: Arc::new(AtomicBool::new(false)),
            port,
        };
        let accept_shared = shared.clone();
        let accept_thread = thread::Builder::new()
            .name("luna-pwa-accept".into())
            .stack_size(64 * 1024)
            .spawn(move || accept_loop(listener, accept_shared))?;
        Ok(Self {
            shared,
            accept_thread: Some(accept_thread),
        })
    }

    pub(super) fn publish(&self, state: Value) {
        if let Ok(mut current) = self.shared.state.write() {
            *current = state;
        }
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PwaServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: TcpListener, shared: Shared) {
    while !shared.stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let client_shared = shared.clone();
                let _ = thread::Builder::new()
                    .name("luna-pwa-client".into())
                    .stack_size(64 * 1024)
                    .spawn(move || handle_client(stream, client_shared));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(30));
            }
            Err(_) => break,
        }
    }
}

fn handle_client(mut stream: TcpStream, shared: Shared) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let request = read_request(&stream);
    let Ok(request) = request else {
        respond(
            &mut stream,
            "GET",
            400,
            "text/plain; charset=utf-8",
            b"Bad request",
            "no-store",
        );
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };

    if !request.host.as_deref().is_some_and(host_allowed) {
        respond(
            &mut stream,
            &request.method,
            403,
            "text/plain; charset=utf-8",
            b"Tailscale host required",
            "no-store",
        );
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }

    let path = request.target.split('?').next().unwrap_or("/");
    let response = match (request.method.as_str(), path) {
        ("GET" | "HEAD", "/") => Some((
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
            "no-cache",
        )),
        ("GET" | "HEAD", "/assets/app.css") => Some((
            200,
            "text/css; charset=utf-8",
            STYLE_CSS.as_bytes(),
            "public, max-age=3600",
        )),
        ("GET" | "HEAD", "/assets/app.js") => Some((
            200,
            "application/javascript; charset=utf-8",
            APP_JS.as_bytes(),
            "no-cache",
        )),
        ("GET" | "HEAD", "/manifest.webmanifest") => Some((
            200,
            "application/manifest+json; charset=utf-8",
            MANIFEST.as_bytes(),
            "no-cache",
        )),
        ("GET" | "HEAD", "/service-worker.js") => Some((
            200,
            "application/javascript; charset=utf-8",
            SERVICE_WORKER.as_bytes(),
            "no-cache",
        )),
        ("GET" | "HEAD", "/icon.png") | ("GET" | "HEAD", "/favicon.ico") => {
            Some((200, "image/png", APP_ICON, "public, max-age=86400"))
        }
        ("GET" | "HEAD", "/icon-192.png") => {
            Some((200, "image/png", APP_ICON_192, "public, max-age=86400"))
        }
        ("GET" | "HEAD", "/icon-512.png") => {
            Some((200, "image/png", APP_ICON_512, "public, max-age=86400"))
        }
        ("GET" | "HEAD", "/api/health") => {
            let body =
                serde_json::to_vec(&json!({"ok": true, "port": shared.port})).unwrap_or_default();
            respond(
                &mut stream,
                &request.method,
                200,
                "application/json",
                &body,
                "no-store",
            );
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        ("GET" | "HEAD", "/api/state") => {
            let body = shared
                .state
                .read()
                .ok()
                .and_then(|state| serde_json::to_vec(&*state).ok());
            if let Some(body) = body {
                respond(
                    &mut stream,
                    &request.method,
                    200,
                    "application/json",
                    &body,
                    "no-store",
                );
            } else {
                respond(
                    &mut stream,
                    &request.method,
                    503,
                    "application/json",
                    br#"{"error":"state unavailable"}"#,
                    "no-store",
                );
            }
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        ("GET" | "HEAD", _) => None,
        _ => {
            respond(
                &mut stream,
                &request.method,
                405,
                "text/plain; charset=utf-8",
                b"Read-only endpoint",
                "no-store",
            );
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    };

    if let Some((status, content_type, body, cache_control)) = response {
        respond(
            &mut stream,
            &request.method,
            status,
            content_type,
            body,
            cache_control,
        );
    } else {
        respond(
            &mut stream,
            &request.method,
            404,
            "text/plain; charset=utf-8",
            b"Not found",
            "no-store",
        );
    }
    let _ = stream.shutdown(Shutdown::Both);
}

struct Request {
    method: String,
    target: String,
    host: Option<String>,
}

fn read_request(stream: &TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut total = 0_usize;
    let mut line = String::new();
    total += reader.read_line(&mut line)?;
    if line.is_empty() || total > MAX_HEADER_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request line"));
    }
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "target"))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "version"))?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || parts.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "version"));
    }

    let mut host = None;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        total = total.saturating_add(read);
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers"));
        }
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                host = Some(value.trim().to_string());
            }
        }
    }

    Ok(Request {
        method,
        target,
        host,
    })
}

fn host_allowed(authority: &str) -> bool {
    let authority = authority.trim();
    if authority.is_empty() || authority.contains(['/', '@']) {
        return false;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            let Ok(port) = port.parse::<u16>() else {
                return false;
            };
            (host, Some(port))
        }
        Some(_) => return false,
        None => (authority, None),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "127.0.0.1" || host == "localhost" {
        return true;
    }
    if !matches!(port.unwrap_or(443), 443 | 8443 | 10000 | 38444) {
        return false;
    }
    let Some(tailnet_name) = host.strip_suffix(".ts.net") else {
        return false;
    };
    !tailnet_name.is_empty()
        && tailnet_name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn respond(
    stream: &mut TcpStream,
    method: &str,
    status: u16,
    content_type: &str,
    body: &[u8],
    cache_control: &str,
) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: {cache_control}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'self'; img-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(headers.as_bytes()).is_ok() && method != "HEAD" {
        let _ = stream.write_all(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allowlist_is_limited_to_loopback_and_tailscale_https_ports() {
        assert!(host_allowed("localhost:4782"));
        assert!(host_allowed("127.0.0.1:4782"));
        assert!(host_allowed("luna.tailnet.ts.net:8443"));
        assert!(host_allowed("luna.tailnet.ts.net:10000"));
        assert!(host_allowed("luna.tailnet.ts.net:38444"));
        assert!(host_allowed("luna.tailnet.ts.net:443"));
        assert!(host_allowed("luna.tailnet.ts.net"));
        assert!(!host_allowed("192.0.2.4:4782"));
        assert!(!host_allowed("luna.tailnet.ts.net:4782"));
        assert!(!host_allowed("luna.tailnet.ts.net.attacker.example:8443"));
        assert!(!host_allowed("user@luna.tailnet.ts.net:8443"));
    }

    #[test]
    fn pwa_serves_only_the_supplied_read_only_snapshot_and_icon() {
        let state = json!({
            "cpu": {"free_percent": 98.0},
            "agents": {"items": [{"name": "alpha-worker", "state": "working"}]},
        });
        let server = PwaServer::bind_at(state.clone(), 0).unwrap();
        let response = request(server.shared.port, "GET", "/api/state");
        let body = response_body(&response);
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert_eq!(serde_json::from_slice::<Value>(body).unwrap(), state);
        assert!(!body.windows(b"cwd".len()).any(|window| window == b"cwd"));

        let icon_response = request(server.shared.port, "GET", "/icon.png");
        assert!(icon_response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(icon_response
            .windows(9)
            .any(|window| window == b"image/png"));
        assert_eq!(response_body(&icon_response), APP_ICON);

        for (path, icon) in [
            ("/icon-192.png", APP_ICON_192),
            ("/icon-512.png", APP_ICON_512),
        ] {
            let response = request(server.shared.port, "GET", path);
            assert!(response.starts_with(b"HTTP/1.1 200 OK"));
            assert_eq!(response_body(&response), icon);
        }

        let write_response = request(server.shared.port, "POST", "/api/command");
        assert!(write_response.starts_with(b"HTTP/1.1 405 Method Not Allowed"));
    }

    #[test]
    fn pwa_keeps_agent_names_without_unconfirmed_status_labels() {
        let server =
            PwaServer::bind_at(json!({"agents": {"status": "ready", "items": []}}), 0).unwrap();
        let response = request(server.shared.port, "GET", "/assets/app.js");
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));

        let script = std::str::from_utf8(response_body(&response)).unwrap();
        assert!(script.contains(
            r#"return ["working", "blocked", "done", "idle"].includes(value) ? value : null;"#
        ));
        assert!(script.contains(".map((agent) => ({ agent, state: safeState(agent.state) }));"));
        assert!(script.contains("label.textContent = agent.name || \"agent\";"));
        assert!(script.contains("if (agentState !== null)"));
        assert!(!script.contains("? value : \"working\";"));
        assert!(!script.contains("? value : \"working\";"));

        let response = request(server.shared.port, "GET", "/service-worker.js");
        let service_worker = std::str::from_utf8(response_body(&response)).unwrap();
        assert!(service_worker.contains("const CACHE = \"luna-monitor-shell-v10\";"));
    }

    #[test]
    fn pwa_brand_is_luna_and_keeps_the_existing_install_identity() {
        assert!(INDEX_HTML.contains("<title>Luna · Monitor</title>"));
        assert!(INDEX_HTML.contains("<strong>Luna</strong>"));
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        assert_eq!(manifest["name"], "Luna Monitor");
        assert_eq!(manifest["short_name"], "Luna");
        assert_eq!(manifest["id"], "/");
        assert_eq!(manifest["scope"], "/");
        assert_eq!(manifest["start_url"], "/");
        assert!(SERVICE_WORKER.contains("key.startsWith(\"luna-monitor-shell-\")"));
    }

    #[test]
    fn pwa_uses_the_mobile_layout_without_a_desktop_breakpoint() {
        assert!(STYLE_CSS.contains("max-width: 600px;"));
        assert!(!STYLE_CSS.contains("(min-width: 761px)"));
        assert!(!STYLE_CSS.contains("(min-width: 1120px)"));
        assert!(!STYLE_CSS.contains("(max-width: 760px)"));
    }

    #[test]
    fn mobile_pwa_keeps_versioned_assets_in_the_offline_shell() {
        for asset in [
            "/assets/app.css?v=luna-limits-20261123",
            "/assets/app.js?v=luna-limits-20261123",
            "/manifest.webmanifest?v=icon-20261022-ringed-planet-cropped",
            "/icon.png?v=icon-20261022-ringed-planet-cropped",
            "/icon-192.png?v=icon-20261022-ringed-planet-cropped",
        ] {
            assert!(INDEX_HTML.contains(asset));
            assert!(SERVICE_WORKER.contains(asset));
        }
        assert!(INDEX_HTML.contains("id=\"codex-meter\""));
        assert!(INDEX_HTML.contains("id=\"claude-meter\""));
        assert!(INDEX_HTML.contains("aria-label=\"Agenten\""));
        assert!(STYLE_CSS.contains(".quota-meter:not([hidden])"));
    }

    fn request(port: u16, method: &str, path: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    fn response_body(response: &[u8]) -> &[u8] {
        let start = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        &response[start..]
    }
}
