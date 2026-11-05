use anyhow::{Context as _, Result};
use pulse::{
    config::{Community, Config, HttpPolicy, Limits, ProbeKind},
    governor::{Class, Governor, Refusal},
    observation::{Observation, Outcome, unix_ms},
    probe::network::address_allowed,
    protocol::{Assignment, Check, Command, Plan, Recipe, Workload},
    scheduler::Scheduler,
    storage::Store,
};
use tokio::time::{Duration, Instant};

fn check(id: &str) -> Check {
    Check {
        revision: 1,
        assignment: Assignment {
            id: id.into(),
            monitor_id: id.into(),
            recipe_id: "http".into(),
            target: "https://example.com".into(),
            phase_offset_ms: 100_000,
            jitter_ms: 0,
            workload: Workload::Routine,
        },
        recipe: Recipe {
            id: "http".into(),
            kind: ProbeKind::Http,
            interval_ms: 300_000,
            timeout_ms: 5000,
            http: HttpPolicy::default(),
        },
    }
}

#[test]
fn monotonic_phases_skip_missed_cycles_without_bursts() -> Result<()> {
    let now = Instant::now();
    let mut scheduler = Scheduler::new(vec![check("phase")], 0, 70_000, now)?;
    assert_eq!(scheduler.next(), Some(now + Duration::from_secs(30)));
    assert!(scheduler.due(now + Duration::from_secs(29))?.is_none());
    let (_, missed) = scheduler
        .due(now + Duration::from_secs(30))?
        .context("expected due check")?;
    assert_eq!(missed, 0);
    let (_, missed) = scheduler
        .due(now + Duration::from_secs(931))?
        .context("expected one late execution")?;
    assert_eq!(missed, 2);
    assert!(scheduler.due(now + Duration::from_secs(931))?.is_none());
    assert_eq!(scheduler.next(), Some(now + Duration::from_secs(1230)));
    Ok(())
}

#[test]
fn jitter_preserves_epoch_and_thousands_of_assignments_remain_bounded() -> Result<()> {
    let now = Instant::now();
    let mut job = check("jitter");
    job.assignment.jitter_ms = 1000;
    let scheduler = Scheduler::new(vec![job.clone()], 0, 70_000, now)?;
    let restarted = Scheduler::new(vec![job], 0, 75_000, now + Duration::from_secs(5))?;
    assert_eq!(scheduler.next(), restarted.next());
    let due = scheduler.next().context("missing jitter deadline")?;
    assert!((now + Duration::from_secs(30)..=now + Duration::from_secs(31)).contains(&due));
    let jobs = (0..4096)
        .map(|index| check(&format!("check-{index}")))
        .collect();
    let mut scheduler = Scheduler::new(jobs, 0, 70_000, now)?;
    let mut count = 0;
    while scheduler.due(now + Duration::from_secs(30))?.is_some() {
        count += 1;
    }
    assert_eq!(count, 4096);
    assert_eq!(scheduler.next(), Some(now + Duration::from_secs(330)));
    Ok(())
}

#[test]
fn admission_reserves_urgent_capacity_and_releases_cancelled_work() -> Result<()> {
    let now = Instant::now();
    let limits = Limits::default();
    let governor = Governor::new(limits.clone(), Community::default(), false, now)?;
    let mut permits = Vec::new();
    for step in 0..3 {
        permits.push(
            governor
                .admit(
                    Class::Routine,
                    now + Duration::from_millis(step * 200),
                    false,
                )
                .map_err(|reason| anyhow::anyhow!("{reason:?}"))?,
        );
    }
    assert_eq!(
        governor
            .admit(Class::Routine, now + Duration::from_secs(1), false)
            .err(),
        Some(Refusal::Concurrency)
    );
    let urgent = governor
        .admit(Class::Urgent, now + Duration::from_secs(1), false)
        .map_err(|reason| anyhow::anyhow!("{reason:?}"))?;
    assert_eq!(
        governor
            .admit(Class::Urgent, now + Duration::from_secs(2), false)
            .err(),
        Some(Refusal::Concurrency)
    );
    drop(permits);
    assert!(
        governor
            .admit(Class::Routine, now + Duration::from_secs(2), false)
            .is_ok()
    );
    drop(urgent);
    assert!(governor.consume_bytes(limits.max_bytes_per_second, now));
    assert!(!governor.consume_bytes(1, now));
    assert!(governor.consume_bytes(1, now + Duration::from_secs(1)));
    Ok(())
}

#[test]
fn community_policy_cannot_bypass_global_rate_or_connection_caps() -> Result<()> {
    let now = Instant::now();
    let governor = Governor::new(Limits::default(), Community::default(), true, now)?;
    assert_eq!(
        governor.admit(Class::Community, now, false).err(),
        Some(Refusal::CommunityDisabled)
    );
    let permit = governor
        .admit(Class::Community, now, true)
        .map_err(|reason| anyhow::anyhow!("{reason:?}"))?;
    assert_eq!(
        governor
            .admit(Class::Community, now + Duration::from_secs(1), true)
            .err(),
        Some(Refusal::Concurrency)
    );
    drop(permit);
    assert_eq!(
        governor
            .admit(Class::Community, now + Duration::from_secs(1), true)
            .err(),
        Some(Refusal::Rate)
    );
    assert!(
        governor
            .admit(Class::Community, now + Duration::from_secs(60), true)
            .is_ok()
    );
    Ok(())
}

