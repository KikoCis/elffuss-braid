// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The hub over HTTP: a blocking client on ureq, with rustls and the ring
//! provider, trusting the operating system's root certificates.
//!
//! Every request carries `Authorization: Bearer <token>` when there is a
//! token, and `User-Agent: braid/<version>`. Redirects are not followed, so
//! the token goes to the hub named and nowhere else. Error answers
//! (`{"error", "detail"}`) become [`HubError::Api`].

use std::cell::OnceCell;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use braid_format::{Kind, ObjectId};
use braid_hubproto::api::{
    DevicePoll, DevicePollRequest, DeviceStart, ErrorBody, LacksRequest, LacksResponse,
    LandRequest, LandResponse, Me, MissingRequest, Refs, RepoCreate, RepoInfo, Root, RootLimits,
    Stored,
};
use braid_hubproto::verify::{VerifyReport, VerifyReset};
use braid_hubproto::{KIND_HEADER, OBJECTS_MEDIA_TYPE, RepoPath, Token};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::http::{HeaderValue, Response};
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Body, RequestBuilder, SendBody};

use super::creds::{TokenSource, authority, is_loopback, redact};
use super::device::DeviceFlow;
use super::remote::{HubError, LACKS_BATCH, MAX_BODY, Remote, shown};
use super::verify::VerifyApi;

/// What braid says it is.
const USER_AGENT: &str = concat!("braid/", env!("CARGO_PKG_VERSION"));

/// The most a JSON answer may hold.
const MAX_JSON: u64 = 64 << 20;

/// The most of an error answer that is read.
const MAX_ERROR: u64 = 1 << 20;

/// A hub, at one URL, with or without a token.
pub(crate) struct HttpHub {
    agent: Agent,
    url: String,
    token: Option<Token>,
    /// Where the token came from, for what a refusal of it says.
    source: TokenSource,
    /// The hub `BRAID_TOKEN` is for, when it is set but not sent to this
    /// one.
    withheld: Option<String>,
    max_body: u64,
    /// The sizes the hub says it takes (`GET /v1`), asked once, by the
    /// first request that needs them; `None` when it did not say.
    announced: OnceCell<Option<RootLimits>>,
}

impl HttpHub {
    /// The same client, saying where its token came from: refusing it then
    /// says what to do about that source.
    pub(crate) fn with_source(mut self, source: TokenSource) -> HttpHub {
        self.source = source;
        self
    }

    /// The same client, knowing that `BRAID_TOKEN` is set for the hub
    /// `meant`, and not sent to this one: a refusal then says so.
    pub(crate) fn with_withheld(mut self, meant: Option<String>) -> HttpHub {
        self.withheld = meant;
        self
    }

