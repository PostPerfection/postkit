//! Shared REST API server utilities.
//!
//! Provides a minimal HTTP server for tool-specific endpoints.
//! Used by dcpwizard and imfwizard for their respective REST APIs.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

pub const JSON_CONTENT_TYPE: &str = "application/json";

// a body longer than this is refused with 413 instead of being read into memory
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

const MAX_HEADER_LINE_BYTES: u64 = 8 * 1024;
const MAX_HEADERS: usize = 100;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

pub struct RouteResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl RouteResponse {
    pub fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: JSON_CONTENT_TYPE,
            body,
        }
    }
}

// one parsed request; header names are lowercased
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(header, _)| *header == wanted)
            .map(|(_, value)| value.as_str())
    }
}

/// Route handler function type.
pub type RouteHandler = Box<dyn Fn(&Request) -> (u16, String) + Send + Sync>;

pub type ContentTypeRouteHandler = Box<dyn Fn(&Request) -> RouteResponse + Send + Sync>;

// takes the one path segment after the registered prefix
pub type ParameterRouteHandler = Box<dyn Fn(&Request, &str) -> (u16, String) + Send + Sync>;

enum RouteAction {
    Whole(ContentTypeRouteHandler),
    Parameter(ParameterRouteHandler),
}

struct Route {
    method: String,
    path: String,
    action: RouteAction,
}

struct ApiKey {
    key: String,
    exempt_paths: Vec<String>,
}

impl ApiKey {
    fn allows(&self, request: &Request) -> bool {
        if self.exempt_paths.contains(&request.path) {
            return true;
        }
        let presented = request
            .header("x-api-key")
            .or_else(|| request.header("authorization").and_then(bearer_token));
        presented
            .is_some_and(|presented| constant_time_eq(presented.as_bytes(), self.key.as_bytes()))
    }
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then(|| token.trim())
}

/// Compare two secrets without stopping at the first differing byte. The length
/// is not hidden.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left_byte, right_byte) in left.iter().zip(right) {
        difference |= left_byte ^ right_byte;
    }
    std::hint::black_box(difference) == 0
}

/// Minimal REST API server configuration.
pub struct RestServer {
    pub bind_address: String,
    routes: Vec<Route>,
    api_key: Option<ApiKey>,
}

impl RestServer {
    pub fn new(bind_address: &str) -> Self {
        Self {
            bind_address: bind_address.to_string(),
            routes: Vec::new(),
            api_key: None,
        }
    }

    /// Register a route handler whose body is JSON.
    pub fn route(&mut self, method: &str, path: &str, handler: RouteHandler) {
        self.route_with_content_type(
            method,
            path,
            Box::new(move |request| {
                let (status, body) = handler(request);
                RouteResponse::json(status, body)
            }),
        );
    }

    pub fn route_with_content_type(
        &mut self,
        method: &str,
        path: &str,
        handler: ContentTypeRouteHandler,
    ) {
        self.routes.push(Route {
            method: method.to_string(),
            path: path.to_string(),
            action: RouteAction::Whole(handler),
        });
    }

    /// Register a route ending in one path segment: the prefix `/api/v1/jobs/`
    /// matches `/api/v1/jobs/7` and hands the handler `7`.
    pub fn route_with_parameter(
        &mut self,
        method: &str,
        prefix: &str,
        handler: ParameterRouteHandler,
    ) {
        assert!(
            prefix.ends_with('/'),
            "a parameter route prefix must end in a slash, got {prefix:?}"
        );
        self.routes.push(Route {
            method: method.to_string(),
            path: prefix.to_string(),
            action: RouteAction::Parameter(handler),
        });
    }

    /// Require `key` in `X-Api-Key` or `Authorization: Bearer` on every request
    /// but the ones whose path is listed in `exempt_paths`.
    pub fn require_api_key(&mut self, key: &str, exempt_paths: &[&str]) {
        self.api_key = Some(ApiKey {
            key: key.to_string(),
            exempt_paths: exempt_paths.iter().map(|path| path.to_string()).collect(),
        });
    }

