//! A compatible local HTTPS fixture, not a production control-plane implementation.

use anyhow::{Context as _, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use prost::Message as _;
use pulse::{
    agent,
    cli::actions::Action,
    config::{Config, HttpPolicy, ProbeKind},
    governor::Governor,
    observation::{Batch, Health, Observation, Origin, Outcome, unix_ms},
    probe::Engine,
    protocol::{
        Acknowledgement, Assignment, Command, Credentials, Deployment, Enrollment, Events, Plan,
        Recipe, Secret, Workload,
    },
    storage::{Identity, Store},
    transport::Control,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{
    sync::{Mutex, Notify},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

struct Backend {
    store: Store,
    plan: Mutex<Plan>,
    commands: Mutex<Vec<Command>>,
    observations: Mutex<HashMap<String, Observation>>,
    deliveries: Mutex<Vec<Vec<String>>>,
    claims: Mutex<HashSet<String>>,
    acknowledgements: Mutex<Vec<u64>>,
    token: Mutex<String>,
    deployment: Deployment,
    available: AtomicBool,
    lose_ack: AtomicBool,
    hits: AtomicU64,
    targets: Mutex<HashSet<String>>,
    enroll_tokens: Mutex<HashSet<String>>,
    renewed: Mutex<Option<Credentials>>,
    lose_renew_ack: AtomicBool,
    conditional: AtomicU64,
    sequence: AtomicU64,
    notify: Notify,
}

struct Fixture {
    root: tempfile::TempDir,
    url: String,
    ca: std::path::PathBuf,
    backend: Arc<Backend>,
    handle: axum_server::Handle<std::net::SocketAddr>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start(deployment: Deployment) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let certificate =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
        let pem = certificate.cert.pem();
        let ca = root.path().join("ca.pem");
        std::fs::write(&ca, &pem)?;
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            pem.into_bytes(),
            certificate.signing_key.serialize_pem().into_bytes(),
        )
        .await?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("https://{}", listener.local_addr()?);
        let backend = backend(
            &url,
            deployment,
            Store::open(
                &root.path().join("backend"),
                &pulse::config::Limits::default(),
            )
            .await?,
        );
        let cloud = Router::new()
            .route("/v1/agents/plan", get(plan))
            .route("/v1/agents/plan/ack", post(ack_plan))
            .route("/v1/agents/observations", post(observations))
            .route("/v1/agents/events", get(events))
            .route("/v1/agents/work", get(work))
            .route("/v1/agents/work/{id}/claim", post(claim))
            .route(
                "/v1/agents/health",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route("/v1/agents/renew", post(renew))
            .route_layer(middleware::from_fn_with_state(backend.clone(), authorize));
        let router = Router::new()
            .merge(cloud)
            .route("/v1/agents/enroll", post(enroll))
            .route("/ok", get(ok))
            .route("/target/{id}", get(target))
            .route(
                "/failure",
                get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "failure") }),
            )
            .route("/large", get(|| async { "x".repeat(65_536) }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .route(
                "/loop",
                get(|| async { (StatusCode::FOUND, [("location", "/loop")]) }),
            )
            .route(
                "/metadata",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "https://169.254.169.254/latest")],
                    )
                }),
            )
            .with_state(backend.clone());
        let handle = axum_server::Handle::new();
        let control = handle.clone();
        let task = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)?
                .handle(control)
                .serve(router.into_make_service())
                .await
        });
        Ok(Self {
            root,
            url,
            ca,
            backend,
            handle,
            task,
        })
    }

    fn config(&self) -> Result<Config> {
        let mut config = Config::parse(&format!(
            "[agent]\nmode='managed'\ncontrol_plane_url='{}'",
            self.url
        ))?;
        config.agent.state_dir = self.root.path().join("state");
        config.agent.ca_file = Some(self.ca.clone());
        config.agent.sync_interval_ms = 1000;
        config.network.allow_private_networks = true;
        config.network.ca_file = Some(self.ca.clone());
        config.validate()?;
        Ok(config)
    }

    async fn enroll(&self) -> Result<Config> {
        let config = self.config()?;
        Action::Enroll {
            server: self.url.clone(),
            token: Secret("bootstrap".into()),
            state_dir: config.agent.state_dir.clone(),
            ca_file: Some(self.ca.clone()),
        }
        .execute()
        .await?;
        Ok(config)
    }

    fn changed(&self) {
        self.backend.sequence.fetch_add(1, Ordering::SeqCst);
        self.backend.notify.notify_waiters();
    }

    async fn stop(self) -> Result<()> {
        self.handle.graceful_shutdown(Some(Duration::from_secs(1)));
        self.task.await??;
        Ok(())
    }
}