    /// A client for the hub at `url`, which [`normalize_url`] made.
    ///
    /// [`normalize_url`]: super::creds::normalize_url
    pub(crate) fn new(url: &str, token: Option<Token>) -> Result<HttpHub, HubError> {
        let mut config = Agent::config_builder()
            // Error answers carry a JSON body: read it rather than fail.
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(600)));
        if is_loopback(authority(url)) {
            // An address on this machine is never reached through a proxy.
            config = config.proxy(None);
        }
        if url.starts_with("https://") {
            config = config.tls_config(tls_config(url)?);
        }
        let source = if token.is_some() {
            TokenSource::File
        } else {
            TokenSource::None
        };
        Ok(HttpHub {
            agent: config.build().new_agent(),
            url: url.to_owned(),
            token,
            source,
            withheld: None,
            max_body: MAX_BODY,
            announced: OnceCell::new(),
        })
    }

    /// The same client, sending upload bodies of at most `max_body` bytes.
    pub(crate) fn with_max_body(mut self, max_body: u64) -> HttpHub {
        self.max_body = max_body.min(MAX_BODY);
        self
    }

    /// Whether requests carry a token.
    pub(crate) fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// The hub's URL.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// One of the hub's repositories.
    pub(crate) fn repo(&self, path: RepoPath) -> HttpRepo<'_> {
        HttpRepo { hub: self, path }
    }

    /// The largest body an upload sends: the size this client was given
    /// (`BRAID_HUB_MAX_BODY`, or the protocol's 256 MiB), or the hub's own
    /// when it takes less, asked once.
    fn upload_limit(&self) -> u64 {
        let hub = self.announced().map(|limits| limits.max_body);
        upload_limit(self.max_body, hub)
    }

    /// The most ids one list of a request holds: the protocol's 10,000, or
    /// fewer when the hub says it takes fewer, asked once.
    fn id_limit(&self) -> usize {
        let hub = self
            .announced()
            .and_then(|limits| usize::try_from(limits.max_ids).ok());
        match hub {
            Some(hub) if hub > 0 => hub.min(LACKS_BATCH),
            _ => LACKS_BATCH,
        }
    }

    /// What `GET /v1` says the hub takes, asked once.
    fn announced(&self) -> Option<&RootLimits> {
        self.announced
            .get_or_init(|| self.root().ok().map(|root| root.limits))
            .as_ref()
    }

    /// `GET /v1`: the protocol version and the sizes the hub takes.
    pub(crate) fn root(&self) -> Result<Root, HubError> {
        self.get_json("/v1")
    }

    /// `POST /v1/login/device`.
    pub(crate) fn device_start(&self) -> Result<DeviceStart, HubError> {
        self.post_json("/v1/login/device", &serde_json::json!({}))
    }

    /// `GET /v1/me`.
    pub(crate) fn me(&self) -> Result<Me, HubError> {
        self.get_json("/v1/me")
    }

    /// `DELETE /v1/tokens/{id}`.
    pub(crate) fn delete_token(&self, id: &str) -> Result<(), HubError> {
        if !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(HubError::Protocol(format!("{id:?} is not a token id")));
        }
        let request = self.authorize(self.agent.delete(self.uri(&format!("/v1/tokens/{id}"))));
        self.check(request.call())?;
        Ok(())
    }

    /// `POST /v1/repos`.
    pub(crate) fn create_repo(&self, request: &RepoCreate) -> Result<RepoInfo, HubError> {
        self.post_json("/v1/repos", request)
    }

    fn uri(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    /// Adds the token, as a header that debug output hides.
    fn authorize<B>(&self, request: RequestBuilder<B>) -> RequestBuilder<B> {
        match &self.token {
            Some(token) => match HeaderValue::from_str(&format!("Bearer {}", token.expose())) {
                Ok(mut value) => {
                    value.set_sensitive(true);
                    request.header("Authorization", value)
                }
                Err(_) => request,
            },
            None => request,
        }
    }

    fn transport(&self, err: ureq::Error) -> HubError {
        HubError::Transport {
            hub: self.url.clone(),
            detail: err.to_string(),
        }
    }

    /// The answer if it is a success; its error otherwise.
    fn check(
        &self,
        result: Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, HubError> {
        let mut response = result.map_err(|err| self.transport(err))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        if (300..400).contains(&status) {
            let to = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("another address");
            return Err(HubError::Protocol(format!(
                "{} redirects to {}; braid follows no redirect, so that the token goes to the hub named and nowhere else: give that hub's URL instead",
                self.url,
                shown(to)
            )));
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok());
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_ERROR)
            .read_to_vec()
            .unwrap_or_default();
        let mut body = match serde_json::from_slice::<ErrorBody>(&bytes) {
            Ok(mut body) => {
                body.retry_after = body.retry_after.or(retry_after);
                body
            }
            Err(_) => {
                let text = excerpt(&bytes);
                ErrorBody {
                    error: format!("http_{status}"),
                    detail: if text.is_empty() {
                        "no detail".to_owned()
                    } else {
                        text
                    },
                    ids: Vec::new(),
                    retry_after,
                    head: None,
                }
            }
        };
        // A hub or a proxy that echoes the request never gets its token
        // into what braid prints.
        body.detail = redact(&body.detail).into_owned();
        if status == 401 && self.token.is_some() {
            return Err(HubError::Refused {
                hub: self.url.clone(),
                source: self.source,
                detail: body.detail,
            });
        }
        if self.token.is_none() && matches!(status, 401 | 403 | 404) {
            match &self.withheld {
                Some(meant) => body.detail.push_str(&format!(
                    " (no token was sent: BRAID_TOKEN is for {meant}, the hub BRAID_HUB_URL names or else the one used by default; set BRAID_HUB_URL={} to send it to this one)",
                    self.url
                )),
                // A private repository hides from whoever is not signed in.
                None if status != 401 => body.detail.push_str(
                    " (no token was sent: sign in with `braid login`, or set BRAID_TOKEN)",
                ),
                None => {}
            }
        }
        Err(HubError::Api { status, body })
    }

    /// The JSON answer of a request to `path`.
    fn read_json<T: DeserializeOwned>(
        &self,
        path: &str,
        mut response: Response<Body>,
    ) -> Result<T, HubError> {
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON)
            .read_to_vec()
            .map_err(|err| self.transport(err))?;
        serde_json::from_slice(&bytes).map_err(|err| {
            HubError::Protocol(format!(
                "{}{path} answered with something that is not the hub protocol ({err}): {}",
                self.url,
                excerpt(&bytes)
            ))
        })
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, HubError> {
        let request = self.authorize(self.agent.get(self.uri(path)));
        let response = self.check(request.call())?;
        self.read_json(path, response)
    }

    fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, HubError> {
        let body = serde_json::to_vec(body).map_err(|err| HubError::Protocol(err.to_string()))?;
        let request = self
            .authorize(self.agent.post(self.uri(path)))
            .header("Content-Type", "application/json");
        let response = self.check(request.send(&body[..]))?;
        self.read_json(path, response)
    }

    /// Any route of the hub: `method` on `path` (`/v1/…`), with the token,
    /// `headers` and `body`, if any. The answer's body when it succeeds,
    /// at most 64 MiB; its error otherwise, as [`HubError::Api`] for an
    /// error answer.
    pub(crate) fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, HubError> {
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(self.uri(path));
        if let Some(token) = &self.token
            && let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        {
            value.set_sensitive(true);
            request = request.header("Authorization", value);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let invalid = |err: ureq::http::Error| {
            HubError::Protocol(format!(
                "{method} {} is not a request braid can send: {err}",
                shown(path)
            ))
        };
        let sent = match body {
            Some(body) => self
                .agent
                .run(request.body(body.to_vec()).map_err(invalid)?),
            None => self.agent.run(request.body(()).map_err(invalid)?),
        };
        let mut response = self.check(sent)?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_JSON)
            .read_to_vec()
            .map_err(|err| self.transport(err))
    }

    /// [`call`](Self::call) with a JSON body, if any, whose answer is JSON.
    #[allow(dead_code)]
    pub(crate) fn call_json<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> Result<T, HubError> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|err| HubError::Protocol(err.to_string()))?;
        let mut all = headers.to_vec();
        if bytes.is_some() {
            all.push(("Content-Type", "application/json"));
        }
        let answer = self.call(method, path, &all, bytes.as_deref())?;
        serde_json::from_slice(&answer).map_err(|err| {
            HubError::Protocol(format!(
                "{}{path} answered with something that is not the hub protocol ({err}): {}",
                self.url,
                excerpt(&answer)
            ))
        })
    }

    /// `POST path` (`/v1/…`) with the JSON `body`, answered as an object
    /// stream, read as it comes, whatever its size: what the `missing`
    /// routes answer to `Accept: application/x-braid-objects`.
    pub(crate) fn post_for_objects(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<Box<dyn Read + '_>, HubError> {
        let body = serde_json::to_vec(body).map_err(|err| HubError::Protocol(err.to_string()))?;
        let http = self
            .authorize(self.agent.post(self.uri(path)))
            .header("Content-Type", "application/json")
            .header("Accept", OBJECTS_MEDIA_TYPE);
        let response = self.check(http.send(&body[..]))?;
        let media = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !media.starts_with(OBJECTS_MEDIA_TYPE) {
            return Err(HubError::Protocol(format!(
                "asked for {OBJECTS_MEDIA_TYPE}, the hub answered {media:?}"
            )));
        }
        Ok(Box::new(response.into_body().into_reader()))
    }

    /// `GET path` (`/v1/…`), with the token, its answer's body written to
    /// `out` as it comes, whatever its size: a raw file. A failure to write
    /// is [`HubError::Io`].
    pub(crate) fn get_to(&self, path: &str, out: &mut dyn std::io::Write) -> Result<(), HubError> {
        let request = self.authorize(self.agent.get(self.uri(path)));
        let response = self.check(request.call())?;
        let mut reader = response.into_body().into_reader();
        let mut buffer = vec![0u8; 64 << 10];
        loop {
            let n = match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err(HubError::Transport {
                        hub: self.url.clone(),
                        detail: err.to_string(),
                    });
                }
            };
            out.write_all(&buffer[..n]).map_err(HubError::Io)?;
        }
    }
}