    /// Bind the listener without serving, so a caller can read the address it
    /// got when the port was 0.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        TcpListener::bind(&self.bind_address)
    }

    pub fn serve_forever(&self, listener: TcpListener) -> std::io::Result<()> {
        std::thread::scope(|scope| {
            for stream in listener.incoming().flatten() {
                scope.spawn(move || self.handle_connection(stream));
            }
        });
        Ok(())
    }

    /// Start the server (blocking).
    pub fn start(&self) -> std::io::Result<()> {
        let listener = self.bind()?;
        tracing::info!("REST API listening on {}", self.bind_address);
        self.serve_forever(listener)
    }

    fn handle_connection(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

        let mut reader = BufReader::new(&stream);
        let answer = match read_request(&mut reader) {
            Ok(Some(request)) => self.answer(&request),
            Ok(None) => return,
            Err(status) => RouteResponse::json(status, error_body(status)),
        };

        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            status = answer.status,
            reason = reason_phrase(answer.status),
            content_type = answer.content_type,
            len = answer.body.len(),
            body = answer.body,
        );
        let _ = stream.write_all(response.as_bytes());
    }

    // the key is checked before routing, so an unknown path tells an unauthorized
    // caller nothing
    fn answer(&self, request: &Request) -> RouteResponse {
        if let Some(api_key) = &self.api_key
            && !api_key.allows(request)
        {
            return RouteResponse::json(401, error_body(401));
        }
        self.dispatch(request)
    }

    fn dispatch(&self, request: &Request) -> RouteResponse {
        for route in &self.routes {
            if route.method != request.method {
                continue;
            }
            match &route.action {
                RouteAction::Whole(handler) => {
                    if route.path == request.path {
                        return handler(request);
                    }
                }
                RouteAction::Parameter(handler) => {
                    if let Some(parameter) = path_parameter(&route.path, &request.path) {
                        let (status, body) = handler(request, parameter);
                        return RouteResponse::json(status, body);
                    }
                }
            }
        }
        RouteResponse::json(404, error_body(404))
    }
}

fn path_parameter<'a>(prefix: &str, path: &'a str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    (!rest.is_empty() && !rest.contains('/')).then_some(rest)
}

