//! The admin API as a scripted caller reaches it: the admin secret as a bearer, one HTTP/1 connection per call,
//! plain or TLS by the base URL's scheme, and the whole call under one deadline. `status` and `tls reload` share it.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use boring::ssl::{SslConnector, SslMethod};
use boring::x509::store::X509StoreBuilder;
use boring::x509::X509;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{HeaderValue, AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use phoebis_store::Secret;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::config::{self, ConfigError};
use crate::{health, tls};

/// The flags every subcommand over the admin API shares, `status` included.
#[derive(clap::Args, Clone, Debug)]
pub struct Common {
    /// A PEM bundle to trust in place of the system roots, for a listener whose pair a private CA signed.
    #[arg(long, value_name = "PATH")]
    pub ca: Option<PathBuf>,
    /// Print the route's answer verbatim on stdout and nothing on stderr but errors.
    #[arg(long)]
    pub json: bool,
    /// The variable holding the admin secret; `admin.secret_env` by default.
    #[arg(long, value_name = "NAME")]
    pub secret_env: Option<String>,
    /// The whole call's deadline, connect and handshake included.
    #[arg(long, value_name = "SECONDS", default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: u64,
    /// The admin listener's base URL; from the file by default. With `--secret-env` too, the file is not read.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
}

/// One admin listener as a scripted caller reaches it.
pub struct AdminClient {
    base: Uri,
    ca: Option<PathBuf>,
    secret: Secret,
    timeout: Duration,
}

/// A route's answer: its status and its body, `null` when the body is empty.
pub struct Answer {
    pub status: u16,
    pub body: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("the admin secret was refused")]
    Refused401,
    #[error("{status} {message}")]
    Refused { status: u16, message: String },
    /// The listener answered, with a body that is not JSON: something other than the admin API holds the address.
    #[error("{url}: answered {status} with a body that is not JSON")]
    NotJson { url: String, status: u16 },
    #[error("{url}: {reason}")]
    Transport { url: String, reason: String },
}

impl ClientError {
    /// 1 for every variant; the subcommand prints the Display and exits with it.
    pub fn exit_code(&self) -> i32 {
        1
    }
}

/// Builds the runtime a subcommand runs on: one current-thread tokio runtime, since one call at a time is made.
///
/// A subcommand ends with `std::process::exit(code)` once it has printed its result, never by dropping this runtime: a
/// host name in the URL resolves on tokio's blocking pool, the deadline abandons a lookup that hangs rather than
/// cancelling it, and dropping the runtime waits for that thread, which would hold the exit past `--timeout`.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Either socket a call runs over, so hyper's connection is built once for both schemes.
trait Connected: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connected for T {}

/// Why a step of the call failed, as one line: the error with each cause its own text does not already carry.
struct Failure(String);

impl<E: Error + 'static> From<E> for Failure {
    fn from(error: E) -> Self {
        let mut line = error.to_string();
        let mut cause = error.source();
        while let Some(next) = cause {
            let text = next.to_string();
            if !line.contains(&text) {
                line.push_str(": ");
                line.push_str(&text);
            }
            cause = next.source();
        }
        Failure(line)
    }
}

impl AdminClient {
    /// The client from the flags and, unless `--url` and `--secret-env` are both given, the configuration file
    /// through `load_unresolved`; the secret is read from the variable and refused unset with the loader's line.
    pub fn from_flags(common: &Common, config_path: &Path) -> Result<AdminClient, ClientError> {
        let (url, ca, secret_env) = match (&common.url, &common.secret_env) {
            (Some(url), Some(secret_env)) => (url.clone(), common.ca.clone(), secret_env.clone()),
            _ => {
                let config = config::load_unresolved(config_path)?;
                let url = common.url.clone().unwrap_or_else(|| {
                    format!(
                        "{}://{}",
                        tls::scheme(&config.admin.tls),
                        health::target(config.admin.listen)
                    )
                });
                let ca = common.ca.clone();
                let secret_env = common
                    .secret_env
                    .clone()
                    .unwrap_or_else(|| config.admin.secret_env.clone());
                (url, ca, secret_env)
            }
        };
        let base = base_of(&url)?;
        let secret = config::from_env(&secret_env)?;
        Ok(AdminClient {
            base,
            ca,
            secret,
            timeout: Duration::from_secs(common.timeout),
        })
    }