fn credentials(state: &Backend, token: String) -> Credentials {
    Credentials {
        schema_version: 1,
        agent_id: "test-agent".into(),
        token: Secret(token),
        expires_unix_ms: unix_ms() + 3_600_000,
        deployment: state.deployment,
    }
}

async fn enroll(State(state): State<Arc<Backend>>, Json(request): Json<Enrollment>) -> Response {
    if !matches!(request.token.0.as_str(), "bootstrap" | "recovery")
        || !state
            .enroll_tokens
            .lock()
            .await
            .insert(request.token.0.clone())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        StatusCode::CREATED,
        Json(credentials(&state, state.token.lock().await.clone())),
    )
        .into_response()
}

async fn authorize(State(state): State<Arc<Backend>>, request: Request, next: Next) -> Response {
    if !state.available.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let token = state.token.lock().await.clone();
    let can_renew = request.uri().path() == "/v1/agents/renew"
        && state
            .renewed
            .lock()
            .await
            .as_ref()
            .is_some_and(|credentials| {
                unix_ms() < credentials.expires_unix_ms.saturating_sub(3_300_000)
            })
        && request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some("Bearer agent-token");
    if !can_renew
        && request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(format!("Bearer {token}").as_str())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn plan(State(state): State<Arc<Backend>>, headers: HeaderMap) -> Response {
    let mut plan = state.plan.lock().await;
    let etag = format!("\"{}\"", plan.revision);
    if headers
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        state.conditional.fetch_add(1, Ordering::SeqCst);
        return StatusCode::NOT_MODIFIED.into_response();
    }
    if plan.schedule_epoch_unix_ms == 0 {
        plan.schedule_epoch_unix_ms = unix_ms() + 1000;
    }
    ([("etag", etag)], Json(plan.clone())).into_response()
}

async fn ack_plan(
    State(state): State<Arc<Backend>>,
    Json(value): Json<serde_json::Value>,
) -> StatusCode {
    if let Some(revision) = value.get("revision").and_then(serde_json::Value::as_u64) {
        state.acknowledgements.lock().await.push(revision);
    }
    StatusCode::NO_CONTENT
}

