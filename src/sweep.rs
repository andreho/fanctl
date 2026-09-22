//! The fan calibration ("sweep") routine: step a fan's duty up from 0 %
//! towards 100 %, observe its tachometer, and derive the range of duty in
//! which the fan actually responds.
//!
//! Why it exists: a 3-wire fan has no feedback wire, so the fan's *own*
//! electronics decide what a given PWM actually does. Most fans do not spin
//! up below roughly 20–30 % duty, and many reach their top speed well before
//! 100 % (a full-duty command then buys nothing). A sweep measures both
//! limits for a channel:
//!
//! * **start** — the first duty at which the tachometer reports any RPM;
//! * **saturation** — the first duty at which the RPM stops rising.
//!
//! This module is deliberately hardware-free: it holds the data types, the
//! pure math and the result text. The [`Engine`](crate::daemon::Engine)
//! performs the actual sysfs writes and sleeps, driving this module both
//! from the one-shot `fanctld --sweep` command and from the TUI (`c` on a
//! fan, served by the running daemon).

use serde::{Deserialize, Serialize};

/// The default duty increment between sweep steps, in percent.
pub const DEFAULT_STEP: u8 = 10;

/// The default settling time after each duty change, in seconds: how long
/// the fan is given to reach its new steady speed before the tachometer is
/// read.
pub const DEFAULT_SETTLE_SECS: f64 = 3.0;

/// A single observed step of a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepStep {
    /// The duty that was applied, in percent (0..=100).
    pub duty: u8,
    /// The tachometer reading after settling, in RPM.
    pub rpm: u32,
}

/// The result of a finished sweep, as the daemon reports it (in the state
/// snapshot, in the TUI status line, or from the one-shot CLI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepReport {
    /// The fan that was swept (its `pwmN` id).
    pub fan: String,
    /// The first duty at which the fan was observed spinning, if any.
    pub min_duty: Option<u8>,
    /// The duty at which the fan reached its top speed, if it was reached
    /// (the last duty where the RPM was still rising, or 100 %).
    pub max_duty: Option<u8>,
    /// True when the RPM stopped rising *before* the sweep reached 100 %:
    /// at `max_duty` the fan is at its top speed and more duty makes no
    /// difference.
    pub saturated: bool,
    /// A human-readable summary of the outcome.
    pub note: String,
}

/// The noise floor below which an RPM change is ignored: `max(2 % of the
/// previous reading, 15 RPM)`. Tachometers jitter a little; a 2 % floor
/// scales with the reading, the 15 RPM floor keeps it meaningful at low
/// speeds.
fn noise_floor(prev_rpm: u32) -> i64 {
    (f64::from(prev_rpm) * 0.02).max(15.0) as i64
}

/// Whether the last step shows the fan has stopped accelerating.
///
/// A step only counts as a "stall" while the fan is actually spinning: a
/// tach that reads 0 at every step is *not* a stall — the sweep keeps
/// going to probe the whole 0–100 % range for a late start (some fans
/// refuse to spin until a surprisingly high duty).
pub fn stalled(steps: &[SweepStep]) -> bool {
    let n = steps.len();
    if n < 2 {
        return false;
    }
    let (prev, last) = (&steps[n - 2], &steps[n - 1]);
    last.rpm > 0 && (last.rpm as i64 - prev.rpm as i64).abs() <= noise_floor(prev.rpm)
}

/// Derive the effective duty range from a finished sweep trace.
///
/// Returns `(min_duty, max_duty, saturated)`:
///
/// * `min_duty` — the first duty at which the fan was observed spinning;
/// * `max_duty` — the last duty at which the RPM was still rising (the fan
///   is at its top speed there), or the last spinning step;
/// * `saturated` — true when the RPM stopped rising before the sweep
///   finished.
///
/// A fan that never spun yields `(None, None, false)`.
pub fn derive(steps: &[SweepStep]) -> (Option<u8>, Option<u8>, bool) {
    let Some(min) = steps.iter().find(|s| s.rpm > 0).map(|s| s.duty) else {
        return (None, None, false);
    };

    let mut max: Option<u8> = None;
    let mut saturated = false;
    for w in steps.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if b.rpm == 0 {
            continue;
        }
        if (b.rpm as i64 - a.rpm as i64).abs() > noise_floor(a.rpm) {
            max = Some(b.duty);
        } else {
            // The fan reached its top speed at the *previous* step.
            max = Some(a.duty);
            saturated = true;
            break;
        }
    }
    if max.is_none() {
        // The RPM kept rising through the whole trace (or there was a
        // single spinning step): the top speed is wherever it last spun.
        max = steps.iter().rev().find(|s| s.rpm > 0).map(|s| s.duty);
    }
    (Some(min), max, saturated)
}