#[test]
fn network_policy_rejects_rebinding_targets_and_metadata_even_with_private_permission() -> Result<()>
{
    for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
        assert!(address_allowed(ip.parse()?, false), "{ip}");
    }
    for ip in [
        "127.0.0.1",
        "10.1.2.3",
        "192.168.1.1",
        "::1",
        "::ffff:127.0.0.1",
        "fd01::1",
    ] {
        assert!(!address_allowed(ip.parse()?, false), "{ip}");
        assert!(address_allowed(ip.parse()?, true), "{ip}");
    }
    for ip in [
        "169.254.169.254",
        "168.63.129.16",
        "100.100.100.200",
        "0.0.0.0",
        "224.0.0.1",
        "fe80::1",
        "fd00:ec2::254",
        "2002:7f00:1::",
        "64:ff9b::7f00:1",
        "2001:db8::1",
    ] {
        assert!(!address_allowed(ip.parse()?, false), "{ip}");
        assert!(!address_allowed(ip.parse()?, true), "{ip}");
    }
    Ok(())
}

#[test]
fn plans_reject_unsafe_revisions_leases_offsets_and_unknown_recipes() -> Result<()> {
    let config = Config::parse("[agent]\nmode='standalone'")?;
    let job = check("assigned");
    let plan = Plan {
        schema_version: 1,
        revision: 1,
        schedule_epoch_unix_ms: 0,
        valid_until_unix_ms: 100_000,
        recipes: vec![job.recipe],
        assignments: vec![job.assignment],
    };
    assert_eq!(plan.compile(&config, 1)?.len(), 1);
    let mut invalid = plan.clone();
    invalid.revision = 0;
    assert!(invalid.compile(&config, 1).is_err());
    invalid = plan.clone();
    invalid.valid_until_unix_ms = 1;
    assert!(invalid.compile(&config, 1).is_err());
    invalid = plan.clone();
    invalid
        .assignments
        .first_mut()
        .context("missing assignment")?
        .phase_offset_ms = 300_000;
    assert!(invalid.compile(&config, 1).is_err());
    invalid = plan.clone();
    invalid.recipes.clear();
    assert!(invalid.compile(&config, 1).is_err());
    invalid = plan.clone();
    invalid.valid_until_unix_ms += 1;
    assert!(invalid.follows(&plan).is_err());
    invalid.revision = 2;
    assert!(invalid.follows(&plan).is_ok());
    invalid
        .assignments
        .first_mut()
        .context("missing assignment")?
        .target = "https://different.example".into();
    assert!(invalid.follows(&plan).is_err());
    Ok(())
}

#[tokio::test]
async fn spool_retries_stable_ids_acknowledges_partially_and_recovers_commands() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(&directory.path().join("state"), &Limits::default()).await?;
    let lock = store.lock()?;
    let job = check("durable");
    let mut first = Observation::new(&job, None);
    first.set_outcome(Outcome::Pass);
    let second = Observation::new(&job, None);
    assert!(store.push(&first).await?);
    assert!(store.push(&second).await?);
    let retry = store.batch().await?;
    assert_eq!(retry, store.batch().await?);
    store.acknowledge(vec![first.id.clone()]).await?;
    assert_eq!(store.queued().await?, 1);
    let command = Command {
        id: "command".into(),
        run_id: "run".into(),
        assignment_id: job.assignment.id.clone(),
        monitor_id: job.assignment.monitor_id.clone(),
        revision: 1,
        deadline_unix_ms: unix_ms() + 30_000,
        priority: pulse::protocol::Priority::Urgent,
        cancelled: false,
    };
    let pending = Observation::new(&job, Some(&command));
    assert!(store.reserve(&command, &pending).await?);
    assert!(!store.reserve(&command, &pending).await?);
    drop(lock);
    drop(store);
    let store = Store::open(&directory.path().join("state"), &Limits::default()).await?;
    let _lock = store.lock()?;
    store.recover().await?;
    let batch = store.batch().await?;
    assert_eq!(batch.len(), 2);
    let recovered = batch
        .iter()
        .find(|record| record.command_id.is_some())
        .context("missing recovered command")?;
    assert_eq!(recovered.id, pending.id);
    assert_eq!(
        recovered.error.as_deref(),
        Some("execution_interrupted_outcome_unknown")
    );
    assert!(!store.reserve(&command, &pending).await?);
    Ok(())
}