async fn observations(State(state): State<Arc<Backend>>, body: Bytes) -> Response {
    let Ok(batch) = Batch::decode(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let ids: Vec<_> = batch
        .observations
        .iter()
        .map(|record| record.id.clone())
        .collect();
    state.deliveries.lock().await.push(ids.clone());
    for record in batch.observations {
        if !state.store.push(&record).await.unwrap_or(false) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        state
            .observations
            .lock()
            .await
            .entry(record.id.clone())
            .or_insert(record);
    }
    if state.lose_ack.swap(false, Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(Acknowledgement {
        schema_version: 1,
        accepted_ids: ids,
    })
    .into_response()
}

async fn events(
    State(state): State<Arc<Backend>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let notified = state.notify.notified();
    let cursor = query
        .get("cursor")
        .and_then(|cursor| cursor.parse::<u64>().ok())
        .unwrap_or(0);
    if state.sequence.load(Ordering::SeqCst) == cursor
        && tokio::time::timeout(Duration::from_secs(25), notified)
            .await
            .is_err()
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    Json(Events {
        cursor: state.sequence.load(Ordering::SeqCst).to_string(),
        plan_changed: true,
        work_available: true,
    })
    .into_response()
}

async fn work(State(state): State<Arc<Backend>>) -> Json<Vec<Command>> {
    Json(state.commands.lock().await.clone())
}
async fn claim(
    State(state): State<Arc<Backend>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> StatusCode {
    if state
        .commands
        .lock()
        .await
        .iter()
        .any(|command| command.id == id && command.cancelled)
    {
        return StatusCode::CONFLICT;
    }
    state.claims.lock().await.insert(id);
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn new_revision_cancels_old_probes_and_activates_a_complete_snapshot() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    {
        let mut plan = fixture.backend.plan.lock().await;
        plan.assignments
            .first_mut()
            .context("missing assignment")?
            .target = format!("{}/slow", fixture.url);
        plan.recipes
            .first_mut()
            .context("missing recipe")?
            .timeout_ms = 5000;
    }
    let config = fixture.enroll().await?;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config, stop.clone()));
    wait_for(async || {
        store
            .get::<Health>("health")
            .await
            .is_ok_and(|health| health.is_some_and(|health| health.active_probes > 0))
    })
    .await?;
    {
        let mut plan = fixture.backend.plan.lock().await;
        plan.revision = 2;
        plan.schedule_epoch_unix_ms = unix_ms() + 500;
        let assignment = plan.assignments.first_mut().context("missing assignment")?;
        assignment.id = "replacement".into();
        assignment.target = format!("{}/ok", fixture.url);
        let recipe = plan.recipes.first_mut().context("missing recipe")?;
        recipe.timeout_ms = 1000;
        recipe.interval_ms = 1000;
    }
    fixture.changed();
    wait_for(async || {
        fixture
            .backend
            .observations
            .lock()
            .await
            .values()
            .any(|record| record.revision == 2 && record.outcome() == Outcome::Pass)
    })
    .await?;
    assert!(
        fixture
            .backend
            .observations
            .lock()
            .await
            .values()
            .any(|record| record.revision == 1 && record.error.as_deref() == Some("cancelled"))
    );
    assert!(fixture.backend.acknowledgements.lock().await.contains(&2));
    stop.cancel();
    task.await??;
    fixture.stop().await
}

#[tokio::test]
async fn renewal_retries_lost_response_and_expired_credentials_recover_without_losing_spool()
-> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let config = fixture.enroll().await?;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    let mut identity = store
        .get::<Identity>("identity")
        .await?
        .context("missing identity")?;
    identity.credentials.expires_unix_ms = unix_ms() + 10_000;
    store.put("identity", &identity).await?;
    fixture.backend.lose_renew_ack.store(true, Ordering::SeqCst);
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || {
        store
            .get::<Identity>("identity")
            .await
            .is_ok_and(|identity| {
                identity.is_some_and(|identity| identity.credentials.token.0 == "renewed-token")
            })
    })
    .await?;
    stop.cancel();
    task.await??;
    let check = fixture
        .backend
        .plan
        .lock()
        .await
        .compile(&config, unix_ms())?
        .into_iter()
        .next()
        .context("missing check")?;
    let pending = Observation::new(&check, None);
    store.push(&pending).await?;
    identity.credentials.expires_unix_ms = unix_ms() - 1;
    store.put("identity", &identity).await?;
    Action::Enroll {
        server: fixture.url.clone(),
        token: Secret("recovery".into()),
        state_dir: config.agent.state_dir.clone(),
        ca_file: Some(fixture.ca.clone()),
    }
    .execute()
    .await?;
    let recovered = store
        .get::<Identity>("identity")
        .await?
        .context("missing recovered identity")?;
    assert_eq!(
        recovered.credentials.agent_id,
        identity.credentials.agent_id
    );
    assert!(recovered.credentials.expires_unix_ms > unix_ms());
    assert_eq!(store.queued().await?, 1);
    fixture.stop().await
}

#[tokio::test]
async fn public_edge_identity_cannot_enable_private_targets_with_local_permission() -> Result<()> {
    let fixture = Fixture::start(Deployment::PublicEdge).await?;
    let config = fixture.enroll().await?;
    assert!(config.network.allow_private_networks);
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config, stop.clone()));
    wait_for(async || !fixture.backend.observations.lock().await.is_empty()).await?;
    assert!(
        fixture
            .backend
            .observations
            .lock()
            .await
            .values()
            .all(|record| record.outcome() == Outcome::Skipped)
    );
    assert_eq!(fixture.backend.hits.load(Ordering::SeqCst), 0);
    stop.cancel();
    task.await??;
    fixture.stop().await
}