/// The start of an answer, for an error to quote: at most 200 characters,
/// without the secret of any token in it.
fn excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    redact(text.trim()).chars().take(200).collect()
}

impl DeviceFlow for HttpHub {
    fn poll(&self, poll: &str) -> Result<DevicePoll, HubError> {
        self.post_json(
            "/v1/login/device/poll",
            &DevicePollRequest {
                poll: poll.to_owned(),
            },
        )
    }
}

/// TLS through rustls with the ring provider, given explicitly, and the
/// root certificates the operating system trusts.
fn tls_config(url: &str) -> Result<TlsConfig, HubError> {
    let found = rustls_native_certs::load_native_certs();
    let roots: Vec<Certificate<'static>> = found
        .certs
        .iter()
        .map(|der| Certificate::from_der(der.as_ref()).to_owned())
        .collect();
    if roots.is_empty() {
        let detail = match found.errors.first() {
            Some(err) => format!("the system's root certificates cannot be read: {err}"),
            None => "the system trusts no root certificate".to_owned(),
        };
        return Err(HubError::Transport {
            hub: url.to_owned(),
            detail,
        });
    }
    Ok(TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build())
}

/// One repository of an [`HttpHub`].
pub(crate) struct HttpRepo<'h> {
    hub: &'h HttpHub,
    path: RepoPath,
}

