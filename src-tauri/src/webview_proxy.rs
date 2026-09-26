use crate::config::settings::{AppSettings, ProxyMode};
use crate::net::NetworkDefaults;
use base64::Engine;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;

/// macOS needs Tauri's `macos-proxy` feature and macOS 14; mobile WebViews have no hook.
pub const SUPPORTED: bool = cfg!(any(target_os = "windows", target_os = "linux"));

const MAX_HEAD_BYTES: usize = 16 * 1024;

static ACTIVE: AtomicBool = AtomicBool::new(false);

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type BoxStream = Box<dyn Stream>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyKind {
    Http,
    Https,
    Socks5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Upstream {
    kind: ProxyKind,
    host: String,
    port: u16,
    credentials: Option<(String, String)>,
}

impl Upstream {
    fn parse(raw: &str, credentials: Option<(String, String)>) -> Result<Self, String> {
        let url = reqwest::Url::parse(raw.trim()).map_err(|e| format!("Invalid proxy URL: {e}"))?;
        let (kind, default_port) = match url.scheme() {
            "http" => (ProxyKind::Http, 80),
            "https" => (ProxyKind::Https, 443),
            // Resolving hostnames here would leak every destination to the local DNS server.
            "socks5" | "socks5h" => (ProxyKind::Socks5, 1080),
            other => {
                return Err(format!(
                    "Proxy scheme '{other}' is not supported for WebView traffic"
                ))
            }
        };
        let host = url
            .host_str()
            .ok_or("Proxy URL has no host")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let from_url = (!url.username().is_empty()).then(|| {
            (
                percent_decode(url.username()),
                percent_decode(url.password().unwrap_or("")),
            )
        });
        Ok(Self {
            kind,
            host,
            port: url.port().unwrap_or(default_port),
            credentials: from_url.or(credentials),
        })
    }
}

fn percent_decode(value: &str) -> String {
    urlencoding::decode(value)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

/// "System" mode is left alone: the WebView follows it natively, including PAC.
pub fn needed(settings: &AppSettings) -> bool {
    settings.proxy_mode != ProxyMode::System || !crate::net::host_routes(settings).is_empty()
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Must run before the window is created: WebViews read proxy settings only then.
pub fn install(
    tauri_config: &mut tauri::Config,
    settings: &AppSettings,
) -> Option<std::net::TcpListener> {
    if !SUPPORTED || !needed(settings) {
        return None;
    }
    let listener = match bind() {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, "WebView proxy could not bind; media will bypass the app proxy");
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    let proxy_url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}")).ok()?;
    for window in &mut tauri_config.app.windows {
        window.proxy_url = Some(proxy_url.clone());
    }
    ACTIVE.store(true, Ordering::Relaxed);
    Some(listener)
}

fn bind() -> std::io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub async fn serve(listener: std::net::TcpListener) {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, "WebView proxy failed to start");
            ACTIVE.store(false, Ordering::Relaxed);
            return;
        }
    };
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                tokio::spawn(async move {
                    if let Err(error) = handle(client).await {
                        tracing::debug!(%error, "WebView proxy connection failed");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "WebView proxy accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle(mut client: TcpStream) -> Result<(), String> {
    let net = crate::net::defaults();
    let (head, body_prefix) =
        tokio::time::timeout(net.request_timeout, read_request_head(&mut client))
            .await
            .map_err(|_| "Timed out waiting for the request".to_string())??;

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = match request.parse(&head) {
        Ok(status) if status.is_complete() => Ok(()),
        Ok(_) => Err("Incomplete request head".to_string()),
        Err(e) => Err(format!("Malformed request: {e}")),
    };
    if let Err(error) = parsed {
        reply_error(&mut client, &error).await;
        return Err(error);
    }
    let method = request.method.unwrap_or_default().to_string();
    let target = request.path.unwrap_or_default().to_string();

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(&target)?;
        let tunnel = match route_for(&host, true, &net) {
            Ok(upstream) => open_tunnel(upstream.as_ref(), &host, port, net.connect_timeout).await,
            Err(error) => Err(error),
        };
        let mut upstream = match tunnel {
            Ok(stream) => stream,
            Err(error) => {
                reply_error(&mut client, &error).await;
                return Err(error);
            }
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        if !body_prefix.is_empty() {
            upstream
                .write_all(&body_prefix)
                .await
                .map_err(|e| e.to_string())?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    let forwarded =
        forward_plain_http(&method, &target, request.version, request.headers, &net).await;
    let (mut upstream, rewritten_head) = match forwarded {
        Ok(pair) => pair,
        Err(error) => {
            reply_error(&mut client, &error).await;
            return Err(error);
        }
    };
    upstream
        .write_all(&rewritten_head)
        .await
        .map_err(|e| e.to_string())?;
    upstream
        .write_all(&body_prefix)
        .await
        .map_err(|e| e.to_string())?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// HTTP proxies get absolute form (some refuse `CONNECT` to port 80), other hops origin
/// form. `Connection: close` stops reuse for another host; WebSocket upgrades keep theirs.
async fn forward_plain_http(
    method: &str,
    target: &str,
    version: Option<u8>,
    headers: &[httparse::Header<'_>],
    net: &NetworkDefaults,
) -> Result<(BoxStream, Vec<u8>), String> {
    let url = reqwest::Url::parse(target)
        .map_err(|_| "Only absolute-form requests can be proxied".to_string())?;
    if url.scheme() != "http" {
        return Err(format!("Unsupported scheme '{}'", url.scheme()));
    }
    let host = url
        .host_str()
        .ok_or("Request has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let upstream = route_for(&host, false, net)?;

    let http_proxy = upstream
        .as_ref()
        .filter(|proxy| matches!(proxy.kind, ProxyKind::Http | ProxyKind::Https));
    let (stream, request_target) = match http_proxy {
        Some(proxy) => (
            connect_proxy(proxy, net.connect_timeout).await?,
            target.to_string(),
        ),
        None => {
            let mut origin = url.path().to_string();
            if let Some(query) = url.query() {
                origin.push('?');
                origin.push_str(query);
            }
            (
                open_tunnel(upstream.as_ref(), &host, port, net.connect_timeout).await?,
                origin,
            )
        }
    };

    let is_upgrade = headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case("upgrade"));
    let hop_by_hop: &[&str] = if is_upgrade {
        &["proxy-connection", "proxy-authorization", "keep-alive"]
    } else {
        &[
            "proxy-connection",
            "proxy-authorization",
            "connection",
            "keep-alive",
        ]
    };
    let mut head = format!(
        "{method} {request_target} HTTP/1.{}\r\n",
        version.unwrap_or(1)
    );
    for header in headers {
        let name = header.name;
        if hop_by_hop.iter().any(|hop| name.eq_ignore_ascii_case(hop)) {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(&String::from_utf8_lossy(header.value));
        head.push_str("\r\n");
    }
    if let Some(credentials) = http_proxy.and_then(|proxy| proxy.credentials.as_ref()) {
        head.push_str(&proxy_authorization(credentials));
    }
    if !is_upgrade {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    Ok((stream, head.into_bytes()))
}

fn route_for(
    host: &str,
    is_https: bool,
    net: &NetworkDefaults,
) -> Result<Option<Upstream>, String> {
    // WebKitGTK also sends the dev server and the local media server here.
    if is_loopback(host) {
        return Ok(None);
    }
    if let Some(route) = net.override_for_host(host) {
        return Upstream::parse(&route.proxy_url, None).map(Some);
    }
    match net.proxy.mode {
        ProxyMode::None => Ok(None),
        ProxyMode::System => Ok(system_upstream(host, is_https)),
        ProxyMode::Custom => {
            let url = net.proxy.url.trim();
            if url.is_empty() {
                return Ok(None);
            }
            let credentials = (!net.proxy.username.is_empty())
                .then(|| (net.proxy.username.clone(), net.proxy.password.clone()));
            Upstream::parse(url, credentials).map(Some)
        }
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// PAC and WPAD are not evaluated; see `needed`.
fn system_upstream(host: &str, is_https: bool) -> Option<Upstream> {
    let env = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
    };
    let from_env = if is_https {
        env(&["HTTPS_PROXY", "https_proxy"])
    } else {
        env(&["HTTP_PROXY", "http_proxy"])
    }
    .or_else(|| env(&["ALL_PROXY", "all_proxy"]));

    if let Some(raw) = from_env {
        let bypassed = env(&["NO_PROXY", "no_proxy"])
            .is_some_and(|no_proxy| matches_no_proxy(host, &no_proxy));
        return (!bypassed)
            .then(|| Upstream::parse(&with_http_scheme(&raw), None).ok())
            .flatten();
    }

    #[cfg(target_os = "windows")]
    {
        windows_system_upstream(host, is_https)
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

fn with_http_scheme(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    }
}

fn matches_no_proxy(host: &str, no_proxy: &str) -> bool {
    let host = host.to_ascii_lowercase();
    no_proxy.split(',').map(str::trim).any(|entry| {
        if entry == "*" {
            return true;
        }
        let entry = entry.trim_start_matches('.').to_ascii_lowercase();
        !entry.is_empty()
            && (host == entry
                || host
                    .strip_suffix(entry.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

#[cfg(target_os = "windows")]
fn windows_system_upstream(host: &str, is_https: bool) -> Option<Upstream> {
    let key = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    if key.get_u32("ProxyEnable").ok()? == 0 {
        return None;
    }
    let overrides = key.get_string("ProxyOverride").unwrap_or_default();
    if matches_windows_override(host, &overrides) {
        return None;
    }
    parse_windows_proxy_server(&key.get_string("ProxyServer").ok()?, is_https)
}

/// WinINet's `https=` entry is an HTTP proxy used for `CONNECT`, not a TLS proxy.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_windows_proxy_server(server: &str, is_https: bool) -> Option<Upstream> {
    if !server.contains('=') {
        return Upstream::parse(&with_http_scheme(server), None).ok();
    }
    let mut http = None;
    let mut https = None;
    let mut socks = None;
    for entry in server.split(';') {
        let Some((protocol, address)) = entry.split_once('=') else {
            continue;
        };
        match protocol.trim().to_ascii_lowercase().as_str() {
            "http" => http = Some(address.trim()),
            "https" => https = Some(address.trim()),
            "socks" => socks = Some(address.trim()),
            _ => {}
        }
    }
    if let Some(address) = if is_https { https } else { http } {
        return Upstream::parse(&with_http_scheme(address), None).ok();
    }
    socks.and_then(|address| Upstream::parse(&format!("socks5://{address}"), None).ok())
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn matches_windows_override(host: &str, overrides: &str) -> bool {
    let host = host.to_ascii_lowercase();
    overrides
        .split(';')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| {
            if pattern.eq_ignore_ascii_case("<local>") {
                !host.contains('.')
            } else {
                wildcard_match(&pattern.to_ascii_lowercase(), &host)
            }
        })
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || !text.ends_with(last) || text.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(index) => rest = &rest[index + middle.len()..],
            None => return false,
        }
    }
    true
}

async fn open_tunnel(
    upstream: Option<&Upstream>,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<BoxStream, String> {
    let Some(proxy) = upstream else {
        return Ok(Box::new(connect_tcp(host, port, timeout).await?));
    };
    let stream = connect_proxy(proxy, timeout).await?;
    let negotiate = async {
        match proxy.kind {
            ProxyKind::Socks5 => socks5_connect(stream, proxy, host, port).await,
            ProxyKind::Http | ProxyKind::Https => http_connect(stream, proxy, host, port).await,
        }
    };
    tokio::time::timeout(timeout, negotiate)
        .await
        .map_err(|_| format!("Proxy did not open a tunnel to {host}:{port} in time"))?
}

async fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    tokio::time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| format!("Connecting to {host}:{port} timed out"))?
        .map_err(|e| format!("Connecting to {host}:{port} failed: {e}"))
}

async fn connect_proxy(proxy: &Upstream, timeout: Duration) -> Result<BoxStream, String> {
    let tcp = connect_tcp(&proxy.host, proxy.port, timeout).await?;
    if proxy.kind != ProxyKind::Https {
        return Ok(Box::new(tcp));
    }
    let server_name = rustls::pki_types::ServerName::try_from(proxy.host.clone())
        .map_err(|e| format!("Invalid proxy host for TLS: {e}"))?;
    let tls = tokio::time::timeout(timeout, tls_connector().connect(server_name, tcp))
        .await
        .map_err(|_| "TLS handshake with the proxy timed out".to_string())?
        .map_err(|e| format!("TLS handshake with the proxy failed: {e}"))?;
    Ok(Box::new(tls))
}

/// OS roots too, so a proxy signed by a corporate CA verifies.
fn tls_connector() -> tokio_rustls::TlsConnector {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Arc::new(config)
    });
    tokio_rustls::TlsConnector::from(config.clone())
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn proxy_authorization((username, password): &(String, String)) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Proxy-Authorization: Basic {token}\r\n")
}

async fn http_connect(
    mut stream: BoxStream,
    proxy: &Upstream,
    host: &str,
    port: u16,
) -> Result<BoxStream, String> {
    let authority = authority(host, port);
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(credentials) = &proxy.credentials {
        request.push_str(&proxy_authorization(credentials));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    let head = read_response_head(&mut stream).await?;
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    response
        .parse(&head)
        .map_err(|e| format!("Malformed proxy response: {e}"))?;
    match response.code {
        Some(code) if (200..300).contains(&code) => Ok(stream),
        Some(407) => Err("Proxy rejected the credentials (HTTP 407)".to_string()),
        Some(code) => Err(format!("Proxy refused CONNECT to {authority}: HTTP {code}")),
        None => Err("Proxy sent no status".to_string()),
    }
}

async fn socks5_connect(
    mut stream: BoxStream,
    proxy: &Upstream,
    host: &str,
    port: u16,
) -> Result<BoxStream, String> {
    let io = |e: std::io::Error| format!("SOCKS5 proxy: {e}");
    // "No auth" is offered alongside credentials; many proxies accept either.
    let greeting: &[u8] = if proxy.credentials.is_some() {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    stream.write_all(greeting).await.map_err(io)?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(io)?;
    if reply[0] != 0x05 {
        return Err("Proxy is not a SOCKS5 server".to_string());
    }
    match (reply[1], &proxy.credentials) {
        (0x00, _) => {}
        (0x02, Some((username, password))) => {
            if username.len() > 255 || password.len() > 255 {
                return Err("SOCKS5 credentials exceed 255 bytes".to_string());
            }
            let mut auth = vec![0x01, username.len() as u8];
            auth.extend_from_slice(username.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await.map_err(io)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await.map_err(io)?;
            if status[1] != 0x00 {
                return Err("SOCKS5 proxy rejected the credentials".to_string());
            }
        }
        (0xFF, _) => return Err("SOCKS5 proxy accepted no offered auth method".to_string()),
        (other, _) => {
            return Err(format!(
                "SOCKS5 proxy chose unsupported method {other:#04x}"
            ))
        }
    }

    let mut request = vec![0x05, 0x01, 0x00];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                return Err("Hostname too long for SOCKS5".to_string());
            }
            request.push(0x03);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(io)?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.map_err(io)?;
    if header[1] != 0x00 {
        return Err(format!(
            "SOCKS5 proxy could not reach {}: {}",
            authority(host, port),
            socks5_error(header[1])
        ));
    }
    let bound_address_len = match header[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(io)?;
            len[0] as usize
        }
        other => return Err(format!("SOCKS5 proxy sent unknown address type {other}")),
    };
    let mut bound = vec![0u8; bound_address_len + 2];
    stream.read_exact(&mut bound).await.map_err(io)?;
    Ok(stream)
}

fn socks5_error(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

fn split_authority(target: &str) -> Result<(String, u16), String> {
    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| format!("CONNECT target '{target}' has no port"))?;
    let port = port
        .parse()
        .map_err(|_| format!("CONNECT target '{target}' has an invalid port"))?;
    Ok((
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        port,
    ))
}

/// Bytes read past the head (a body or tunnel data) are returned to be forwarded.
async fn read_request_head(client: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let read = client.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("Client closed before sending a request".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_head_end(&buffer) {
            let rest = buffer.split_off(end);
            return Ok((buffer, rest));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err("Request head too large".to_string());
        }
    }
}

/// One byte at a time: after a `CONNECT` reply every following byte belongs to the tunnel.
async fn read_response_head(stream: &mut BoxStream) -> Result<Vec<u8>, String> {
    let mut head = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while find_head_end(&head).is_none() {
        if head.len() > MAX_HEAD_BYTES {
            return Err("Proxy response head too large".to_string());
        }
        if stream.read(&mut byte).await.map_err(|e| e.to_string())? == 0 {
            return Err("Proxy closed the connection during the handshake".to_string());
        }
        head.push(byte[0]);
    }
    Ok(head)
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

async fn reply_error(client: &mut TcpStream, message: &str) {
    let body = message.as_bytes();
    let head = format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = client.write_all(head.as_bytes()).await;
    let _ = client.write_all(body).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{HostRoute, ProxySettings};

    fn net_with(proxy: ProxySettings, host_routes: Vec<HostRoute>) -> NetworkDefaults {
        NetworkDefaults {
            proxy,
            host_routes,
            ..NetworkDefaults::default()
        }
    }

    fn custom_proxy(url: &str, username: &str, password: &str) -> ProxySettings {
        ProxySettings {
            mode: ProxyMode::Custom,
            url: url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            bypass_local: true,
        }
    }

    #[test]
    fn upstream_urls_parse_with_defaults_and_credentials() {
        let http = Upstream::parse("http://proxy.test", None).unwrap();
        assert_eq!(
            (http.kind, http.port, http.credentials),
            (ProxyKind::Http, 80, None)
        );

        let https = Upstream::parse("https://proxy.test", None).unwrap();
        assert_eq!((https.kind, https.port), (ProxyKind::Https, 443));

        let socks = Upstream::parse("socks5h://u%40x:p%3Ass@10.0.0.1:9050", None).unwrap();
        assert_eq!(socks.kind, ProxyKind::Socks5);
        assert_eq!(socks.port, 9050);
        assert_eq!(
            socks.credentials,
            Some(("u@x".to_string(), "p:ss".to_string()))
        );

        let separate = Upstream::parse(
            "http://proxy.test:3128",
            Some(("user".into(), "pass".into())),
        )
        .unwrap();
        assert_eq!(separate.credentials, Some(("user".into(), "pass".into())));

        let ipv6 = Upstream::parse("http://[::1]:8080", None).unwrap();
        assert_eq!(ipv6.host, "::1");

        assert!(Upstream::parse("socks4://proxy.test", None).is_err());
        assert!(Upstream::parse("not a url", None).is_err());
    }

    #[test]
    fn loopback_always_goes_direct() {
        let net = net_with(custom_proxy("http://proxy.test:3128", "", ""), Vec::new());
        for host in ["localhost", "tauri.localhost", "127.0.0.1", "[::1]", "::1"] {
            assert_eq!(route_for(host, true, &net).unwrap(), None, "{host}");
        }
    }

    #[test]
    fn host_overrides_beat_the_global_proxy_and_the_most_specific_wins() {
        let net = net_with(
            custom_proxy("http://global.test:3128", "", ""),
            vec![
                HostRoute {
                    domain: "coomer.st".into(),
                    proxy_url: "socks5://provider.test:1080".into(),
                },
                HostRoute {
                    domain: "img.coomer.st".into(),
                    proxy_url: "http://images.test:8080".into(),
                },
            ],
        );
        let host_of = |host: &str| route_for(host, true, &net).unwrap().unwrap().host;
        assert_eq!(host_of("c3.coomer.st"), "provider.test");
        assert_eq!(host_of("coomer.st"), "provider.test");
        assert_eq!(host_of("img.coomer.st"), "images.test");
        assert_eq!(host_of("notcoomer.st"), "global.test");
        assert_eq!(host_of("example.com"), "global.test");
    }

    #[test]
    fn global_modes_route_as_the_app_clients_do() {
        let direct = net_with(
            ProxySettings {
                mode: ProxyMode::None,
                ..ProxySettings::default()
            },
            Vec::new(),
        );
        assert_eq!(route_for("example.com", true, &direct).unwrap(), None);

        let empty_custom = net_with(custom_proxy("  ", "", ""), Vec::new());
        assert_eq!(route_for("example.com", true, &empty_custom).unwrap(), None);

        let custom = net_with(
            custom_proxy("http://proxy.test:3128", "user", "pw"),
            Vec::new(),
        );
        let upstream = route_for("example.com", true, &custom).unwrap().unwrap();
        assert_eq!(upstream.port, 3128);
        assert_eq!(upstream.credentials, Some(("user".into(), "pw".into())));
    }

    #[test]
    fn no_proxy_matches_hosts_and_subdomains_only() {
        assert!(matches_no_proxy("anything.test", "*"));
        assert!(matches_no_proxy("example.com", "foo.test, example.com"));
        assert!(matches_no_proxy("cdn.example.com", ".example.com"));
        assert!(!matches_no_proxy("badexample.com", "example.com"));
        assert!(!matches_no_proxy("example.com", ""));
    }

    #[test]
    fn windows_proxy_server_formats() {
        let single = parse_windows_proxy_server("proxy.corp:8080", true).unwrap();
        assert_eq!(
            (single.kind, single.host.as_str(), single.port),
            (ProxyKind::Http, "proxy.corp", 8080)
        );

        let split = "http=web.corp:80;https=secure.corp:443;socks=sock.corp:1080";
        let for_https = parse_windows_proxy_server(split, true).unwrap();
        assert_eq!(for_https.host, "secure.corp");
        assert_eq!(for_https.kind, ProxyKind::Http);
        assert_eq!(
            parse_windows_proxy_server(split, false).unwrap().host,
            "web.corp"
        );

        let socks_only = parse_windows_proxy_server("socks=sock.corp:1080", true).unwrap();
        assert_eq!(socks_only.kind, ProxyKind::Socks5);
    }

    #[test]
    fn windows_overrides_support_local_and_wildcards() {
        let overrides = "<local>;*.corp.test;10.*";
        assert!(matches_windows_override("intranet", overrides));
        assert!(matches_windows_override("wiki.corp.test", overrides));
        assert!(matches_windows_override("10.1.2.3", overrides));
        assert!(!matches_windows_override("example.com", overrides));
        assert!(wildcard_match("a*b*c", "axxbyyc"));
        assert!(!wildcard_match("a*b*c", "axxc"));
    }

    #[test]
    fn connect_targets_split_including_ipv6() {
        assert_eq!(
            split_authority("example.com:443").unwrap(),
            ("example.com".into(), 443)
        );
        assert_eq!(split_authority("[::1]:8443").unwrap(), ("::1".into(), 8443));
        assert!(split_authority("example.com").is_err());
    }

    async fn echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = socket.split();
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
        });
        port
    }

    async fn proxy_under_test() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (client, _) = listener.accept().await.unwrap();
            let _ = handle(client).await;
        });
        port
    }

    async fn read_until_blank_line(stream: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while find_head_end(&head).is_none() {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        String::from_utf8(head).unwrap()
    }

    #[tokio::test]
    async fn connect_tunnel_carries_bytes_both_ways() {
        let echo = echo_server().await;
        let proxy = proxy_under_test().await;

        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        let request =
            format!("CONNECT 127.0.0.1:{echo} HTTP/1.1\r\nHost: 127.0.0.1:{echo}\r\n\r\n");
        client.write_all(request.as_bytes()).await.unwrap();
        let reply = read_until_blank_line(&mut client).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    }

    #[tokio::test]
    async fn unreachable_target_answers_502_instead_of_hanging() {
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let proxy = proxy_under_test().await;

        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        let request = format!("CONNECT 127.0.0.1:{closed} HTTP/1.1\r\n\r\n");
        client.write_all(request.as_bytes()).await.unwrap();
        let reply = read_until_blank_line(&mut client).await;
        assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");
    }

    #[tokio::test]
    async fn plain_http_is_rewritten_to_origin_form_with_connection_close() {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        let received = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let head = read_until_blank_line(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            head
        });
        let proxy = proxy_under_test().await;

        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        let request = format!(
            "GET http://127.0.0.1:{origin_port}/media/a.jpg?w=1 HTTP/1.1\r\n\
             Host: 127.0.0.1:{origin_port}\r\n\
             Proxy-Connection: keep-alive\r\n\
             Connection: keep-alive\r\n\
             Accept: image/*\r\n\r\n"
        );
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("ok"), "{response}");

        let head = received.await.unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with("get /media/a.jpg?w=1 http/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("accept: image/*"), "{head}");
        assert!(head.contains("connection: close"), "{head}");
        assert!(!head.contains("keep-alive"), "{head}");
        assert!(!head.contains("proxy-connection"), "{head}");
    }

    #[tokio::test]
    async fn websocket_upgrade_keeps_its_connection_headers() {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        let received = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let head = read_until_blank_line(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
                .await
                .unwrap();
            head
        });
        let proxy = proxy_under_test().await;

        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        let request = format!(
            "GET http://localhost:{origin_port}/hmr HTTP/1.1\r\n\
             Host: localhost:{origin_port}\r\n\
             Connection: Upgrade\r\n\
             Upgrade: websocket\r\n\r\n"
        );
        client.write_all(request.as_bytes()).await.unwrap();
        let reply = read_until_blank_line(&mut client).await;
        assert!(reply.starts_with("HTTP/1.1 101"), "{reply}");

        let head = received.await.unwrap().to_ascii_lowercase();
        assert!(head.contains("connection: upgrade"), "{head}");
        assert!(head.contains("upgrade: websocket"), "{head}");
        assert!(!head.contains("connection: close"), "{head}");
    }

    #[tokio::test]
    async fn http_upstream_gets_connect_with_basic_auth() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let head = read_until_blank_line(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let (mut reader, mut writer) = socket.split();
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
            head
        });

        let proxy = Upstream::parse(
            &format!("http://127.0.0.1:{upstream_port}"),
            Some(("user".into(), "secret".into())),
        )
        .unwrap();
        let mut tunnel = open_tunnel(
            Some(&proxy),
            "files.example.test",
            443,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        tunnel.write_all(b"tls-bytes").await.unwrap();
        let mut echoed = [0u8; 9];
        tunnel.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"tls-bytes");
        drop(tunnel);

        let head = seen.await.unwrap();
        assert!(
            head.starts_with("CONNECT files.example.test:443 HTTP/1.1\r\n"),
            "{head}"
        );
        let expected = base64::engine::general_purpose::STANDARD.encode("user:secret");
        assert!(
            head.contains(&format!("Proxy-Authorization: Basic {expected}")),
            "{head}"
        );
    }

    #[tokio::test]
    async fn http_upstream_auth_failure_is_reported() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            read_until_blank_line(&mut socket).await;
            let _ = socket
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await;
        });
        let proxy = Upstream::parse(&format!("http://127.0.0.1:{upstream_port}"), None).unwrap();
        let error =
            match open_tunnel(Some(&proxy), "example.test", 443, Duration::from_secs(2)).await {
                Ok(_) => panic!("a 407 must fail the tunnel"),
                Err(error) => error,
            };
        assert!(error.contains("407"), "{error}");
    }

    #[tokio::test]
    async fn socks5_upstream_authenticates_and_sends_the_hostname() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut greeting = [0u8; 4];
            socket.read_exact(&mut greeting).await.unwrap();
            socket.write_all(&[0x05, 0x02]).await.unwrap();

            let mut header = [0u8; 2];
            socket.read_exact(&mut header).await.unwrap();
            let mut username = vec![0u8; header[1] as usize];
            socket.read_exact(&mut username).await.unwrap();
            let mut password_len = [0u8; 1];
            socket.read_exact(&mut password_len).await.unwrap();
            let mut password = vec![0u8; password_len[0] as usize];
            socket.read_exact(&mut password).await.unwrap();
            socket.write_all(&[0x01, 0x00]).await.unwrap();

            let mut request = [0u8; 5];
            socket.read_exact(&mut request).await.unwrap();
            let mut host = vec![0u8; request[4] as usize];
            socket.read_exact(&mut host).await.unwrap();
            let mut port = [0u8; 2];
            socket.read_exact(&mut port).await.unwrap();
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let (mut reader, mut writer) = socket.split();
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
            (
                greeting,
                String::from_utf8(username).unwrap(),
                String::from_utf8(password).unwrap(),
                request[3],
                String::from_utf8(host).unwrap(),
                u16::from_be_bytes(port),
            )
        });

        let proxy = Upstream::parse(
            &format!("socks5://alice:pw@127.0.0.1:{upstream_port}"),
            None,
        )
        .unwrap();
        let mut tunnel = open_tunnel(Some(&proxy), "mega.nz", 443, Duration::from_secs(2))
            .await
            .unwrap();
        tunnel.write_all(b"hi").await.unwrap();
        let mut echoed = [0u8; 2];
        tunnel.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hi");
        drop(tunnel);

        let (greeting, username, password, address_type, host, port) = seen.await.unwrap();
        assert_eq!(greeting, [0x05, 0x02, 0x00, 0x02]);
        assert_eq!((username.as_str(), password.as_str()), ("alice", "pw"));
        assert_eq!(address_type, 0x03);
        assert_eq!((host.as_str(), port), ("mega.nz", 443));
    }
}