#[tokio::test]
async fn standalone_monitors_multiple_local_targets_without_cloud() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let mut config = Config::parse(&format!(
        "[agent]\nmode='standalone'\n[output]\nstdout=true\n[[probes]]\nid='first'\nkind='http'\ntarget='{}/target/first'\ninterval_ms=1000\ntimeout_ms=500\nphase_offset_ms=0\n[[probes]]\nid='second'\nkind='http'\ntarget='{}/target/second'\ninterval_ms=1000\ntimeout_ms=500\nphase_offset_ms=500\n[probes.http]\nmethod='HEAD'",
        fixture.url, fixture.url
    ))?;
    config.agent.state_dir = fixture.root.path().join("local");
    config.network.allow_private_networks = true;
    config.network.ca_file = Some(fixture.ca.clone());
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || fixture.backend.targets.lock().await.len() == 2).await?;
    stop.cancel();
    let health = task.await??;
    assert!(health.completed >= 2);
    assert!(health.peak_active_probes <= config.limits.max_concurrent_probes);
    assert_eq!(fixture.backend.deliveries.lock().await.len(), 0);
    assert_eq!(fixture.backend.acknowledgements.lock().await.len(), 0);
    fixture.stop().await
}

#[tokio::test]
async fn live_command_cancellation_uses_no_extra_capacity() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    {
        let mut plan = fixture.backend.plan.lock().await;
        plan.assignments
            .first_mut()
            .context("missing assignment")?
            .target = format!("{}/slow", fixture.url);
        plan.recipes
            .first_mut()
            .context("missing recipe")?
            .timeout_ms = 5000;
    }
    let config = fixture.enroll().await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config, stop.clone()));
    wait_for(async || fixture.backend.acknowledgements.lock().await.contains(&1)).await?;
    fixture.backend.commands.lock().await.push(Command {
        id: "cancel-live".into(),
        run_id: "cancel-run".into(),
        monitor_id: "monitor".into(),
        assignment_id: "assigned".into(),
        revision: 1,
        deadline_unix_ms: unix_ms() + 10_000,
        priority: pulse::protocol::Priority::Urgent,
        cancelled: false,
    });
    fixture.changed();
    wait_for(async || fixture.backend.claims.lock().await.contains("cancel-live")).await?;
    fixture
        .backend
        .commands
        .lock()
        .await
        .first_mut()
        .context("missing command")?
        .cancelled = true;
    fixture.changed();
    wait_for(async || {
        fixture
            .backend
            .observations
            .lock()
            .await
            .values()
            .any(|record| record.command_id.as_deref() == Some("cancel-live"))
    })
    .await?;
    let record = fixture
        .backend
        .observations
        .lock()
        .await
        .values()
        .find(|record| record.command_id.as_deref() == Some("cancel-live"))
        .cloned()
        .context("missing cancellation")?;
    assert_eq!(record.outcome(), Outcome::Skipped);
    assert_eq!(record.error.as_deref(), Some("cancelled"));
    stop.cancel();
    task.await??;
    fixture.stop().await
}
async fn renew(State(state): State<Arc<Backend>>) -> Response {
    let credentials = {
        let mut renewed = state.renewed.lock().await;
        renewed
            .get_or_insert_with(|| credentials(&state, "renewed-token".into()))
            .clone()
    };
    *state.token.lock().await = credentials.token.0.clone();
    if state.lose_renew_ack.swap(false, Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(credentials).into_response()
}
async fn ok(State(state): State<Arc<Backend>>) -> &'static str {
    state.hits.fetch_add(1, Ordering::SeqCst);
    "ok"
}

async fn target(
    State(state): State<Arc<Backend>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> &'static str {
    state.targets.lock().await.insert(id);
    state.hits.fetch_add(1, Ordering::SeqCst);
    "ok"
}

