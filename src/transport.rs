//! HTTPS-only control client. Redirects and ambient proxies cannot move credentials.

use crate::{
    config::target,
    observation::{Batch, Health, Observation, unix_ms},
    protocol::{Acknowledgement, Command, Credentials, Enrollment, Events},
};
use anyhow::{Context, Result, ensure};
use prost::Message as _;
use reqwest::{
    Client, Response, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::de::DeserializeOwned;
use std::{collections::HashSet, path::Path, time::Duration};
use tokio::io::AsyncReadExt as _;

/// Control traffic shares one origin and one bounded idle connection cache.
#[derive(Clone)]
pub struct Control {
    client: Client,
    server: String,
}

impl Control {
    /// Canonical origin used to bind persisted credentials to this issuer.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }
    /// Build a client with certificate verification, optional private CA, and no redirects.
    /// # Errors
    /// Rejects non-HTTPS/non-root origins and invalid CA files.
    pub async fn new(server: &str, ca_file: Option<&Path>) -> Result<Self> {
        target::control_plane(server)?;
        let url = target::web_url(server)?;
        ensure!(url.path() == "/", "control-plane URL must have a root path");
        let mut builder = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .http2_max_header_list_size(16_384);
        if let Some(path) = ca_file {
            builder = builder.add_root_certificate(certificate(path).await?);
        }
        Ok(Self {
            client: builder
                .build()
                .context("cannot initialize HTTPS control client")?,
            server: url.as_str().trim_end_matches('/').to_owned(),
        })
    }

    /// Exchange explicitly supplied bootstrap proof; no local role or tenant claim is sent.
    /// # Errors
    /// Returns bounded protocol or transport errors, omitting server response bodies.
    pub async fn enroll(&self, request: &Enrollment) -> Result<Credentials> {
        let response = self
            .client
            .post(format!("{}/v1/agents/enroll", self.server))
            .json(request)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("HTTPS enrollment failed"))?;
        ensure!(
            response.status() == StatusCode::CREATED,
            "enrollment returned HTTP {}",
            response.status().as_u16()
        );
        let credentials: Credentials = json(response, 8192).await?;
        credentials.validate(unix_ms())?;
        Ok(credentials)
    }

    fn authenticated(
        &self,
        credentials: &Credentials,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        credentials.validate(unix_ms())?;
        let mut value = HeaderValue::from_str(&format!("Bearer {}", credentials.token.0))
            .context("invalid agent credential")?;
        value.set_sensitive(true);
        Ok(self
            .client
            .request(method, format!("{}{path}", self.server))
            .header(AUTHORIZATION, value))
    }

    /// Fetch a full snapshot conditionally. A 304 carries no lease renewal semantics.
    /// # Errors
    /// Rejects non-success responses, oversized documents, and inconsistent `ETags`.
    pub async fn plan(
        &self,
        credentials: &Credentials,
        revision: u64,
    ) -> Result<Option<crate::protocol::Plan>> {
        let mut request =
            self.authenticated(credentials, reqwest::Method::GET, "/v1/agents/plan")?;
        if revision > 0 {
            request = request.header(reqwest::header::IF_NONE_MATCH, format!("\"{revision}\""));
        }
        let response = send(request).await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        ensure!(
            response.status() == StatusCode::OK,
            "plan fetch returned HTTP {}",
            response.status().as_u16()
        );
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .context("plan response requires ETag")?;
        let plan: crate::protocol::Plan = json(response, 1_048_576).await?;
        ensure!(
            etag == format!("\"{}\"", plan.revision),
            "plan ETag disagrees with revision"
        );
        Ok(Some(plan))
    }

    /// Acknowledge a revision only after the executor has persisted and activated it.
    /// # Errors
    /// Returns HTTP/transport errors so the caller can retry the acknowledgement.
    pub async fn acknowledge_plan(&self, credentials: &Credentials, revision: u64) -> Result<()> {
        let response = send(
            self.authenticated(credentials, reqwest::Method::POST, "/v1/agents/plan/ack")?
                .json(&serde_json::json!({"schema_version":1,"revision":revision})),
        )
        .await?;
        ensure!(
            response.status() == StatusCode::NO_CONTENT,
            "plan acknowledgement failed"
        );
        Ok(())
    }

    /// Upload a typed Protobuf batch; retain everything without an explicit durable ack.
    /// # Errors
    /// Rejects malformed acknowledgements and IDs not present in this request.
    pub async fn upload(
        &self,
        credentials: &Credentials,
        observations: Vec<Observation>,
    ) -> Result<Vec<String>> {
        let ids: HashSet<_> = observations
            .iter()
            .map(|observation| observation.id.clone())
            .collect();
        let bytes = Batch {
            schema_version: 1,
            observations,
        }
        .encode_to_vec();
        ensure!(bytes.len() <= 262_144, "observation batch exceeds 256 KiB");
        let response = send(
            self.authenticated(
                credentials,
                reqwest::Method::POST,
                "/v1/agents/observations",
            )?
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .body(bytes),
        )
        .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "observation upload returned HTTP {}",
            response.status().as_u16()
        );
        let ack: Acknowledgement = json(response, 8192).await?;
        ensure!(
            ack.schema_version == 1 && ack.accepted_ids.len() <= ids.len(),
            "invalid durable acknowledgement"
        );
        let mut unique = HashSet::new();
        ensure!(
            ack.accepted_ids
                .iter()
                .all(|id| ids.contains(id) && unique.insert(id)),
            "acknowledgement contains unknown or duplicate IDs"
        );
        Ok(ack.accepted_ids)
    }

    /// Long-poll hints with a 30-second deadline around the server's 25-second wait.
    /// # Errors
    /// Returns bounded transport/schema errors; callers reconnect with backoff.
    pub async fn events(&self, credentials: &Credentials, cursor: &str) -> Result<Option<Events>> {
        let response = send(
            self.authenticated(credentials, reqwest::Method::GET, "/v1/agents/events")?
                .query(&[("cursor", cursor), ("wait_seconds", "25")])
                .timeout(Duration::from_secs(30)),
        )
        .await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        ensure!(
            response.status() == StatusCode::OK,
            "control long-poll failed"
        );
        let events: Events = json(response, 4096).await?;
        ensure!(
            events.cursor.len() <= 128,
            "control cursor exceeds 128 bytes"
        );
        Ok(Some(events))
    }

    /// Reconcile durable short-lived work; events themselves are never the work queue.
    /// # Errors
    /// Rejects oversized responses or more than 16 commands per poll.
    pub async fn work(&self, credentials: &Credentials) -> Result<Vec<Command>> {
        let response =
            send(self.authenticated(credentials, reqwest::Method::GET, "/v1/agents/work")?).await?;
        ensure!(
            response.status() == StatusCode::OK,
            "work reconciliation failed"
        );
        let commands: Vec<Command> = json(response, 16_384).await?;
        ensure!(commands.len() <= 16, "work response exceeds 16 commands");
        Ok(commands)
    }

    /// Idempotently claim an admitted command; a lost claim response never starts a probe.
    /// # Errors
    /// Returns conflict/expiry/transport errors without executing the command.
    pub async fn claim(&self, credentials: &Credentials, command: &Command) -> Result<()> {
        command.validate(unix_ms())?;
        let response = send(
            self.authenticated(
                credentials,
                reqwest::Method::POST,
                &format!("/v1/agents/work/{}/claim", command.id),
            )?
            .json(&serde_json::json!({"schema_version":1})),
        )
        .await?;
        ensure!(
            response.status() == StatusCode::NO_CONTENT,
            "command claim rejected"
        );
        Ok(())
    }

    /// Renew scoped credentials without allowing an identity/role substitution.
    /// # Errors
    /// Rejects expired credentials, identity changes, and invalid renewal responses.
    pub async fn renew(&self, credentials: &Credentials) -> Result<Credentials> {
        let response = send(
            self.authenticated(credentials, reqwest::Method::POST, "/v1/agents/renew")?
                .json(&serde_json::json!({"schema_version":1})),
        )
        .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "credential renewal rejected"
        );
        let updated: Credentials = json(response, 8192).await?;
        updated.validate(unix_ms())?;
        ensure!(
            updated.agent_id == credentials.agent_id
                && updated.deployment == credentials.deployment,
            "renewal cannot change provisioned identity or deployment role"
        );
        Ok(updated)
    }

    /// Send bounded executor health without private host metrics.
    /// # Errors
    /// Returns HTTP or transport errors.
    pub async fn health(&self, credentials: &Credentials, health: &Health) -> Result<()> {
        let response = send(
            self.authenticated(credentials, reqwest::Method::POST, "/v1/agents/health")?
                .json(health),
        )
        .await?;
        ensure!(
            response.status() == StatusCode::NO_CONTENT,
            "health reporting failed"
        );
        Ok(())
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<Response> {
    request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("HTTPS control request failed"))
}