    pub fn base(&self) -> &Uri {
        &self.base
    }

    /// One request on one connection, `Connection: close`, under the deadline; a JSON body is sent as
    /// `application/json`. The answer's body is parsed as JSON when it has one.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Answer, ClientError> {
        let https = self.base.scheme_str() == Some("https");
        let authority = self.authority();
        // `Uri::host` keeps an IPv6 literal's brackets; the socket and the certificate check want the bare address.
        let host = self
            .base
            .host()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = self.base.port_u16().unwrap_or(if https { 443 } else { 80 });
        let transport = |reason: String| ClientError::Transport {
            url: self.url(),
            reason,
        };

        // The connection's driver task, kept so the deadline and not the peer decides when the connection ends.
        let mut driver: Option<tokio::task::JoinHandle<hyper::Result<()>>> = None;
        let outcome = tokio::time::timeout(self.timeout, async {
            let tls = if https {
                Some(connector(self.ca.as_deref())?)
            } else {
                None
            };
            let tcp = TcpStream::connect((host, port)).await?;
            let io: Box<dyn Connected> = match tls {
                Some(tls) => Box::new(tokio_boring::connect(tls.configure()?, host, tcp).await?),
                None => Box::new(tcp),
            };
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
            driver = Some(tokio::spawn(connection));
            let mut bearer = HeaderValue::try_from(format!("Bearer {}", self.secret.expose()))?;
            bearer.set_sensitive(true);
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(HOST, authority.as_str())
                .header(CONNECTION, "close")
                .header(AUTHORIZATION, bearer);
            let request = match body {
                Some(body) => request
                    .header(CONTENT_TYPE, "application/json")
                    .body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?,
                None => request.body(Full::new(Bytes::new()))?,
            };
            let response = sender.send_request(request).await?;
            // Inside the same deadline as the request: a peer that answers its headers and then stalls the body would
            // otherwise hold the command past `--timeout`.
            let (parts, body) = response.into_parts();
            Ok::<_, Failure>((parts.status, body.collect().await?.to_bytes()))
        })
        .await;
        // After a finished call this is a no-op (`Connection: close` ended the task); on the deadline it closes the
        // socket a stalled peer still holds, so nothing of the call outlives the answer.
        if let Some(driver) = driver.take() {
            driver.abort();
        }
        let (status, bytes) = match outcome {
            Err(_) => {
                return Err(transport(format!(
                    "the call did not finish within {:?}",
                    self.timeout
                )))
            }
            Ok(Err(Failure(reason))) => return Err(transport(reason)),
            Ok(Ok(answer)) => answer,
        };
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| ClientError::NotJson {
                url: self.url(),
                status: status.as_u16(),
            })?
        };
        Ok(Answer {
            status: status.as_u16(),
            body,
        })
    }

    /// `call`, then a `2xx` as the answer and anything else as `ClientError::Refused` with the body's `error` line.
    pub async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Answer, ClientError> {
        let answer = self.call(method, path, body).await?;
        match answer.status {
            200..=299 => Ok(answer),
            401 => Err(ClientError::Refused401),
            status => {
                let message = answer
                    .body
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        StatusCode::from_u16(status)
                            .ok()
                            .and_then(|code| code.canonical_reason())
                            .unwrap_or("refused")
                            .to_owned()
                    });
                Err(ClientError::Refused { status, message })
            }
        }
    }

    /// `host:port` as the base names it; `base_of` admits no credential, so this is also the `Host` header.
    fn authority(&self) -> String {
        self.base
            .authority()
            .map(|authority| authority.as_str().to_owned())
            .unwrap_or_default()
    }

    /// The base as every line names it: scheme and authority, without the path `Uri` prints.
    pub fn url(&self) -> String {
        format!(
            "{}://{}",
            self.base.scheme_str().unwrap_or_default(),
            self.authority()
        )
    }
}