async fn wait_for(condition: impl AsyncFn() -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("local integration condition timed out")
}

#[tokio::test]
async fn https_plan_executes_autonomously_and_live_checks_keep_correlation() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let config = fixture.enroll().await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || !fixture.backend.observations.lock().await.is_empty()).await?;
    let records = fixture.backend.observations.lock().await;
    let first = records.values().next().context("no routine observation")?;
    assert_eq!(first.outcome(), Outcome::Pass);
    assert_eq!(first.origin(), Origin::Routine);
    assert_eq!(first.revision, 1);
    assert!(
        first.dns_duration_us.is_none()
            && first.tcp_duration_us.is_none()
            && first.tls_duration_us.is_none()
            && first.ttfb_us.is_none()
    );
    drop(records);
    assert!(fixture.backend.acknowledgements.lock().await.contains(&1));
    fixture.backend.commands.lock().await.push(Command {
        id: "live-1".into(),
        run_id: "run-1".into(),
        monitor_id: "monitor".into(),
        assignment_id: "assigned".into(),
        revision: 1,
        deadline_unix_ms: unix_ms() + 10_000,
        priority: pulse::protocol::Priority::Urgent,
        cancelled: false,
    });
    fixture.changed();
    wait_for(async || {
        fixture
            .backend
            .observations
            .lock()
            .await
            .values()
            .any(|record| record.run_id.as_deref() == Some("run-1"))
    })
    .await?;
    let live = fixture
        .backend
        .observations
        .lock()
        .await
        .values()
        .find(|record| record.command_id.as_deref() == Some("live-1"))
        .cloned()
        .context("no correlated Live Check")?;
    assert_eq!(live.origin(), Origin::Live);
    assert_eq!(live.outcome(), Outcome::Pass);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fixture.backend.hits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.backend.claims.lock().await.len(), 1);
    assert!(fixture.backend.conditional.load(Ordering::SeqCst) > 0);
    stop.cancel();
    let health = task.await??;
    assert_eq!(health.active_probes, 0);
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    assert!(!store.running()?);
    fixture.stop().await
}

#[tokio::test]
async fn lost_durable_acknowledgement_retries_the_same_ids_without_duplication() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    fixture.backend.lose_ack.store(true, Ordering::SeqCst);
    let config = fixture.enroll().await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || fixture.backend.deliveries.lock().await.len() >= 2).await?;
    let deliveries = fixture.backend.deliveries.lock().await;
    assert_eq!(deliveries.first(), deliveries.get(1));
    drop(deliveries);
    assert_eq!(fixture.backend.observations.lock().await.len(), 1);
    stop.cancel();
    task.await??;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    assert_eq!(store.queued().await?, 0);
    fixture.stop().await
}

#[tokio::test]
async fn outage_keeps_authorized_work_and_restart_uses_last_known_good_state() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    fixture
        .backend
        .plan
        .lock()
        .await
        .recipes
        .first_mut()
        .context("missing recipe")?
        .interval_ms = 1000;
    let config = fixture.enroll().await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || fixture.backend.hits.load(Ordering::SeqCst) >= 1).await?;
    fixture.backend.available.store(false, Ordering::SeqCst);
    let before = fixture.backend.hits.load(Ordering::SeqCst);
    wait_for(async || fixture.backend.hits.load(Ordering::SeqCst) >= before + 2).await?;
    stop.cancel();
    task.await??;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    assert!(store.queued().await? > 0);
    let before = fixture.backend.hits.load(Ordering::SeqCst);
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || fixture.backend.hits.load(Ordering::SeqCst) > before).await?;
    fixture.backend.available.store(true, Ordering::SeqCst);
    fixture.changed();
    wait_for(async || store.queued().await.is_ok_and(|count| count == 0)).await?;
    stop.cancel();
    task.await??;
    fixture.stop().await
}