/// Whether a measured range is worth persisting in the config: a proper
/// subrange (the fan responds somewhere in the middle, not across the whole
/// 0–100 % span, and not at a single duty).
pub fn worth_saving(min: Option<u8>, max: Option<u8>) -> bool {
    matches!(
        (min, max),
        (Some(m), Some(x)) if m < x && !(m == 0 && x == 100)
    )
}

/// A human-readable summary of a finished sweep (TUI status line, one-shot
/// CLI output).
pub fn describe(
    fan: &str,
    min: Option<u8>,
    max: Option<u8>,
    saturated: bool,
    steps: &[SweepStep],
    saved: bool,
) -> String {
    match (min, max) {
        // The sweep never took a single reading (a write failed, e.g.
        // permissions): the "no fan response" text below would blame the
        // hardware for a bug in our own write path.
        (None, _) if steps.is_empty() => format!(
            "{fan}: the sweep could not take a single reading (a write failed — check the error above) — nothing was saved."
        ),
        (None, _) => format!(
            "{fan}: no fan response — the tachometer stayed at 0 rpm through the whole 0–100 % sweep; \
             this channel is probably not connected to a measurable fan (or the chip exposes no \
             tachometer for it). Nothing was saved."
        ),
        (Some(0), Some(0)) => format!(
            "{fan}: the tachometer reads {} rpm at 0 % duty and never moves with the duty — the \
             reading is probably not from this fan (a shared or stuck tach). Nothing was saved.",
            steps.first().map(|s| s.rpm).unwrap_or(0)
        ),
        (Some(m), Some(x)) if m == x => format!(
            "{fan}: the tachometer only moves around {m} % duty — the measured range is a single \
             point, too narrow to be useful. Nothing was saved."
        ),
        (Some(m), Some(x)) => {
            let mut s = format!("{fan}: the fan spins between {m} % and {x} % duty");
            if saturated {
                s.push_str(&format!(" (it saturates at {x} % — more duty won't make it faster)"));
            }
            if steps.last().is_some_and(|s| s.rpm == 0) {
                s.push_str(
                    &format!(
                        "; it stopped spinning again at {} % — check the wiring",
                        steps.last().unwrap().duty
                    ),
                );
            }
            if saved {
                s.push_str(&format!(
                    ". Saved duty_min={m} / duty_max={x} in the config — its curve is clamped to \
                     that range"
                ));
            } else {
                s.push_str(" (nothing to save: the fan responds across the whole 0–100 % span)");
            }
            s
        }
        // A derived range always has a `max` when it has a `min` (see
        // `derive`); the arm exists for exhaustiveness only.
        (Some(_), None) => format!("{fan}: no usable range could be derived from the sweep."),
    }
}