#[tokio::test]
async fn spool_overflow_and_disk_growth_are_bounded() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let limits = Limits {
        max_spool_mb: 1,
        ..Limits::default()
    };
    let store = Store::open(&directory.path().join("state"), &limits).await?;
    let mut accepted = 0;
    for _ in 0..32 {
        let mut record = Observation::new(&check("overflow"), None);
        record.error = Some("x".repeat(30_000));
        if store.push(&record).await? {
            accepted += 1;
        }
    }
    assert!((1..32).contains(&accepted));
    assert_eq!(store.queued().await?, accepted);
    let mut bytes = 0;
    for entry in std::fs::read_dir(directory.path().join("state"))? {
        bytes += entry?.metadata()?.len();
    }
    assert!(bytes <= 1_048_576, "state uses {bytes} bytes");
    Ok(())
}

#[tokio::test]
async fn batch_limit_includes_protobuf_framing_and_duplicate_inserts_are_idempotent() -> Result<()>
{
    use prost::Message as _;
    let directory = tempfile::tempdir()?;
    let store = Store::open(&directory.path().join("state"), &Limits::default()).await?;
    let mut first = None;
    for _ in 0..4 {
        let mut record = Observation::new(&check("framing"), None);
        record.error = Some("x".repeat(65_000));
        let overhead = record.encoded_len() - 65_000;
        record.error = Some("x".repeat(65_536 - overhead));
        assert_eq!(record.encoded_len(), 65_536);
        assert!(store.push(&record).await?);
        first.get_or_insert(record);
    }
    let first = first.context("no observation")?;
    let bytes = store.queued_bytes().await?;
    assert!(store.push(&first).await?);
    assert_eq!(store.queued_bytes().await?, bytes);
    let mut conflicting = first.clone();
    conflicting.set_outcome(Outcome::Pass);
    assert!(store.push(&conflicting).await.is_err());
    let batch = pulse::observation::Batch {
        schema_version: 1,
        observations: store.batch().await?,
    };
    assert_eq!(batch.observations.len(), 3);
    assert!(batch.encoded_len() <= 262_144);
    let json = serde_json::to_value(&first)?;
    assert_eq!(
        json.get("outcome").and_then(serde_json::Value::as_str),
        Some("ERROR")
    );
    assert!(
        json.get("dns_duration_us")
            .is_some_and(serde_json::Value::is_null)
    );
    store.acknowledge(vec![first.id]).await?;
    assert!(store.queued_bytes().await? < bytes);
    Ok(())
}

#[test]
fn memory_reservations_limit_active_probes_and_invalid_rates_are_rejected() -> Result<()> {
    let now = Instant::now();
    let limits = Limits {
        max_probe_memory_bytes: 4_194_304,
        ..Limits::default()
    };
    let governor = Governor::new(limits.clone(), Community::default(), false, now)?;
    let permit = governor
        .admit(Class::Routine, now, false)
        .map_err(|reason| anyhow::anyhow!("{reason:?}"))?;
    assert_eq!(
        governor
            .admit(Class::Urgent, now + Duration::from_secs(1), false)
            .err(),
        Some(Refusal::Concurrency)
    );
    drop(permit);
    let invalid = Limits {
        max_probe_starts_per_second: 0,
        ..limits
    };
    assert!(Governor::new(invalid, Community::default(), false, now).is_err());
    Ok(())
}

#[tokio::test]
async fn state_rejects_public_permissions_symlinks_and_future_schemas() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir()?;
    let path = root.path().join("state");
    let store = Store::open(&path, &Limits::default()).await?;
    drop(store);
    let alias = root.path().join("alias");
    symlink(&path, &alias)?;
    let error = Store::open(&alias, &Limits::default())
        .await
        .err()
        .context("accepted state symlink")?;
    assert!(format!("{error:#}").contains("private directory"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    assert!(Store::open(&path, &Limits::default()).await.is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    let connection = rusqlite::Connection::open(path.join("state.sqlite3"))?;
    connection.pragma_update(None, "user_version", 2)?;
    drop(connection);
    let error = Store::open(&path, &Limits::default())
        .await
        .err()
        .context("accepted future schema")?;
    assert!(format!("{error:#}").contains("newer than this binary"));
    Ok(())
}

#[test]
fn non_divisor_rates_round_up_spacing_instead_of_exceeding_budgets() -> Result<()> {
    let now = Instant::now();
    let limits = Limits {
        max_probe_starts_per_second: 3,
        ..Limits::default()
    };
    let community = Community {
        max_checks_per_hour: 7,
        ..Community::default()
    };
    let governor = Governor::new(limits, community, false, now)?;
    drop(
        governor
            .admit(Class::Community, now, true)
            .map_err(|reason| anyhow::anyhow!("{reason:?}"))?,
    );
    assert_eq!(
        governor
            .admit(Class::Urgent, now + Duration::from_nanos(333_333_333), true)
            .err(),
        Some(Refusal::Rate)
    );
    assert_eq!(
        governor
            .admit(Class::Community, now + Duration::from_millis(514_285), true)
            .err(),
        Some(Refusal::Rate)
    );
    assert!(
        governor
            .admit(Class::Community, now + Duration::from_millis(514_286), true)
            .is_ok()
    );
    Ok(())
}