#[tokio::test]
async fn expired_plan_is_not_renewed_by_304_and_invalid_revision_keeps_previous_state() -> Result<()>
{
    let fixture = Fixture::start(Deployment::Private).await?;
    {
        let mut plan = fixture.backend.plan.lock().await;
        plan.recipes
            .first_mut()
            .context("missing recipe")?
            .interval_ms = 1000;
        plan.valid_until_unix_ms = unix_ms() + 4500;
    }
    let config = fixture.enroll().await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config.clone(), stop.clone()));
    wait_for(async || fixture.backend.hits.load(Ordering::SeqCst) >= 1).await?;
    {
        let mut plan = fixture.backend.plan.lock().await;
        plan.revision = 2;
        plan.assignments
            .first_mut()
            .context("missing assignment")?
            .recipe_id = "missing".into();
    }
    fixture.changed();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    let retained = store
        .get::<Plan>("plan")
        .await?
        .context("missing retained plan")?;
    assert_eq!(retained.revision, 1);
    *fixture.backend.plan.lock().await = retained;
    fixture.changed();
    wait_for(async || {
        store
            .get::<Health>("health")
            .await
            .is_ok_and(|health| health.is_some_and(|health| health.plan_expired))
    })
    .await?;
    let plan = store
        .get::<Plan>("plan")
        .await?
        .context("last-known-good plan missing")?;
    assert_eq!(plan.revision, 1);
    let count = fixture.backend.hits.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fixture.backend.hits.load(Ordering::SeqCst), count);
    stop.cancel();
    task.await??;
    fixture.stop().await
}

#[tokio::test]
async fn http_policy_enforces_status_body_redirect_network_and_deadline_limits() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let config = fixture.config()?;
    let governor = Governor::new(
        config.limits.clone(),
        config.community.clone(),
        false,
        Instant::now(),
    )?;
    let engine = Engine::new(&config, governor.clone(), true).await?;
    let check = fixture
        .backend
        .plan
        .lock()
        .await
        .compile(&config, unix_ms())?
        .into_iter()
        .next()
        .context("missing check")?;
    for (path, outcome, error) in [
        ("failure", Outcome::Fail, Some("unexpected_status")),
        ("metadata", Outcome::Skipped, Some("address_policy")),
        ("loop", Outcome::Fail, Some("redirect_limit")),
        ("slow", Outcome::Error, Some("timeout")),
    ] {
        let mut check = check.clone();
        check.assignment.target = format!("{}/{path}", fixture.url);
        check.recipe.http.max_redirects = 2;
        check.recipe.timeout_ms = if path == "slow" { 100 } else { 1000 };
        let record = engine
            .execute(&check, None, &CancellationToken::new(), None)
            .await;
        assert_eq!(record.outcome(), outcome, "{path}");
        assert_eq!(record.error.as_deref(), error, "{path}");
    }
    let mut large = check.clone();
    large.assignment.target = format!("{}/large", fixture.url);
    large.recipe.http.inspect_body = true;
    let record = engine
        .execute(&large, None, &CancellationToken::new(), None)
        .await;
    assert_eq!(
        record.inspected_bytes,
        u64::try_from(config.limits.max_response_bytes)?
    );
    assert!(record.body_truncated);
    let public = Engine::new(&config, governor, false).await?;
    let record = public
        .execute(&check, None, &CancellationToken::new(), None)
        .await;
    assert_eq!(record.outcome(), Outcome::Skipped);
    let mut resolved = check.clone();
    resolved.assignment.target = resolved.assignment.target.replace("127.0.0.1", "localhost");
    let record = public
        .execute(&resolved, None, &CancellationToken::new(), None)
        .await;
    assert_eq!(record.outcome(), Outcome::Skipped);
    assert_eq!(record.error.as_deref(), Some("address_policy"));
    let untrusted = Control::new(&fixture.url, None).await?;
    assert!(
        untrusted
            .enroll(&Enrollment {
                schema_version: 1,
                token: Secret("bootstrap".into()),
                capabilities: vec!["http".into()]
            })
            .await
            .is_err()
    );
    fixture.stop().await
}