/// Clamp a (possibly hand-edited) duty range into 0..=100 and order it
/// (`min <= max`).
pub fn normalize(min: Option<f64>, max: Option<f64>) -> (Option<f64>, Option<f64>) {
    let mut lo = min.map(|v| v.clamp(0.0, 100.0));
    let mut hi = max.map(|v| v.clamp(0.0, 100.0));
    if lo
        .as_ref()
        .is_some_and(|l| hi.as_ref().is_some_and(|h| l > h))
    {
        std::mem::swap(&mut lo, &mut hi);
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(pairs: &[(u8, u32)]) -> Vec<SweepStep> {
        pairs
            .iter()
            .map(|(duty, rpm)| SweepStep {
                duty: *duty,
                rpm: *rpm,
            })
            .collect()
    }

    #[test]
    fn a_typical_ramp_saturates_before_the_top() {
        // A fan that starts at 20 % and reaches its top speed at 80 %.
        let s = steps(&[
            (0, 0),
            (10, 0),
            (20, 950),
            (30, 1420),
            (40, 1900),
            (50, 2350),
            (60, 2700),
            (70, 2950),
            (80, 3150),
            (90, 3160),
            (100, 3155),
        ]);
        assert_eq!(derive(&s), (Some(20), Some(80), true));
        assert!(worth_saving(Some(20), Some(80)));
    }

    #[test]
    fn still_rising_at_100_is_not_saturated() {
        let s = steps(&[
            (0, 0),
            (10, 400),
            (20, 900),
            (30, 1400),
            (40, 1900),
            (50, 2400),
            (60, 2800),
            (70, 3100),
            (80, 3350),
            (90, 3500),
            (100, 3600),
        ]);
        assert_eq!(derive(&s), (Some(10), Some(100), false));
    }

    #[test]
    fn a_fan_that_never_spins_yields_no_range() {
        let s = steps(&[
            (0, 0),
            (10, 0),
            (20, 0),
            (30, 0),
            (40, 0),
            (50, 0),
            (60, 0),
            (70, 0),
            (80, 0),
            (90, 0),
            (100, 0),
        ]);
        assert_eq!(derive(&s), (None, None, false));
        assert!(!worth_saving(None, None));
    }

    #[test]
    fn a_single_spinning_step_is_a_degenerate_range() {
        // Only spins at 100 %: one spinning step, no stall window.
        let s = steps(&[(0, 0), (50, 0), (100, 100)]);
        assert_eq!(derive(&s), (Some(100), Some(100), false));
        assert!(!worth_saving(Some(100), Some(100)));
    }

    #[test]
    fn small_jitter_is_ignored_but_a_stall_is_detected() {
        // 2 % of 1000 is 20: a 25-rpm step still counts as rising …
        let rising = steps(&[(20, 1000), (30, 1025), (40, 1050)]);
        assert_eq!(derive(&rising), (Some(20), Some(40), false));
        // … a 10-rpm step is noise, and the fan saturates at 20 %.
        let flat = steps(&[(20, 1000), (30, 1010), (40, 1008)]);
        assert_eq!(derive(&flat), (Some(20), Some(20), true));
        // A dip (a flaky connection) also counts as a stall.
        let dip = steps(&[(20, 1000), (30, 990)]);
        assert!(stalled(&dip));
    }

    #[test]
    fn stalled_needs_two_steps_and_a_spinning_fan() {
        assert!(!stalled(&[]), "no steps");
        assert!(!stalled(&steps(&[(10, 950)])), "one step");
        assert!(
            !stalled(&steps(&[(0, 0), (10, 0)])),
            "not spinning is not a stall"
        );
        assert!(stalled(&steps(&[(20, 1000), (30, 1010)])));
        assert!(
            !stalled(&steps(&[(20, 1000), (30, 2000)])),
            "a real rise is no stall"
        );
    }

    #[test]
    fn worth_saving_rejects_the_whole_span_and_single_points() {
        assert!(!worth_saving(None, None));
        assert!(!worth_saving(Some(0), Some(100)));
        assert!(!worth_saving(Some(50), Some(50)));
        assert!(worth_saving(Some(20), Some(80)));
        assert!(worth_saving(Some(0), Some(80)));
    }

    #[test]
    fn normalize_clamps_and_orders_ranges() {
        assert_eq!(normalize(Some(20.0), Some(80.0)), (Some(20.0), Some(80.0)));
        assert_eq!(normalize(Some(-5.0), Some(200.0)), (Some(0.0), Some(100.0)));
        assert_eq!(
            normalize(Some(80.0), Some(20.0)),
            (Some(20.0), Some(80.0)),
            "a hand-edited inverted range is swapped, not rejected"
        );
        assert_eq!(normalize(None, Some(50.0)), (None, Some(50.0)));
        assert_eq!(normalize(None, None), (None, None));
    }

    #[test]
    fn describe_covers_the_outcomes() {
        let empty = describe("pwm1", None, None, false, &[], false);
        assert!(empty.contains("could not take a single reading"), "{empty}");

        let none = describe("pwm1", None, None, false, &steps(&[(0, 0), (10, 0)]), false);
        assert!(none.contains("no fan response"), "{none}");
        assert!(none.contains("probably not connected"), "{none}");

        let range = describe(
            "pwm1",
            Some(20),
            Some(80),
            true,
            &steps(&[(0, 0), (10, 0), (20, 950), (80, 3150)]),
            true,
        );
        assert!(range.contains("between 20 % and 80 %"), "{range}");
        assert!(range.contains("saturates at 80 %"), "{range}");
        assert!(range.contains("duty_min=20 / duty_max=80"), "{range}");

        let full_span = describe(
            "pwm1",
            Some(0),
            Some(100),
            false,
            &steps(&[(0, 950), (100, 3150)]),
            false,
        );
        assert!(full_span.contains("nothing to save"), "{full_span}");

        let stuck = describe(
            "pwm1",
            Some(0),
            Some(0),
            true,
            &steps(&[(0, 1234), (50, 1234)]),
            false,
        );
        assert!(stuck.contains("stuck tach"), "{stuck}");

        let stopped = describe(
            "pwm1",
            Some(20),
            Some(50),
            false,
            &steps(&[(0, 0), (20, 950), (50, 1400), (60, 0)]),
            true,
        );
        assert!(
            stopped.contains("stopped spinning again at 60 %"),
            "{stopped}"
        );
    }
}
