//! Platform-independent walking accounting. Native and browser/Wasm use this
//! implementation; transports and persistence never define counter rules.

pub const SESSION_DEBOUNCE_S: f64 = 3.0;
pub const METRICS: [(i64, f64, f64); 4] = [
    (50, 8.0, 60.0),
    (600, 2.0, 150.0),
    (200, 1.0, 20.0),
    (100, 1.0, 60.0),
];

/// Emit increments over a counter stream; seed is the last accepted earlier
/// counter. Callers own time buckets, session attribution and retention floors.
pub fn increments(
    values: &[i64],
    spike: i64,
    reset_max: i64,
    seed: Option<i64>,
    mut emit: impl FnMut(usize, i64),
) {
    let mut previous = seed;
    for (i, &v) in values.iter().enumerate() {
        if i > 0 && i + 1 < values.len() {
            let (p, n) = (values[i - 1], values[i + 1]);
            if (v.saturating_sub(p) > spike && v.saturating_sub(n) > spike)
                || (p.saturating_sub(v) > spike && n.saturating_sub(v) > spike)
            {
                continue;
            }
        }
        match previous {
            None => {
                if i + 1 < values.len() && v.saturating_sub(values[i + 1]) > spike {
                    continue;
                }
                if v > 0 {
                    emit(i, v);
                }
                previous = Some(v);
            }
            Some(p) => {
                if v > p {
                    emit(i, v.saturating_sub(p));
                    previous = Some(v);
                } else if v < p && (v <= reset_max || (v as i128) * 2 < p as i128) {
                    previous = Some(v);
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct FieldGate {
    /// The envelope anchor: an accepted (value, ts). The allowed value at
    /// `now` is `value + ceiling × (now − ts) + burst`.
    anchor: Option<(u32, f64)>,
    /// A deep decrease (value fell below half the anchor) seen on the
    /// previous field-bearing sample: EITHER a genuine counter reset OR a
    /// one-frame stale-low read — a causal gate cannot tell which, so the
    /// judgement is DEFERRED one frame, mirroring `deglitch_walk`'s
    /// lookahead: if the next value continues the low series, the drop was
    /// a reset and the anchor adopts it; if the next value is back inside
    /// the old envelope, the dip was a stale frame and the old anchor
    /// stands. Without this, one stale-low read would wedge the anchor and
    /// reject minutes of good samples — exactly the bad interaction with
    /// the de-glitcher this comment exists to prevent.
    pending_reset: Option<(u32, f64)>,
}

impl FieldGate {
    pub fn anchor(&self) -> Option<(u32, f64)> {
        self.anchor
    }
    /// Admit or refuse `v` at `now`. Refusal means "strip the field from
    /// this sample"; the anchor is left untouched so a genuinely absurd
    /// stream stays refused (and counted) instead of ratcheting the
    /// envelope up.
    pub fn admit(&mut self, v: u32, now: f64, ceiling: f64, burst: f64) -> bool {
        let Some((mut av, mut ats)) = self.anchor else {
            // First reading of this connection: the baseline. The stored
            // layer judges stale-high openers (`deglitch_walk`); the gate
            // has no context to.
            self.anchor = Some((v, now));
            return true;
        };
        if let Some((pv, pts)) = self.pending_reset.take() {
            // One deferred frame after a deep drop (see the field's doc):
            // does `v` continue the low series?
            if (v as f64) <= pv as f64 + ceiling * (now - pts) + burst {
                // Yes — the drop was a real reset. Adopt it as the anchor.
                self.anchor = Some((pv, pts));
                (av, ats) = (pv, pts);
            }
            // No — the dip was a one-frame stale read; the old anchor stands
            // and `v` is judged against it below.
        }
        let allowed = av as f64 + ceiling * (now - ats) + burst;
        if v as f64 > allowed {
            return false;
        }
        if v < av {
            // Decreases ALWAYS pass (resets are the storage layer's to
            // judge); a deep one starts the one-frame reset deferral.
            if (v as u64) * 2 < av as u64 {
                self.pending_reset = Some((v, now));
            }
        } else if v == av {
            // Idle counter: keep the envelope tight — without this, an idle
            // hour would grow `allowed` by ceiling × 3600 and blind the gate.
            self.anchor = Some((v, now));
        } else if now - ats >= 60.0 {
            self.anchor = Some((v, now));
        }
        true
    }
}

/// Timed debounce shared by ingestion adapters. Missing/unstable input must not
/// be interpreted as a confirmed running transition by the caller.
pub fn held(held: &mut Option<(bool, f64)>, running: bool, now: f64) -> bool {
    match *held {
        Some((r, since)) if r == running && now >= since => now - since >= SESSION_DEBOUNCE_S,
        _ => {
            *held = Some((running, now));
            false
        }
    }
}

// A tiny Wasm ABI avoids any JS reimplementation and a runtime binding dependency.
// Buffers are allocated/freed by this module; the adapter owns their lifetimes.
#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    #[no_mangle]
    pub extern "C" fn alloc(len: usize) -> *mut f64 {
        Box::into_raw(vec![0.0; len].into_boxed_slice()) as *mut f64
    }
    #[no_mangle]
    pub unsafe extern "C" fn release(ptr: *mut f64, len: usize) {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
    #[no_mangle]
    pub unsafe extern "C" fn bank(ptr: *mut f64, len: usize, metric: usize, seed: f64) {
        let rows = std::slice::from_raw_parts_mut(ptr, len * 2);
        let values: Vec<i64> = rows[..len].iter().map(|v| *v as i64).collect();
        rows[len..].fill(0.0);
        increments(
            &values,
            METRICS[metric].0,
            10,
            if seed.is_nan() {
                None
            } else {
                Some(seed as i64)
            },
            |i, d| rows[len + i] = d as f64,
        );
    }
    #[no_mangle]
    pub unsafe extern "C" fn gate(ptr: *mut f64, value: u32, now: f64, metric: usize) -> u32 {
        let s = std::slice::from_raw_parts_mut(ptr, 6);
        let mut gate = FieldGate {
            anchor: if s[0] != 0.0 {
                Some((s[1] as u32, s[2]))
            } else {
                None
            },
            pending_reset: if s[3] != 0.0 {
                Some((s[4] as u32, s[5]))
            } else {
                None
            },
        };
        let accepted = gate.admit(value, now, METRICS[metric].1, METRICS[metric].2);
        if let Some((v, t)) = gate.anchor {
            s[0] = 1.0;
            s[1] = v as f64;
            s[2] = t;
        }
        s[3] = 0.0;
        if let Some((v, t)) = gate.pending_reset {
            s[3] = 1.0;
            s[4] = v as f64;
            s[5] = t;
        }
        accepted as u32
    }
    #[no_mangle]
    pub unsafe extern "C" fn debounce(ptr: *mut f64, running: u32, now: f64) -> u32 {
        let s = std::slice::from_raw_parts_mut(ptr, 3);
        let mut state = if s[0] != 0.0 {
            Some((s[1] != 0.0, s[2]))
        } else {
            None
        };
        let confirmed = held(&mut state, running != 0, now);
        if let Some((r, t)) = state {
            s[0] = 1.0;
            s[1] = r as u32 as f64;
            s[2] = t;
        }
        confirmed as u32
    }
}