impl HttpRepo<'_> {
    fn route(&self, rest: &str) -> String {
        format!("{}{rest}", self.path.api_path())
    }
}

impl Remote for HttpRepo<'_> {
    fn refs(&self) -> Result<Refs, HubError> {
        self.hub.get_json(&self.route("/refs"))
    }

    fn info(&self) -> Result<RepoInfo, HubError> {
        self.hub.get_json(&self.path.api_path())
    }

    fn kind(&self, id: &ObjectId) -> Result<Option<Kind>, HubError> {
        let uri = self.hub.uri(&self.route(&format!("/o/{id}")));
        let request = self.hub.authorize(self.hub.agent.head(uri));
        let response = match self.hub.check(request.call()) {
            Ok(response) => response,
            Err(HubError::Api { status: 404, .. }) => return Ok(None),
            Err(err) => return Err(err),
        };
        let name = response
            .headers()
            .get(KIND_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Kind::ALL
            .into_iter()
            .find(|kind| kind.name() == name)
            .map(Some)
            .ok_or_else(|| HubError::Protocol(format!("object {id} has no valid {KIND_HEADER}")))
    }

    fn lacks(&self, ids: &[ObjectId]) -> Result<Vec<ObjectId>, HubError> {
        let request = LacksRequest { ids: ids.to_vec() };
        let response: LacksResponse = self.hub.post_json(&self.route("/lacks"), &request)?;
        Ok(response.lacks)
    }

    fn send_objects(&self, body: &mut dyn Read, len: u64) -> Result<u64, HubError> {
        let route = self.route("/objects");
        let request = self
            .hub
            .authorize(self.hub.agent.post(self.hub.uri(&route)))
            .header("Content-Type", OBJECTS_MEDIA_TYPE)
            .header("Content-Length", len.to_string());
        let response = self.hub.check(request.send(SendBody::from_reader(body)))?;
        let stored: Stored = self.hub.read_json(&route, response)?;
        Ok(stored.stored)
    }

    fn missing(&self, request: &MissingRequest) -> Result<Box<dyn Read + '_>, HubError> {
        self.hub.post_for_objects(&self.route("/missing"), request)
    }

    fn land(&self, request: &LandRequest) -> Result<LandResponse, HubError> {
        self.hub.post_json(&self.route("/land"), request)
    }

    fn max_body(&self) -> u64 {
        self.hub.upload_limit()
    }

    fn max_ids(&self) -> usize {
        self.hub.id_limit()
    }
}

