//! Model catalog discovery: the models request on the account's own dispatch path, the sync that turns its
//! answer into served-model rows, the built-in seed when an OAuth account cannot be asked, and the scheduler.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use phoebis_oauth::{Clock, CredentialManager};
use phoebis_profile::catalog::{built_in, MODELS_TARGET, MODELS_USER_AGENT};
use phoebis_routing::RouterHandle;
use phoebis_store::{AccountKind, AccountRow, AccountState, Secret, Store, StoreError, SyncDelta};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding};
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::request::{write_request, RawRequest};
use phoebis_transport::response::{framing, read_head, BodyReader};
use tokio::io::{AsyncRead, AsyncWrite};

/// A models list is a few hundred short objects; anything past this is not one.
const MAX_BODY: usize = 4 * 1024 * 1024;
const LONG_CONTEXT_ABOVE: u64 = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("the models response is malformed: {0}")]
    Malformed(String),
    #[error("the account has no credential")]
    NoCredential,
    #[error("the models endpoint answered {0}")]
    Status(u16),
    #[error("the credential was rejected")]
    Unauthorized,
    #[error("the models endpoint is unreachable: {0}")]
    Unreachable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub models: Vec<(String, bool)>,
}

#[derive(Clone, Debug, Default)]
pub struct SyncOutcome {
    pub delta: Option<SyncDelta>,
    pub error: Option<String>,
    pub seeded: bool,
}

pub struct CatalogService {
    clock: Clock,
    manager: Arc<CredentialManager>,
    pool: Arc<Pool>,
    resolver: Resolver,
    router: RouterHandle,
    store: Arc<Store>,
    timeout: Duration,
}