async fn json<T: DeserializeOwned>(mut response: Response, limit: usize) -> Result<T> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("control response read failed"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= limit,
            "control response exceeds operation size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid control response schema"))
}

/// Load at most 64 KiB of explicitly trusted PEM roots without disabling verification.
/// # Errors
/// Returns file, size, or certificate parsing errors.
pub async fn certificate(path: &Path) -> Result<reqwest::Certificate> {
    let file = tokio::fs::File::open(path)
        .await
        .context("cannot open CA certificate")?;
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .await
        .context("cannot read CA certificate")?;
    ensure!(bytes.len() <= 65_536, "CA certificate exceeds 64 KiB");
    reqwest::Certificate::from_pem(&bytes).context("invalid CA certificate")
}

/// Capped exponential backoff with fresh jitter; ordinary success resets the attempt.
#[derive(Default)]
pub struct Backoff {
    attempt: u32,
}
impl Backoff {
    /// Delay remains bounded even through an unlimited control-plane outage.
    pub fn delay(&mut self) -> Duration {
        let base = (1000_u64 << self.attempt.min(5)).min(30_000);
        self.attempt = self.attempt.saturating_add(1);
        let jitter =
            u64::try_from(uuid::Uuid::new_v4().as_u128() % u128::from(base / 5 + 1)).unwrap_or(0);
        Duration::from_millis(base + jitter)
    }
    /// Resume the initial retry budget after a successful reconciliation.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}