fn backend(url: &str, deployment: Deployment, store: Store) -> Arc<Backend> {
    Arc::new(Backend {
        store,
        plan: Mutex::new(Plan {
            schema_version: 1,
            revision: 1,
            schedule_epoch_unix_ms: 0,
            valid_until_unix_ms: unix_ms() + 60_000,
            recipes: vec![Recipe {
                id: "http".into(),
                kind: ProbeKind::Http,
                interval_ms: 300_000,
                timeout_ms: 1000,
                http: HttpPolicy::default(),
            }],
            assignments: vec![Assignment {
                id: "assigned".into(),
                monitor_id: "monitor".into(),
                recipe_id: "http".into(),
                target: format!("{url}/ok"),
                phase_offset_ms: 0,
                jitter_ms: 0,
                workload: Workload::Routine,
            }],
        }),
        commands: Mutex::new(Vec::new()),
        observations: Mutex::new(HashMap::new()),
        deliveries: Mutex::new(Vec::new()),
        claims: Mutex::new(HashSet::new()),
        acknowledgements: Mutex::new(Vec::new()),
        token: Mutex::new("agent-token".into()),
        deployment,
        available: AtomicBool::new(true),
        lose_ack: AtomicBool::new(false),
        hits: AtomicU64::new(0),
        targets: Mutex::new(HashSet::new()),
        enroll_tokens: Mutex::new(HashSet::new()),
        renewed: Mutex::new(None),
        lose_renew_ack: AtomicBool::new(false),
        conditional: AtomicU64::new(0),
        sequence: AtomicU64::new(0),
        notify: Notify::new(),
    })
}

#[tokio::test]
async fn blocked_stdout_does_not_prevent_sigterm_shutdown() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let state = fixture.root.path().join("blocked");
    let path = fixture.root.path().join("blocked.toml");
    let mut config = format!(
        "[agent]\nmode='standalone'\nstate_dir={state:?}\n[output]\nstdout=true\n[network]\nallow_private_networks=true\nca_file={ca:?}\n[limits]\nmax_concurrent_probes=8\nmax_connections=16\nmax_probe_memory_bytes=33554432\nmax_probe_starts_per_second=1000\n",
        state = state.to_str().context("state path is not UTF-8")?,
        ca = fixture.ca.to_str().context("CA path is not UTF-8")?,
    );
    for index in 0..1000 {
        use std::fmt::Write as _;
        writeln!(
            config,
            "[[probes]]\nid='blocked-{index}'\nkind='http'\ntarget='{}/ok'\ninterval_ms=1000\ntimeout_ms=500\nphase_offset_ms={index}",
            fixture.url
        )?;
    }
    std::fs::write(&path, config)?;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["run", "--config"])
        .arg(&path)
        .env("RUST_LOG", "off")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = child
        .stdout
        .take()
        .context("missing output pipe")?
        .into_owned_fd()?;
    rustix::pipe::fcntl_setpipe_size(&output, 4096)?;
    wait_for(async || fixture.backend.hits.load(Ordering::SeqCst) > 100).await?;
    let pid =
        rustix::process::Pid::from_raw(i32::try_from(child.id().context("missing child ID")?)?)
            .context("invalid child PID")?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    assert!(status.success(), "blocked output exit: {status}");
    let store = Store::open(&state, &pulse::config::Limits::default()).await?;
    let health = store
        .get::<Health>("health")
        .await?
        .context("missing health")?;
    assert_eq!(health.active_probes, 0);
    assert!(health.dropped > 0);
    fixture.stop().await
}

