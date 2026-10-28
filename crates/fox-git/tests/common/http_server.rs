//! A small HTTP/1.1 server on 127.0.0.1 for the HTTP tests: static files,
//! and git's smart HTTP protocol through `git http-backend` as a CGI
//! program.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

/// A request as the handler sees it.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// A response: status line text, headers and body.
pub struct Response {
    pub status: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: status.to_owned(),
            headers: Vec::new(),
            body: body.into(),
        }
    }
}

type Handler = dyn Fn(Request) -> Response + Send + Sync;

/// Serves `handler` until the test process ends, and returns the base URL.
pub fn serve(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handler: Arc<Handler> = Arc::new(handler);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let handler = handler.clone();
            thread::spawn(move || answer(stream.unwrap(), &*handler));
        }
    });
    format!("http://{address}")
}

fn answer(stream: TcpStream, handler: &Handler) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap().to_owned();
    let target = parts.next().unwrap().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .map_or(0, |length| length.parse().unwrap());
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let response = handler(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body,
    });
    let mut stream = stream;
    write!(stream, "HTTP/1.1 {}\r\n", response.status).unwrap();
    for (name, value) in &response.headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    )
    .unwrap();
    stream.write_all(&response.body).unwrap();
}

/// Serves the files under `root`, 404 for anything else.
pub fn serve_files(root: &Path) -> String {
    let root = root.to_path_buf();
    serve(
        move |request| match std::fs::read(root.join(&request.path[1..])) {
            Ok(body) => Response::new("200 OK", body),
            Err(_) => Response::new("404 Not Found", ""),
        },
    )
}

/// Serves the repositories under `root` with `git http-backend`.
pub fn serve_git(root: &Path) -> String {
    let root: PathBuf = root.to_path_buf();
    serve(move |request| git_http_backend(&root, request))
}

fn git_http_backend(root: &Path, request: Request) -> Response {
    let header = |name: &str| request.headers.get(name).cloned().unwrap_or_default();
    let mut child = Command::new("git")
        .arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &request.method)
        .env("PATH_INFO", &request.path)
        .env("QUERY_STRING", &request.query)
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .env("GIT_PROTOCOL", header("git-protocol"))
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("git http-backend runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&request.body)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let split = output
        .stdout
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("CGI headers end");
    let head = String::from_utf8_lossy(&output.stdout[..split]).into_owned();
    let mut response = Response::new("200 OK", output.stdout[split + 4..].to_vec());
    for line in head.lines() {
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("status") {
            response.status = value.trim().to_owned();
        } else {
            response
                .headers
                .push((name.to_owned(), value.trim().to_owned()));
        }
    }
    response
}
