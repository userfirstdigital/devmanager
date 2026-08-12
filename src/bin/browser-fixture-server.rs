//! Deterministic loopback HTTP/1.1 fixture server for Phase 8 browser proof scripts.
//!
//! This binary never launches WebView2, a stock provider, or the installed
//! DevManager app. It binds 127.0.0.1 only, serves static files from a validated
//! root, and exits on an explicit stdin `shutdown`/`quit` line or process kill.

use std::env;
use std::error::Error;
use std::fmt;
use std::io::{BufRead as _, Write as _};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const READY_PREFIX: &str = "BROWSER_FIXTURE_SERVER_READY";

#[derive(Debug)]
struct ServerError(String);

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ServerError {}

struct Args {
    root: PathBuf,
    port: u16,
    isolated_parent: Option<PathBuf>,
}

fn print_usage() {
    eprintln!(
        "Usage: browser-fixture-server --root <dir> [--port <u16>] [--isolated-parent <dir>]\n\
         Binds 127.0.0.1 only. Port 0 (default) asks the OS for a free port.\n\
         --root must resolve beneath tests/fixtures, the process temp directory,\n\
         or --isolated-parent. Does not launch browsers or providers."
    );
}

fn parse_args() -> Result<Args, ServerError> {
    let mut raw = env::args().skip(1);
    let mut root = None;
    let mut port = 0u16;
    let mut isolated_parent = None;
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--root" => {
                root = Some(PathBuf::from(required_value("--root", raw.next())?));
            }
            "--port" => {
                let value = required_value("--port", raw.next())?;
                port = value.parse::<u16>().map_err(|_| {
                    ServerError("--port must be an integer from 0 to 65535".to_string())
                })?;
            }
            "--isolated-parent" => {
                isolated_parent = Some(PathBuf::from(required_value(
                    "--isolated-parent",
                    raw.next(),
                )?));
            }
            other => {
                return Err(ServerError(format!("unknown argument: {other}")));
            }
        }
    }
    let root = root.ok_or_else(|| ServerError("--root is required".to_string()))?;
    Ok(Args {
        root,
        port,
        isolated_parent,
    })
}

fn required_value(flag: &str, value: Option<String>) -> Result<String, ServerError> {
    value.ok_or_else(|| ServerError(format!("{flag} requires a value")))
}

fn process_temp_dir() -> PathBuf {
    env::temp_dir()
}

