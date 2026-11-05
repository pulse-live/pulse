//! Shared admission before spawning work. Permits release capacity on every exit path.

use crate::config::{Community, Limits};
use std::sync::{Arc, Mutex};
use tokio::{
    sync::Semaphore,
    time::{Duration, Instant},
};

/// Distinct local budgets; all classes still share the same hard ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Routine,
    Urgent,
    Community,
}

/// Explicit reasons for locally skipped work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Concurrency,
    Rate,
    CommunityDisabled,
    Unavailable,
}

#[derive(Debug)]
struct State {
    active: usize,
    routine: usize,
    community: usize,
    next_start: Instant,
    next_community: Instant,
    byte_window: Instant,
    bytes: usize,
}

/// One governor is shared by every recurring and on-demand probe.
#[derive(Clone, Debug)]
pub struct Governor {
    state: Arc<Mutex<State>>,
    resolver_slots: Arc<Semaphore>,
    limits: Limits,
    community: Community,
    capacity: usize,
}

/// An admitted execution owns one capacity slot until dropped, including cancellation.
#[derive(Debug)]
pub struct Permit {
    state: Arc<Mutex<State>>,
    class: Class,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = state.active.saturating_sub(1);
            if self.class != Class::Urgent {
                state.routine = state.routine.saturating_sub(1);
            }
            if self.class == Class::Community {
                state.community = state.community.saturating_sub(1);
            }
        }
    }
}

impl Governor {
    /// Reserve three control sockets and two connection-race slots per probe.
    /// # Errors
    /// Rejects unusable rate, connection, and community budgets.
    pub fn new(
        limits: Limits,
        community: Community,
        managed: bool,
        now: Instant,
    ) -> anyhow::Result<Self> {
        limits.validate()?;
        anyhow::ensure!(
            (1..=1000).contains(&limits.max_probe_starts_per_second)
                && (2..=256).contains(&limits.max_connections),
            "invalid governor rate or connection budget"
        );
        anyhow::ensure!(
            (1..=3600).contains(&community.max_checks_per_hour)
                && community.max_concurrent_probes > 0,
            "invalid community admission budget"
        );
        let capacity = limits
            .max_concurrent_probes
            .min(
                limits
                    .max_connections
                    .saturating_sub(if managed { 3 } else { 0 })
                    / 2,
            )
            .min(limits.max_probe_memory_bytes / 4_194_304);
        anyhow::ensure!(capacity > 0, "connection budget leaves no probe capacity");
        Ok(Self {
            // libc jobs survive request cancellation: cap them independently and
            // share the same pool across public/private clients and generations.
            resolver_slots: Arc::new(Semaphore::new(capacity.min(8))),
            state: Arc::new(Mutex::new(State {
                active: 0,
                routine: 0,
                community: 0,
                next_start: now,
                next_community: now,
                byte_window: now,
                bytes: 0,
            })),
            limits,
            community,
            capacity,
        })
    }

    pub(crate) fn resolver_slots(&self) -> Arc<Semaphore> {
        self.resolver_slots.clone()
    }

    /// Admit synchronously before creating any task; overload never waits in a queue.
    /// # Errors
    /// Returns a bounded local refusal reason without modifying active capacity.
    pub fn admit(
        &self,
        class: Class,
        now: Instant,
        community_enabled: bool,
    ) -> Result<Permit, Refusal> {
        let mut state = self.state.lock().map_err(|_| Refusal::Unavailable)?;
        if class == Class::Community && !community_enabled {
            return Err(Refusal::CommunityDisabled);
        }
        let routine_limit = self.capacity.saturating_sub(
            self.limits
                .reserved_urgent_probes
                .min(self.capacity.saturating_sub(1)),
        );
        if state.active >= self.capacity
            || (class != Class::Urgent && state.routine >= routine_limit)
            || (class == Class::Community
                && state.community >= self.community.max_concurrent_probes)
        {
            return Err(Refusal::Concurrency);
        }
        if now < state.next_start || (class == Class::Community && now < state.next_community) {
            return Err(Refusal::Rate);
        }
        state.next_start = now
            + Duration::from_nanos(
                1_000_000_000_u64.div_ceil(u64::from(self.limits.max_probe_starts_per_second)),
            );
        if class == Class::Community {
            state.community += 1;
            state.next_community = now
                + Duration::from_millis(
                    3_600_000_u64.div_ceil(u64::from(self.community.max_checks_per_hour)),
                );
        }
        state.active += 1;
        if class != Class::Urgent {
            state.routine += 1;
        }
        Ok(Permit {
            state: self.state.clone(),
            class,
        })
    }

    /// Charge inspected response bytes against a shared monotonic one-second budget.
    #[must_use]
    pub fn consume_bytes(&self, bytes: usize, now: Instant) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if now.duration_since(state.byte_window) >= Duration::from_secs(1) {
            state.byte_window = now;
            state.bytes = 0;
        }
        if bytes > self.limits.max_bytes_per_second.saturating_sub(state.bytes) {
            return false;
        }
        state.bytes += bytes;
        true
    }
}
