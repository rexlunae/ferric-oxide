//! HTTP and HTTPS for fox, on every target including Redox.
//!
//! One [`ureq`] client serves both blob downloads ([`get`]) and git's smart
//! HTTP protocol ([`GitHttp`], plugged into gix as its HTTP backend), so
//! the same code runs on the host and on the target. TLS is rustls with the
//! `ring` provider (aws-lc-rs does not build for Redox) and the system's
//! certificate store through `rustls-platform-verifier`; on Redox that is
//! the `ca-certificates` package. The usual `HTTPS_PROXY`, `ALL_PROXY` and
//! `NO_PROXY` variables are honored.
//!
//! Git requests never follow redirects: a moved repository is an error
//! naming the new location, so the URL that was pinned is the one that is
//! fetched. Blob downloads follow redirects, because their content is
//! checked against its BLAKE3 hash wherever it comes from.

use std::any::Any;
use std::io::{self, BufReader, Cursor, Read, Write};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use gix::protocol::transport::client::blocking_io::http::{
    GetResponse, Http, PostBodyDataKind, PostResponse,
};
use ureq::http::{Response, StatusCode, header};

/// How many redirects a blob download follows.
const BLOB_REDIRECTS: u32 = 10;

/// Why an HTTP request failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The request could not be made or its response read.
    #[error("request to {url} failed")]
    Request {
        /// The URL.
        url: String,
        /// The cause.
        #[source]
        source: Box<ureq::Error>,
    },
    /// The server answered with an error status.
    #[error("{url} answered {status}")]
    Status {
        /// The URL.
        url: String,
        /// The status.
        status: StatusCode,
    },
    /// The server answered a git request with a redirect, which is not
    /// followed.
    #[error("{url} redirects to {location:?}; use the new URL")]
    Redirect {
        /// The URL.
        url: String,
        /// Where the server redirects to, if it said.
        location: Option<String>,
    },
}

