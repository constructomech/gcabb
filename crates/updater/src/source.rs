//! Discovering published releases.
//!
//! Release discovery is deliberately separated from trust. This module decides
//! only *which bytes to fetch*; whether those bytes may be installed is decided
//! by [`crate::verify`]. That split means an untrusted or compromised discovery
//! response can misdirect a client but cannot make it install anything.

use std::collections::HashMap;
use std::error::Error as _;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::Deserialize;

use crate::verify::DetachedSignature;

/// Asset name carrying the signed manifest within a GitHub Release.
pub const MANIFEST_ASSET: &str = "update-manifest.json";
/// Asset name carrying the manifest's detached signature.
pub const SIGNATURE_ASSET: &str = "update-manifest.json.sig";

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("network request failed: {0}")]
    Transport(String),
    #[error("release feed could not be parsed: {0}")]
    MalformedFeed(String),
    #[error("release {tag} is missing its {asset} asset")]
    MissingAsset { tag: String, asset: String },
    #[error("no published release was found for this channel")]
    NoReleases,
}

/// Boxed future returned by the updater's async traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Reports bytes received and, when the server declares it, the total expected.
pub type ProgressCallback = std::sync::Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// Minimal HTTP surface the updater needs.
///
/// Behind a trait so update logic can be tested end to end without a network,
/// which is what makes the failure paths (corrupt download, forged signature,
/// truncated artifact) practical to cover.
pub trait HttpClient: Send + Sync {
    /// Fetches a URL, returning the response body.
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, SourceError>>;

    /// Fetches a URL, reporting progress as the body arrives.
    ///
    /// Defaults to a non-streaming fetch so simple clients need only implement
    /// [`HttpClient::get`].
    fn download<'a>(
        &'a self,
        url: &'a str,
        progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
        Box::pin(async move {
            let bytes = self.get(url).await?;
            let total = bytes.len() as u64;
            progress(total, Some(total));
            Ok(bytes)
        })
    }
}

/// A manifest exactly as fetched, with its detached signature.
/// `bytes` is retained verbatim because the signature covers the transmitted
/// bytes; re-serialising a parsed manifest would invalidate it.
#[derive(Clone, Debug)]
pub struct SignedManifest {
    pub bytes: Vec<u8>,
    pub signature: DetachedSignature,
}
/// Shared clients satisfy the trait too, so one client can back both release
/// discovery and artifact downloads without being cloned into two connections.
impl<T: HttpClient + ?Sized> HttpClient for std::sync::Arc<T> {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
        (**self).get(url)
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
        (**self).download(url, progress)
    }
}

/// A place published releases can be discovered.
pub trait ReleaseSource: Send + Sync {
    /// Returns candidate releases, newest first.
    fn candidates(
        &self,
        include_prereleases: bool,
    ) -> BoxFuture<'_, Result<Vec<ReleaseCandidate>, SourceError>>;

    /// Fetches the signed manifest for a candidate.
    fn manifest<'a>(
        &'a self,
        candidate: &'a ReleaseCandidate,
    ) -> BoxFuture<'a, Result<SignedManifest, SourceError>>;
}

/// A release found by discovery, before any trust decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseCandidate {
    pub tag: String,
    pub version: Version,
    pub prerelease: bool,
    pub manifest_url: String,
    pub signature_url: String,
}

/// GitHub Releases as the publication surface.
pub struct GitHubReleaseSource {
    client: Box<dyn HttpClient>,
    api_base: String,
    repository: String,
}

impl GitHubReleaseSource {
    #[must_use]
    pub fn new(client: Box<dyn HttpClient>, repository: impl Into<String>) -> Self {
        Self {
            client,
            api_base: "https://api.github.com".to_owned(),
            repository: repository.into(),
        }
    }