/// The routes of section 25, each a call of [`HttpHub::call_json`] whose
/// answer comes as the hub gave it.
impl VerifyApi for HttpRepo<'_> {
    fn features(&self) -> Result<Vec<String>, HubError> {
        Ok(self.hub.root()?.features)
    }

    fn status(&self) -> Result<Value, HubError> {
        self.hub.call_json("GET", &self.route("/verify"), &[], None)
    }

    fn report(&self, report: &VerifyReport, key: &str) -> Result<Value, HubError> {
        let body =
            serde_json::to_value(report).map_err(|err| HubError::Protocol(err.to_string()))?;
        self.hub.call_json(
            "POST",
            &self.route("/verify/report"),
            &[("Idempotency-Key", key)],
            Some(&body),
        )
    }

    fn policy(&self) -> Result<Value, HubError> {
        self.hub
            .call_json("GET", &self.route("/verify/policy"), &[], None)
    }

    fn put_policy(&self, policy: &Value) -> Result<Value, HubError> {
        self.hub
            .call_json("PUT", &self.route("/verify/policy"), &[], Some(policy))
    }

    fn log(&self, after: Option<u64>, limit: Option<u32>) -> Result<Value, HubError> {
        let query: Vec<String> = after
            .map(|after| format!("after={after}"))
            .into_iter()
            .chain(limit.map(|limit| format!("limit={limit}")))
            .collect();
        let mut route = self.route("/verify/log");
        if !query.is_empty() {
            route = format!("{route}?{}", query.join("&"));
        }
        self.hub.call_json("GET", &route, &[], None)
    }

    fn reset(&self, reset: &VerifyReset, key: &str) -> Result<Value, HubError> {
        let body =
            serde_json::to_value(reset).map_err(|err| HubError::Protocol(err.to_string()))?;
        self.hub.call_json(
            "POST",
            &self.route("/verify/reset"),
            &[("Idempotency-Key", key)],
            Some(&body),
        )
    }
}

