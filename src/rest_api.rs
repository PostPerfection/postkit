//! Shared REST API server utilities.
//!
//! Provides a minimal HTTP server for tool-specific endpoints.
//! Used by dcpwizard and imfwizard for their respective REST APIs.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

pub const JSON_CONTENT_TYPE: &str = "application/json";

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

/// Route handler function type.
pub type RouteHandler = Box<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

pub type ContentTypeRouteHandler = Box<dyn Fn(&str, &str) -> RouteResponse + Send + Sync>;

/// Minimal REST API server configuration.
pub struct RestServer {
    pub bind_address: String,
    pub routes: Vec<(String, String, ContentTypeRouteHandler)>,
}

impl RestServer {
    pub fn new(bind_address: &str) -> Self {
        Self {
            bind_address: bind_address.to_string(),
            routes: Vec::new(),
        }
    }

    /// Register a route handler whose body is JSON.
    pub fn route(&mut self, method: &str, path: &str, handler: RouteHandler) {
        self.route_with_content_type(
            method,
            path,
            Box::new(move |request_method, request_path| {
                let (status, body) = handler(request_method, request_path);
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
        self.routes
            .push((method.to_string(), path.to_string(), handler));
    }

    /// Start the server (blocking).
    pub fn start(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(&self.bind_address)?;
        tracing::info!("REST API listening on {}", self.bind_address);

        for stream in listener.incoming().flatten() {
            self.handle_connection(stream);
        }
        Ok(())
    }

    fn handle_connection(&self, mut stream: TcpStream) {
        let reader = BufReader::new(&stream);
        let request_line = match reader.lines().next() {
            Some(Ok(line)) => line,
            _ => return,
        };

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0];
        let path = parts[1];

        let answer = self.dispatch(method, path);
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

    fn dispatch(&self, method: &str, path: &str) -> RouteResponse {
        for (route_method, route_path, handler) in &self.routes {
            if route_method == method && route_path == path {
                return handler(method, path);
            }
        }
        RouteResponse::json(404, r#"{"error":"not found"}"#.to_string())
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

    fn test_server() -> RestServer {
        let mut server = RestServer::new("127.0.0.1:0");
        server.route(
            "GET",
            "/api/thing",
            Box::new(|_method, _path| (200, r#"{"thing":1}"#.to_string())),
        );
        server.route_with_content_type(
            "GET",
            "/",
            Box::new(|_method, _path| RouteResponse {
                status: 200,
                content_type: HTML_CONTENT_TYPE,
                body: "<html></html>".to_string(),
            }),
        );
        server
    }

    #[test]
    fn a_plain_route_answers_json() {
        let answer = test_server().dispatch("GET", "/api/thing");
        assert_eq!(answer.status, 200);
        assert_eq!(answer.content_type, JSON_CONTENT_TYPE);
        assert_eq!(answer.body, r#"{"thing":1}"#);
    }

    #[test]
    fn a_content_type_route_keeps_its_own_type() {
        let answer = test_server().dispatch("GET", "/");
        assert_eq!(answer.content_type, HTML_CONTENT_TYPE);
        assert_eq!(answer.body, "<html></html>");
    }

    #[test]
    fn an_unknown_path_is_a_json_404() {
        let answer = test_server().dispatch("GET", "/nope");
        assert_eq!(answer.status, 404);
        assert_eq!(answer.content_type, JSON_CONTENT_TYPE);
    }

    #[test]
    fn the_written_response_carries_the_content_type() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut server = test_server();
        server.bind_address = format!("127.0.0.1:{port}");
        std::thread::spawn(move || server.start());

        let response = get(port, "/");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("Content-Type: text/html; charset=utf-8"),
            "{response}"
        );
        assert!(response.ends_with("<html></html>"), "{response}");

        let response = get(port, "/api/thing");
        assert!(
            response.contains("Content-Type: application/json"),
            "{response}"
        );
    }

    fn get(port: u16, path: &str) -> String {
        use std::io::Read;
        for _ in 0..100 {
            let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            };
            stream
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            return response;
        }
        panic!("server never answered on port {port}");
    }
}
