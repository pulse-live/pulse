//! One monotonic deadline heap, not a task per assignment. Late cycles are skipped.

use crate::protocol::Check;
use anyhow::{Context, Result};
use std::{cmp::Reverse, collections::BinaryHeap};
use tokio::time::{Duration, Instant};

/// Compiled schedules retain bounded check definitions and one heap entry per check.
#[derive(Default)]
pub struct Scheduler {
    checks: Vec<Check>,
    deadlines: BinaryHeap<Reverse<(Instant, usize)>>,
}

impl Scheduler {
    /// Anchor the declared wall-clock epoch to a monotonic clock exactly once.
    /// # Errors
    /// Returns arithmetic/clock range errors rather than wrapping deadlines.
    pub fn new(checks: Vec<Check>, epoch_ms: u64, wall_ms: u64, now: Instant) -> Result<Self> {
        let mut deadlines = BinaryHeap::new();
        for (index, check) in checks.iter().enumerate() {
            anyhow::ensure!(
                check.recipe.interval_ms > 0
                    && check.assignment.jitter_ms < check.recipe.interval_ms,
                "scheduler requires a validated positive interval"
            );
            let delay = next_delay(check, epoch_ms, wall_ms)?;
            deadlines.push(Reverse((
                now.checked_add(Duration::from_millis(delay))
                    .context("schedule exceeds monotonic clock range")?,
                index,
            )));
        }
        Ok(Self { checks, deadlines })
    }

    /// Earliest execution time; an empty plan has no recurring wakeup.
    #[must_use]
    pub fn next(&self) -> Option<Instant> {
        self.deadlines
            .peek()
            .map(|Reverse((deadline, _))| *deadline)
    }

    /// Pop at most one due execution and advance directly beyond now, counting lost cycles.
    /// # Errors
    /// Returns corrupt-index or arithmetic range errors.
    pub fn due(&mut self, now: Instant) -> Result<Option<(Check, u64)>> {
        if self.next().is_none_or(|deadline| deadline > now) {
            return Ok(None);
        }
        let Reverse((deadline, index)) =
            self.deadlines.pop().context("missing scheduler deadline")?;
        let check = self.checks.get(index).context("invalid scheduler index")?;
        let elapsed = u64::try_from(now.duration_since(deadline).as_millis())?;
        let missed = elapsed / check.recipe.interval_ms;
        let advance = missed
            .checked_add(1)
            .and_then(|steps| check.recipe.interval_ms.checked_mul(steps))
            .context("schedule arithmetic overflow")?;
        let next = deadline
            .checked_add(Duration::from_millis(advance))
            .context("schedule clock overflow")?;
        self.deadlines.push(Reverse((next, index)));
        Ok(Some((check.clone(), missed)))
    }
}

fn next_delay(check: &Check, epoch: u64, wall: u64) -> Result<u64> {
    let id_hash = check
        .assignment
        .id
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        });
    let jitter = if check.assignment.jitter_ms == 0 {
        0
    } else {
        id_hash % (check.assignment.jitter_ms + 1)
    };
    let base = epoch
        .checked_add(check.assignment.phase_offset_ms)
        .and_then(|value| value.checked_add(jitter))
        .context("schedule epoch overflow")?;
    if wall <= base {
        return Ok(base - wall);
    }
    let remainder = (wall - base) % check.recipe.interval_ms;
    Ok(if remainder == 0 {
        0
    } else {
        check.recipe.interval_ms - remainder
    })
}

/// Spread local checks unless their operator explicitly supplies a phase.
pub(crate) fn local_phase(id: &str, interval_ms: u64, jitter_ms: u64) -> u64 {
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    });
    hash % (interval_ms - jitter_ms)
}