/// The largest body to send, given the size this client was given and the
/// one the hub says it takes, if it said.
fn upload_limit(given: u64, hub: Option<u64>) -> u64 {
    match hub {
        Some(hub) if hub > 0 => given.min(hub),
        _ => given,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;

    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn uploads_fit_the_hub_and_the_client() {
        assert_eq!(upload_limit(MAX_BODY, None), MAX_BODY);
        assert_eq!(upload_limit(MAX_BODY, Some(64 << 10)), 64 << 10);
        assert_eq!(upload_limit(1 << 20, Some(MAX_BODY)), 1 << 20);
        assert_eq!(upload_limit(1 << 20, Some(0)), 1 << 20);
    }

    /// A request as [`fake_hub`] read it: its request line, its headers
    /// with their names lowercase, and its body.
    #[derive(Debug, Default)]
    struct Seen {
        line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }
    }

    /// A stand-in for a hub, listening on this machine: it reads one
    /// request per connection, gives each of `answers` in turn (a status,
    /// and a JSON body, or none when it is empty), and hands back what it
    /// was sent.
    fn fake_hub(answers: Vec<(&'static str, &'static str)>) -> (String, JoinHandle<Vec<Seen>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in answers {
                let (stream, _) = listener.accept().unwrap();
                seen.push(read_request(&stream));
                answer(stream, status, body);
            }
            seen
        });
        (url, server)
    }

    fn read_request(stream: &TcpStream) -> Seen {
        let mut reader = BufReader::new(stream);
        let mut seen = Seen::default();
        reader.read_line(&mut seen.line).unwrap();
        seen.line.truncate(seen.line.trim_end().len());
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            seen.headers
                .push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        let length = seen
            .header("content-length")
            .map_or(0, |n| n.parse().unwrap());
        seen.body = vec![0; length];
        reader.read_exact(&mut seen.body).unwrap();
        seen
    }

    /// Answers with `status` and the JSON `body`, if any, and closes.
    fn answer(mut stream: TcpStream, status: &str, body: &str) {
        if body.is_empty() {
            write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\n\r\n").unwrap();
        } else {
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    }

    #[test]
    fn any_route_with_or_without_a_body() {
        let (url, server) = fake_hub(vec![
            ("200 OK", r#"{"login":"ada"}"#),
            ("201 Created", r#"{"name":"demo","private":true}"#),
            ("204 No Content", ""),
            (
                "404 Not Found",
                r#"{"error":"not_found","detail":"no such repository"}"#,
            ),
            ("200 OK", r#"{"version":1}"#),
        ]);
        let token = Token::from_random([1; 5], [2; 32]);
        let client = HttpHub::new(&url, Some(token.clone())).unwrap();
        let me: Value = client.call_json("GET", "/v1/me", &[], None).unwrap();
        assert_eq!(me["login"], "ada");
        let body = json!({"name": "demo"});
        let made: Value = client
            .call_json(
                "POST",
                "/v1/repos",
                &[("Idempotency-Key", "k1")],
                Some(&body),
            )
            .unwrap();
        assert_eq!(made["private"], true);
        // An answer without a body; an error answer.
        let gone = client.call("DELETE", "/v1/r/ada/demo", &[], None).unwrap();
        assert!(gone.is_empty());
        match client.call("GET", "/v1/r/ada/demo", &[], None) {
            Err(HubError::Api { status: 404, body }) => assert_eq!(body.error, "not_found"),
            other => panic!("{other:?}"),
        }
        let anonymous = HttpHub::new(&url, None).unwrap();
        let root: Value = anonymous.call_json("GET", "/v1", &[], None).unwrap();
        assert_eq!(root["version"], 1);

        let seen = server.join().unwrap();
        let lines: Vec<&str> = seen.iter().map(|request| request.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "GET /v1/me HTTP/1.1",
                "POST /v1/repos HTTP/1.1",
                "DELETE /v1/r/ada/demo HTTP/1.1",
                "GET /v1/r/ada/demo HTTP/1.1",
                "GET /v1 HTTP/1.1",
            ]
        );
        // The token goes with every request of the client that has one, and
        // with no other; every request says it is braid.
        let bearer = format!("Bearer {}", token.expose());
        for request in &seen[..4] {
            assert_eq!(request.header("authorization"), Some(bearer.as_str()));
        }
        assert_eq!(seen[4].header("authorization"), None);
        assert!(
            seen.iter()
                .all(|request| request.header("user-agent") == Some(USER_AGENT))
        );
        // A JSON body goes as JSON, with the headers asked for, and the
        // requests without one carry none.
        assert_eq!(seen[1].header("content-type"), Some("application/json"));
        assert_eq!(seen[1].header("idempotency-key"), Some("k1"));
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[1].body).unwrap(),
            body
        );
        for (n, request) in seen.iter().enumerate().filter(|(n, _)| *n != 1) {
            assert!(request.body.is_empty(), "request {n}");
            assert_eq!(request.header("content-type"), None, "request {n}");
        }
    }

    #[test]
    fn the_routes_of_verification() {
        let (url, server) = fake_hub(vec![
            (
                "200 OK",
                r#"{"version":1,"limits":{"max_body":1,"max_put":1,"max_json":1,"max_ids":1,"max_paths":1,"missing_page":1,"missing_page_max":1,"missing_objects":1,"tree_entries":1,"tree_depth":1,"max_list":1},"features":["verify"]}"#,
            ),
            ("200 OK", r#"{"mode":"report","state":"green","later":1}"#),
            ("201 Created", r#"{"effect":"green_moved","status":{}}"#),
            ("200 OK", r#"{"mode":"off"}"#),
            ("200 OK", r#"{"mode":"report"}"#),
            ("200 OK", r#"{"events":[],"next":null}"#),
            ("200 OK", r#"{"events":[],"next":null}"#),
            ("200 OK", r#"{"state":"pending"}"#),
        ]);
        let hub = HttpHub::new(&url, Some(Token::from_random([1; 5], [2; 32]))).unwrap();
        let repo = hub.repo("ada/demo".parse().unwrap());
        assert_eq!(repo.features().unwrap(), ["verify"]);
        // What the hub answers comes whole, fields braid does not know
        // included.
        assert_eq!(repo.status().unwrap()["later"], 1);
        let report = VerifyReport {
            git_commit: Some("0a".repeat(20)),
            changeset: None,
            outcome: "pass".into(),
            suite: "ci".into(),
        };
        assert_eq!(repo.report(&report, "k1").unwrap()["effect"], "green_moved");
        assert_eq!(repo.policy().unwrap()["mode"], "off");
        let policy = json!({"mode": "report", "github": {"sources": []}});
        repo.put_policy(&policy).unwrap();
        repo.log(Some(17), Some(5)).unwrap();
        repo.log(None, None).unwrap();
        repo.reset(&VerifyReset::default(), "k2").unwrap();

        let seen = server.join().unwrap();
        let lines: Vec<&str> = seen.iter().map(|request| request.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "GET /v1 HTTP/1.1",
                "GET /v1/r/ada/demo/verify HTTP/1.1",
                "POST /v1/r/ada/demo/verify/report HTTP/1.1",
                "GET /v1/r/ada/demo/verify/policy HTTP/1.1",
                "PUT /v1/r/ada/demo/verify/policy HTTP/1.1",
                "GET /v1/r/ada/demo/verify/log?after=17&limit=5 HTTP/1.1",
                "GET /v1/r/ada/demo/verify/log HTTP/1.1",
                "POST /v1/r/ada/demo/verify/reset HTTP/1.1",
            ]
        );
        // A report and a reset carry their key, and nothing else does.
        let keys: Vec<Option<&str>> = seen
            .iter()
            .map(|request| request.header("idempotency-key"))
            .collect();
        assert_eq!(
            keys,
            [None, None, Some("k1"), None, None, None, None, Some("k2")]
        );
        let body = |n: usize| serde_json::from_slice::<Value>(&seen[n].body).unwrap();
        assert_eq!(
            body(2),
            json!({"git_commit": "0a".repeat(20), "outcome": "pass", "suite": "ci"})
        );
        assert_eq!(body(4), policy);
        assert_eq!(body(7), json!({"green": null, "expect": null}));
        assert_eq!(seen[4].header("content-type"), Some("application/json"));
    }
}
