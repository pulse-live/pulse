//! Always-local diagnostics with explicitly enabled, optional OTLP tracing.

use std::io::{self, IsTerminal as _};

use anyhow::{Context, Result};
use clap::ColorChoice;
use tracing::Level;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[cfg(feature = "telemetry")]
use opentelemetry_sdk::trace::SdkTracerProvider;

/// Owns diagnostic exporter lifetime. The binary always awaits shutdown before exit.
#[derive(Default)]
pub struct Telemetry {
    #[cfg(feature = "telemetry")]
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    /// Flush queued diagnostic spans and stop the exporter off the Tokio runtime thread.
    ///
    /// # Errors
    /// Returns provider shutdown failures or a failed blocking task.
    #[cfg(feature = "telemetry")]
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(provider) = self.provider.take() {
            tokio::task::spawn_blocking(move || {
                // The SDK's batch shutdown does not propagate a final export error.
                // Flush explicitly, then always shut down even if that flush failed.
                let flush = provider.force_flush();
                let shutdown = provider.shutdown_with_timeout(std::time::Duration::from_secs(5));
                flush.and(shutdown)
            })
            .await
            .context("telemetry shutdown task failed")?
            .context("diagnostic tracing could not be flushed and shut down")?;
        }
        Ok(())
    }

    /// Complete immediately when diagnostic export was not compiled in.
    #[cfg(not(feature = "telemetry"))]
    pub fn shutdown(self) -> std::future::Ready<Result<()>> {
        std::future::ready(Ok(()))
    }
}

/// Map repeatable verbosity flags to diagnostic severity levels.
#[must_use]
pub const fn verbosity_level(count: u8) -> Option<Level> {
    match count {
        0 => None,
        1 => Some(Level::INFO),
        2 => Some(Level::DEBUG),
        _ => Some(Level::TRACE),
    }
}

pub(crate) fn init(verbosity: Option<Level>, color: ColorChoice) -> Result<Telemetry> {
    let directives = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(_) => anyhow::bail!("RUST_LOG must be UTF-8"),
    };
    let filter = EnvFilter::builder()
        .with_regex(false)
        .with_default_directive(verbosity.unwrap_or(Level::ERROR).into())
        .parse(directives)
        .context("invalid RUST_LOG filter")?;
    let ansi = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stderr().is_terminal(),
    };
    let local = tracing_subscriber::fmt::layer()
        .compact()
        .with_ansi(ansi)
        .with_writer(io::stderr);

    #[cfg(feature = "telemetry")]
    {
        use opentelemetry::trace::TracerProvider as _;

        let provider = configured_provider()?;
        let otlp = provider.as_ref().map(|provider| {
            tracing_opentelemetry::layer().with_tracer(provider.tracer(env!("CARGO_PKG_NAME")))
        });
        let setup = tracing_subscriber::registry()
            .with(filter)
            .with(local)
            .with(otlp)
            .try_init();
        if let Err(error) = setup {
            // No action has executed or emitted spans; still clean up an initialized exporter.
            if let Some(provider) = provider {
                provider
                    .shutdown_with_timeout(std::time::Duration::from_secs(1))
                    .context("telemetry cleanup after subscriber initialization failure failed")?;
            }
            return Err(anyhow::anyhow!(
                "cannot initialize tracing subscriber: {error}"
            ));
        }
        Ok(Telemetry { provider })
    }
    #[cfg(not(feature = "telemetry"))]
    {
        tracing_subscriber::registry()
            .with(filter)
            .with(local)
            .try_init()
            .context("cannot initialize tracing subscriber")?;
        Ok(Telemetry::default())
    }
}

#[cfg(feature = "telemetry")]
fn configured_provider() -> Result<Option<SdkTracerProvider>> {
    use std::{env, time::Duration};

    use anyhow::ensure;
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{WithExportConfig as _, WithTonicConfig as _};
    use opentelemetry_sdk::{
        Resource,
        trace::{BatchConfigBuilder, BatchSpanProcessor},
    };

    // Capability is separate from consent: no endpoint means no exporter or worker.
    let endpoint = match env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(endpoint) => endpoint,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(_) => anyhow::bail!("OTEL_EXPORTER_OTLP_ENDPOINT must be UTF-8"),
    };
    validate_endpoint(&endpoint)?;
    for name in [
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    ] {
        if let Some(protocol) = env::var_os(name) {
            ensure!(
                protocol == "grpc",
                "{name} must be grpc; HTTP OTLP is not supported"
            );
        }
    }

    let secure = endpoint.starts_with("https://");
    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(3));
    if secure {
        builder =
            builder.with_tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots());
    }
    let exporter = builder
        .build()
        // SDK errors can contain endpoints or metadata; omit values and their source chain.
        .map_err(|_| {
            anyhow::anyhow!(
                "cannot initialize OTLP gRPC exporter; check endpoint and OTEL configuration"
            )
        })?;
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(256)
        .with_max_export_batch_size(64)
        .with_scheduled_delay(Duration::from_secs(5))
        .build();
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch)
        .build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_max_attributes_per_span(16)
        .with_max_events_per_span(16)
        .with_max_links_per_span(8)
        .with_resource(
            Resource::builder_empty()
                .with_attributes([
                    KeyValue::new("service.name", env!("CARGO_PKG_NAME")),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                ])
                .build(),
        )
        .build();
    Ok(Some(provider))
}