fn compiled_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn cwd_fixture_root() -> Option<PathBuf> {
    let mut dir = env::current_dir().ok()?;
    for _ in 0..8 {
        if dir.join("Cargo.toml").is_file() && dir.join("tests").join("fixtures").is_dir() {
            return Some(dir.join("tests").join("fixtures"));
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, ServerError> {
    path.canonicalize()
        .map_err(|error| ServerError(format!("cannot canonicalize {}: {error}", path.display())))
}

fn path_is_within(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

fn validate_root(root: &Path, isolated_parent: Option<&Path>) -> Result<PathBuf, ServerError> {
    if !root.is_dir() {
        return Err(ServerError(format!(
            "--root must be an existing directory: {}",
            root.display()
        )));
    }
    let canon = canonicalize_existing(root)?;
    let mut allowed = vec![compiled_fixture_root(), process_temp_dir()];
    if let Some(cwd_root) = cwd_fixture_root() {
        allowed.push(cwd_root);
    }
    if let Some(parent) = isolated_parent {
        if !parent.is_dir() {
            return Err(ServerError(format!(
                "--isolated-parent must be an existing directory: {}",
                parent.display()
            )));
        }
        allowed.push(canonicalize_existing(parent)?);
    }
    let mut resolved_allowed = Vec::new();
    for prefix in allowed {
        if let Ok(canon_prefix) = prefix.canonicalize() {
            resolved_allowed.push(canon_prefix);
        }
    }
    if resolved_allowed
        .iter()
        .any(|prefix| path_is_within(prefix, &canon))
    {
        return Ok(canon);
    }
    Err(ServerError(format!(
        "--root {} is not beneath tests/fixtures, the process temp directory, or --isolated-parent",
        canon.display()
    )))
}

fn percent_decode(input: &str) -> Result<String, ServerError> {
    let mut bytes = Vec::with_capacity(input.len());
    let raw = input.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'%' => {
                if index + 2 >= raw.len() {
                    return Err(ServerError("invalid percent-encoding".to_string()));
                }
                let hex = std::str::from_utf8(&raw[index + 1..index + 3])
                    .map_err(|_| ServerError("invalid percent-encoding".to_string()))?;
                let value = u8::from_str_radix(hex, 16)
                    .map_err(|_| ServerError("invalid percent-encoding".to_string()))?;
                bytes.push(value);
                index += 3;
            }
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            other => {
                bytes.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| ServerError("path is not UTF-8".to_string()))
}

fn requested_relative_path(target: &str) -> Result<String, u16> {
    let path = target.split('?').next().unwrap_or(target);
    if path.is_empty() || !path.starts_with('/') {
        return Err(400);
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(400);
    }
    let decoded = percent_decode(path).map_err(|_| 400_u16)?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return Err(400);
    }
    let trimmed = decoded.trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok("index.html".to_string());
    }
    if trimmed
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || part.contains(':'))
    {
        return Err(400);
    }
    Ok(trimmed.to_string())
}

fn resolve_static_file(root: &Path, target: &str) -> Result<PathBuf, u16> {
    let relative = requested_relative_path(target)?;
    let mut joined = root.to_path_buf();
    for component in Path::new(&relative).components() {
        match component {
            Component::Normal(part) => joined.push(part),
            _ => return Err(400),
        }
    }
    if !joined.is_file() {
        return Err(404);
    }
    let canon = joined.canonicalize().map_err(|_| 404_u16)?;
    if !path_is_within(root, &canon) {
        return Err(400);
    }
    Ok(canon)
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

struct ParsedRequest {
    method: String,
    target: String,
    content_length: usize,
}

async fn read_request(stream: &mut TcpStream) -> Result<ParsedRequest, u16> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|_| 400_u16)?;
        if read == 0 {
            return Err(400);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST_LINE_BYTES + MAX_HEADER_BYTES + 4 {
            return Err(413);
        }
        if let Some(index) = find_header_end(&buffer) {
            let header_block = &buffer[..index];
            let rest = buffer[index + 4..].to_vec();
            let parsed = parse_headers(header_block)?;
            if parsed.content_length > MAX_BODY_BYTES {
                return Err(413);
            }
            let mut body = rest;
            while body.len() < parsed.content_length {
                let remaining = parsed.content_length - body.len();
                let read = stream
                    .read(&mut chunk[..remaining.min(chunk.len())])
                    .await
                    .map_err(|_| 400_u16)?;
                if read == 0 {
                    return Err(400);
                }
                body.extend_from_slice(&chunk[..read]);
                if body.len() > MAX_BODY_BYTES {
                    return Err(413);
                }
            }
            return Ok(parsed);
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_headers(block: &[u8]) -> Result<ParsedRequest, u16> {
    let text = std::str::from_utf8(block).map_err(|_| 400_u16)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(400_u16)?;
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        return Err(413);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(400_u16)?.to_ascii_uppercase();
    let target = parts.next().ok_or(400_u16)?.to_string();
    let version = parts.next().ok_or(400_u16)?;
    if parts.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(400);
    }
    let mut content_length = 0usize;
    let mut header_count = 0usize;
    let mut header_bytes = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        header_count += 1;
        header_bytes += line.len();
        if header_count > MAX_HEADER_COUNT
            || header_bytes > MAX_HEADER_BYTES
            || line.len() > MAX_HEADER_LINE_BYTES
        {
            return Err(413);
        }
        let (name, value) = line.split_once(':').ok_or(400_u16)?;
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().map_err(|_| 400_u16)?;
            if content_length > MAX_BODY_BYTES {
                return Err(413);
            }
        }
        if name.eq_ignore_ascii_case("host") {
            let host = value.trim();
            if !(host.starts_with("127.0.0.1") || host.starts_with("localhost")) {
                return Err(400);
            }
        }
    }
    Ok(ParsedRequest {
        method,
        target,
        content_length,
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), std::io::Error> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn handle_connection(mut stream: TcpStream, root: Arc<PathBuf>) {
    let outcome = read_request(&mut stream).await;
    let result = match outcome {
        Ok(request) => serve_request(&request, root.as_path()).await,
        Err(status) => Err(status),
    };
    let _ = match result {
        Ok((content_type, body)) => {
            write_response(&mut stream, 200, "OK", content_type, &body).await
        }
        Err(400) => {
            write_response(
                &mut stream,
                400,
                "Bad Request",
                "text/plain",
                b"bad request",
            )
            .await
        }
        Err(404) => write_response(&mut stream, 404, "Not Found", "text/plain", b"not found").await,
        Err(405) => {
            write_response(
                &mut stream,
                405,
                "Method Not Allowed",
                "text/plain",
                b"method not allowed",
            )
            .await
        }
        Err(413) => {
            write_response(
                &mut stream,
                413,
                "Payload Too Large",
                "text/plain",
                b"payload too large",
            )
            .await
        }
        Err(_) => {
            write_response(
                &mut stream,
                500,
                "Internal Server Error",
                "text/plain",
                b"internal error",
            )
            .await
        }
    };
}

async fn serve_request(
    request: &ParsedRequest,
    root: &Path,
) -> Result<(&'static str, Vec<u8>), u16> {
    match request.method.as_str() {
        "GET" | "HEAD" => {}
        _ => return Err(405),
    }
    if request.target == "/health" {
        let body = br#"{"ok":true,"service":"browser-fixture-server"}"#.to_vec();
        if request.method == "HEAD" {
            return Ok(("application/json; charset=utf-8", Vec::new()));
        }
        return Ok(("application/json; charset=utf-8", body));
    }
    let path = resolve_static_file(root, &request.target)?;
    let body = if request.method == "HEAD" {
        Vec::new()
    } else {
        std::fs::read(&path).map_err(|_| 404_u16)?
    };
    Ok((content_type_for(&path), body))
}

fn emit_ready_line(addr: SocketAddr, root: &Path) -> Result<(), ServerError> {
    let payload = serde_json::json!({
        "url": format!("http://127.0.0.1:{}/", addr.port()),
        "pid": std::process::id(),
        "root": root,
    });
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{READY_PREFIX} {payload}")
        .and_then(|_| stdout.flush())
        .map_err(|error| ServerError(format!("failed to emit ready line: {error}")))
}

fn spawn_shutdown_watchers(notify: Arc<Notify>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => {
                    // Launchers often close stdin. Keep serving until the process is killed.
                    std::thread::park();
                    break;
                }
                Ok(_) => {
                    let command = line.trim();
                    if command.eq_ignore_ascii_case("quit")
                        || command.eq_ignore_ascii_case("shutdown")
                    {
                        stop.store(true, Ordering::SeqCst);
                        notify.notify_waiters();
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("browser-fixture-server: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ServerError> {
    let args = parse_args()?;
    let root = validate_root(&args.root, args.isolated_parent.as_deref())?;
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], args.port)))
        .await
        .map_err(|error| ServerError(format!("failed to bind 127.0.0.1: {error}")))?;
    let addr = listener
        .local_addr()
        .map_err(|error| ServerError(format!("failed to read bound address: {error}")))?;
    emit_ready_line(addr, &root)?;

    let root = Arc::new(root);
    let stop = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    spawn_shutdown_watchers(Arc::clone(&notify), Arc::clone(&stop));

    loop {
        tokio::select! {
            _ = notify.notified() => break,
            accepted = listener.accept() => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match accepted {
                    Ok((stream, peer)) => {
                        if !peer.ip().is_loopback() {
                            continue;
                        }
                        let root = Arc::clone(&root);
                        tokio::spawn(handle_connection(stream, root));
                    }
                    Err(_) => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
