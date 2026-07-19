//! Shared REST API server utilities.
//!
//! Provides a minimal HTTP server for tool-specific endpoints.
//! Used by dcpwizard and imfwizard for their respective REST APIs.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

/// Route handler function type.
pub type RouteHandler = Box<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

/// Minimal REST API server configuration.
pub struct RestServer {
    pub bind_address: String,
    pub routes: Vec<(String, String, RouteHandler)>,
}

impl RestServer {
    pub fn new(bind_address: &str) -> Self {
        Self {
            bind_address: bind_address.to_string(),
            routes: Vec::new(),
        }
    }

    /// Register a route handler.
    pub fn route(&mut self, method: &str, path: &str, handler: RouteHandler) {
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

        let (status, body) = self.dispatch(method, path);
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            status = status,
            reason = reason_phrase(status),
            len = body.len(),
            body = body,
        );
        let _ = stream.write_all(response.as_bytes());
    }

    fn dispatch(&self, method: &str, path: &str) -> (u16, String) {
        for (route_method, route_path, handler) in &self.routes {
            if route_method == method && route_path == path {
                return handler(method, path);
            }
        }
        (404, r#"{"error":"not found"}"#.to_string())
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
