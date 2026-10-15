//! The `Instant` ↔ wall-clock projection of Design A D5 / §5.6 — the sole owner
//! of `Instant` conversion for sessions.
//!
//! `std::time::Instant` is NOT serializable in any meaningful way: its zero is
//! process-local and undefined. The durable form is a pair (A §3.9):
//!
//! * `elapsed_secs` — seconds since a **process-lifetime monotonic anchor**, and
//! * `wall` — an absolute wall-clock reading sampled in the same statement.
//!
//! Rules implemented here (A §5.6):
//!
//! * **T1 (capture)** — [`TimePoint::now`] samples `Instant::now()` and
//!   `chrono::Local::now()` back to back, both measured against
//!   [`anchor`]. The anchor's own wall-clock position is written once per bundle
//!   as `Manifest::anchor_wall`.
//! * **T2 (re-anchor)** — [`TimePoint::to_instant`] rebuilds an `Instant` from
//!   the wall delta, following the sanctioned precedent
//!   [`manager::phase::get_plan_start_time`] (`src/manager/phase.rs`, which
//!   re-derives `Instant::now() - elapsed` from a file's mtime). **Documented
//!   approximation:** suspend/hibernate and wall-clock steps (NTP, manual set)
//!   make the delta wrong by the size of the discontinuity — the same trade-off
//!   the plan-start fallback already makes, and the only one available across a
//!   process boundary.
//! * **T3 (durations are numbers)** — durations come from the persisted `f64`
//!   (`WorkerSection::worked_secs`, `SubagentPanel::worked_duration_secs`), never
//!   from a difference of two independently re-anchored instants. There is
//!   deliberately no helper that subtracts two `TimePoint`s into an `Instant`.
//! * **T4 (ordering only)** — `elapsed_secs` orders members *within one process*
//!   and must never be compared across processes or used to reconstruct an
//!   `Instant`.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

use crate::session::format::TimePoint;

/// The process-lifetime monotonic anchor: an `Instant` sampled at first use, and
/// the wall clock at that same instant.
///
/// One anchor per process is what makes `elapsed_secs` monotonic and comparable
/// inside a bundle; `Manifest::anchor_wall` records its wall-clock position so a
/// later process can describe (not reconstruct) it.
static ANCHOR: LazyLock<(Instant, DateTime<Local>)> = LazyLock::new(|| {
    let instant = Instant::now();
    let wall = Local::now();
    (instant, wall)
});

/// The process anchor: `(monotonic instant, wall clock at that instant)`.
pub fn anchor() -> (Instant, DateTime<Local>) {
    *ANCHOR
}

/// Wall-clock position of the process anchor — written as `Manifest::anchor_wall`
/// so every `elapsed_secs` in the bundle has a stated origin (A §3.2).
pub fn anchor_wall() -> DateTime<Local> {
    ANCHOR.1
}

impl TimePoint {
    /// T1: capture the current instant as a durable pair. Both halves are
    /// sampled back to back so the pair is internally consistent to within a
    /// scheduling quantum.
    pub fn now() -> Self {
        let (anchor, anchor_wall) = anchor();
        let instant = Instant::now();
        let wall = Local::now();
        TimePoint {
            elapsed_secs: elapsed_since(anchor, instant),
            wall: wall.max(anchor_wall),
        }
    }

    /// T2: rebuild an `Instant` in this process from the stored wall delta
    /// (`instant' = Instant::now() - (wall_now - wall_recorded)`), exactly the
    /// `Plan::get_plan_start_time()` shape.
    ///
    /// The result is clamped to `Instant::now()` when the recorded wall clock is
    /// in the future (a clock step backwards, or a machine whose clock was
    /// corrected) — `Instant::checked_sub` cannot go negative, and silently
    /// minting an instant far in the past would make every later duration
    /// nonsense. Call sites that care SHALL report the clamp, not swallow it.
    pub fn to_instant(&self) -> Instant {
        let now_wall = Local::now();
        let age = match now_wall.signed_duration_since(self.wall).to_std() {
            Ok(age) => age,
            // `to_std` fails only for a negative delta: the recorded point is in
            // the future, i.e. the wall clock moved backwards.
            Err(_) => Duration::ZERO,
        };
        Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
    }

    /// Project a live `Instant` (one measured in **this** process, e.g.
    /// `ActiveWorkerInfo::started_at`) onto the durable pair.
    ///
    /// An instant older than the anchor cannot be expressed as a negative offset
    /// (A §5.6 forbids a negative `elapsed_secs`), so such an input is floored at
    /// `0.0` and keeps its own wall-clock companion when the caller supplies one;
    /// [`TimePoint::from_instant_with_wall`] is the form capture code should use
    /// for pre-existing timestamps.
    pub fn from_instant(instant: Instant) -> Self {
        TimePoint::from_instant_with_wall(instant, Local::now())
    }

    /// The capture form: pair a process `Instant` with the wall clock that was
    /// sampled alongside it (e.g. `ActiveWorkerInfo::started_wall`,
    /// `WorkerState::completed_wall`). The wall half is taken verbatim — it is
    /// what T2 re-anchors from.
    pub fn from_instant_with_wall(instant: Instant, wall: DateTime<Local>) -> Self {
        let (anchor, anchor_wall) = anchor();
        TimePoint {
            elapsed_secs: elapsed_since(anchor, instant),
            wall: if wall < anchor_wall {
                anchor_wall
            } else {
                wall
            },
        }
    }
}

/// `f64` seconds between two instants of this process's monotonic clock, floored
/// at `0.0` because the durable shape has no negative offsets (A §5.6).
fn elapsed_since(anchor: Instant, instant: Instant) -> f64 {
    instant
        .checked_duration_since(anchor)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic_and_never_negative() {
        let a = TimePoint::now();
        let b = TimePoint::now();
        assert!(a.elapsed_secs >= 0.0);
        assert!(b.elapsed_secs >= a.elapsed_secs);
        assert!(a.is_sane() && b.is_sane());
        // The anchor is stable for the process lifetime, so the two points are
        // measured from the same origin (T4).
        assert_eq!(anchor().0, anchor().0);
        assert_eq!(anchor_wall(), anchor().1);
    }

    #[test]
    fn round_trip_through_an_instant_keeps_the_offset() {
        let before = TimePoint::now();
        let instant = before.to_instant();
        let after = TimePoint::from_instant(instant);
        // Re-anchoring an instant that was itself re-anchored must land within a
        // scheduling quantum of the original offset, never drift by minutes.
        assert!(
            (after.elapsed_secs - before.elapsed_secs).abs() < 5.0,
            "{} vs {}",
            after.elapsed_secs,
            before.elapsed_secs
        );
    }

    #[test]
    fn a_future_wall_reading_clamps_to_now_instead_of_underflowing() {
        // Simulates a backwards wall-clock step: the recorded point claims to be
        // an hour ahead.
        let tp = TimePoint {
            elapsed_secs: 42.0,
            wall: Local::now() + chrono::Duration::hours(1),
        };
        let rebuilt = tp.to_instant();
        assert!(rebuilt <= Instant::now() + Duration::from_secs(1));
    }

    #[test]
    fn an_instant_older_than_the_anchor_floors_at_zero() {
        // The anchor is a `LazyLock` sampled at first use, so a `from_instant`
        // call made with an earlier process instant must not produce a negative
        // offset (which A §5.6 makes a structural violation).
        let tp = TimePoint::from_instant(Instant::now() - Duration::from_secs(1_000_000));
        assert_eq!(tp.elapsed_secs, 0.0);
        assert!(tp.is_sane());
    }
}