/// The TLS side of a call: the `ca` bundle alone when one is named, else the system roots, and ALPN `http/1.1`.
fn connector(ca: Option<&Path>) -> Result<SslConnector, Failure> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    if let Some(ca) = ca {
        // The builder has loaded the system roots already and `set_ca_file` would add to them; a store holding the
        // bundle alone replaces them, so no root the host trusts (a TLS-inspection CA among them) can vouch for the
        // listener the bearer goes to.
        let refused = |reason: String| Failure(format!("--ca {}: {reason}", ca.display()));
        let pem = std::fs::read(ca).map_err(|error| refused(error.to_string()))?;
        let certificates =
            X509::stack_from_pem(&pem).map_err(|error| refused(error.to_string()))?;
        if certificates.is_empty() {
            return Err(refused("holds no PEM certificate".to_owned()));
        }
        let mut store = X509StoreBuilder::new()?;
        for certificate in certificates {
            store.add_cert(certificate)?;
        }
        builder.set_cert_store(store.build());
    }
    builder.set_alpn_protos(b"\x08http/1.1")?;
    Ok(builder.build())
}

/// The base names the listener and nothing else. A credential in the URL would sit in the process list and in every
/// `Transport` line, and a path or query has nowhere to go, since every route's path is absolute under `/admin`.
fn base_of(url: &str) -> Result<Uri, ClientError> {
    let refused = || ClientError::Transport {
        url: "--url".to_owned(),
        reason: "must be http:// or https:// and a host, with no credential, path or query"
            .to_owned(),
    };
    let base: Uri = url.parse().map_err(|_| refused())?;
    let scheme = matches!(base.scheme_str(), Some("http" | "https"));
    let authority = base
        .authority()
        .is_some_and(|a| !a.host().is_empty() && !a.as_str().contains('@'));
    let bare = matches!(base.path(), "" | "/") && base.query().is_none();
    if scheme && authority && bare {
        Ok(base)
    } else {
        Err(refused())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct Flags {
        #[command(flatten)]
        common: Common,
    }

    #[test]
    fn the_common_flags_wait_ten_seconds_by_default_and_never_zero() {
        Flags::command().debug_assert();
        let defaults = Flags::try_parse_from(["phoebis"]).unwrap().common;
        assert_eq!(defaults.timeout, 10);
        assert!(
            (defaults.ca, defaults.secret_env, defaults.url) == (None, None, None)
                && !defaults.json
        );
        assert!(Flags::try_parse_from(["phoebis", "--timeout", "0"]).is_err());
        let given = Flags::try_parse_from([
            "phoebis",
            "--url",
            "https://phoebis.lan:8081",
            "--secret-env",
            "PHOEBIS_ADMIN_SECRET",
            "--ca",
            "/etc/phoebis/ca.pem",
            "--timeout",
            "1",
            "--json",
        ])
        .unwrap()
        .common;
        assert_eq!(given.url.as_deref(), Some("https://phoebis.lan:8081"));
        assert_eq!(given.secret_env.as_deref(), Some("PHOEBIS_ADMIN_SECRET"));
        assert_eq!(given.ca, Some(PathBuf::from("/etc/phoebis/ca.pem")));
        assert!(given.timeout == 1 && given.json);
    }

    #[test]
    fn a_ca_the_client_cannot_use_is_refused_naming_the_flag_and_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        for (path, reason) in [
            (&missing, "No such file or directory"),
            (&empty, "holds no PEM certificate"),
        ] {
            let Err(Failure(line)) = connector(Some(path)) else {
                panic!("{} was accepted as a CA bundle", path.display());
            };
            assert!(
                line.starts_with(&format!("--ca {}: ", path.display())) && line.contains(reason),
                "{line}"
            );
        }
    }

    #[test]
    fn a_base_names_a_scheme_and_a_host_and_nothing_else() {
        for good in [
            "http://127.0.0.1:8081",
            "https://phoebis.lan:8443/",
            "https://[::1]:8081",
        ] {
            assert!(base_of(good).is_ok(), "{good}");
        }
        for bad in [
            "127.0.0.1:8081",
            "ftp://127.0.0.1:8081",
            "http://operator:hunter2-hunter2@127.0.0.1:8081",
            "http://127.0.0.1:8081/admin",
            "http://127.0.0.1:8081/?x=1",
            "not a url",
        ] {
            let line = base_of(bad).map(|_| ()).unwrap_err().to_string();
            assert_eq!(
                line,
                "--url: must be http:// or https:// and a host, with no credential, path or query"
            );
        }
    }
}