impl CatalogService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        manager: Arc<CredentialManager>,
        pool: Arc<Pool>,
        resolver: Resolver,
        router: RouterHandle,
        clock: Clock,
        timeout: Duration,
    ) -> CatalogService {
        CatalogService {
            clock,
            manager,
            pool,
            resolver,
            router,
            store,
            timeout,
        }
    }

    pub async fn discover(&self, account: &AccountRow) -> Result<Discovered, DiscoveryError> {
        self.discover_with(account, None).await
    }

    /// `secret` is a staged replacement: the admin route verifies it against the account before the store
    /// learns of it.
    pub async fn discover_with(
        &self,
        account: &AccountRow,
        secret: Option<&Secret>,
    ) -> Result<Discovered, DiscoveryError> {
        let credential = match (account.kind, secret) {
            (AccountKind::Oauth, _) => {
                let token = self.manager.acquire(&account.id).await.map_err(|error| {
                    tracing::warn!(
                        account = %account.id,
                        %error,
                        "the catalog discovery could not obtain the account's credential"
                    );
                    DiscoveryError::NoCredential
                })?;
                (
                    "Authorization".to_string(),
                    format!("Bearer {}", token.secret.expose()),
                )
            }
            (AccountKind::ApiKey, Some(staged)) => {
                ("x-api-key".to_string(), staged.expose().to_string())
            }
            (AccountKind::ApiKey, None) => {
                let stored = match self.store.api_key_secret(&account.id) {
                    Ok(secret) => secret,
                    Err(error) => {
                        tracing::warn!(
                            account = %account.id,
                            %error,
                            "the catalog discovery could not obtain the account's credential"
                        );
                        None
                    }
                };
                (
                    "x-api-key".to_string(),
                    stored
                        .ok_or(DiscoveryError::NoCredential)?
                        .expose()
                        .to_string(),
                )
            }
        };
        let authority = "api.anthropic.com".to_string();
        let headers = vec![
            credential,
            ("User-Agent".into(), MODELS_USER_AGENT.into()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Accept".into(), "*/*".into()),
            ("Host".into(), authority.clone()),
            ("Accept-Encoding".into(), "gzip, deflate, br, zstd".into()),
        ];
        let request = RawRequest {
            method: "GET",
            target: MODELS_TARGET.into(),
            headers,
            body: Bytes::new(),
        };
        let addr = (self.resolver)(&authority)
            .ok_or_else(|| DiscoveryError::Unreachable(format!("no address for {authority}")))?;
        let (status, body) = tokio::time::timeout(self.timeout, async {
            {
                let key = PoolKey {
                    account_id: account.id.clone(),
                    host: authority.clone(),
                };
                let (mut conn, _) = self
                    .pool
                    .checkout(key.clone(), &authority, addr)
                    .await
                    .map_err(|e| DiscoveryError::Unreachable(e.to_string()))?;
                let (status, body, reusable) = exchange(&mut conn, &request).await?;
                if reusable {
                    self.pool.checkin(key, conn);
                }
                Ok((status, body))
            }
        })
        .await
        .map_err(|_| DiscoveryError::Unreachable("timed out".into()))??;
        match status {
            200 => parse_models(&body),
            401 => Err(DiscoveryError::Unauthorized),
            other => Err(DiscoveryError::Status(other)),
        }
    }

    pub async fn sync(&self, account_id: &str, actor: &str) -> Result<SyncOutcome, StoreError> {
        let account = self
            .store
            .account(account_id)?
            .ok_or_else(|| StoreError::AccountNotFound(account_id.to_string()))?;
        let now = (self.clock)();
        let outcome = match self.discover(&account).await {
            Ok(discovered) => {
                match self
                    .store
                    .apply_catalog_sync(account_id, &discovered.models, now, actor)
                {
                    Ok(delta) => SyncOutcome {
                        delta: Some(delta),
                        error: None,
                        seeded: false,
                    },
                    Err(StoreError::CanonicalClash { canonical_id, .. }) => {
                        let error = format!("two spellings of {canonical_id} in one response");
                        self.store
                            .set_catalog_status(account_id, None, Some(&error))?;
                        SyncOutcome {
                            delta: None,
                            error: Some(error),
                            seeded: false,
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => {
                let message = error.to_string();
                self.store
                    .set_catalog_status(account_id, None, Some(&message))?;
                // Whether the OAuth models endpoint answers a subscription token is unverified, so an OAuth
                // account that cannot be asked serves the ids the pinned client can name until a sync succeeds.
                let seeded = account.kind == AccountKind::Oauth
                    && self
                        .store
                        .seed_catalog(account_id, &built_in(), now, actor)?;
                SyncOutcome {
                    delta: None,
                    error: Some(message),
                    seeded,
                }
            }
        };
        tracing::info!(account = %account_id, seeded = outcome.seeded, error = ?outcome.error, "catalog sync");
        let _ = self.router.reload().await;
        Ok(outcome)
    }

    pub fn spawn_scheduler(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let jitter = 0.9 + rand::random::<f64>() * 0.2;
                tokio::time::sleep(interval.mul_f64(jitter)).await;
                let Ok(accounts) = self.store.accounts() else {
                    continue;
                };
                for account in accounts {
                    if matches!(
                        account.state,
                        AccountState::Disconnected | AccountState::Pairing | AccountState::Unpaired
                    ) {
                        continue;
                    }
                    if let Err(error) = self.sync(&account.id, "scheduler").await {
                        tracing::warn!(account = %account.id, %error, "scheduled catalog sync failed in the store");
                    }
                }
            }
        })
    }
}

async fn exchange<C: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut C,
    request: &RawRequest,
) -> Result<(u16, Vec<u8>, bool), DiscoveryError> {
    write_request(conn, request)
        .await
        .map_err(|f| DiscoveryError::Unreachable(f.error.to_string()))?;
    let mut leftover = BytesMut::new();
    let head = read_head(conn, &mut leftover)
        .await
        .map_err(|e| DiscoveryError::Unreachable(e.to_string()))?;
    let framing = framing(&head);
    let reusable = phoebis_transport::response::keep_alive(&head, &framing);
    let mut decoder =
        decoder(encoding(&head).map_err(|e| DiscoveryError::Malformed(e.to_string()))?);
    let mut reader = BodyReader::new(&mut *conn, leftover, framing);
    let mut out = Vec::new();
    while let Some(chunk) = reader
        .next_chunk()
        .await
        .map_err(|e| DiscoveryError::Unreachable(e.to_string()))?
    {
        out.extend(
            decoder
                .feed(&chunk)
                .map_err(|e| DiscoveryError::Malformed(e.to_string()))?,
        );
        if out.len() > MAX_BODY {
            return Err(DiscoveryError::Malformed(
                "models response too large".into(),
            ));
        }
    }
    out.extend(
        decoder
            .finish()
            .map_err(|e| DiscoveryError::Malformed(e.to_string()))?,
    );
    Ok((head.status, out, reusable))
}

fn parse_models(body: &[u8]) -> Result<Discovered, DiscoveryError> {
    let root: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    let data = root
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| DiscoveryError::Malformed("no data array".into()))?;
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            let long_context = entry
                .get("max_input_tokens")
                .and_then(|v| v.as_u64())
                .is_some_and(|n| n > LONG_CONTEXT_ABOVE);
            Some((id, long_context))
        })
        .collect();
    Ok(Discovered { models })
}