    /// Overrides the API base, for testing against a local stub.
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Parses a release tag of the form `v1.2.3` into a version.
#[must_use]
pub fn version_from_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

impl ReleaseSource for GitHubReleaseSource {
    fn candidates(
        &self,
        include_prereleases: bool,
    ) -> BoxFuture<'_, Result<Vec<ReleaseCandidate>, SourceError>> {
        Box::pin(async move {
            let url = format!(
                "{}/repos/{}/releases?per_page=10",
                self.api_base, self.repository
            );
            let body = self.client.get(&url).await?;
            let releases: Vec<GitHubRelease> = serde_json::from_slice(&body)
                .map_err(|error| SourceError::MalformedFeed(error.to_string()))?;

            let mut candidates = Vec::new();
            for release in releases {
                if release.draft || (release.prerelease && !include_prereleases) {
                    continue;
                }
                let Some(version) = version_from_tag(&release.tag_name) else {
                    continue;
                };
                let asset = |name: &str| {
                    release
                        .assets
                        .iter()
                        .find(|asset| asset.name == name)
                        .map(|asset| asset.browser_download_url.clone())
                };
                // A release without signed metadata is skipped rather than
                // treated as an error: unrelated tags may exist in the repo.
                let (Some(manifest_url), Some(signature_url)) =
                    (asset(MANIFEST_ASSET), asset(SIGNATURE_ASSET))
                else {
                    continue;
                };
                candidates.push(ReleaseCandidate {
                    tag: release.tag_name,
                    version,
                    prerelease: release.prerelease,
                    manifest_url,
                    signature_url,
                });
            }
            candidates.sort_by(|left, right| right.version.cmp(&left.version));
            Ok(candidates)
        })
    }

    fn manifest<'a>(
        &'a self,
        candidate: &'a ReleaseCandidate,
    ) -> BoxFuture<'a, Result<SignedManifest, SourceError>> {
        Box::pin(async move {
            let bytes = self.client.get(&candidate.manifest_url).await?;
            let signature_bytes = self.client.get(&candidate.signature_url).await?;
            let signature: DetachedSignature = serde_json::from_slice(&signature_bytes)
                .map_err(|error| SourceError::MalformedFeed(error.to_string()))?;
            Ok(SignedManifest { bytes, signature })
        })
    }
}

/// `reqwest`-backed HTTP client.
pub struct ReqwestClient {
    inner: reqwest::Client,
    cache: Mutex<HashMap<String, CachedResponse>>,
}

#[derive(Clone)]
struct CachedResponse {
    etag: String,
    body: Vec<u8>,
}

const MAX_REQUEST_ATTEMPTS: usize = 3;
#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const READ_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const READ_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
#[cfg(test)]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(1);
const MAX_INLINE_RETRY_DELAY: Duration = Duration::from_mins(1);

impl ReqwestClient {
    /// Builds a client with a GCABB user agent, which the GitHub API requires.
    ///
    /// # Errors
    ///
    /// Returns an error when the TLS backend cannot be initialised.
    pub fn new() -> Result<Self, SourceError> {
        let inner = reqwest::Client::builder()
            .user_agent(concat!("gcabb/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|error| SourceError::Transport(error.to_string()))?;
        Ok(Self {
            inner,
            cache: Mutex::new(HashMap::new()),
        })
    }

    async fn response(
        &self,
        url: &str,
        accept_github_json: bool,
        etag: Option<&str>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut request = self.inner.get(url);
        if accept_github_json {
            request = request.header("Accept", "application/vnd.github+json");
        }
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        request.send().await
    }

    fn cached(&self, url: &str) -> Option<CachedResponse> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(url)
            .cloned()
    }

    fn cache(&self, url: &str, etag: String, body: Vec<u8>) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(url.to_owned(), CachedResponse { etag, body });
    }

    fn clear_cached(&self, url: &str) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(url);
    }
}

fn retryable(error: &reqwest::Error) -> bool {
    if error.is_builder() || error.is_redirect() {
        return false;
    }
    if let Some(status) = error.status() {
        return status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error();
    }
    error.is_connect()
        || error.is_timeout()
        || error.is_request()
        || error.is_body()
        || error.is_decode()
}

fn retryable_status(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || (status == reqwest::StatusCode::FORBIDDEN
            && (headers.contains_key(reqwest::header::RETRY_AFTER)
                || headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0")))
}

fn transport(error: &reqwest::Error) -> SourceError {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(source) = cause {
        let detail = source.to_string();
        if !message.contains(&detail) {
            let _ = write!(message, ": {detail}");
        }
        cause = source.source();
    }
    SourceError::Transport(message)
}

fn retry_delay_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(seconds) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }

    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(reset))?
        .duration_since(SystemTime::now())
        .ok()
}