fn agent(redirects: u32) -> ureq::Agent {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(provider)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .max_redirects(redirects)
        .user_agent(concat!("fox/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// Checks the response's status, turning anything but success into an
/// error.
fn checked(url: &str, response: Response<ureq::Body>) -> Result<Response<ureq::Body>, HttpError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status.is_redirection() {
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        return Err(HttpError::Redirect {
            url: url.to_owned(),
            location,
        });
    }
    Err(HttpError::Status {
        url: url.to_owned(),
        status,
    })
}

fn request_error(url: &str) -> impl FnOnce(ureq::Error) -> HttpError + '_ {
    move |source| HttpError::Request {
        url: url.to_owned(),
        source: Box::new(source),
    }
}

/// Fetches `url`, following redirects, and returns a reader over the body.
///
/// # Errors
///
/// [`HttpError::Request`] if the request fails and [`HttpError::Status`]
/// for an error status.
pub(crate) fn get(url: &str) -> Result<impl Read + use<>, HttpError> {
    // One agent for every download, so its connection pool is reused.
    static BLOB_AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let response = BLOB_AGENT
        .get_or_init(|| agent(BLOB_REDIRECTS))
        .get(url)
        .call()
        .map_err(request_error(url))?;
    Ok(checked(url, response)?.into_body().into_reader())
}

/// gix's HTTP backend: git's smart HTTP protocol over [`ureq`].
///
/// A `POST`'s body is collected as gix writes it and sent when gix first
/// reads the response. Fetching only posts the client's wants and haves,
/// which are small, so this needs no second thread.
pub(crate) struct GitHttp {
    agent: ureq::Agent,
}

impl GitHttp {
    pub(crate) fn new() -> Self {
        Self { agent: agent(0) }
    }
}

/// Splits git's `Name: value` header lines.
fn header_pairs(headers: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .filter_map(|line| {
            let (name, value) = line.as_ref().split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

/// The response headers as the `Name: value` lines gix reads.
fn header_lines(response: &Response<ureq::Body>) -> Vec<u8> {
    let mut lines = Vec::new();
    for (name, value) in response.headers() {
        lines.extend_from_slice(name.as_str().as_bytes());
        lines.extend_from_slice(b": ");
        lines.extend_from_slice(value.as_bytes());
        lines.push(b'\n');
    }
    lines
}

impl Http for GitHttp {
    type Headers = BufReader<Box<dyn Read>>;
    type ResponseBody = BufReader<Box<dyn Read>>;
    type PostBody = PostBody;

    fn get(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> gix_error::Result<GetResponse<Self::Headers, Self::ResponseBody>> {
        let mut request = self.agent.get(url);
        for (name, value) in header_pairs(headers) {
            request = request.header(name, value);
        }
        let response = request
            .call()
            .map_err(request_error(url))
            .and_then(|response| checked(url, response))
            .map_err(gix_error::Error::from_error)?;
        Ok(GetResponse {
            headers: BufReader::new(Box::new(Cursor::new(header_lines(&response)))),
            body: BufReader::new(Box::new(response.into_body().into_reader())),
        })
    }

    fn post(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        _body: PostBodyDataKind,
    ) -> gix_error::Result<PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>> {
        let exchange = Arc::new(Mutex::new(Exchange::Pending {
            agent: self.agent.clone(),
            url: url.to_owned(),
            headers: header_pairs(headers),
            body: Vec::new(),
        }));
        Ok(PostResponse {
            post_body: PostBody(exchange.clone()),
            headers: BufReader::new(Box::new(ResponsePart {
                exchange: exchange.clone(),
                part: Part::Headers,
            })),
            body: BufReader::new(Box::new(ResponsePart {
                exchange,
                part: Part::Body,
            })),
        })
    }

    fn configure(&mut self, _config: &dyn Any) -> gix_error::Result {
        // gix's http.* options (extra headers, proxies, timeouts) are not
        // used: fox fetches public repositories with its own settings.
        Ok(())
    }
}

/// One `POST`, from collecting its body to reading its response.
enum Exchange {
    /// The body is still being written.
    Pending {
        agent: ureq::Agent,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The request was sent.
    Sent {
        headers: Cursor<Vec<u8>>,
        body: Box<dyn Read + Send>,
    },
    /// The request failed; reads report it.
    Failed(Arc<HttpError>),
}

impl Exchange {
    /// Sends the request if it has not been sent yet.
    fn send(&mut self) {
        let Exchange::Pending {
            agent,
            url,
            headers,
            body,
        } = self
        else {
            return;
        };
        let mut request = agent.post(url.as_str());
        for (name, value) in headers.drain(..) {
            request = request.header(name, value);
        }
        *self = match request
            .send(&body[..])
            .map_err(request_error(url))
            .and_then(|response| checked(url, response))
        {
            Ok(response) => Exchange::Sent {
                headers: Cursor::new(header_lines(&response)),
                body: Box::new(response.into_body().into_reader()),
            },
            Err(error) => Exchange::Failed(Arc::new(error)),
        };
    }
}

fn lock(exchange: &Mutex<Exchange>) -> std::sync::MutexGuard<'_, Exchange> {
    // A panic while holding the lock leaves the exchange as it was, which
    // reads report on like any other state.
    exchange.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Collects a `POST`'s body.
pub(crate) struct PostBody(Arc<Mutex<Exchange>>);

impl Write for PostBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut *lock(&self.0) {
            Exchange::Pending { body, .. } => {
                body.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the request was already sent",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Part {
    Headers,
    Body,
}

/// Reads one part of a `POST`'s response, sending the request first.
struct ResponsePart {
    exchange: Arc<Mutex<Exchange>>,
    part: Part,
}

impl Read for ResponsePart {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut exchange = lock(&self.exchange);
        exchange.send();
        match (&mut *exchange, self.part) {
            (Exchange::Sent { headers, .. }, Part::Headers) => headers.read(buf),
            (Exchange::Sent { body, .. }, Part::Body) => body.read(buf),
            (Exchange::Failed(error), _) => Err(io::Error::other(error.clone())),
            // send() leaves no exchange pending.
            (Exchange::Pending { .. }, _) => Err(io::ErrorKind::Other.into()),
        }
    }
}
