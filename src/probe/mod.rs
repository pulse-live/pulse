//! Bounded HTTP execution with shared admission and explicit cancellation.
//! Redirects and literal IPs are revalidated; DNS is pinned by the connection resolver.

pub mod network;

use crate::{
    config::{Config, Method, target},
    governor::Governor,
    observation::{Observation, Outcome, unix_ms},
    protocol::{Check, Command},
};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, StatusCode};
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

/// Shared HTTP client with no idle per-target pool or implicit environment proxy.
#[derive(Clone)]
pub struct Engine {
    client: Client,
    private: bool,
    bytes: usize,
    governor: Governor,
}

impl Engine {
    /// Build a verified TLS client and bounded resolver under local network policy.
    /// # Errors
    /// Returns TLS root or HTTP client construction errors without target values.
    pub async fn new(config: &Config, governor: Governor, private: bool) -> Result<Self> {
        let resolver = network::Resolver {
            private,
            slots: governor.resolver_slots(),
        };
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .dns_resolver(Arc::new(resolver))
            .retry(reqwest::retry::never())
            .http2_max_header_list_size(16_384)
            .http2_initial_stream_window_size(32_768)
            .http2_initial_connection_window_size(65_536)
            .http2_max_frame_size(16_384)
            .connect_timeout(Duration::from_millis(config.limits.max_timeout_ms));
        if let Some(path) = &config.network.ca_file {
            builder = builder.add_root_certificate(crate::transport::certificate(path).await?);
        }
        Ok(Self {
            client: builder.build().context("cannot build HTTP probe client")?,
            private,
            bytes: config.limits.max_response_bytes,
            governor,
        })
    }

    /// Execute one admitted check. All unavailable phase timings remain absent.
    #[tracing::instrument(skip_all, fields(revision = check.revision, live = command.is_some()))]
    pub async fn execute(
        &self,
        check: &Check,
        command: Option<&Command>,
        cancellation: &CancellationToken,
        lease: Option<Instant>,
    ) -> Observation {
        let mut observation = Observation::new(check, command);
        let start = Instant::now();
        let timeout_ms = command.map_or(check.recipe.timeout_ms, |command| {
            check
                .recipe
                .timeout_ms
                .min(command.deadline_unix_ms.saturating_sub(unix_ms()))
        });
        let duration = lease.map_or(Duration::from_millis(timeout_ms), |lease| {
            Duration::from_millis(timeout_ms).min(lease.saturating_duration_since(Instant::now()))
        });
        let result = if duration.is_zero() {
            Err("deadline_expired")
        } else {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err("cancelled"),
                result = tokio::time::timeout(duration, self.measure(check, &mut observation)) => {
                    match result { Ok(result) => result, Err(_) => Err("timeout") }
                }
            }
        };
        if let Err(error) = result {
            observation.outcome = match error {
                "address_policy" | "invalid_target" | "payload_budget" | "cancelled"
                | "deadline_expired" => Outcome::Skipped,
                "redirect_limit" | "redirect_downgrade" | "invalid_redirect" => Outcome::Fail,
                _ => Outcome::Error,
            }
            .into();
            observation.error = Some(error.into());
        }
        observation.completed_unix_ms = unix_ms();
        observation.total_duration_us =
            u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        observation
    }

    async fn measure(
        &self,
        check: &Check,
        observation: &mut Observation,
    ) -> Result<(), &'static str> {
        let mut url = target::web_url(&check.assignment.target).map_err(|_| "invalid_target")?;
        loop {
            self.validate_url(&url).map_err(|_| "address_policy")?;
            let request = match check.recipe.http.method {
                Method::Get => self.client.get(url.clone()),
                Method::Head => self.client.head(url.clone()),
            };
            let mut response = request
                .send()
                .await
                .map_err(|error| transport_error(&error))?;
            observation.http_status = Some(u32::from(response.status().as_u16()));
            observation.resolved_address = response
                .remote_addr()
                .map(|address| address.ip().to_string());
            if matches!(
                response.status(),
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) && check.recipe.http.max_redirects > 0
            {
                if observation.redirects >= u32::from(check.recipe.http.max_redirects) {
                    return Err("redirect_limit");
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|location| location.to_str().ok())
                    .ok_or("invalid_redirect")?;
                let next = url.join(location).map_err(|_| "invalid_redirect")?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    return Err("redirect_downgrade");
                }
                target::web_url(next.as_str()).map_err(|_| "invalid_redirect")?;
                url = next;
                observation.redirects += 1;
                continue;
            }
            let status = response.status().as_u16();
            observation.outcome = if (check.recipe.http.expected_status.min
                ..=check.recipe.http.expected_status.max)
                .contains(&status)
            {
                Outcome::Pass
            } else {
                Outcome::Fail
            }
            .into();
            if observation.outcome() == Outcome::Fail {
                observation.error = Some("unexpected_status".into());
            }
            observation.content_length = response.content_length();
            if check.recipe.http.inspect_body && check.recipe.http.method != Method::Head {
                let mut remaining = self.bytes;
                while remaining > 0 {
                    let Some(chunk) = response.chunk().await.map_err(|_| "response_body")? else {
                        return Ok(());
                    };
                    let bytes = remaining.min(chunk.len());
                    if !self.governor.consume_bytes(bytes, Instant::now()) {
                        return Err("payload_budget");
                    }
                    observation.inspected_bytes +=
                        u64::try_from(bytes).map_err(|_| "response_size")?;
                    remaining -= bytes;
                }
                observation.body_truncated =
                    observation.content_length != Some(observation.inspected_bytes);
            } else {
                observation.body_truncated = check.recipe.http.method != Method::Head
                    && observation.content_length != Some(0);
            }
            return Ok(());
        }
    }

    fn validate_url(&self, url: &Url) -> Result<()> {
        match url.host().context("HTTP target requires host")? {
            Host::Ipv4(address) => ensure!(
                network::address_allowed(address.into(), self.private),
                "address rejected"
            ),
            Host::Ipv6(address) => ensure!(
                network::address_allowed(address.into(), self.private),
                "address rejected"
            ),
            Host::Domain(_) => {}
        }
        Ok(())
    }
}

fn transport_error(error: &reqwest::Error) -> &'static str {
    use std::error::Error as _;
    let mut source = error.source();
    while let Some(cause) = source {
        if cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            return "address_policy";
        }
        source = cause.source();
    }
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "dns_connect_or_tls"
    } else {
        "transport"
    }
}