fn read_request(reader: &mut impl BufRead) -> Result<Option<Request>, u16> {
    let Some(request_line) = read_line(reader)? else {
        return Ok(None);
    };
    if request_line.is_empty() {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(400);
    };
    let path = target.split('?').next().unwrap_or(target).to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        // EOF before the blank line means the head never arrived
        let Some(line) = read_line(reader)? else {
            return Err(400);
        };
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(400);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(400);
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    // chunked bodies are not decoded here, so refuse rather than read an empty one
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(400);
    }
    let lengths: Vec<&String> = headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .map(|(_, value)| value)
        .collect();
    if lengths.len() > 1 {
        return Err(400);
    }
    let length = match lengths.first() {
        Some(value) => value.parse::<usize>().map_err(|_| 400u16)?,
        None => 0,
    };
    if length > MAX_BODY_BYTES {
        return Err(413);
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|_| 400u16)?;
    let body = String::from_utf8(body).map_err(|_| 400u16)?;

    Ok(Some(Request {
        method: method.to_string(),
        path,
        headers,
        body,
    }))
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, u16> {
    let mut line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_HEADER_LINE_BYTES)
        .read_line(&mut line)
        .map_err(|_| 400u16)?;
    if read == 0 {
        return Ok(None);
    }
    // no newline within the cap: the line is over-long or the client stopped
    if !line.ends_with('\n') {
        return Err(400);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

fn error_body(status: u16) -> String {
    format!(
        r#"{{"error":"{}"}}"#,
        reason_phrase(status).to_ascii_lowercase()
    )
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
    const API_KEY: &str = "correct-horse-battery-staple";

    fn test_server() -> RestServer {
        let mut server = RestServer::new("127.0.0.1:0");
        server.route(
            "GET",
            "/api/thing",
            Box::new(|_request| (200, r#"{"thing":1}"#.to_string())),
        );
        server.route_with_content_type(
            "GET",
            "/",
            Box::new(|_request| RouteResponse {
                status: 200,
                content_type: HTML_CONTENT_TYPE,
                body: "<html></html>".to_string(),
            }),
        );
        // echoes what the parser saw, so a test can check the body and a header
        server.route(
            "POST",
            "/api/echo",
            Box::new(|request| {
                let agent = request.header("X-Agent").unwrap_or("none");
                (
                    200,
                    serde_json::json!({
                        "length": request.body.len(),
                        "body": request.body,
                        "agent": agent,
                    })
                    .to_string(),
                )
            }),
        );
        server.route_with_parameter(
            "GET",
            "/api/thing/",
            Box::new(|_request, parameter| (200, format!(r#"{{"id":"{parameter}"}}"#))),
        );
        server
    }

    fn serve(server: RestServer) -> SocketAddr {
        let listener = server.bind().unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || server.serve_forever(listener).unwrap());
        address
    }

    fn send(address: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn get(address: SocketAddr, path: &str) -> String {
        send(
            address,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        )
    }

    #[test]
    fn a_plain_route_answers_json() {
        let address = serve(test_server());
        let response = get(address, "/api/thing");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("Content-Type: application/json"),
            "{response}"
        );
        assert!(response.ends_with(r#"{"thing":1}"#), "{response}");
    }

    #[test]
    fn a_content_type_route_keeps_its_own_type() {
        let address = serve(test_server());
        let response = get(address, "/");
        assert!(
            response.contains("Content-Type: text/html; charset=utf-8"),
            "{response}"
        );
        assert!(response.ends_with("<html></html>"), "{response}");
    }

    #[test]
    fn an_unknown_path_is_a_json_404() {
        let address = serve(test_server());
        let response = get(address, "/nope");
        assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
        assert!(
            response.contains("Content-Type: application/json"),
            "{response}"
        );
    }

    #[test]
    fn a_route_matches_its_whole_path_only() {
        let address = serve(test_server());
        assert!(
            get(address, "/api/thingXYZ").starts_with("HTTP/1.1 404"),
            "a longer path must not reach the /api/thing handler"
        );
        assert!(
            get(address, "/api/thing/7").contains(r#"{"id":"7"}"#),
            "the parameter route takes the segment after the prefix"
        );
        assert!(
            get(address, "/api/thing/7/8").starts_with("HTTP/1.1 404"),
            "a second segment is not a parameter"
        );
    }

    #[test]
    fn a_body_split_across_two_writes_arrives_whole() {
        let address = serve(test_server());
        let body = "x".repeat(4096);
        let head = format!(
            "POST /api/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let (first, second) = body.split_at(2000);

        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(format!("{head}{first}").as_bytes())
            .unwrap();
        stream.flush().unwrap();
        // the second segment arrives after the server has already read once
        std::thread::sleep(Duration::from_millis(150));
        stream.write_all(second.as_bytes()).unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.contains(&format!(r#""length":{}"#, body.len())),
            "{response}"
        );
        assert!(
            response.contains(&body),
            "the whole body must reach the handler"
        );
    }

    #[test]
    fn a_header_is_found_whatever_its_case() {
        let address = serve(test_server());
        let response = send(
            address,
            "POST /api/echo HTTP/1.1\r\nHost: localhost\r\nx-AgEnT: farm\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(response.contains(r#""agent":"farm""#), "{response}");
    }

    #[test]
    fn a_post_without_a_content_length_has_an_empty_body() {
        let address = serve(test_server());
        let response = send(
            address,
            "POST /api/echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(response.contains(r#""length":0"#), "{response}");
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        let address = serve(test_server());
        let response = send(
            address,
            &format!(
                "POST /api/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 413 Payload Too Large"),
            "{response}"
        );
    }

    fn guarded_server() -> RestServer {
        let mut server = test_server();
        server.require_api_key(API_KEY, &["/api/v1/health"]);
        server.route(
            "GET",
            "/api/v1/health",
            Box::new(|_request| (200, r#"{"status":"ok"}"#.to_string())),
        );
        server.route(
            "GET",
            "/api/v1/healthx",
            Box::new(|_request| (200, r#"{"status":"ok"}"#.to_string())),
        );
        server
    }

    #[test]
    fn the_exempt_path_is_the_listed_one_only() {
        let address = serve(guarded_server());
        assert!(
            get(address, "/api/v1/health").starts_with("HTTP/1.1 200 OK"),
            "the listed path answers without a key"
        );
        assert!(
            get(address, "/api/v1/healthx").starts_with("HTTP/1.1 401"),
            "a path that merely contains the exempt one still needs the key"
        );
    }

    #[test]
    fn the_key_counts_only_in_a_header() {
        let address = serve(guarded_server());
        let body = format!(r#"{{"key":"{API_KEY}"}}"#);
        let response = send(
            address,
            &format!(
                "POST /api/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "a key in the body must not authorize the request: {response}"
        );
    }

    #[test]
    fn either_key_header_authorizes() {
        let address = serve(guarded_server());
        for header in [
            format!("X-Api-Key: {API_KEY}"),
            format!("Authorization: Bearer {API_KEY}"),
        ] {
            let response = send(
                address,
                &format!(
                    "GET /api/thing HTTP/1.1\r\nHost: localhost\r\n{header}\r\nConnection: close\r\n\r\n"
                ),
            );
            assert!(
                response.starts_with("HTTP/1.1 200 OK"),
                "{header}: {response}"
            );
        }
        let response = send(
            address,
            &format!(
                "GET /api/thing HTTP/1.1\r\nHost: localhost\r\nX-Api-Key: {API_KEY}x\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    }

    #[test]
    fn constant_time_eq_accepts_only_an_identical_key() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(API_KEY.as_bytes(), API_KEY.as_bytes()));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(!constant_time_eq(b"secret", b""));
    }
}