#[tokio::test]
async fn community_requires_opt_in_and_local_opt_out_stops_assigned_work() -> Result<()> {
    let fixture = Fixture::start(Deployment::Community).await?;
    {
        let mut plan = fixture.backend.plan.lock().await;
        plan.assignments
            .first_mut()
            .context("missing assignment")?
            .workload = Workload::Community;
        plan.recipes
            .first_mut()
            .context("missing recipe")?
            .interval_ms = 1000;
    }
    let config = fixture.enroll().await?;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    let stop = CancellationToken::new();
    let task = tokio::spawn(agent::run(config, stop.clone()));
    wait_for(async || fixture.backend.acknowledgements.lock().await.contains(&1)).await?;
    assert!(fixture.backend.observations.lock().await.is_empty());
    store.put("community_enabled", &true).await?;
    wait_for(async || !fixture.backend.observations.lock().await.is_empty()).await?;
    let record = fixture
        .backend
        .observations
        .lock()
        .await
        .values()
        .next()
        .cloned()
        .context("missing community result")?;
    assert_eq!(record.origin(), Origin::Community);
    assert_eq!(record.error.as_deref(), Some("address_policy"));
    store.put("community_enabled", &false).await?;
    wait_for(async || {
        store
            .get::<bool>("community_enabled")
            .await
            .is_ok_and(|enabled| enabled == Some(false))
    })
    .await?;
    assert_eq!(fixture.backend.hits.load(Ordering::SeqCst), 0);
    stop.cancel();
    task.await??;
    fixture.stop().await
}

#[tokio::test]
async fn expired_deadline_and_pre_cancelled_work_never_send_requests() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let config = fixture.config()?;
    let governor = Governor::new(
        config.limits.clone(),
        config.community.clone(),
        false,
        Instant::now(),
    )?;
    let engine = Engine::new(&config, governor, true).await?;
    let check = fixture
        .backend
        .plan
        .lock()
        .await
        .compile(&config, unix_ms())?
        .into_iter()
        .next()
        .context("missing check")?;
    let record = engine
        .execute(
            &check,
            None,
            &CancellationToken::new(),
            Some(Instant::now()),
        )
        .await;
    assert_eq!(record.outcome(), Outcome::Skipped);
    assert_eq!(record.error.as_deref(), Some("deadline_expired"));
    let stop = CancellationToken::new();
    stop.cancel();
    let record = engine.execute(&check, None, &stop, None).await;
    assert_eq!(record.error.as_deref(), Some("cancelled"));
    assert_eq!(fixture.backend.hits.load(Ordering::SeqCst), 0);
    fixture.stop().await
}

#[tokio::test]
async fn enrollment_trace_output_redacts_bootstrap_and_agent_credentials() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args(["enroll", "--server", &fixture.url, "--ca-file"])
        .arg(&fixture.ca)
        .arg("--state-dir")
        .arg(fixture.root.path().join("trace-state"))
        .env("PULSE_TOKEN", "bootstrap")
        .env("RUST_LOG", "trace")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .kill_on_drop(true)
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("bootstrap"));
        assert!(!text.contains("agent-token"));
    }
    fixture.stop().await
}

#[tokio::test]
async fn local_status_and_community_opt_out_support_larger_configured_spools() -> Result<()> {
    let fixture = Fixture::start(Deployment::Private).await?;
    let mut config = fixture.enroll().await?;
    config.limits.max_spool_mb = 128;
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    let lock = store.lock()?;
    let check = fixture
        .backend
        .plan
        .lock()
        .await
        .compile(&config, unix_ms())?
        .into_iter()
        .next()
        .context("missing check")?;
    for _ in 0..192 {
        let mut record = Observation::new(&check, None);
        record.error = Some("x".repeat(65_000));
        assert!(store.push(&record).await?);
    }
    assert!(
        std::fs::metadata(config.agent.state_dir.join("state.sqlite3"))?.len() > 32 * 1_048_576 / 3
    );
    Action::Status {
        state_dir: config.agent.state_dir.clone(),
    }
    .execute()
    .await?;
    store.put("community_enabled", &true).await?;
    Action::Community {
        operation: pulse::cli::actions::CommunityOperation::Disable,
        state_dir: config.agent.state_dir.clone(),
    }
    .execute()
    .await?;
    assert_eq!(store.get::<bool>("community_enabled").await?, Some(false));
    drop(lock);
    Action::Enroll {
        server: fixture.url.clone(),
        token: Secret("recovery".into()),
        state_dir: config.agent.state_dir.clone(),
        ca_file: Some(fixture.ca.clone()),
    }
    .execute()
    .await?;
    assert_eq!(store.queued().await?, 192);
    fixture.stop().await
}