#[cfg(feature = "telemetry")]
fn validate_endpoint(endpoint: &str) -> Result<()> {
    use anyhow::ensure;
    use url::Host;

    let url =
        crate::config::target::web_url(endpoint).context("invalid OTEL_EXPORTER_OTLP_ENDPOINT")?;
    ensure!(
        url.query().is_none() && url.path() == "/",
        "OTLP gRPC endpoint must not contain a query or non-root path"
    );
    let loopback = match url.host() {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    ensure!(
        url.scheme() == "https" || loopback,
        "OTLP endpoints require HTTPS except for loopback collectors"
    );
    Ok(())
}

#[cfg(all(test, feature = "telemetry"))]
mod lifecycle_tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
    use opentelemetry_sdk::{
        error::{OTelSdkError, OTelSdkResult},
        trace::{BatchConfigBuilder, BatchSpanProcessor, Sampler, SpanData, SpanExporter},
    };

    use super::{SdkTracerProvider, Telemetry};

    // Completion depends on the caller's runtime remaining responsive during shutdown.
    #[derive(Debug)]
    struct RuntimeExporter {
        runtime: tokio::runtime::Handle,
        exported: Arc<AtomicUsize>,
        fail: bool,
    }

    impl SpanExporter for RuntimeExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            let exported = self.exported.clone();
            let count = batch.len();
            self.runtime
                .spawn(async move {
                    tokio::task::yield_now().await;
                    exported.fetch_add(count, Ordering::SeqCst);
                })
                .await
                .map_err(|error| OTelSdkError::InternalFailure(error.to_string()))?;
            if self.fail {
                Err(OTelSdkError::InternalFailure("test export failure".into()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_flushes_without_blocking_runtime_and_reports_export_failure() {
        for fail in [false, true] {
            let count = Arc::new(AtomicUsize::new(0));
            let exporter = RuntimeExporter {
                runtime: tokio::runtime::Handle::current(),
                exported: count.clone(),
                fail,
            };
            let batch = BatchConfigBuilder::default()
                .with_max_queue_size(8)
                .with_max_export_batch_size(4)
                .with_scheduled_delay(Duration::from_secs(3600))
                .build();
            let processor = BatchSpanProcessor::builder(exporter)
                .with_batch_config(batch)
                .build();
            let provider = SdkTracerProvider::builder()
                .with_sampler(Sampler::AlwaysOn)
                .with_span_processor(processor)
                .build();
            provider.tracer("test").start("shutdown-test").end();
            let result = Telemetry {
                provider: Some(provider),
            }
            .shutdown()
            .await;
            assert_eq!(result.is_err(), fail, "{result:?}");
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::verbosity_level;
    use tracing::Level;

    #[test]
    fn verbosity_maps_to_diagnostic_levels() {
        for (count, level) in [
            (0, None),
            (1, Some(Level::INFO)),
            (2, Some(Level::DEBUG)),
            (3, Some(Level::TRACE)),
            (4, Some(Level::TRACE)),
            (255, Some(Level::TRACE)),
        ] {
            assert_eq!(verbosity_level(count), level);
        }
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn exporter_configuration_is_explicit_and_secure() {
        for endpoint in [
            "https://collector.example:4317",
            "http://localhost:4317",
            "http://127.0.0.1:4317",
            "http://[::1]:4317",
        ] {
            assert!(super::validate_endpoint(endpoint).is_ok(), "{endpoint}");
        }
        for endpoint in [
            "",
            "collector:4317",
            "http://collector.example:4317",
            "https://collector/v1/traces",
            "https://user:secret@collector",
            "https://collector?token=secret",
            "https://collector#fragment",
        ] {
            assert!(super::validate_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }
}