fn inline_retry_delay(attempt: usize, server_delay: Option<Duration>) -> Option<Duration> {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    let multiplier = 2_u32.saturating_pow(exponent);
    let delay = server_delay.unwrap_or(RETRY_BASE_DELAY * multiplier);
    (delay <= MAX_INLINE_RETRY_DELAY).then_some(delay)
}

async fn wait_before_retry(
    url: &str,
    attempt: usize,
    error: &reqwest::Error,
    server_delay: Option<Duration>,
) -> bool {
    let Some(delay) = inline_retry_delay(attempt, server_delay) else {
        tracing::warn!(
            url,
            attempt,
            %error,
            "server retry delay is too long for an inline update check"
        );
        return false;
    };
    tracing::warn!(
        url,
        attempt,
        max_attempts = MAX_REQUEST_ATTEMPTS,
        delay_ms = delay.as_millis(),
        %error,
        "transient update request failed; retrying"
    );
    tokio::time::sleep(delay).await;
    true
}

impl HttpClient for ReqwestClient {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
        Box::pin(async move {
            for attempt in 1..=MAX_REQUEST_ATTEMPTS {
                let cached = self.cached(url);
                let response = match self
                    .response(url, true, cached.as_ref().map(|entry| entry.etag.as_str()))
                    .await
                {
                    Ok(response) => response,
                    Err(error) if attempt < MAX_REQUEST_ATTEMPTS && retryable(&error) => {
                        if !wait_before_retry(url, attempt, &error, None).await {
                            return Err(transport(&error));
                        }
                        continue;
                    }
                    Err(error) => return Err(transport(&error)),
                };

                if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                    return cached.map(|entry| entry.body).ok_or_else(|| {
                        SourceError::Transport(format!(
                            "{url} returned 304 Not Modified without a cached response"
                        ))
                    });
                }

                let status = response.status();
                let server_delay = retry_delay_from_headers(response.headers());
                let should_retry = retryable_status(status, response.headers());
                let etag = response
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let response = match response.error_for_status() {
                    Ok(response) => response,
                    Err(error) if attempt < MAX_REQUEST_ATTEMPTS && should_retry => {
                        if !wait_before_retry(url, attempt, &error, server_delay).await {
                            return Err(transport(&error));
                        }
                        continue;
                    }
                    Err(error) => return Err(transport(&error)),
                };
                match response.bytes().await {
                    Ok(bytes) => {
                        let bytes = bytes.to_vec();
                        if let Some(etag) = etag {
                            self.cache(url, etag, bytes.clone());
                        } else {
                            self.clear_cached(url);
                        }
                        return Ok(bytes);
                    }
                    Err(error) if attempt < MAX_REQUEST_ATTEMPTS && retryable(&error) => {
                        if !wait_before_retry(url, attempt, &error, None).await {
                            return Err(transport(&error));
                        }
                    }
                    Err(error) => return Err(transport(&error)),
                }
            }
            unreachable!("the request attempt loop always returns")
        })
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
        Box::pin(async move {
            for attempt in 1..=MAX_REQUEST_ATTEMPTS {
                let mut response = match self.response(url, false, None).await {
                    Ok(response) => response,
                    Err(error) if attempt < MAX_REQUEST_ATTEMPTS && retryable(&error) => {
                        if !wait_before_retry(url, attempt, &error, None).await {
                            return Err(transport(&error));
                        }
                        continue;
                    }
                    Err(error) => return Err(transport(&error)),
                };
                let status = response.status();
                let server_delay = retry_delay_from_headers(response.headers());
                let should_retry = retryable_status(status, response.headers());
                response = match response.error_for_status() {
                    Ok(response) => response,
                    Err(error) if attempt < MAX_REQUEST_ATTEMPTS && should_retry => {
                        if !wait_before_retry(url, attempt, &error, server_delay).await {
                            return Err(transport(&error));
                        }
                        continue;
                    }
                    Err(error) => return Err(transport(&error)),
                };

                let total = response.content_length();
                let mut bytes =
                    Vec::with_capacity(usize::try_from(total.unwrap_or(0)).unwrap_or_default());
                progress(0, total);
                let result = async {
                    while let Some(chunk) = response.chunk().await? {
                        bytes.extend_from_slice(&chunk);
                        progress(bytes.len() as u64, total);
                    }
                    Ok(bytes)
                }
                .await;
                match result {
                    Ok(bytes) => return Ok(bytes),
                    Err(error) if attempt < MAX_REQUEST_ATTEMPTS && retryable(&error) => {
                        if !wait_before_retry(url, attempt, &error, None).await {
                            return Err(transport(&error));
                        }
                    }
                    Err(error) => return Err(transport(&error)),
                }
            }
            unreachable!("the request attempt loop always returns")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    use super::{
        BoxFuture, GitHubReleaseSource, HttpClient, ProgressCallback, ReleaseSource, ReqwestClient,
        SourceError, inline_retry_delay, retry_delay_from_headers, version_from_tag,
    };

    #[derive(Default)]
    pub struct StubHttp {
        responses: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl StubHttp {
        fn with(responses: &[(&str, &[u8])]) -> Self {
            let map = responses
                .iter()
                .map(|(url, body)| ((*url).to_owned(), (*body).to_vec()))
                .collect();
            Self {
                responses: Mutex::new(map),
            }
        }
    }

    impl HttpClient for StubHttp {
        fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
            let found = self.responses.lock().expect("stub lock").get(url).cloned();
            Box::pin(async move {
                found.ok_or_else(|| SourceError::Transport(format!("no stub for {url}")))
            })
        }
    }

    fn serve(responses: Vec<&'static [u8]>) -> (String, thread::JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept()?;
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request)?;
                stream.write_all(response)?;
            }
            Ok(())
        });
        (format!("http://{address}/update"), server)
    }

    fn serve_conditional() -> (String, thread::JoinHandle<std::io::Result<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept()?;
            let mut request = [0_u8; 2048];
            let _ = first.read(&mut request)?;
            first.write_all(
                b"HTTP/1.1 200 OK\r\nETag: \"gcabb-test\"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )?;

            let (mut second, _) = listener.accept()?;
            let length = second.read(&mut request)?;
            second.write_all(
                b"HTTP/1.1 304 Not Modified\r\nETag: \"gcabb-test\"\r\nConnection: close\r\n\r\n",
            )?;
            Ok(String::from_utf8_lossy(&request[..length]).into_owned())
        });
        (format!("http://{address}/update"), server)
    }

    fn serve_stalled_then(
        partial: &'static [u8],
        retried: &'static [u8],
    ) -> (String, thread::JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut stalled_stream, _) = listener.accept()?;
            let mut request = [0_u8; 2048];
            let _ = stalled_stream.read(&mut request)?;
            stalled_stream.write_all(partial)?;

            // Accepting the retry is the barrier: the partial response remains
            // open until the client has observed its read timeout and reconnects.
            let (mut retried_stream, _) = listener.accept()?;
            let _ = retried_stream.read(&mut request)?;
            retried_stream.write_all(retried)?;
            Ok(())
        });
        (format!("http://{address}/update"), server)
    }

    #[tokio::test]
    async fn retries_transient_server_errors() {
        let (url, server) = serve(vec![
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        ]);
        let client = ReqwestClient::new().expect("client");

        assert_eq!(client.get(&url).await.expect("retried response"), b"ok");
        server
            .join()
            .expect("server thread")
            .expect("serve responses");
    }

    #[tokio::test]
    async fn conditional_get_returns_cached_body_after_not_modified() {
        let (url, server) = serve_conditional();
        let client = ReqwestClient::new().expect("client");

        assert_eq!(client.get(&url).await.expect("first response"), b"ok");
        assert_eq!(client.get(&url).await.expect("cached response"), b"ok");

        let request = server
            .join()
            .expect("server thread")
            .expect("serve conditional responses")
            .to_ascii_lowercase();
        assert!(request.contains("if-none-match: \"gcabb-test\""));
    }

    #[test]
    fn retry_after_header_overrides_backoff() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("17"),
        );

        assert_eq!(
            retry_delay_from_headers(&headers),
            Some(Duration::from_secs(17))
        );
    }

    #[test]
    fn long_rate_limit_delays_are_left_for_the_next_periodic_check() {
        assert_eq!(inline_retry_delay(1, Some(Duration::from_mins(2))), None);
    }

    #[tokio::test]
    async fn retries_a_stalled_download() {
        let (url, server) = serve_stalled_then(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nno",
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        let client = ReqwestClient::new().expect("client");
        let progress: ProgressCallback = std::sync::Arc::new(|_, _| {});

        assert_eq!(
            client
                .download(&url, progress)
                .await
                .expect("retried download"),
            b"hello"
        );
        server
            .join()
            .expect("server thread")
            .expect("serve responses");
    }

    #[tokio::test]
    async fn retries_an_interrupted_download_from_the_beginning() {
        let (url, server) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nno",
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        ]);
        let client = ReqwestClient::new().expect("client");
        let progress: ProgressCallback = std::sync::Arc::new(|_, _| {});

        assert_eq!(
            client
                .download(&url, progress)
                .await
                .expect("retried download"),
            b"hello"
        );
        server
            .join()
            .expect("server thread")
            .expect("serve responses");
    }

    const FEED: &[u8] = br#"[
      {"tag_name":"v0.3.0","draft":false,"prerelease":true,"assets":[
        {"name":"update-manifest.json","browser_download_url":"https://x/0.3.0/m"},
        {"name":"update-manifest.json.sig","browser_download_url":"https://x/0.3.0/s"}]},
      {"tag_name":"v0.2.0","draft":false,"prerelease":false,"assets":[
        {"name":"update-manifest.json","browser_download_url":"https://x/0.2.0/m"},
        {"name":"update-manifest.json.sig","browser_download_url":"https://x/0.2.0/s"}]},
      {"tag_name":"v0.9.0","draft":true,"prerelease":false,"assets":[
        {"name":"update-manifest.json","browser_download_url":"https://x/0.9.0/m"},
        {"name":"update-manifest.json.sig","browser_download_url":"https://x/0.9.0/s"}]},
      {"tag_name":"nightly","draft":false,"prerelease":false,"assets":[]}
    ]"#;

    fn source(include: bool) -> (GitHubReleaseSource, bool) {
        let http = StubHttp::with(&[(
            "https://api.test/repos/constructomech/gcabb/releases?per_page=10",
            FEED,
        )]);
        (
            GitHubReleaseSource::new(Box::new(http), "constructomech/gcabb")
                .with_api_base("https://api.test"),
            include,
        )
    }

    #[tokio::test]
    async fn candidates_are_returned_newest_first() {
        let (source, include) = source(true);
        let candidates = source.candidates(include).await.unwrap();
        let tags: Vec<_> = candidates.iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, vec!["v0.3.0", "v0.2.0"]);
    }

    #[tokio::test]
    async fn draft_releases_are_never_offered() {
        let (source, include) = source(true);
        let candidates = source.candidates(include).await.unwrap();
        assert!(candidates.iter().all(|c| c.tag != "v0.9.0"));
    }

    #[tokio::test]
    async fn prereleases_are_excluded_for_stable_clients() {
        let (source, _) = source(false);
        let candidates = source.candidates(false).await.unwrap();
        let tags: Vec<_> = candidates.iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, vec!["v0.2.0"]);
    }

    #[tokio::test]
    async fn releases_without_signed_metadata_are_skipped() {
        let (source, include) = source(true);
        let candidates = source.candidates(include).await.unwrap();
        assert!(candidates.iter().all(|c| c.tag != "nightly"));
    }

    #[test]
    fn tags_parse_with_and_without_the_v_prefix() {
        assert_eq!(version_from_tag("v1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(version_from_tag("1.2.3").unwrap().to_string(), "1.2.3");
        assert!(version_from_tag("nightly").is_none());
    }
}
