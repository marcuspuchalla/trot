//! SQLite storage — sessions, raw samples, per-minute rollups, daily totals.
//! Ported from `backend/db.py` in the author's own earlier proprietary
//! project (Lifespan SC110 / Treadmill Dashboard, © Marcus Puchalla),
//! relicensed here under GPLv3 by its sole copyright holder. Not
//! third-party work, hence no THIRD-PARTY-NOTICES.md entry. Single connection guarded by a Mutex (our write
//! rate is ~3 Hz, reads are light), WAL mode, foreign keys on.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_ts REAL NOT NULL,
    ended_ts REAL,
    local_date TEXT NOT NULL,
    display_unit TEXT NOT NULL,
    start_steps INTEGER,
    start_duration_s INTEGER,
    steps_end INTEGER,
    duration_s_end INTEGER,
    distance_raw_end INTEGER,
    calories_end INTEGER,
    speed_raw_last INTEGER,
    closed_reason TEXT,
    source TEXT,
    -- The recording device's own de-glitched verdict on what this session
    -- contained. Only the recorder holds the raw samples the de-glitch needs,
    -- so it banks the answer here and every other device sums these columns
    -- instead of re-deriving a different number from the odometer endpoints.
    steps_total INTEGER,
    duration_s_total INTEGER,
    distance_raw_total INTEGER,
    calories_total INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sessions_date ON sessions(local_date);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_ts);

CREATE TABLE IF NOT EXISTS samples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER,
    ts REAL NOT NULL,
    steps INTEGER,
    duration_s INTEGER,
    speed_raw INTEGER,
    distance_raw INTEGER,
    calories INTEGER,
    status INTEGER,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
CREATE INDEX IF NOT EXISTS idx_samples_session ON samples(session_id);
CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(ts);

CREATE TABLE IF NOT EXISTS sample_rollups_1m (
    bucket_ts INTEGER NOT NULL,
    session_id INTEGER,
    steps_delta INTEGER NOT NULL DEFAULT 0,
    distance_raw_delta INTEGER NOT NULL DEFAULT 0,
    calories_delta INTEGER NOT NULL DEFAULT 0,
    duration_s_delta INTEGER NOT NULL DEFAULT 0,
    speed_raw_min INTEGER,
    speed_raw_avg REAL,
    speed_raw_max INTEGER,
    running_samples INTEGER NOT NULL DEFAULT 0,
    total_samples INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_ts, session_id),
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
CREATE INDEX IF NOT EXISTS idx_rollups_1m_ts ON sample_rollups_1m(bucket_ts);

CREATE TABLE IF NOT EXISTS rollup_state (
    kind TEXT PRIMARY KEY,
    last_rolled_ts REAL NOT NULL DEFAULT 0,
    last_run_ts REAL
);

CREATE TABLE IF NOT EXISTS speed_marks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    set_speed REAL NOT NULL,
    unit TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_speed_marks_ts ON speed_marks(ts);
"#;

/// Nominal spacing of PERSISTED raw samples, in seconds.
///
/// The BLE worker polls far faster than this (a poll every ~50 ms plus the radio
/// round trip) but only writes a row this often — see `ble::SAMPLE_MIN_INTERVAL_S`,
/// which is derived from this constant. It is a real unit, not a tuning knob:
/// `duration_running_s` reconstructs "time spent walking" as
/// `count(running samples) * SAMPLE_INTERVAL_S`, so the two must agree or that
/// metric is wrong by their ratio.
pub const SAMPLE_INTERVAL_S: f64 = 1.0;

/// How long raw samples are kept before the retention loop prunes them. History
/// older than this is served from the per-minute rollups. Reported verbatim by
/// `/api/rollup/status`, so it lives here rather than being restated per module.
pub const RETENTION_DAYS: f64 = 7.0;
/// How often the rollup + prune loop runs.
///
/// Was 300 s, which is invisible on the machine doing the walking — `day_totals`
/// reads the raw tail above the rollup floor, so the local screen is always
/// current. It is very visible to another device following over sync: rollups
/// are the bulk of what gets exported, so a follower could sit five minutes
/// behind however fast it polled. Sixty seconds matches the one-minute bucket
/// resolution, so the loop now banks each bucket about as soon as it is
/// complete rather than five at a time.
pub const ROLLUP_INTERVAL_S: f64 = 60.0;

const ROLLUP_RESOLUTION_S: i64 = 60;
const ROLLUP_KIND: &str = "samples_1m";
/// Raw samples older than this are never (re)inserted by `import_dump` — a
/// belt-and-braces guard so a stale peer or an old full backup can't refill
/// pruned history behind the retention loop. Matches the engine's 7-day window.
const IMPORT_MAX_RAW_AGE_S: f64 = 7.0 * 86400.0;
/// How far before `last_rolled` the de-glitch walk re-reads samples purely to
/// establish the previous-value context (so the first new bucket's increment
/// and any boundary spike are judged correctly). Those older buckets are not
/// re-written.
const ROLLUP_DEGLITCH_LOOKBACK_S: f64 = 180.0;

/// Seconds since the Unix epoch. A clock set before 1970 yields 0.0 rather than
/// panicking — the timestamp would be wrong either way, but this is called from
/// the ingest hot path and taking the whole engine down over a misconfigured
/// clock helps nobody. Matches `config::now()`.
pub fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn local_date(ts: f64) -> String {
    // chrono local time, matching Python datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// De-glitched cumulative total over the SC110's noisy free-running odometer.
///
/// The console reports a counter that (a) carries across BLE reconnects, (b)
/// resets to ~0 on a power-cycle, and (c) occasionally emits a single garbage
/// sample. We:
///   - discard isolated spike-and-revert outliers (a value far from BOTH
///     neighbours in the same direction — the signature of a stale frame),
///   - sum genuine positive increments (so real steps walked across a
///     reconnect gap are kept),
///   - treat a drop to `<= reset_max` as a power-cycle reset (the post-reset
///     climb is then counted incrementally), and
///   - ignore any other decrease (a non-reset dip).
///
/// This replaces a naive LAG accumulator that added the full counter value on
/// every decrease, which turned a stale `…1800, 346, 1891…` read into ~1500
/// phantom steps after a reconnect.
///
/// Core pass: calls `emit(i, delta)` once per *accepted* positive increment at
/// sample index `i` (including the first-sample baseline). Day totals and the
/// hourly breakdown both drive off this so they always reconcile.
/// Reference walk over a full sequence. Production reads go through
/// `deglitch_tail`, which applies the same rules from the rollup floor up;
/// this stays as the executable statement of those rules.
#[cfg(test)]
fn deglitch_walk(values: &[i64], spike: i64, reset_max: i64, mut emit: impl FnMut(usize, i64)) {
    let n = values.len();
    let mut prev: Option<i64> = None;
    for i in 0..n {
        let v = values[i];
        // Drop an isolated outlier: far from both neighbours in the same
        // direction (spikes up or down that immediately revert).
        if i > 0 && i + 1 < n {
            let p = values[i - 1];
            let nx = values[i + 1];
            let spike_up = v - p > spike && v - nx > spike;
            let spike_down = p - v > spike && nx - v > spike;
            if spike_up || spike_down {
                continue;
            }
        }
        match prev {
            None => {
                // Drop a stale-HIGH opening frame the next sample contradicts by
                // more than `spike`: the interior spike rule needs both neighbours,
                // but the first sample has only the forward one, so a garbage
                // opening reading (e.g. 5000 before a real 1800) would otherwise be
                // counted as baseline steps. Leave `prev` unset so the next sample
                // becomes the baseline instead.
                if i + 1 < n && v - values[i + 1] > spike {
                    continue;
                }
                // First accepted reading already reflects steps walked today.
                let base = v.max(0);
                if base > 0 {
                    emit(i, base);
                }
                prev = Some(v);
            }
            Some(pv) => {
                let d = v - pv;
                if d > 0 {
                    emit(i, d);
                    prev = Some(v);
                } else if d < 0 && (v <= reset_max || v * 2 < pv) {
                    // Genuine counter reset: dropped to ~0, OR fell by more than
                    // half — the SC110 zeroes its step counter between sessions and
                    // we often catch it after it has already climbed a little
                    // (e.g. 488 -> 42). The post-reset climb is counted from here.
                    prev = Some(v);
                }
                // else: shallow non-reset dip — keep prev, add nothing.
            }
        }
    }
}

/// De-glitched cumulative total — sum of every accepted increment.
#[cfg(test)]
fn deglitch_total(values: &[i64], spike: i64, reset_max: i64) -> i64 {
    let mut total: i64 = 0;
    deglitch_walk(values, spike, reset_max, |_, d| total += d);
    total
}

/// De-glitched increments per (bucket_ts, session_id) for the rollup writer.
/// Walks the continuous cross-session stream (samples must be ordered by ts,id
/// with NULLs pre-filtered) so a stale frame at a session boundary still has
/// neighbour context. Increments only — the starting baseline is not a "step
/// added", matching the analytics range semantics.
fn deglitch_bucketed(
    samples: &[(i64, i64, i64)], // (ts, session_id, value)
    resolution_s: i64,
    spike: i64,
    reset_max: i64,
    // Last accepted value before this window, at ANY age. Without it the walk
    // restarts blind after a sample gap longer than the lookback and the
    // increment accrued across that gap is never banked.
    seed: Option<i64>,
) -> std::collections::HashMap<(i64, i64), i64> {
    let n = samples.len();
    let mut out: std::collections::HashMap<(i64, i64), i64> = std::collections::HashMap::new();
    let mut prev: Option<i64> = seed;
    for i in 0..n {
        let (ts, sess, v) = samples[i];
        if i > 0 && i + 1 < n {
            let p = samples[i - 1].2;
            let nx = samples[i + 1].2;
            if (v - p > spike && v - nx > spike) || (p - v > spike && nx - v > spike) {
                continue;
            }
        }
        match prev {
            None => {
                // No prior context anywhere: this reading is steps ALREADY
                // walked (the belt was moving before the app connected), so it
                // is a baseline that must be banked — `deglitch_walk` banks it,
                // and if the rollups do not, the day total silently shrinks by
                // the pre-connect walk as soon as the rollup loop runs, then
                // becomes unrecoverable once raw is pruned.
                // Same stale-HIGH opening guard as deglitch_walk.
                if i + 1 < n && v - samples[i + 1].2 > spike {
                    continue;
                }
                let base = v.max(0);
                if base > 0 {
                    let bucket = (ts / resolution_s) * resolution_s;
                    *out.entry((bucket, sess)).or_insert(0) += base;
                }
                prev = Some(v);
            }
            Some(pv) => {
                let d = v - pv;
                if d > 0 {
                    let bucket = (ts / resolution_s) * resolution_s;
                    *out.entry((bucket, sess)).or_insert(0) += d;
                    prev = Some(v);
                } else if d < 0 && (v <= reset_max || v * 2 < pv) {
                    prev = Some(v); // reset to ~0 or a drop of more than half
                }
            }
        }
    }
    out
}

/// De-glitched increments for the *un-rolled raw tail* of a day.
///
/// Walks the day's samples exactly like `deglitch_walk`, but only *counts* an
/// accepted increment when the current sample's `ts >= floor` (the rollup
/// `last_rolled_ts`). Samples older than `floor` are used purely as de-glitch
/// *context* (so the increment straddling the rollup boundary is judged and
/// counted correctly against its true predecessor). This lets a day's total be
/// composed as `SUM(rollup deltas below floor) + tail(raw at/above floor)`
/// without double-counting the boundary. Since 0.3.2 the rollups DO bank the
/// first-reading baseline (a day's opening value is steps already walked, and
/// dropping it made the total shrink as soon as anything was rolled), so the
/// tail must not bank it again once `floor > 0` — which is exactly what the
/// `counts` gate below achieves.
///
/// When `floor == 0` (nothing rolled yet) every sample counts *including* the
/// first-reading baseline, so this degrades exactly to the historical
/// `deglitch_total` over the full day — preserving pre-rollup numbers.
fn deglitch_tail(
    samples: &[(f64, i64)], // (ts, value) ordered by ts
    floor: f64,
    spike: i64,
    reset_max: i64,
    mut emit: impl FnMut(usize, i64),
) {
    let n = samples.len();
    let mut prev: Option<i64> = None;
    for i in 0..n {
        let (ts, v) = samples[i];
        if i > 0 && i + 1 < n {
            let p = samples[i - 1].1;
            let nx = samples[i + 1].1;
            if (v - p > spike && v - nx > spike) || (p - v > spike && nx - v > spike) {
                continue;
            }
        }
        let counts = ts >= floor;
        match prev {
            None => {
                // Drop a stale-HIGH opening frame (see `deglitch_walk`): a garbage
                // first reading must not become a baseline. The next sample becomes
                // the baseline instead.
                if i + 1 < n && v - samples[i + 1].1 > spike {
                    continue;
                }
                let base = v.max(0);
                if base > 0 && counts {
                    emit(i, base);
                }
                prev = Some(v);
            }
            Some(pv) => {
                let d = v - pv;
                if d > 0 {
                    if counts {
                        emit(i, d);
                    }
                    prev = Some(v);
                } else if d < 0 && (v <= reset_max || v * 2 < pv) {
                    prev = Some(v);
                }
            }
        }
    }
}

/// Sum of `deglitch_tail`'s counted increments.
fn deglitch_tail_total(samples: &[(f64, i64)], floor: f64, spike: i64, reset_max: i64) -> i64 {
    let mut total = 0i64;
    deglitch_tail(samples, floor, spike, reset_max, |_, d| total += d);
    total
}

/// The rollup floor (`last_rolled_ts`): raw samples below it are already
/// captured in `sample_rollups_1m`, so day/hour/timeseries reads must not
/// double-count them. 0 when nothing has been rolled.
fn raw_floor(c: &Connection) -> f64 {
    c.query_row(
        "SELECT last_rolled_ts FROM rollup_state WHERE kind=?",
        params![ROLLUP_KIND],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(0.0)
}

/// Local hour (0..23) of a unix timestamp.
fn local_hour(ts: f64) -> usize {
    use chrono::{Local, TimeZone, Timelike};
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.hour() as usize)
        .unwrap_or(0)
        .min(23)
}

#[cfg(test)]
thread_local! {
    static TEST_DEVICE_NAME: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Which install this is, for ownership decisions. Sessions are stamped with it
/// at `open_session`, so `source == this_device()` means "we recorded it".
///
/// Compared by plain equality including the empty string: a fresh install whose
/// name the client has not seeded yet stamps its sessions `""` and must still
/// own them. Two devices sharing a name is a pre-existing hazard (they can
/// overwrite each other's rows) and is why the client seeds a distinct label.
fn this_device() -> String {
    #[cfg(test)]
    if let Some(n) = TEST_DEVICE_NAME.with(|c| c.borrow().clone()) {
        return n;
    }
    crate::config::device_name()
}

/// Act as `name` for the rest of this test thread.
#[cfg(test)]
fn set_test_device(name: &str) {
    TEST_DEVICE_NAME.with(|c| *c.borrow_mut() = Some(name.to_string()));
}

/// The four banked totals as stored on a session row, any of which is NULL on a
/// row nobody has banked yet.
type BankedTotals = (Option<i64>, Option<i64>, Option<i64>, Option<i64>);

/// One session's totals, in raw device units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionTotals {
    pub steps: i64,
    pub duration_s: i64,
    pub distance_raw: i64,
    pub calories: i64,
}

/// The de-glitch parameters per metric: (spike, reset_max). Kept in one place so
/// a session total and a day total can never be computed with different ones.
const SPIKE_STEPS: (i64, i64) = (50, 10);
const SPIKE_DURATION: (i64, i64) = (600, 10);
const SPIKE_CALORIES: (i64, i64) = (100, 10);
const SPIKE_DISTANCE: (i64, i64) = (200, 10);

/// De-glitched per-session totals for one local date, for the sessions this
/// device actually recorded.
///
/// The walk stays DAY-WIDE and each accepted increment is attributed to the
/// session that owns its sample. It cannot be split into independent per-session
/// walks: the treadmill console counts across sessions (a day might run 30→113,
/// then 120→150), so the day's opening value is banked once, by the first
/// session, and every later session is worth only what it added on top. Walking
/// each session from scratch re-banks its opening reading and inflates the day
/// by roughly one session total per session.
///
/// By construction the returned values sum to exactly what a day-wide total
/// would be — which is what lets a peer add them up and reach the same number.
/// A session absent from the map is one this device holds no data for.
fn day_session_totals(
    c: &Connection,
    local_date_s: &str,
) -> Result<std::collections::HashMap<i64, SessionTotals>> {
    use std::collections::HashMap;
    let floor = raw_floor(c);
    let mut acc: HashMap<i64, SessionTotals> = HashMap::new();

    // Tier 1: deltas already banked in the per-minute rollups, per session.
    {
        let mut stmt = c.prepare(
            "SELECT r.session_id,
                    COALESCE(SUM(r.steps_delta),0), COALESCE(SUM(r.duration_s_delta),0),
                    COALESCE(SUM(r.distance_raw_delta),0), COALESCE(SUM(r.calories_delta),0)
             FROM sample_rollups_1m r JOIN sessions se ON se.id = r.session_id
             WHERE se.local_date = ? AND r.bucket_ts < ?
             GROUP BY r.session_id",
        )?;
        let mut q = stmt.query(params![local_date_s, floor as i64])?;
        while let Some(r) = q.next()? {
            acc.insert(
                r.get(0)?,
                SessionTotals {
                    steps: r.get(1)?,
                    duration_s: r.get(2)?,
                    distance_raw: r.get(3)?,
                    calories: r.get(4)?,
                },
            );
        }
    }

    // Tier 0: the raw tail at/above the floor, walked once per metric across the
    // whole day so continuity between sessions is preserved.
    let mut steps_v: Vec<(f64, i64)> = Vec::new();
    let mut dur_v: Vec<(f64, i64)> = Vec::new();
    let mut dist_v: Vec<(f64, i64)> = Vec::new();
    let mut cal_v: Vec<(f64, i64)> = Vec::new();
    let (mut steps_sid, mut dur_sid, mut dist_sid, mut cal_sid) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    {
        let mut stmt = c.prepare(
            "SELECT s.ts, s.session_id, s.steps, s.duration_s, s.distance_raw, s.calories
             FROM samples s JOIN sessions se ON se.id = s.session_id
             WHERE se.local_date = ? ORDER BY s.ts, s.id",
        )?;
        let mut q = stmt.query(params![local_date_s])?;
        while let Some(r) = q.next()? {
            let ts: f64 = r.get(0)?;
            let Some(sid) = r.get::<_, Option<i64>>(1)? else {
                continue;
            };
            acc.entry(sid).or_default();
            if let Some(x) = r.get::<_, Option<i64>>(2)? {
                steps_v.push((ts, x));
                steps_sid.push(sid);
            }
            if let Some(x) = r.get::<_, Option<i64>>(3)? {
                dur_v.push((ts, x));
                dur_sid.push(sid);
            }
            if let Some(x) = r.get::<_, Option<i64>>(4)? {
                dist_v.push((ts, x));
                dist_sid.push(sid);
            }
            if let Some(x) = r.get::<_, Option<i64>>(5)? {
                cal_v.push((ts, x));
                cal_sid.push(sid);
            }
        }
    }

    let mut walk = |vals: &[(f64, i64)],
                    sids: &[i64],
                    spike: (i64, i64),
                    pick: fn(&mut SessionTotals) -> &mut i64| {
        deglitch_tail(vals, floor, spike.0, spike.1, |i, d| {
            *pick(acc.entry(sids[i]).or_default()) += d;
        });
    };
    walk(&steps_v, &steps_sid, SPIKE_STEPS, |t| &mut t.steps);
    walk(&dur_v, &dur_sid, SPIKE_DURATION, |t| &mut t.duration_s);
    walk(&dist_v, &dist_sid, SPIKE_DISTANCE, |t| &mut t.distance_raw);
    walk(&cal_v, &cal_sid, SPIKE_CALORIES, |t| &mut t.calories);

    Ok(acc)
}

/// Last-resort total for a session this device never recorded and whose recorder
/// banked nothing (a pre-banking peer, or a legacy summary-only import): the
/// odometer endpoints. An end BELOW the recorded start means the counter reset
/// mid-session and the baseline is stale, so the end value IS the total.
fn session_totals_from_endpoints(end: Option<i64>, start: Option<i64>) -> i64 {
    let end = end.unwrap_or(0);
    let start = start.unwrap_or(0);
    if end < start {
        end.max(0)
    } else {
        end - start
    }
}

/// Write this device's verdict for `sid` onto the session row, so every other
/// device can sum it instead of guessing. No-op when we hold no data for the
/// session (we are not its recorder) or when the stored value is already right.
fn bank_session_totals(c: &Connection, sid: i64) -> Result<()> {
    let date: Option<String> = c
        .query_row(
            "SELECT local_date FROM sessions WHERE id = ?",
            params![sid],
            |r| r.get(0),
        )
        .optional()?;
    let Some(date) = date else { return Ok(()) };
    // Only the recording device may write a verdict. Without this a follower
    // banked its own partial recomputation over the walker's total and then
    // published it to the shared account blob.
    let source: Option<String> = c
        .query_row(
            "SELECT source FROM sessions WHERE id = ?",
            params![sid],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let mine = this_device();
    if let Some(src) = source.as_deref() {
        if src != mine {
            return Ok(());
        }
    }
    let totals = day_session_totals(c, &date)?;
    let Some(t) = totals.get(&sid).copied() else {
        return Ok(());
    };
    write_banked_totals(c, sid, t)
}

fn write_banked_totals(c: &Connection, sid: i64, t: SessionTotals) -> Result<()> {
    let stored: Option<BankedTotals> = c
        .query_row(
            "SELECT steps_total, duration_s_total, distance_raw_total, calories_total
             FROM sessions WHERE id = ?",
            params![sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if stored
        == Some((
            Some(t.steps),
            Some(t.duration_s),
            Some(t.distance_raw),
            Some(t.calories),
        ))
    {
        return Ok(()); // already banked and unchanged — skip the write
    }
    c.execute(
        "UPDATE sessions SET steps_total = ?, duration_s_total = ?,
             distance_raw_total = ?, calories_total = ? WHERE id = ?",
        params![t.steps, t.duration_s, t.distance_raw, t.calories, sid],
    )?;
    Ok(())
}

/// Unix timestamp of local midnight for a "YYYY-MM-DD" date string, using the
/// same local timezone the rest of the engine derives `local_date` in. `None`
/// if the string doesn't parse (or the wall-clock is ambiguous, e.g. a DST gap).
fn local_midnight(date: &str) -> Option<f64> {
    use chrono::{Local, NaiveDate, TimeZone};
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let naive = d.and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp() as f64)
}

/// Human-readable local timestamp for diagnostic dumps ("YYYY-MM-DD HH:MM:SS").
fn iso_local(ts: f64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: i64,
    pub started_ts: f64,
    pub ended_ts: Option<f64>,
    pub local_date: String,
    pub display_unit: String,
    pub start_steps: Option<i64>,
    pub steps_end: Option<i64>,
    pub duration_s_end: Option<i64>,
    pub distance_raw_end: Option<i64>,
    pub calories_end: Option<i64>,
    pub speed_raw_last: Option<i64>,
    pub source: Option<String>,
    /// The recording device's own de-glitched verdict on this session. Every
    /// client sums these rather than re-deriving a number from the endpoints
    /// above, which is what makes two devices agree. Null on rows written by a
    /// peer that predates banking.
    pub steps_total: Option<i64>,
    pub duration_s_total: Option<i64>,
    pub distance_raw_total: Option<i64>,
    pub calories_total: Option<i64>,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // busy_timeout: WAL allows one writer at a time, and a second process (or
        // the rollup transaction overlapping a sample insert) would otherwise fail
        // instantly with SQLITE_BUSY. Wait instead of dropping the write.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        // Additive columns for DBs created before they existed. `CREATE TABLE IF
        // NOT EXISTS` won't add columns to an existing table, so patch them in.
        // Nullable + no default → no row rewrite, safe on a large table.
        ensure_column(&conn, "sessions", "source", "TEXT")?;
        for col in [
            "steps_total",
            "duration_s_total",
            "distance_raw_total",
            "calories_total",
        ] {
            ensure_column(&conn, "sessions", col, "INTEGER")?;
        }
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the connection, recovering from a poisoned mutex (a prior panic
    /// while holding the lock) instead of cascading the panic into every call.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- sessions --------------------------------------------------------

    pub fn open_session(
        &self,
        ts: f64,
        display_unit: &str,
        start_steps: Option<u32>,
        start_duration_s: Option<u32>,
        source: Option<&str>,
    ) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO sessions(started_ts, local_date, display_unit, start_steps, start_duration_s, source)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![ts, local_date(ts), display_unit, start_steps, start_duration_s, source],
        )?;
        Ok(c.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn close_session(
        &self,
        session_id: i64,
        ts: f64,
        steps: Option<u32>,
        duration_s: Option<u32>,
        distance_raw: Option<u32>,
        calories: Option<u32>,
        speed_raw: Option<u32>,
        reason: &str,
    ) -> Result<()> {
        let c = self.conn();
        c.execute(
            "UPDATE sessions SET ended_ts=?, steps_end=?, duration_s_end=?, distance_raw_end=?,
                                 calories_end=?, speed_raw_last=?, closed_reason=?
             WHERE id=? AND ended_ts IS NULL",
            params![
                ts,
                steps,
                duration_s,
                distance_raw,
                calories,
                speed_raw,
                reason,
                session_id
            ],
        )?;
        // Bank our verdict now the session is final, so the row that syncs out
        // carries the same number this device will display for it for ever.
        bank_session_totals(&c, session_id)?;
        Ok(())
    }

    pub fn update_active_session(
        &self,
        session_id: i64,
        steps: Option<u32>,
        duration_s: Option<u32>,
        distance_raw: Option<u32>,
        calories: Option<u32>,
        speed_raw: Option<u32>,
    ) -> Result<()> {
        let c = self.conn();

        // Self-heal a stale baseline before recording progress.
        //
        // open_session stores the telemetry that OPENED the session, but the
        // SC110 zeroes its counters shortly AFTER the belt starts — so that
        // value is often still the PREVIOUS session's total (observed:
        // start_steps=765 on a session whose own samples run 0→87). Left alone
        // it makes steps_end - start_steps negative, which shows as a session
        // of 0 steps in the UI and in `trot log`.
        //
        // Within the first seconds of a session, a reading near zero — or below
        // half the recorded baseline — IS the true post-reset baseline. The 5 s
        // window is what stops a later stale low frame rebaselining a live
        // session, and the `< start_steps` guard leaves genuine mid-walk
        // adoption (counter never resets, next reading is higher) untouched.
        let now = now_ts();
        if let Some(v) = steps {
            c.execute(
                "UPDATE sessions SET start_steps = ?1
                 WHERE id = ?2 AND ended_ts IS NULL
                   AND start_steps IS NOT NULL AND ?1 < start_steps
                   AND (?1 <= 10 OR ?1 * 2 < start_steps)
                   AND ?3 - started_ts <= 5.0",
                params![v as i64, session_id, now],
            )?;
        }
        if let Some(v) = duration_s {
            c.execute(
                "UPDATE sessions SET start_duration_s = ?1
                 WHERE id = ?2 AND ended_ts IS NULL
                   AND start_duration_s IS NOT NULL AND ?1 < start_duration_s
                   AND (?1 <= 10 OR ?1 * 2 < start_duration_s)
                   AND ?3 - started_ts <= 5.0",
                params![v as i64, session_id, now],
            )?;
        }

        c.execute(
            "UPDATE sessions SET steps_end=?, duration_s_end=?, distance_raw_end=?,
                                 calories_end=?, speed_raw_last=? WHERE id=?",
            params![
                steps,
                duration_s,
                distance_raw,
                calories,
                speed_raw,
                session_id
            ],
        )?;
        Ok(())
    }

    /// Close sessions this device left open — and ONLY this device's.
    ///
    /// `mine` is the local device_name. Sessions recorded elsewhere arrive here
    /// through cloud sync and are open because that device is still walking; a
    /// restart here must not invent an end time for a walk happening on someone
    /// else's phone. It used to, and then published the fabricated row back to
    /// the cloud, so a follower restarting mid-walk truncated the walker's
    /// session for every device on the account.
    ///
    /// Sessions with no source predate device attribution and can only be ours.
    pub fn close_stale_active(&self, reason: &str, mine: Option<&str>) -> Result<usize> {
        let c = self.conn();
        let n = match mine {
            Some(name) if !name.is_empty() => c.execute(
                "UPDATE sessions SET ended_ts=?, closed_reason=?
                 WHERE ended_ts IS NULL AND (source IS NULL OR source = ?)",
                params![now_ts(), reason, name],
            )?,
            // No identity of our own yet: only close unattributed sessions,
            // never another device's.
            _ => c.execute(
                "UPDATE sessions SET ended_ts=?, closed_reason=?
                 WHERE ended_ts IS NULL AND source IS NULL",
                params![now_ts(), reason],
            )?,
        };
        Ok(n)
    }

    /// Is another device recording a walk right now?
    ///
    /// A session with no end time whose source is not ours, touched recently.
    /// The freshness bound matters because a remote session only ends here when
    /// its end arrives by sync — a phone that goes offline mid-walk would
    /// otherwise look like it is still walking for ever.
    pub fn remote_active(&self, mine: &str, fresh_secs: f64) -> Result<bool> {
        let c = self.conn();
        // Freshness has to be measured from the most recent EVIDENCE of the
        // walk, not from when it started. Testing `started_ts` answers "did a
        // walk on another device begin recently", which is a different question
        // and stays true long after the walking stopped: a session only gets an
        // `ended_ts` once the other device's close reaches us, and if that sync
        // is delayed — a backgrounded app, a dropped link — the row sits open
        // and this reported a walk in progress for the rest of the window.
        //
        // Any sample or rollup bucket we hold for the session is proof somebody
        // was moving at that moment. A rollup bucket is stamped at its start, so
        // it counts until the end of its minute.
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM sessions se
             WHERE se.ended_ts IS NULL
               AND se.source IS NOT NULL AND se.source <> ''
               AND se.source <> ?
               AND MAX(
                     COALESCE((SELECT MAX(s.ts) FROM samples s
                               WHERE s.session_id = se.id), 0),
                     COALESCE((SELECT MAX(r.bucket_ts) + ? FROM sample_rollups_1m r
                               WHERE r.session_id = se.id), 0),
                     se.started_ts
                   ) > ?",
            params![mine, ROLLUP_RESOLUTION_S, now_ts() - fresh_secs],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn list_sessions(&self, limit: i64) -> Result<Vec<Session>> {
        let c = self.conn();
        let mut stmt = c.prepare("SELECT * FROM sessions ORDER BY started_ts DESC LIMIT ?")?;
        let rows = stmt
            .query_map(params![limit], row_to_session)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_session(&self, id: i64) -> Result<Option<Session>> {
        let c = self.conn();
        let row = c
            .query_row(
                "SELECT * FROM sessions WHERE id=?",
                params![id],
                row_to_session,
            )
            .optional()?;
        Ok(row)
    }

    // --- samples ---------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_sample(
        &self,
        session_id: Option<i64>,
        ts: f64,
        steps: Option<u32>,
        duration_s: Option<u32>,
        speed_raw: Option<u32>,
        distance_raw: Option<u32>,
        calories: Option<u32>,
        status: Option<u8>,
    ) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT INTO samples(session_id, ts, steps, duration_s, speed_raw, distance_raw, calories, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![session_id, ts, steps, duration_s, speed_raw, distance_raw, calories, status],
        )?;
        Ok(())
    }

    // --- aggregates ------------------------------------------------------

    /// Per-day totals from the raw samples via a de-glitched odometer
    /// accumulator, falling back to SUM(end-start) from session rows for any
    /// metric that has no samples. See `deglitch_total` for why the naive
    /// LAG/`ELSE value` accumulator over-counted: a single stale BLE frame
    /// (e.g. a steps reading of 346 wedged between 1800 and 1891 after a
    /// reconnect) injected ~1500 phantom steps.
    pub fn day_totals(&self, local_date_s: &str) -> Result<Value> {
        let c = self.conn();

        // One rule, everywhere: a day is the sum of its sessions, and a session
        // is worth whatever its RECORDER says it is worth.
        //
        // De-glitching needs the raw samples, and only the device that recorded
        // a session ever has them — a synced peer receives session rows and
        // nothing else. So each device computes its own sessions from samples
        // (and banks the answer on the row, which is what sync carries), and
        // takes every other device's sessions at the banked value. Before this,
        // the recorder summed de-glitched sample deltas while a follower summed
        // odometer endpoints: two different algorithms over two different
        // datasets, which agreed only by luck.
        // Everything this device holds raw data for on this date, decomposed per
        // session from one day-wide de-glitched walk. Only consulted for sessions
        // this device actually RECORDED — see the authority rule below.
        let ours = day_session_totals(&c, local_date_s)?;
        let mine = this_device();

        let mut rows = c.prepare(
            "SELECT id, steps_total, duration_s_total, distance_raw_total, calories_total,
                    steps_end, start_steps, duration_s_end, start_duration_s,
                    distance_raw_end, calories_end, source
             FROM sessions WHERE local_date = ? ORDER BY started_ts, id",
        )?;
        let mut q = rows.query(params![local_date_s])?;

        let mut sessions = 0i64;
        let (mut steps, mut duration_s, mut distance_raw, mut calories) = (0i64, 0i64, 0i64, 0i64);
        // Sessions whose banked totals are missing or out of date. Written after
        // the read loop so the statement is no longer borrowing the connection —
        // this is also what backfills history recorded before banking existed.
        let mut to_bank: Vec<(i64, SessionTotals)> = Vec::new();

        while let Some(r) = q.next()? {
            sessions += 1;
            let sid: i64 = r.get(0)?;
            let source: Option<String> = r.get(11)?;
            let banked = (
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            );

            // Authority is decided by WHO RECORDED the session, never by how much
            // of it we happen to hold. Holding data is not the same as having
            // recorded it: a follower imports another device's sessions, rollups
            // and (while live-following) its raw tail, and would otherwise
            // recompute that walk from a partial copy — against its OWN rollup
            // floor, which stalls as soon as it stops recording anything itself.
            // It then displayed a number that could never rise again, wrote it
            // over the recorder's verdict, and pushed that back to the account,
            // so any device bootstrapping from the blob inherited it. That was
            // the permanent Mac-vs-iPhone disagreement.
            //
            // A NULL source with no banked total is the one exception: history
            // recorded here before attribution existed, which we may still
            // recompute and backfill.
            // A NULL source is history from before attribution existed, which
            // only ever means local history — the engine treats it as ours
            // everywhere else too (`close_stale_active`). Deciding this on
            // whether a total happens to be banked would be worse than useless:
            // the first read banks it, and the device would then lose authority
            // over its own session and stop noticing that it had grown.
            let recorded_here = match source.as_deref() {
                Some(src) => src == mine,
                None => true,
            };

            let t = match ours.get(&sid).copied().filter(|_| recorded_here) {
                // We recorded it: our samples are the authority.
                Some(t) => {
                    if banked
                        != (
                            Some(t.steps),
                            Some(t.duration_s),
                            Some(t.distance_raw),
                            Some(t.calories),
                        )
                    {
                        to_bank.push((sid, t));
                    }
                    t
                }
                // Someone else recorded it. Take their banked verdict; fall back
                // to the odometer endpoints only for rows that predate banking.
                None => SessionTotals {
                    steps: banked.0.unwrap_or_else(|| {
                        session_totals_from_endpoints(
                            r.get::<_, Option<i64>>(5).unwrap_or(None),
                            r.get::<_, Option<i64>>(6).unwrap_or(None),
                        )
                    }),
                    duration_s: banked.1.unwrap_or_else(|| {
                        session_totals_from_endpoints(
                            r.get::<_, Option<i64>>(7).unwrap_or(None),
                            r.get::<_, Option<i64>>(8).unwrap_or(None),
                        )
                    }),
                    distance_raw: banked.2.unwrap_or_else(|| {
                        session_totals_from_endpoints(
                            r.get::<_, Option<i64>>(9).unwrap_or(None),
                            None,
                        )
                    }),
                    calories: banked.3.unwrap_or_else(|| {
                        session_totals_from_endpoints(
                            r.get::<_, Option<i64>>(10).unwrap_or(None),
                            None,
                        )
                    }),
                },
            };
            steps += t.steps;
            duration_s += t.duration_s;
            distance_raw += t.distance_raw;
            calories += t.calories;
        }
        drop(q);
        drop(rows);
        for (sid, t) in to_bank {
            write_banked_totals(&c, sid, t)?;
        }

        Ok(json!({
            "sessions": sessions,
            "steps": steps,
            "duration_s": duration_s,
            "calories": calories,
            "distance_raw": distance_raw,
        }))
    }

    /// Mean speed_raw across moving samples (>0) for the day.
    pub fn day_avg_speed_raw(&self, local_date_s: &str) -> Result<Option<f64>> {
        let c = self.conn();
        let row: Option<(Option<f64>, i64)> = c
            .query_row(
                "SELECT AVG(s.speed_raw) AS avg_raw, COUNT(*) AS n
                 FROM samples s JOIN sessions se ON se.id = s.session_id
                 WHERE se.local_date = ? AND s.speed_raw IS NOT NULL AND s.speed_raw > 0",
                params![local_date_s],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(avg), n)) if n > 0 => Some(avg),
            _ => None,
        })
    }

    /// Daily step totals grouped by recording device (`source`) for every local
    /// day >= `since_local_date`. Built from the per-minute rollups (de-glitched
    /// and permanent), so past days are exact; today lags by however much of the
    /// current minute has not been rolled up yet — `/api/steps/by-device` returns
    /// `complete_through_ts` so a client can say so. Sessions recorded before
    /// device attribution have no source and group under an empty string
    /// (surfaced as "Unknown" by the client).
    ///
    /// `source` is captured when the session opens, so renaming this install
    /// splits its history between the old and new label rather than rewriting it.
    pub fn steps_by_device(&self, since_local_date: &str) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut stmt = c.prepare(
            "SELECT se.local_date AS d,
                    COALESCE(NULLIF(se.source, ''), '') AS src,
                    COALESCE(SUM(r.steps_delta), 0) AS steps
             FROM sessions se
             JOIN sample_rollups_1m r ON r.session_id = se.id
             WHERE se.local_date >= ?
             GROUP BY se.local_date, src
             HAVING steps > 0
             ORDER BY d DESC, steps DESC",
        )?;
        let rows = stmt
            .query_map(params![since_local_date], |r| {
                Ok(json!({
                    "date": r.get::<_, String>(0)?,
                    "source": r.get::<_, String>(1)?,
                    "steps": r.get::<_, i64>(2)?,
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Steps added per hour on the given date (24 buckets, "00".."23").
    ///
    /// Drives off the same `deglitch_walk` as `day_totals`, attributing each
    /// accepted increment to the local hour of its sample — so the bars sum to
    /// the day's step total and a stale frame can't spike a single hour (this is
    /// what made one afternoon hour show the day's max after a reconnect).
    pub fn hourly_steps(&self, local_date_s: &str) -> Result<Vec<Value>> {
        let mut buckets = [0i64; 24];
        let c = self.conn();
        let floor = raw_floor(&c);

        // Rolled hours: SUM(steps_delta) grouped by the local hour of the bucket.
        {
            let mut rstmt = c.prepare(
                "SELECT CAST(strftime('%H', datetime(r.bucket_ts, 'unixepoch', 'localtime')) AS INTEGER) AS hour,
                        COALESCE(SUM(r.steps_delta), 0)
                 FROM sample_rollups_1m r JOIN sessions se ON se.id = r.session_id
                 WHERE se.local_date = ? AND r.bucket_ts < ?
                 GROUP BY hour",
            )?;
            let mut rows = rstmt.query(params![local_date_s, floor as i64])?;
            while let Some(r) = rows.next()? {
                let h: i64 = r.get(0)?;
                buckets[h.clamp(0, 23) as usize] += r.get::<_, i64>(1)?;
            }
        }

        // Raw tail: increments at/after `floor`, attributed to the local hour of
        // their sample. Same (spike, reset_max) as day_totals so bars reconcile.
        let mut samples: Vec<(f64, i64)> = Vec::new();
        {
            let mut sstmt = c.prepare(
                "SELECT s.ts, s.steps
                 FROM samples s JOIN sessions se ON se.id = s.session_id
                 WHERE se.local_date = ? AND s.steps IS NOT NULL
                 ORDER BY s.ts, s.id",
            )?;
            let mut rows = sstmt.query(params![local_date_s])?;
            while let Some(r) = rows.next()? {
                samples.push((r.get(0)?, r.get(1)?));
            }
        }
        deglitch_tail(&samples, floor, 50, 10, |i, d| {
            buckets[local_hour(samples[i].0)] += d;
        });

        Ok((0..24)
            .map(|h| json!({"hour": format!("{h:02}"), "steps": buckets[h]}))
            .collect())
    }

    /// Wipe every data table (sessions, samples, rollups, speed marks, rollup
    /// state). Used by the "reset to empty" flow after a snapshot has been saved.
    pub fn wipe_all(&self) -> Result<()> {
        let c = self.conn();
        c.execute_batch(
            "DELETE FROM samples;
             DELETE FROM sample_rollups_1m;
             DELETE FROM speed_marks;
             DELETE FROM sessions;
             DELETE FROM rollup_state;",
        )?;
        Ok(())
    }

    /// Record the speed the user has dialed on the treadmill, timestamped, so a
    /// human-known set speed can be correlated against the device's averaged
    /// `0x82` reading (the SC110 doesn't broadcast the instantaneous set speed).
    pub fn insert_speed_mark(&self, set_speed: f64, unit: &str) -> Result<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO speed_marks(ts, set_speed, unit) VALUES (?, ?, ?)",
            params![now_ts(), set_speed, unit],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Most recent speed marks (newest first) for display + diagnostics.
    pub fn recent_speed_marks(&self, limit: i64) -> Result<Vec<Value>> {
        let c = self.conn();
        let mut stmt =
            c.prepare("SELECT ts, set_speed, unit FROM speed_marks ORDER BY ts DESC LIMIT ?")?;
        let rows = stmt
            .query_map(params![limit], |r| {
                let ts: f64 = r.get(0)?;
                Ok(json!({
                    "ts": ts,
                    "iso": iso_local(ts),
                    "set_speed": r.get::<_, f64>(1)?,
                    "unit": r.get::<_, String>(2)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Diagnostic dump for a single local date: the raw sessions, every sample
    /// in accumulator order, and the per-minute rollups, plus the computed day
    /// totals and hourly buckets. Read-only support tool — lets us reconstruct
    /// exactly what the device counter reported across the day (e.g. to find a
    /// double-count after a crash/reconnect).
    pub fn diag_day(&self, local_date_s: &str) -> Result<Value> {
        let sessions: Vec<Value>;
        let samples: Vec<Value>;
        let rollups: Vec<Value>;
        {
            let c = self.conn();

            let mut sstmt = c.prepare(
                "SELECT id, started_ts, ended_ts, local_date, display_unit, start_steps,
                        start_duration_s, steps_end, duration_s_end, distance_raw_end,
                        calories_end, speed_raw_last, closed_reason
                 FROM sessions WHERE local_date = ? ORDER BY started_ts, id",
            )?;
            sessions = sstmt
                .query_map(params![local_date_s], |r| {
                    let started: f64 = r.get(1)?;
                    let ended: Option<f64> = r.get(2)?;
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "started_ts": started,
                        "started_iso": iso_local(started),
                        "ended_ts": ended,
                        "ended_iso": ended.map(iso_local),
                        "local_date": r.get::<_, String>(3)?,
                        "display_unit": r.get::<_, String>(4)?,
                        "start_steps": r.get::<_, Option<i64>>(5)?,
                        "start_duration_s": r.get::<_, Option<i64>>(6)?,
                        "steps_end": r.get::<_, Option<i64>>(7)?,
                        "duration_s_end": r.get::<_, Option<i64>>(8)?,
                        "distance_raw_end": r.get::<_, Option<i64>>(9)?,
                        "calories_end": r.get::<_, Option<i64>>(10)?,
                        "speed_raw_last": r.get::<_, Option<i64>>(11)?,
                        "closed_reason": r.get::<_, Option<String>>(12)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            // Every raw sample for the day, ordered exactly as day_totals walks them.
            let mut smt = c.prepare(
                "SELECT s.id, s.session_id, s.ts, s.steps, s.duration_s, s.speed_raw,
                        s.distance_raw, s.calories, s.status
                 FROM samples s JOIN sessions se ON se.id = s.session_id
                 WHERE se.local_date = ? ORDER BY s.ts, s.id",
            )?;
            samples = smt
                .query_map(params![local_date_s], |r| {
                    let ts: f64 = r.get(2)?;
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "session_id": r.get::<_, Option<i64>>(1)?,
                        "ts": ts,
                        "iso": iso_local(ts),
                        "steps": r.get::<_, Option<i64>>(3)?,
                        "duration_s": r.get::<_, Option<i64>>(4)?,
                        "speed_raw": r.get::<_, Option<i64>>(5)?,
                        "distance_raw": r.get::<_, Option<i64>>(6)?,
                        "calories": r.get::<_, Option<i64>>(7)?,
                        "status": r.get::<_, Option<i64>>(8)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut rmt = c.prepare(
                "SELECT r.bucket_ts, r.session_id, r.steps_delta, r.distance_raw_delta,
                        r.calories_delta, r.duration_s_delta, r.speed_raw_min,
                        r.speed_raw_avg, r.speed_raw_max, r.running_samples, r.total_samples
                 FROM sample_rollups_1m r JOIN sessions se ON se.id = r.session_id
                 WHERE se.local_date = ? ORDER BY r.bucket_ts",
            )?;
            rollups = rmt
                .query_map(params![local_date_s], |r| {
                    let bucket: i64 = r.get(0)?;
                    Ok(json!({
                        "bucket_ts": bucket,
                        "bucket_iso": iso_local(bucket as f64),
                        "session_id": r.get::<_, Option<i64>>(1)?,
                        "steps_delta": r.get::<_, i64>(2)?,
                        "distance_raw_delta": r.get::<_, i64>(3)?,
                        "calories_delta": r.get::<_, i64>(4)?,
                        "duration_s_delta": r.get::<_, i64>(5)?,
                        "speed_raw_min": r.get::<_, Option<i64>>(6)?,
                        "speed_raw_avg": r.get::<_, Option<f64>>(7)?,
                        "speed_raw_max": r.get::<_, Option<i64>>(8)?,
                        "running_samples": r.get::<_, i64>(9)?,
                        "total_samples": r.get::<_, i64>(10)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        } // release the connection lock before re-locking in day_totals/hourly_steps

        let day_totals = self.day_totals(local_date_s)?;
        let hourly_steps = self.hourly_steps(local_date_s)?;

        Ok(json!({
            "date": local_date_s,
            "day_totals": day_totals,
            "hourly_steps": hourly_steps,
            "sessions": sessions,
            "samples": samples,
            "rollups": rollups,
        }))
    }

    // --- analytics timeseries -------------------------------------------

    fn bucket_expr(ts_col: &str, resolution_s: i64) -> String {
        if resolution_s >= 86400 {
            format!("CAST(strftime('%s', date({ts_col}, 'unixepoch', 'localtime')) AS INTEGER)")
        } else {
            format!("(CAST({ts_col} AS INTEGER) / {resolution_s}) * {resolution_s}")
        }
    }

    /// Bucketed timeseries for charting, merging raw samples + per-minute rollups.
    /// Ported from Python `timeseries`.
    pub fn timeseries(
        &self,
        metric: &str,
        resolution_s: i64,
        start_ts: f64,
        end_ts: f64,
    ) -> Result<Vec<Value>> {
        let raw_bucket = Self::bucket_expr("s.ts", resolution_s);
        let roll_bucket = Self::bucket_expr("r.bucket_ts", resolution_s);

        let c = self.conn();
        let raw_floor: f64 = c
            .query_row(
                "SELECT last_rolled_ts FROM rollup_state WHERE kind=?",
                params![ROLLUP_KIND],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0.0);
        let effective_start = start_ts.max(raw_floor);

        // merge helpers
        let mut merged: std::collections::BTreeMap<i64, (f64, f64)> =
            std::collections::BTreeMap::new();

        match metric {
            "steps" | "calories" | "distance_raw" => {
                let (col, delta_col, spike, reset) = match metric {
                    "steps" => ("s.steps", "r.steps_delta", 50i64, 10i64),
                    "calories" => ("s.calories", "r.calories_delta", 100i64, 10i64),
                    _ => ("s.distance_raw", "r.distance_raw_delta", 200i64, 10i64),
                };

                // Rollup tier (below the raw floor): already de-glitched deltas.
                let roll_sql = format!(
                    "SELECT {roll_bucket} AS bucket_ts, SUM({delta_col}) AS value
                     FROM sample_rollups_1m r WHERE r.bucket_ts >= ? AND r.bucket_ts < ? GROUP BY bucket_ts"
                );
                Self::accumulate_sum(&c, &roll_sql, start_ts, end_ts, &mut merged)?;

                // Raw tail (at/after the floor): de-glitch it the SAME way the
                // rollup writer does, instead of the old `MAX(col) - MIN(col)` per
                // bucket — that let a single stale frame (e.g. 346 wedged between
                // 1800 and 1891) spike a chart bucket that the day/hour views
                // correctly suppress. We walk the continuous stream (increments
                // only, no first-sample baseline — matching stored rollups) and
                // bucket each accepted increment the same way the SQL would.
                let bucket_of = |ts: f64| -> i64 {
                    if resolution_s >= 86400 {
                        local_midnight(&local_date(ts))
                            .map(|m| m as i64)
                            .unwrap_or((ts as i64 / resolution_s) * resolution_s)
                    } else {
                        (ts as i64 / resolution_s) * resolution_s
                    }
                };
                let mut samples: Vec<(f64, i64)> = Vec::new();
                {
                    let raw_sql = format!(
                        "SELECT s.ts, {col} FROM samples s
                         WHERE s.ts >= ? AND s.ts < ? AND {col} IS NOT NULL AND s.session_id IS NOT NULL
                         ORDER BY s.ts, s.id"
                    );
                    let mut stmt = c.prepare(&raw_sql)?;
                    let mut rows = stmt.query(params![effective_start, end_ts])?;
                    while let Some(r) = rows.next()? {
                        samples.push((r.get(0)?, r.get(1)?));
                    }
                }
                let n = samples.len();
                let mut prev: Option<i64> = None;
                for i in 0..n {
                    let (ts, v) = samples[i];
                    if i > 0 && i + 1 < n {
                        let p = samples[i - 1].1;
                        let nx = samples[i + 1].1;
                        if (v - p > spike && v - nx > spike) || (p - v > spike && nx - v > spike) {
                            continue;
                        }
                    }
                    match prev {
                        None => prev = Some(v),
                        Some(pv) => {
                            let d = v - pv;
                            if d > 0 {
                                merged.entry(bucket_of(ts)).or_insert((0.0, 0.0)).0 += d as f64;
                                prev = Some(v);
                            } else if d < 0 && (v <= reset || v * 2 < pv) {
                                prev = Some(v);
                            }
                        }
                    }
                }

                Ok(merged
                    .into_iter()
                    .map(|(ts, (v, _))| json!({"bucket_ts": ts, "value": v}))
                    .collect())
            }
            "speed_raw" => {
                let raw_sql = format!(
                    "SELECT {raw_bucket} AS bucket_ts, SUM(speed_raw) AS sum_v, COUNT(*) AS n
                     FROM samples s WHERE s.ts >= ? AND s.ts < ? AND speed_raw IS NOT NULL AND speed_raw > 0
                     GROUP BY bucket_ts"
                );
                let roll_sql = format!(
                    "SELECT {roll_bucket} AS bucket_ts, SUM(speed_raw_avg * running_samples) AS sum_v,
                            SUM(running_samples) AS n
                     FROM sample_rollups_1m r WHERE r.bucket_ts >= ? AND r.bucket_ts < ?
                       AND speed_raw_avg IS NOT NULL GROUP BY bucket_ts"
                );
                Self::accumulate_avg(&c, &raw_sql, effective_start, end_ts, &mut merged)?;
                Self::accumulate_avg(&c, &roll_sql, start_ts, end_ts, &mut merged)?;
                Ok(merged
                    .into_iter()
                    .map(|(ts, (s, n))| {
                        json!({"bucket_ts": ts, "value": if n != 0.0 { s / n } else { 0.0 }})
                    })
                    .collect())
            }
            "duration_running_s" => {
                // In-session samples only — the SAME definition the rollup
                // writer uses for `running_samples` (see `aggregate_and_upsert`
                // below). This filter is load-bearing twice over:
                //
                // * Consistency across the rollup floor. Without it, running
                //   time counted from un-attributed raw samples showed in
                //   recent chart buckets and then VANISHED once the rollup ran
                //   (rollups only ever count in-session samples) — walking
                //   time evaporating from the chart.
                // * `status = 3` alone is not "walking". The stored byte is
                //   the contract's presentation value, and `BeltState::Other`
                //   passes unrecognised device bytes through raw — an unknown
                //   byte of 0x03 stores as status 3 without ever opening a
                //   session (see `BeltState::Other`'s rustdoc in drivers/mod.rs).
                //   Session attribution is the engine's judgement that the belt
                //   was genuinely running; counting only attributed samples
                //   keeps that judgement authoritative here too.
                let raw_sql = format!(
                    "SELECT {raw_bucket} AS bucket_ts, SUM(CASE WHEN status = 3 THEN 1 ELSE 0 END) * {SAMPLE_INTERVAL_S} AS value
                     FROM samples s WHERE s.ts >= ? AND s.ts < ? AND s.session_id IS NOT NULL GROUP BY bucket_ts"
                );
                let roll_sql = format!(
                    "SELECT {roll_bucket} AS bucket_ts, SUM(running_samples) * {SAMPLE_INTERVAL_S} AS value
                     FROM sample_rollups_1m r WHERE r.bucket_ts >= ? AND r.bucket_ts < ? GROUP BY bucket_ts"
                );
                Self::accumulate_sum(&c, &raw_sql, effective_start, end_ts, &mut merged)?;
                Self::accumulate_sum(&c, &roll_sql, start_ts, end_ts, &mut merged)?;
                Ok(merged
                    .into_iter()
                    .map(|(ts, (v, _))| json!({"bucket_ts": ts, "value": v}))
                    .collect())
            }
            other => anyhow::bail!("unknown metric: {other}"),
        }
    }

    fn accumulate_sum(
        c: &Connection,
        sql: &str,
        a: f64,
        b: f64,
        merged: &mut std::collections::BTreeMap<i64, (f64, f64)>,
    ) -> Result<()> {
        let mut stmt = c.prepare(sql)?;
        let rows = stmt.query_map(params![a, b], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
            ))
        })?;
        for row in rows {
            let (ts, v) = row?;
            merged.entry(ts).or_insert((0.0, 0.0)).0 += v;
        }
        Ok(())
    }

    fn accumulate_avg(
        c: &Connection,
        sql: &str,
        a: f64,
        b: f64,
        merged: &mut std::collections::BTreeMap<i64, (f64, f64)>,
    ) -> Result<()> {
        let mut stmt = c.prepare(sql)?;
        let rows = stmt.query_map(params![a, b], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            ))
        })?;
        for row in rows {
            let (ts, s, n) = row?;
            let e = merged.entry(ts).or_insert((0.0, 0.0));
            e.0 += s;
            e.1 += n;
        }
        Ok(())
    }

    // --- rollups / retention --------------------------------------------

    pub fn rollup_status(&self) -> Result<Value> {
        let c = self.conn();
        let state: Option<(f64, Option<f64>)> = c
            .query_row(
                "SELECT last_rolled_ts, last_run_ts FROM rollup_state WHERE kind=?",
                params![ROLLUP_KIND],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (last_rolled, last_run) = state.unwrap_or((0.0, None));
        let raw_count: i64 = c.query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))?;
        let rollup_count: i64 =
            c.query_row("SELECT COUNT(*) FROM sample_rollups_1m", [], |r| r.get(0))?;
        let oldest_raw: Option<f64> =
            c.query_row("SELECT MIN(ts) FROM samples", [], |r| r.get(0))?;
        Ok(json!({
            "last_rolled_ts": last_rolled,
            "last_run_ts": last_run,
            "raw_samples": raw_count,
            "rollup_buckets": rollup_count,
            "oldest_raw_ts": oldest_raw,
        }))
    }

    /// Aggregate unprocessed raw samples into per-minute buckets. Idempotent via
    /// rollup_state.last_rolled_ts. Returns buckets_written.
    pub fn rollup_samples(&self) -> Result<Value> {
        self.rollup_samples_at(now_ts())
    }

    /// `rollup_samples` with an injectable clock, so the bucket-boundary
    /// behaviour can actually be tested.
    pub fn rollup_samples_at(&self, now: f64) -> Result<Value> {
        let res = ROLLUP_RESOLUTION_S as f64;
        // Roll only buckets that are COMPLETE — i.e. stop at the start of the
        // minute still being written to.
        //
        // This used to be `now - ROLLUP_RESOLUTION_S`, which lands in the MIDDLE
        // of a bucket. That bucket was then written from just the samples seen
        // so far, and `last_rolled` advanced past its END, so the rest of that
        // minute was never rolled. Because the upsert REPLACES `steps_delta`
        // rather than adding to it, the partial value became permanent — one
        // truncated minute per rollup run, silently under-counting the day
        // forever. Observed in the wild at ~28% of a minute's samples retained.
        let cutoff = (now / res).floor() * res;
        let mut c = self.conn();
        let last_rolled: f64 = c
            .query_row(
                "SELECT last_rolled_ts FROM rollup_state WHERE kind=?",
                params![ROLLUP_KIND],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0.0);

        if cutoff <= last_rolled {
            c.execute(
                "INSERT INTO rollup_state(kind, last_rolled_ts, last_run_ts) VALUES (?, ?, ?)
                 ON CONFLICT(kind) DO UPDATE SET last_run_ts=excluded.last_run_ts",
                params![ROLLUP_KIND, last_rolled, now],
            )?;
            return Ok(
                json!({"buckets_written": 0, "last_rolled_ts": last_rolled, "cutoff_ts": cutoff}),
            );
        }

        let tx = c.transaction()?;
        // Strict incremental window (last_rolled, cutoff]; a small lookback only
        // gives the de-glitch walk prior-value context across the window edge.
        let lookback = (last_rolled - ROLLUP_DEGLITCH_LOOKBACK_S).max(0.0);
        let (written, max_bucket_end) =
            Self::aggregate_and_upsert(&tx, last_rolled, cutoff, lookback)?;
        let new_mark = max_bucket_end.max(last_rolled);
        tx.execute(
            "INSERT INTO rollup_state(kind, last_rolled_ts, last_run_ts) VALUES (?, ?, ?)
             ON CONFLICT(kind) DO UPDATE SET last_rolled_ts=excluded.last_rolled_ts, last_run_ts=excluded.last_run_ts",
            params![ROLLUP_KIND, new_mark, now],
        )?;
        tx.commit()?;
        Ok(json!({"buckets_written": written, "last_rolled_ts": new_mark, "cutoff_ts": cutoff}))
    }

    /// Core rollup writer: de-glitch-aggregate every raw sample in
    /// `(agg_start, agg_end)` into per-minute (bucket, session) rows and **upsert**
    /// them (`ON CONFLICT … DO UPDATE`). `deglitch_start (<= agg_start)` only
    /// widens the de-glitch *read* for boundary context — those older increments
    /// are not written. Returns `(buckets_written, max_bucket_end_ts)` where the
    /// end ts is `bucket_ts + resolution` of the newest bucket (0.0 if none).
    /// **Never deletes** — safe to run over ranges whose older raw is already
    /// pruned. Caller owns `rollup_state`.
    fn aggregate_and_upsert(
        c: &Connection,
        agg_start: f64,
        agg_end: f64,
        deglitch_start: f64,
    ) -> Result<(i64, f64)> {
        let res_s = ROLLUP_RESOLUTION_S;

        // De-glitched per-(bucket,session) metric deltas over the context-widened read.
        let mut steps_s: Vec<(i64, i64, i64)> = Vec::new();
        let mut dist_s: Vec<(i64, i64, i64)> = Vec::new();
        let mut cal_s: Vec<(i64, i64, i64)> = Vec::new();
        let mut dur_s: Vec<(i64, i64, i64)> = Vec::new();
        {
            let mut q = c.prepare(
                // Only samples from sessions THIS device recorded. A follower
                // also holds the walker's raw tail, and rolling that up here
                // banked the tail's first odometer reading as a fresh day
                // baseline (there is no older local sample to seed from) and
                // then overwrote the walker's correct bucket through the
                // upsert. NULL source is legacy local history.
                "SELECT CAST(s.ts AS INTEGER), s.session_id, s.steps, s.distance_raw,
                        s.calories, s.duration_s
                 FROM samples s JOIN sessions se ON se.id = s.session_id
                 WHERE s.ts > ? AND s.ts < ? AND s.session_id IS NOT NULL
                   AND (se.source IS NULL OR se.source = ?)
                 ORDER BY s.ts, s.id",
            )?;
            let mine = this_device();
            let mut rows = q.query(params![deglitch_start, agg_end, mine])?;
            while let Some(r) = rows.next()? {
                let ts: i64 = r.get(0)?;
                let sess: i64 = r.get(1)?;
                if let Some(v) = r.get::<_, Option<i64>>(2)? {
                    steps_s.push((ts, sess, v));
                }
                if let Some(v) = r.get::<_, Option<i64>>(3)? {
                    dist_s.push((ts, sess, v));
                }
                if let Some(v) = r.get::<_, Option<i64>>(4)? {
                    cal_s.push((ts, sess, v));
                }
                if let Some(v) = r.get::<_, Option<i64>>(5)? {
                    dur_s.push((ts, sess, v));
                }
            }
        }
        // One seed per metric: the last recorded value at or before the read
        // window, at ANY age. The 180 s lookback only widens the READ; after a
        // longer outage the walk would otherwise restart with no predecessor and
        // drop the increment the treadmill accrued while we were away.
        let seed_of = |col: &str| -> Result<Option<i64>> {
            Ok(c.query_row(
                &format!(
                    "SELECT s.{col} FROM samples s
                     WHERE s.ts <= ? AND s.{col} IS NOT NULL AND s.session_id IS NOT NULL
                     ORDER BY s.ts DESC, s.id DESC LIMIT 1"
                ),
                params![deglitch_start],
                |r| r.get(0),
            )
            .optional()?)
        };
        let steps_d = deglitch_bucketed(&steps_s, res_s, 50, 10, seed_of("steps")?);
        let dist_d = deglitch_bucketed(&dist_s, res_s, 200, 10, seed_of("distance_raw")?);
        let cal_d = deglitch_bucketed(&cal_s, res_s, 100, 10, seed_of("calories")?);
        let dur_d = deglitch_bucketed(&dur_s, res_s, 600, 10, seed_of("duration_s")?);

        // Stateless speed/running/total aggregates per (bucket,session) over the
        // [agg_start, agg_end) window — the authoritative bucket set.
        //
        // The lower bound is INCLUSIVE. `agg_start` is the end of the last rolled
        // bucket, and the previous run used `ts < agg_start`, so a sample landing
        // exactly on that boundary belonged to no window at all — it was counted
        // by neither run. With one sample per second that is exactly the case
        // that happens, costing a sample from every bucket at a run boundary and
        // under-reporting running time with it.
        let agg_sql = format!(
            "SELECT (CAST(s.ts AS INTEGER) / {res_s}) * {res_s} AS bucket_ts, s.session_id,
                    MIN(CASE WHEN s.speed_raw > 0 THEN s.speed_raw END) AS speed_raw_min,
                    AVG(CASE WHEN s.speed_raw > 0 THEN s.speed_raw END) AS speed_raw_avg,
                    MAX(CASE WHEN s.speed_raw > 0 THEN s.speed_raw END) AS speed_raw_max,
                    -- running_samples counts in-session status-3 samples (the
                    -- session filter is on the enclosing WHERE). The raw-path
                    -- `duration_running_s` query above uses the SAME in-session
                    -- definition — keep the two in step, or running time
                    -- changes value when the rollup crosses it. On why
                    -- `status = 3` needs the session filter at all, see
                    -- BeltState::Other's rustdoc in drivers/mod.rs.
                    SUM(CASE WHEN s.status = 3 THEN 1 ELSE 0 END) AS running_samples,
                    COUNT(*) AS total_samples
             FROM samples s WHERE s.ts >= ? AND s.ts < ? AND s.session_id IS NOT NULL
             GROUP BY bucket_ts, s.session_id"
        );
        let mut agg = c.prepare(&agg_sql)?;
        let groups: Vec<RollupRow> = agg
            .query_map(params![agg_start, agg_end], |r| {
                let bucket_ts: i64 = r.get(0)?;
                let session_id: Option<i64> = r.get(1)?;
                let key = (bucket_ts, session_id.unwrap_or(0));
                Ok(RollupRow {
                    bucket_ts,
                    session_id,
                    steps_delta: *steps_d.get(&key).unwrap_or(&0),
                    distance_raw_delta: *dist_d.get(&key).unwrap_or(&0),
                    calories_delta: *cal_d.get(&key).unwrap_or(&0),
                    duration_s_delta: *dur_d.get(&key).unwrap_or(&0),
                    speed_raw_min: r.get(2)?,
                    speed_raw_avg: r.get(3)?,
                    speed_raw_max: r.get(4)?,
                    running_samples: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    total_samples: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(agg);

        let mut written = 0i64;
        let mut max_bucket_end = 0.0f64;
        for r in &groups {
            c.execute(
                "INSERT INTO sample_rollups_1m(bucket_ts, session_id, steps_delta, distance_raw_delta,
                    calories_delta, duration_s_delta, speed_raw_min, speed_raw_avg, speed_raw_max,
                    running_samples, total_samples)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(bucket_ts, session_id) DO UPDATE SET
                    steps_delta=excluded.steps_delta, distance_raw_delta=excluded.distance_raw_delta,
                    calories_delta=excluded.calories_delta, duration_s_delta=excluded.duration_s_delta,
                    speed_raw_min=excluded.speed_raw_min, speed_raw_avg=excluded.speed_raw_avg,
                    speed_raw_max=excluded.speed_raw_max, running_samples=excluded.running_samples,
                    total_samples=excluded.total_samples",
                params![
                    r.bucket_ts, r.session_id, r.steps_delta, r.distance_raw_delta,
                    r.calories_delta, r.duration_s_delta, r.speed_raw_min, r.speed_raw_avg,
                    r.speed_raw_max, r.running_samples, r.total_samples
                ],
            )?;
            written += 1;
            max_bucket_end = max_bucket_end.max((r.bucket_ts + res_s) as f64);
        }
        Ok((written, max_bucket_end))
    }

    /// Non-destructive rollup (re)builder over `[from_ts, to_ts)`. Recomputes and
    /// upserts buckets **only for minutes that actually have raw samples** in the
    /// range, and **never deletes** buckets — so it is safe to run after raw has
    /// been pruned (buckets whose raw is gone are simply left untouched). Advances
    /// `last_rolled_ts` to cover the range (clamped to now, never rewound) so
    /// day/hour/timeseries reads treat the range as rolled. Replaces the old
    /// destructive `rebuild_rollups` (which DELETEd every bucket first — a data-loss
    /// footgun once retention actually prunes raw).
    pub fn backfill_rollups(&self, from_ts: f64, to_ts: f64) -> Result<Value> {
        let now = now_ts();
        let mut c = self.conn();
        let last_rolled: f64 = c
            .query_row(
                "SELECT last_rolled_ts FROM rollup_state WHERE kind=?",
                params![ROLLUP_KIND],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0.0);
        let tx = c.transaction()?;
        let deglitch_start = (from_ts - ROLLUP_DEGLITCH_LOOKBACK_S).max(0.0);
        let (written, max_bucket_end) =
            Self::aggregate_and_upsert(&tx, from_ts, to_ts, deglitch_start)?;
        // Advance the floor to cover what we rolled, never past now, never backward.
        let new_mark = last_rolled.max(max_bucket_end.min(now));
        tx.execute(
            "INSERT INTO rollup_state(kind, last_rolled_ts, last_run_ts) VALUES (?, ?, ?)
             ON CONFLICT(kind) DO UPDATE SET last_rolled_ts=excluded.last_rolled_ts, last_run_ts=excluded.last_run_ts",
            params![ROLLUP_KIND, new_mark, now],
        )?;
        tx.commit()?;
        Ok(json!({
            "buckets_written": written,
            "last_rolled_ts": new_mark,
            "from_ts": from_ts,
            "to_ts": to_ts,
        }))
    }

    pub fn prune_raw_samples(&self, retention_s: f64) -> Result<usize> {
        let now = now_ts();
        let cutoff = now - retention_s;
        let c = self.conn();
        let last_rolled: f64 = c
            .query_row(
                "SELECT last_rolled_ts FROM rollup_state WHERE kind=?",
                params![ROLLUP_KIND],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0.0);
        let effective_cutoff = cutoff.min(last_rolled);
        if effective_cutoff <= 0.0 {
            return Ok(0);
        }
        let n = c.execute(
            "DELETE FROM samples WHERE ts < ?",
            params![effective_cutoff],
        )?;
        Ok(n)
    }

    // --- export / import -------------------------------------------------

    /// Serialise the durable tiers for backup / sync. By default (`include_raw =
    /// false`) this is **sessions + 1-minute rollups + speed marks** — NO raw
    /// samples. Raw is ~99.5% of the bytes, serves no product feature beyond
    /// today, and is deliberately kept out of the sync/backup path (a syncing
    /// peer must not be able to resurrect pruned history). Pass `include_raw =
    /// true` for a full manual archive (the UI "export with raw" button). The
    /// dump `version` stays 2 and remains importable by older builds; the extra
    /// `speed_marks` key is ignored by importers that don't know it.
    /// Export everything, optionally with the raw samples.
    ///
    /// `raw_since` bounds those samples to a timestamp, which is what makes
    /// live following affordable: a full raw export is a day's worth of rows,
    /// far too large to push every twenty seconds, while the last few minutes
    /// is a handful. It matters because `day_totals` is rollups PLUS the raw
    /// tail above the rollup floor — so a device that only ever receives
    /// rollups is missing however much walking has happened since the rollup
    /// loop last ran, and reads low by exactly that much.
    pub fn export_since(&self, include_raw: bool, raw_since: Option<f64>) -> Result<Value> {
        let c = self.conn();
        // A dump is how our sessions reach every other device, and they will sum
        // the banked columns verbatim. Refresh the still-open one first (its last
        // minute is not rolled up yet) so a mid-walk push is exact rather than a
        // rollup-interval behind.
        let open_ids: Vec<i64> = {
            let mut stmt = c.prepare("SELECT id FROM sessions WHERE ended_ts IS NULL")?;
            let ids = stmt
                .query_map([], |r| r.get::<_, i64>(0))?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };
        for sid in open_ids {
            bank_session_totals(&c, sid)?;
        }
        let sessions = rows_as_json(&c, "SELECT * FROM sessions ORDER BY id")?;
        let rollups = rows_as_json(
            &c,
            "SELECT * FROM sample_rollups_1m ORDER BY bucket_ts, session_id",
        )?;
        let speed_marks = rows_as_json(&c, "SELECT * FROM speed_marks ORDER BY id")?;
        let mut out = json!({
            "format": "lifespan-sc110-dump",
            "version": 2,
            "exported_at": now_ts(),
            // Which device produced this dump. The importer uses it to decide
            // whose rows it may UPDATE rather than merely insert: a device owns
            // the sessions it recorded, and is the only authority on how many
            // steps they contain. Additive — older importers ignore it and keep
            // the previous insert-only behaviour.
            "origin": this_device(),
            "include_raw": include_raw,
            "sessions": sessions,
            "rollups_1m": rollups,
            "speed_marks": speed_marks,
        });
        if include_raw {
            let samples = match raw_since {
                Some(ts) => rows_as_json_p(
                    &c,
                    "SELECT * FROM samples WHERE ts >= ? ORDER BY id",
                    params![ts],
                )?,
                None => rows_as_json(&c, "SELECT * FROM samples ORDER BY id")?,
            };
            out["samples"] = json!(samples);
        }
        Ok(out)
    }

    /// Backwards-compatible shorthand: every raw sample, or none.
    pub fn export_all(&self, include_raw: bool) -> Result<Value> {
        self.export_since(include_raw, None)
    }

    /// Load a previous export back in. mode="merge" skips sessions whose
    /// started_ts already exists (idempotent re-import); mode="replace" wipes
    /// first. Ported from Python `import_dump`.
    pub fn import_dump(&self, dump: &Value, mode: &str) -> Result<Value> {
        if dump.get("format").and_then(|v| v.as_str()) != Some("lifespan-sc110-dump") {
            anyhow::bail!("not a lifespan-sc110 dump");
        }
        match dump.get("version").and_then(|v| v.as_i64()) {
            Some(1) | Some(2) => {}
            other => anyhow::bail!("unsupported dump version: {other:?}"),
        }
        if mode != "merge" && mode != "replace" {
            anyhow::bail!("mode must be 'merge' or 'replace', got {mode}");
        }
        // Who produced this dump. A device is the sole authority on the sessions
        // IT recorded, so rows carrying that source may be UPDATED here rather
        // than skipped. Without this the merge is insert-only, and a follower
        // that has rolled up a half-arrived minute keeps its short bucket for
        // ever: the walker's correct one arrives and is discarded as a
        // duplicate, so the two devices' day totals never reconcile.
        // Absent (older peers) means "authoritative for nothing" — previous
        // behaviour exactly.
        let origin = dump
            .get("origin")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let empty: Vec<Value> = Vec::new();
        let sessions = dump
            .get("sessions")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let samples = dump
            .get("samples")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let rollups = dump
            .get("rollups_1m")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let speed_marks = dump
            .get("speed_marks")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);

        // Belt-and-braces retention guard: never let an import (a stale peer or an
        // old full backup) resurrect raw samples older than the live retention
        // window. Rollups and sessions still merge fully — only ancient RAW is
        // dropped, because that is the payload the prune loop is meant to shed.
        let raw_cutoff = now_ts() - IMPORT_MAX_RAW_AGE_S;

        let mut counts = serde_json::Map::new();
        for k in [
            "sessions",
            "samples",
            "rollups",
            "speed_marks",
            "skipped_sessions",
            "skipped_samples",
            "skipped_rollups",
            "skipped_speed_marks",
            "skipped_old_samples",
            "updated_sessions",
            "updated_rollups",
        ] {
            counts.insert(k.into(), json!(0));
        }
        fn bump(m: &mut serde_json::Map<String, Value>, k: &str) {
            let n = m.get(k).and_then(|v| v.as_i64()).unwrap_or(0) + 1;
            m.insert(k.into(), json!(n));
        }

        let f64_of = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_f64());
        let i64_of = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_i64());
        let str_of = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());

        let mut c = self.conn();
        let tx = c.transaction()?;
        if mode == "replace" {
            tx.execute("DELETE FROM sample_rollups_1m", [])?;
            tx.execute("DELETE FROM samples", [])?;
            tx.execute("DELETE FROM sessions", [])?;
            tx.execute("DELETE FROM speed_marks", [])?;
            tx.execute("DELETE FROM rollup_state", [])?;
        }

        let mut id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        // Local ids of the sessions this dump is authoritative for, so the
        // rollup pass below can tell whose buckets it may replace without
        // re-querying per row.
        let mut owned_sids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for s in sessions {
            let started_ts = match f64_of(s, "started_ts") {
                Some(t) => t,
                None => continue,
            };
            let old_id = i64_of(s, "id");
            if mode == "merge" {
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM sessions WHERE started_ts = ?",
                        params![started_ts],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(eid) = existing {
                    if let Some(oid) = old_id {
                        id_map.insert(oid, eid);
                    }
                    // The producer owns this session: take its numbers. They
                    // change all through a walk (steps_end climbs, ended_ts
                    // lands at the end), and a frozen first copy is how a
                    // follower ends up showing a session that never finishes.
                    let owned = match (&origin, str_of(s, "source")) {
                        (Some(o), Some(src)) => &src == o,
                        _ => false,
                    };
                    if owned {
                        tx.execute(
                            "UPDATE sessions SET ended_ts=?, local_date=?, display_unit=?,
                                start_steps=?, start_duration_s=?, steps_end=?, duration_s_end=?,
                                distance_raw_end=?, calories_end=?, speed_raw_last=?,
                                closed_reason=?, source=?,
                                steps_total=?, duration_s_total=?,
                                distance_raw_total=?, calories_total=? WHERE id=?",
                            params![
                                f64_of(s, "ended_ts"),
                                str_of(s, "local_date").unwrap_or_default(),
                                str_of(s, "display_unit").unwrap_or_else(|| "km/h".into()),
                                i64_of(s, "start_steps"),
                                i64_of(s, "start_duration_s"),
                                i64_of(s, "steps_end"),
                                i64_of(s, "duration_s_end"),
                                i64_of(s, "distance_raw_end"),
                                i64_of(s, "calories_end"),
                                i64_of(s, "speed_raw_last"),
                                str_of(s, "closed_reason"),
                                str_of(s, "source"),
                                // The producer's own verdict on this session.
                                // Carried verbatim: we have none of the samples
                                // it was de-glitched from, so re-deriving one
                                // here is exactly how the two used to disagree.
                                i64_of(s, "steps_total"),
                                i64_of(s, "duration_s_total"),
                                i64_of(s, "distance_raw_total"),
                                i64_of(s, "calories_total"),
                                eid,
                            ],
                        )?;
                        owned_sids.insert(eid);
                        bump(&mut counts, "updated_sessions");
                    } else {
                        bump(&mut counts, "skipped_sessions");
                    }
                    continue;
                }
            }
            tx.execute(
                "INSERT INTO sessions(started_ts, ended_ts, local_date, display_unit, start_steps,
                    start_duration_s, steps_end, duration_s_end, distance_raw_end, calories_end,
                    speed_raw_last, closed_reason, source,
                    steps_total, duration_s_total, distance_raw_total, calories_total)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    started_ts,
                    f64_of(s, "ended_ts"),
                    str_of(s, "local_date").unwrap_or_else(|| local_date(started_ts)),
                    str_of(s, "display_unit").unwrap_or_else(|| "km/h".into()),
                    i64_of(s, "start_steps"),
                    i64_of(s, "start_duration_s"),
                    i64_of(s, "steps_end"),
                    i64_of(s, "duration_s_end"),
                    i64_of(s, "distance_raw_end"),
                    i64_of(s, "calories_end"),
                    i64_of(s, "speed_raw_last"),
                    str_of(s, "closed_reason"),
                    str_of(s, "source"),
                    i64_of(s, "steps_total"),
                    i64_of(s, "duration_s_total"),
                    i64_of(s, "distance_raw_total"),
                    i64_of(s, "calories_total"),
                ],
            )?;
            let new_id = tx.last_insert_rowid();
            if let Some(oid) = old_id {
                id_map.insert(oid, new_id);
            }
            // Newly inserted sessions from the producing device are owned too,
            // so its buckets for them may be replaced on later syncs.
            if let (Some(o), Some(src)) = (&origin, str_of(s, "source")) {
                if &src == o {
                    owned_sids.insert(new_id);
                }
            }
            bump(&mut counts, "sessions");
        }

        for sm in samples {
            let ts = match f64_of(sm, "ts") {
                Some(t) => t,
                None => continue,
            };
            // Never re-insert raw older than the retention window (both modes).
            if ts < raw_cutoff {
                bump(&mut counts, "skipped_old_samples");
                continue;
            }
            let new_sid = i64_of(sm, "session_id").and_then(|o| id_map.get(&o).copied());
            if mode == "merge" {
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM samples WHERE ts = ? AND (session_id IS ? OR session_id = ?) LIMIT 1",
                        params![ts, new_sid, new_sid],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_some() {
                    bump(&mut counts, "skipped_samples");
                    continue;
                }
            }
            tx.execute(
                "INSERT INTO samples(session_id, ts, steps, duration_s, speed_raw, distance_raw, calories, status)
                 VALUES (?,?,?,?,?,?,?,?)",
                params![
                    new_sid, ts, i64_of(sm, "steps"), i64_of(sm, "duration_s"),
                    i64_of(sm, "speed_raw"), i64_of(sm, "distance_raw"),
                    i64_of(sm, "calories"), i64_of(sm, "status")
                ],
            )?;
            bump(&mut counts, "samples");
        }

        for rr in rollups {
            let bucket_ts = match i64_of(rr, "bucket_ts") {
                Some(t) => t,
                None => continue,
            };
            let new_sid = i64_of(rr, "session_id").and_then(|o| id_map.get(&o).copied());
            if mode == "merge" {
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM sample_rollups_1m WHERE bucket_ts=? AND (session_id IS ? OR session_id = ?) LIMIT 1",
                        params![bucket_ts, new_sid, new_sid],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_some() {
                    // The producer's bucket replaces ours when it owns the
                    // session. This is the repair: a follower rolls up a minute
                    // whose raw has only partly arrived, writes a short bucket,
                    // and advances its rollup floor past it — after which the
                    // clipped samples can never be re-rolled. Skipping the
                    // producer's correct bucket here made that loss permanent
                    // and the two devices' totals never converged.
                    let owned = new_sid.map(|id| owned_sids.contains(&id)).unwrap_or(false);
                    if owned {
                        tx.execute(
                            "UPDATE sample_rollups_1m SET steps_delta=?, distance_raw_delta=?,
                                calories_delta=?, duration_s_delta=?, speed_raw_min=?,
                                speed_raw_avg=?, speed_raw_max=?, running_samples=?,
                                total_samples=?
                             WHERE bucket_ts=? AND (session_id IS ? OR session_id = ?)",
                            params![
                                i64_of(rr, "steps_delta").unwrap_or(0),
                                i64_of(rr, "distance_raw_delta").unwrap_or(0),
                                i64_of(rr, "calories_delta").unwrap_or(0),
                                i64_of(rr, "duration_s_delta").unwrap_or(0),
                                i64_of(rr, "speed_raw_min"),
                                f64_of(rr, "speed_raw_avg"),
                                i64_of(rr, "speed_raw_max"),
                                i64_of(rr, "running_samples").unwrap_or(0),
                                i64_of(rr, "total_samples").unwrap_or(0),
                                bucket_ts,
                                new_sid,
                                new_sid,
                            ],
                        )?;
                        bump(&mut counts, "updated_rollups");
                    } else {
                        bump(&mut counts, "skipped_rollups");
                    }
                    continue;
                }
            }
            tx.execute(
                "INSERT INTO sample_rollups_1m(bucket_ts, session_id, steps_delta, distance_raw_delta,
                    calories_delta, duration_s_delta, speed_raw_min, speed_raw_avg, speed_raw_max,
                    running_samples, total_samples) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    bucket_ts, new_sid,
                    i64_of(rr, "steps_delta").unwrap_or(0),
                    i64_of(rr, "distance_raw_delta").unwrap_or(0),
                    i64_of(rr, "calories_delta").unwrap_or(0),
                    i64_of(rr, "duration_s_delta").unwrap_or(0),
                    i64_of(rr, "speed_raw_min"),
                    f64_of(rr, "speed_raw_avg"),
                    i64_of(rr, "speed_raw_max"),
                    i64_of(rr, "running_samples").unwrap_or(0),
                    i64_of(rr, "total_samples").unwrap_or(0),
                ],
            )?;
            bump(&mut counts, "rollups");
        }

        // Speed marks (present in v2+ dumps; older dumps simply omit the key).
        for mk in speed_marks {
            let ts = match f64_of(mk, "ts") {
                Some(t) => t,
                None => continue,
            };
            let set_speed = f64_of(mk, "set_speed").unwrap_or(0.0);
            if mode == "merge" {
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM speed_marks WHERE ts = ? AND set_speed = ? LIMIT 1",
                        params![ts, set_speed],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_some() {
                    bump(&mut counts, "skipped_speed_marks");
                    continue;
                }
            }
            tx.execute(
                "INSERT INTO speed_marks(ts, set_speed, unit) VALUES (?, ?, ?)",
                params![
                    ts,
                    set_speed,
                    str_of(mk, "unit").unwrap_or_else(|| "km/h".into())
                ],
            )?;
            bump(&mut counts, "speed_marks");
        }

        tx.commit()?;
        Ok(Value::Object(counts))
    }

    /// Cumulative walking on `date` (local) up to `until_sod` seconds after that
    /// day's local midnight — powers "steps vs the same point on a previous day".
    /// Reads the durable 1-minute rollups (correct even after raw is pruned) and,
    /// for a still-partly-unrolled `date` (i.e. today), unions the raw tail via
    /// the `raw_floor` boundary so it doesn't double-count. Uses the same local
    /// date / local-midnight convention as the rest of the engine.
    pub fn timeofday_totals(&self, date: &str, until_sod: i64) -> Result<Value> {
        let c = self.conn();
        let midnight = match local_midnight(date) {
            Some(m) => m,
            // Unparseable date → empty rather than error (caller validated shape).
            None => {
                return Ok(
                    json!({"date": date, "until_sod": until_sod, "steps": 0, "distance_raw": 0}),
                )
            }
        };
        let end = midnight + until_sod as f64;
        let floor = raw_floor(&c);

        // Tier-1: rollup deltas for buckets on this local day, up to the cutoff.
        let (mut steps, mut dist): (i64, i64) = c.query_row(
            "SELECT COALESCE(SUM(steps_delta),0), COALESCE(SUM(distance_raw_delta),0)
             FROM sample_rollups_1m
             WHERE bucket_ts >= ? AND bucket_ts <= ?",
            params![midnight as i64, end as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        // Tier-0 tail: raw increments at/after `floor` up to the same cutoff. For
        // a fully-rolled past date `floor` is beyond `end`, so nothing is added.
        if end >= floor {
            let mut steps_v: Vec<(f64, i64)> = Vec::new();
            let mut dist_v: Vec<(f64, i64)> = Vec::new();
            let mut stmt = c.prepare(
                "SELECT s.ts, s.steps, s.distance_raw
                 FROM samples s JOIN sessions se ON se.id = s.session_id
                 WHERE se.local_date = ? AND s.ts <= ? ORDER BY s.ts, s.id",
            )?;
            let mut rows = stmt.query(params![date, end])?;
            while let Some(r) = rows.next()? {
                let ts: f64 = r.get(0)?;
                if let Some(v) = r.get::<_, Option<i64>>(1)? {
                    steps_v.push((ts, v));
                }
                if let Some(v) = r.get::<_, Option<i64>>(2)? {
                    dist_v.push((ts, v));
                }
            }
            drop(rows);
            drop(stmt);
            steps += deglitch_tail_total(&steps_v, floor, 50, 10);
            dist += deglitch_tail_total(&dist_v, floor, 200, 10);
        }

        Ok(json!({"date": date, "until_sod": until_sod, "steps": steps, "distance_raw": dist}))
    }

    /// One-time Phase-0 data-retention migration, gated by `PRAGMA user_version`.
    /// Idempotent: runs its body exactly once (subsequent boots are a no-op).
    ///
    /// Order is critical (the whole point): **(a)** backfill 1-minute rollups over
    /// the FULL existing raw history — this is the last moment minute-grain history
    /// can be built, so it MUST happen before any prune — then **(b)** prune raw
    /// older than the retention window, then **(c)** `VACUUM` to reclaim the space.
    pub fn run_startup_migration(&self, retention_s: f64) -> Result<Value> {
        let version: i64 = {
            let c = self.conn();
            c.pragma_query_value(None, "user_version", |r| r.get(0))?
        };
        if version >= 2 {
            return Ok(json!({"ran": false, "user_version": version}));
        }
        let mut out = serde_json::Map::new();

        if version < 1 {
            // Phase 0 (unchanged): build rollups over ALL raw before pruning —
            // the last moment minute-grain history can be derived — then prune
            // and reclaim.
            let backfill = self.backfill_rollups(0.0, now_ts())?;
            let pruned = self.prune_raw_samples(retention_s)?;
            {
                let c = self.conn();
                c.execute_batch("VACUUM;")?;
            }
            out.insert("backfill".into(), backfill);
            out.insert("pruned_samples".into(), json!(pruned));
        }

        // Phase 1 (0.3.2): repair the damage done by the pre-0.3.1 mid-bucket
        // truncation, recomputing every bucket from whatever raw the retention
        // window still holds. Upsert-only, so days whose raw has already been
        // pruned keep their (under-counted) totals — which is exactly why this
        // is worth shipping promptly rather than perfectly.
        let repair = self.backfill_rollups(0.0, now_ts())?;
        let rebaselined = self.repair_session_baselines()?;
        {
            let c = self.conn();
            c.pragma_update(None, "user_version", 2)?;
        }
        out.insert("repair_backfill".into(), repair);
        out.insert("rebaselined_sessions".into(), json!(rebaselined));
        out.insert("ran".into(), json!(true));
        Ok(Value::Object(out))
    }

    /// Repair sessions whose `start_steps` / `start_duration_s` were captured
    /// stale. `open_session` records the telemetry that opened the session, but
    /// the treadmill zeroes its counters shortly AFTER the belt starts, so the
    /// baseline can still be the previous session's total (observed:
    /// start_steps=765 on a session whose own samples run 0→87).
    ///
    /// Two tiers, strongest evidence first:
    ///   1. Sessions that still have raw samples — the baseline becomes the
    ///      session's own first recorded reading. Ground truth.
    ///   2. Sessions without raw (pruned, or a summary-only import) — a session
    ///      ending BELOW its recorded start can only be a post-reset session, so
    ///      the baseline is zeroed. A stale baseline the session later outgrew
    ///      is indistinguishable from genuine mid-walk adoption without raw, and
    ///      is deliberately left alone.
    pub fn repair_session_baselines(&self) -> Result<usize> {
        let c = self.conn();
        let mut n = 0usize;
        n += c.execute(
            "UPDATE sessions SET start_steps = (
                 SELECT s.steps FROM samples s
                 WHERE s.session_id = sessions.id AND s.steps IS NOT NULL
                 ORDER BY s.ts, s.id LIMIT 1)
             WHERE EXISTS (SELECT 1 FROM samples s
                           WHERE s.session_id = sessions.id AND s.steps IS NOT NULL)",
            [],
        )?;
        n += c.execute(
            "UPDATE sessions SET start_duration_s = (
                 SELECT s.duration_s FROM samples s
                 WHERE s.session_id = sessions.id AND s.duration_s IS NOT NULL
                 ORDER BY s.ts, s.id LIMIT 1)
             WHERE EXISTS (SELECT 1 FROM samples s
                           WHERE s.session_id = sessions.id AND s.duration_s IS NOT NULL)",
            [],
        )?;
        n += c.execute(
            "UPDATE sessions SET start_steps = 0
             WHERE steps_end IS NOT NULL AND start_steps IS NOT NULL
               AND steps_end < start_steps",
            [],
        )?;
        n += c.execute(
            "UPDATE sessions SET start_duration_s = 0
             WHERE duration_s_end IS NOT NULL AND start_duration_s IS NOT NULL
               AND duration_s_end < start_duration_s",
            [],
        )?;
        Ok(n)
    }
}

struct RollupRow {
    bucket_ts: i64,
    session_id: Option<i64>,
    steps_delta: i64,
    distance_raw_delta: i64,
    calories_delta: i64,
    duration_s_delta: i64,
    speed_raw_min: Option<i64>,
    speed_raw_avg: Option<f64>,
    speed_raw_max: Option<i64>,
    running_samples: i64,
    total_samples: i64,
}

/// Add `col` to `table` if it isn't there yet (idempotent schema patch for DBs
/// created before the column existed).
fn ensure_column(conn: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let present = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == col);
    if !present {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
    }
    Ok(())
}

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get("id")?,
        started_ts: r.get("started_ts")?,
        ended_ts: r.get("ended_ts")?,
        local_date: r.get("local_date")?,
        display_unit: r.get("display_unit")?,
        start_steps: r.get("start_steps")?,
        steps_end: r.get("steps_end")?,
        duration_s_end: r.get("duration_s_end")?,
        distance_raw_end: r.get("distance_raw_end")?,
        calories_end: r.get("calories_end")?,
        speed_raw_last: r.get("speed_raw_last")?,
        steps_total: r.get("steps_total").unwrap_or(None),
        duration_s_total: r.get("duration_s_total").unwrap_or(None),
        distance_raw_total: r.get("distance_raw_total").unwrap_or(None),
        calories_total: r.get("calories_total").unwrap_or(None),
        source: r.get("source")?,
    })
}

/// Generic "dump a SELECT to a JSON array of objects" using column names.
fn rows_as_json(c: &Connection, sql: &str) -> Result<Vec<Value>> {
    rows_as_json_p(c, sql, [])
}

/// Same, with bound parameters — used by the time-bounded raw export.
fn rows_as_json_p<P: rusqlite::Params>(c: &Connection, sql: &str, p: P) -> Result<Vec<Value>> {
    let mut stmt = c.prepare(sql)?;
    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let mut rows = stmt.query(p)?;
    while let Some(row) = rows.next()? {
        let mut obj = serde_json::Map::new();
        for (i, name) in col_names.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(x) => json!(x),
                rusqlite::types::ValueRef::Real(x) => json!(x),
                rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(b) => json!(b),
            };
            obj.insert(name.clone(), v);
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Db {
        Db::open(":memory:").unwrap()
    }

    /// The invariant the whole "following" feature rests on: two devices that
    /// have exchanged the same data must report the same number of steps.
    ///
    /// This reproduces the failure that shipped. A follower rolls up a minute
    /// whose raw samples have only partly arrived, banking a SHORT bucket and
    /// advancing its rollup floor past that minute — after which the clipped
    /// samples can never be re-rolled. The walker's correct bucket then arrives
    /// and, under insert-only merge, was discarded as a duplicate. The follower
    /// kept the short figure for ever: not lag, permanent loss.
    #[test]
    fn a_remote_walk_stops_counting_as_live_once_it_goes_quiet() {
        // This drives the spinning menu-bar icon, so it answers "is walking
        // happening", not "did a walk begin today". A session only gets an
        // ended_ts when the other device's close reaches us; if that sync is
        // delayed the row sits open, and measuring freshness from started_ts
        // left the icon spinning at somebody who had long since stopped.
        let db = mem();
        set_test_device("Mac");
        let start = now_ts() - 1800.0; // began half an hour ago

        let sid = db
            .open_session(start, "km/h", Some(0), Some(0), Some("iPhone"))
            .unwrap();

        // Last sign of life: twenty minutes ago. The walk began recently enough
        // that a started_ts test would still call it live.
        db.insert_sample(
            Some(sid),
            now_ts() - 1200.0,
            Some(500),
            Some(600),
            Some(300),
            Some(0),
            Some(0),
            Some(1),
        )
        .unwrap();
        assert!(
            !db.remote_active("Mac", 240.0).unwrap(),
            "a walk silent for twenty minutes must not still be reported as live"
        );

        // A frame arrives now — that is what walking looks like.
        db.insert_sample(
            Some(sid),
            now_ts() - 5.0,
            Some(560),
            Some(660),
            Some(300),
            Some(0),
            Some(0),
            Some(1),
        )
        .unwrap();
        assert!(
            db.remote_active("Mac", 240.0).unwrap(),
            "fresh movement on another device must read as live"
        );

        // Our own sessions are never "remote", however fresh.
        let mine = db
            .open_session(now_ts() - 10.0, "km/h", Some(0), Some(0), Some("Mac"))
            .unwrap();
        db.insert_sample(
            Some(mine),
            now_ts(),
            Some(10),
            Some(10),
            Some(300),
            Some(0),
            Some(0),
            Some(1),
        )
        .unwrap();
        assert!(
            !db.remote_active("Mac", 240.0).unwrap() || db.remote_active("iPhone", 240.0).unwrap(),
            "sanity: ownership is by source, not by recency"
        );
    }

    #[test]
    fn a_follower_that_saw_half_a_walk_does_not_freeze_on_half_the_steps() {
        // The shipped convergence test imports the walker's COMPLETE raw stream,
        // so it never exercised the case that actually broke: a follower that
        // live-follows part of a walk, rolls that part up, and thereby advances
        // its own rollup floor past the rest.
        //
        // It then held data for the session, claimed authority over it, summed
        // only the buckets below its own stalled floor, and displayed a number
        // that could never rise again — while also writing that number over the
        // walker's banked verdict and publishing it to the shared account.
        let walker = mem();
        let follower = mem();
        let base = (now_ts() / 60.0).floor() * 60.0 - 600.0;

        set_test_device("Mac");
        let sid = walker
            .open_session(base, "km/h", Some(0), Some(0), Some("Mac"))
            .unwrap();
        let push = |upto: usize| {
            for i in 0..upto {
                walker
                    .insert_sample(
                        Some(sid),
                        base + (i as f64) * 30.0,
                        Some((i as u32) * 100),
                        Some((i as u32) * 30),
                        Some(300),
                        Some(0),
                        Some(0),
                        Some(1),
                    )
                    .unwrap();
            }
        };

        // First half of the walk reaches the follower while it is live-following.
        push(5);
        let mut half = walker.export_since(true, Some(0.0)).unwrap();
        half["origin"] = json!("Mac");
        set_test_device("iPhone");
        follower.import_dump(&half, "merge").unwrap();
        // The follower's own rollup loop runs on a timer regardless of BLE.
        follower.rollup_samples_at(base + 300.0).unwrap();

        // The walk continues and finishes on the Mac.
        set_test_device("Mac");
        push(10);
        walker.rollup_samples_at(base + 600.0).unwrap();
        walker
            .close_session(
                sid,
                base + 300.0,
                Some(900),
                Some(270),
                Some(0),
                Some(0),
                Some(0),
                "stop",
            )
            .unwrap();

        let date = local_date(base);
        let walker_day = walker.day_totals(&date).unwrap();

        let mut full = walker.export_since(true, Some(0.0)).unwrap();
        full["origin"] = json!("Mac");
        set_test_device("iPhone");
        follower.import_dump(&full, "merge").unwrap();
        follower.rollup_samples_at(base + 600.0).unwrap();

        let follower_day = follower.day_totals(&date).unwrap();
        assert_eq!(
            follower_day["steps"], walker_day["steps"],
            "follower froze on a partial view: {follower_day} vs {walker_day}"
        );

        // And it must not have overwritten the recorder's verdict, or the wrong
        // number would spread to every device that bootstraps from the account.
        let banked: Option<i64> = follower
            .conn()
            .query_row(
                "SELECT steps_total FROM sessions WHERE started_ts = ?",
                params![base],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            banked,
            Some(walker_day["steps"].as_i64().unwrap()),
            "follower rebanked its own recomputation over the walker's"
        );

        // The walker must be unmoved by the follower's echo.
        let echo = follower.export_since(true, Some(0.0)).unwrap();
        set_test_device("Mac");
        walker.import_dump(&echo, "merge").unwrap();
        assert_eq!(
            walker.day_totals(&date).unwrap()["steps"],
            walker_day["steps"],
            "the walker's own total moved after importing a follower's echo"
        );
    }

    #[test]
    fn a_device_holding_only_session_rows_reads_the_same_day_total() {
        // The web app receives session rows and nothing else — no samples, no
        // rollups — so it cannot run the de-glitch the walker runs. Before the
        // recorder banked its verdict on the row, the two summed different
        // things and disagreed for ever: on a real day the desktop showed 4,909
        // steps over 6 sessions and the web showed 4,496 over 5.
        let walker = mem();
        let follower = mem();
        let base = (now_ts() / 60.0).floor() * 60.0 - 300.0;
        set_test_device("Mac");

        // The console counts ACROSS sessions: 30→113, then 120→150. The day is
        // worth 150, not 113 + 150.
        let s1 = walker
            .open_session(base, "km/h", Some(30), Some(10), Some("Mac"))
            .unwrap();
        for (i, steps) in [30u32, 55, 80, 110, 113].iter().enumerate() {
            walker
                .insert_sample(
                    Some(s1),
                    base + (i as f64) * 10.0,
                    Some(*steps),
                    Some(10 + i as u32 * 8),
                    Some(300),
                    Some(0),
                    Some(0),
                    Some(1),
                )
                .unwrap();
        }
        walker
            .close_session(
                s1,
                base + 50.0,
                Some(113),
                Some(42),
                Some(5),
                Some(5),
                Some(0),
                "pause",
            )
            .unwrap();

        let s2 = walker
            .open_session(base + 60.0, "km/h", Some(120), Some(50), Some("Mac"))
            .unwrap();
        for (i, steps) in [120u32, 135, 150].iter().enumerate() {
            walker
                .insert_sample(
                    Some(s2),
                    base + 60.0 + (i as f64) * 10.0,
                    Some(*steps),
                    Some(50 + i as u32 * 8),
                    Some(300),
                    Some(0),
                    Some(0),
                    Some(1),
                )
                .unwrap();
        }
        walker
            .close_session(
                s2,
                base + 80.0,
                Some(150),
                Some(65),
                Some(7),
                Some(7),
                Some(0),
                "stop",
            )
            .unwrap();

        let date = local_date(base);
        let walker_day = walker.day_totals(&date).unwrap();
        assert_eq!(walker_day["steps"], 150, "walker: {walker_day}");

        // What actually crosses the wire to a samples-less device.
        let mut dump = walker.export_since(false, None).unwrap();
        dump["origin"] = json!("Mac");
        set_test_device("iPhone"); // the follower is a different install
        dump["rollups_1m"] = json!([]);
        assert!(
            dump["samples"].is_null(),
            "this test is only meaningful without raw samples"
        );

        follower.import_dump(&dump, "merge").unwrap();
        let follower_day = follower.day_totals(&date).unwrap();
        assert_eq!(
            follower_day["steps"], walker_day["steps"],
            "follower disagreed with the walker: {follower_day} vs {walker_day}"
        );
        assert_eq!(follower_day["duration_s"], walker_day["duration_s"]);
        assert_eq!(follower_day["sessions"], walker_day["sessions"]);
    }

    #[test]
    fn a_follower_converges_on_the_walkers_total() {
        let walker = mem();
        let follower = mem();

        // Recent timestamps: raw older than the retention window is refused on
        // import, and then the floor never moves and nothing counts at all.
        let base = (now_ts() / 60.0).floor() * 60.0 - 300.0;
        set_test_device("iPhone"); // this test's walker install

        let sid = walker
            .open_session(base, "km/h", Some(0), Some(0), Some("iPhone"))
            .unwrap();
        for (i, steps) in [0u32, 30, 60, 90, 120].iter().enumerate() {
            walker
                .insert_sample(
                    Some(sid),
                    base + (i as f64) * 12.0,
                    Some(*steps),
                    Some(i as u32 * 12),
                    Some(400),
                    Some(0),
                    Some(0),
                    Some(1),
                )
                .unwrap();
        }

        // The export stamps `origin` from this process's device name, which is
        // empty in a test. Set it: what is under test is the importer's
        // ownership rule, not how the name is discovered.
        let mut partial = walker.export_since(true, Some(0.0)).unwrap();
        partial["origin"] = json!("iPhone");

        // The follower sees only the first half of the minute, and rolls it up
        // itself — banking a short bucket and moving its floor past the minute.
        let mut half = partial.clone();
        let cut: Vec<Value> = half["samples"].as_array().unwrap()[..3].to_vec();
        half["samples"] = json!(cut);
        follower.import_dump(&half, "merge").unwrap();
        follower.rollup_samples_at(base + 180.0).unwrap();

        // Now the rest arrives, with the walker's own complete bucket.
        walker.rollup_samples_at(base + 180.0).unwrap();
        let mut full = walker.export_since(true, Some(0.0)).unwrap();
        full["origin"] = json!("iPhone");
        follower.import_dump(&full, "merge").unwrap();

        let date = local_date(base);
        let w = walker.day_totals(&date).unwrap()["steps"].as_i64().unwrap();
        let f = follower.day_totals(&date).unwrap()["steps"]
            .as_i64()
            .unwrap();
        assert_eq!(
            w, 120,
            "the walking device is authoritative and must be right"
        );
        assert_eq!(
            f, w,
            "a follower must converge on the walker's total; it read {f} against {w}. \
             Insert-only merge leaves the follower's clipped bucket in place for ever."
        );
    }

    /// The other half of the contract: a dump may only correct rows it owns.
    ///
    /// Without this, "update on conflict" would let any device overwrite any
    /// other's history — a follower echoing stale copies back would clobber the
    /// walker's own numbers. Ownership is what makes the upsert safe, so it is
    /// worth a test that fails if the check is ever dropped.
    #[test]
    fn a_dump_cannot_rewrite_another_devices_session() {
        let db = mem();
        let base = (now_ts() / 60.0).floor() * 60.0 - 300.0;

        let sid = db
            .open_session(base, "km/h", Some(0), Some(0), Some("iPhone"))
            .unwrap();
        db.update_active_session(sid, Some(500), Some(60), Some(0), Some(0), Some(60))
            .unwrap();

        // A dump from a DIFFERENT device carrying a mangled copy of that session.
        let mut dump = db.export_since(false, None).unwrap();
        dump["origin"] = json!("SomebodyElse");
        dump["sessions"][0]["steps_end"] = json!(9);

        db.import_dump(&dump, "merge").unwrap();

        let after = db.get_session(sid).unwrap().unwrap();
        assert_eq!(
            after.steps_end,
            Some(500),
            "a device that does not own a session must not be able to rewrite it"
        );
    }

    /// A follower reads today's total as rollups PLUS the raw tail, so live
    /// sync has to carry that tail — but carrying ALL of it every twenty
    /// seconds would ship the whole day. `since` is what makes it affordable.
    #[test]
    fn export_since_bounds_the_raw_tail() {
        let db = mem();
        let sid = db
            .open_session(now_ts(), "km/h", Some(0), Some(0), None)
            .unwrap();
        let base = now_ts();
        for (i, steps) in [10u32, 20, 30, 40].iter().enumerate() {
            db.insert_sample(
                Some(sid),
                base + (i as f64) * 60.0,
                Some(*steps),
                Some(60),
                Some(0),
                Some(0),
                Some(0),
                Some(1),
            )
            .unwrap();
        }

        let all = db.export_since(true, None).unwrap();
        let n_all = all["samples"].as_array().unwrap().len();
        assert_eq!(n_all, 4, "unbounded export keeps every sample");

        // Only the last two minutes — what a live sync would ask for.
        let recent = db.export_since(true, Some(base + 120.0)).unwrap();
        let n_recent = recent["samples"].as_array().unwrap().len();
        assert_eq!(
            n_recent, 2,
            "`since` must bound the raw tail, or live sync ships the whole day"
        );

        // The rest of the dump is unaffected by the bound.
        assert_eq!(
            all["sessions"].as_array().unwrap().len(),
            recent["sessions"].as_array().unwrap().len(),
            "bounding raw must not drop sessions — they are how a follower sees the walk at all"
        );
    }

    #[test]
    fn day_totals_accumulates_across_counter_reset() {
        let db = mem();
        let today = local_date(now_ts());
        let sid = db
            .open_session(now_ts(), "km/h", Some(0), Some(0), None)
            .unwrap();
        let base = now_ts();
        // Walk 1: steps climb 0->10, then a reset (new walk) 0->5 => total 15.
        for (i, steps) in [0u32, 4, 10, 0, 3, 5].iter().enumerate() {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(*steps),
                Some(0),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        let totals = db.day_totals(&today).unwrap();
        assert_eq!(
            totals["steps"].as_i64().unwrap(),
            15,
            "monotonic accumulator should sum positive deltas + reset value"
        );
    }

    #[test]
    fn deglitch_handles_spikes_resets_and_dips() {
        // Real-world shape from a crash/reconnect: a stale low frame (346) wedged
        // between 1800 and 1891. The old accumulator added 346 + (1891-346)=~1500
        // phantom steps; the de-glitcher drops the spike and keeps the real climb.
        assert_eq!(
            deglitch_total(&[1797, 1800, 346, 1891, 1896, 1901], 50, 10),
            1901
        );
        // Genuine power-cycle reset to ~0 then a fresh climb: 0..10 then 0..5 = 15.
        assert_eq!(deglitch_total(&[0, 4, 10, 0, 3, 5], 50, 10), 15);
        // A one-off dip that reverts (not a reset) is dropped, never re-added.
        assert_eq!(deglitch_total(&[100, 103, 40, 106, 109], 50, 10), 109);
        // Real steps walked across a reconnect gap (no glitch) are kept.
        assert_eq!(deglitch_total(&[10, 20, 55, 60], 50, 10), 60);
        // Counter reset caught after it already climbed past reset_max (488 -> 42):
        // the drop is >half, so it's a reset and the post-reset climb (42->45) counts.
        assert_eq!(deglitch_total(&[400, 402, 42, 44, 45], 50, 10), 400 + 2 + 3);
        // A garbage stale-HIGH opening frame is dropped, not counted as baseline:
        // 1800 becomes the baseline, then +10 -> 1810 (was 5010 before the guard).
        assert_eq!(deglitch_total(&[5000, 1800, 1810], 50, 10), 1810);
    }

    #[test]
    fn day_totals_ignores_stale_reconnect_frame() {
        let db = mem();
        let today = local_date(now_ts());
        let sid = db
            .open_session(now_ts(), "km/h", Some(0), Some(0), None)
            .unwrap();
        let base = now_ts();
        // 1800 -> stale 346 -> 1891 -> 1901: only +101 of real climb after 1800.
        for (i, steps) in [1797u32, 1800, 346, 1891, 1896, 1901].iter().enumerate() {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(*steps),
                Some(0),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        let totals = db.day_totals(&today).unwrap();
        assert_eq!(
            totals["steps"].as_i64().unwrap(),
            1901,
            "a stale reconnect frame must not inject phantom steps"
        );
    }

    #[test]
    fn hourly_steps_reconcile_with_day_total() {
        let db = mem();
        let today = local_date(now_ts());
        let sid = db
            .open_session(now_ts(), "km/h", Some(0), Some(0), None)
            .unwrap();
        let base = now_ts();
        for (i, steps) in [1797u32, 1800, 346, 1891, 1896, 1901].iter().enumerate() {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(*steps),
                Some(0),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        let day = db.day_totals(&today).unwrap()["steps"].as_i64().unwrap();
        let sum: i64 = db
            .hourly_steps(&today)
            .unwrap()
            .iter()
            .map(|h| h["steps"].as_i64().unwrap())
            .sum();
        assert_eq!(day, 1901);
        assert_eq!(
            sum, day,
            "hourly buckets must sum to the de-glitched day total"
        );
    }

    #[test]
    fn rollup_deglitches_stale_frame() {
        let db = mem();
        let sid = db
            .open_session(now_ts() - 600.0, "km/h", Some(0), Some(0), None)
            .unwrap();
        // Align to a minute boundary ~10 min ago so all samples share one bucket
        // and fall before the rollup cutoff (now - 60s).
        let base = (((now_ts() as i64 - 600) / 60) * 60) as f64 + 1.0;
        for (i, steps) in [1797u32, 1800, 346, 1891, 1896, 1901].iter().enumerate() {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(*steps),
                Some(0),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        db.rollup_samples().unwrap();
        let delta: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(steps_delta), 0) FROM sample_rollups_1m WHERE session_id = ?",
                params![sid],
                |r| r.get(0),
            )
            .unwrap();
        // 1797 baseline + de-glitched increments (3 + 91 + 5 + 5) = 1901.
        //
        // The baseline is banked deliberately: a day's first reading is steps
        // already walked, and if the rollups don't carry it, the day total
        // shrinks by that amount the moment the rollup loop runs. (This
        // assertion used to be 104 — the value that made the pre-connect walk
        // disappear.)
        //
        // The stale 346 frame is still what this test is about: without the
        // spike drop it would be read as a counter reset and the recovery to
        // 1891 counted as fresh steps, giving 3355 instead of 1901.
        assert_eq!(delta, 1901, "rollup writer must drop the stale 346 frame");
    }

    #[test]
    fn timeseries_deglitches_raw_tail() {
        let db = mem();
        let base = now_ts() - 100.0;
        let sid = db
            .open_session(base, "km/h", Some(0), Some(0), None)
            .unwrap();
        // Same stale-frame shape as the day/hour tests: 346 wedged between 1800
        // and 1891. Nothing is rolled yet (floor 0), so this exercises the pure
        // raw-tail path of `timeseries`.
        for (i, steps) in [1797u32, 1800, 346, 1891, 1896, 1901].iter().enumerate() {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(*steps),
                Some(0),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        let series = db
            .timeseries("steps", 3600, now_ts() - 86_400.0, now_ts() + 1.0)
            .unwrap();
        let total: f64 = series.iter().map(|b| b["value"].as_f64().unwrap()).sum();
        // De-glitched increments = 3 + 91 + 5 + 5 = 104. The old MAX-MIN path gave
        // 1901 - 346 = 1555 — a phantom spike the day/hour views never showed.
        assert_eq!(
            total as i64, 104,
            "timeseries raw tail must de-glitch, not MAX-MIN"
        );
    }

    /// `duration_running_s` must mean the same thing on both sides of the
    /// rollup floor: in-session status-3 samples ONLY. Un-attributed status-3
    /// samples exist legitimately (the debounce window before a session
    /// confirms, the closing frame stored after the close) and illegitimately
    /// (a device emitting `BeltState::Other(0x03)`, whose raw byte stores as
    /// status 3 without ever opening a session — see BeltState::Other's
    /// rustdoc in drivers/mod.rs). Neither kind may count: before this was
    /// pinned, the raw-tail query had no session filter, so such samples
    /// showed as running time in recent chart buckets and then VANISHED when
    /// the rollup ran.
    #[test]
    fn duration_running_s_counts_the_same_before_and_after_the_rollup() {
        let db = mem();
        let base = (((now_ts() as i64) / 60) * 60 - 600) as f64;
        let sid = db
            .open_session(base, "km/h", Some(0), Some(0), None)
            .unwrap();
        // 60 s of genuine in-session walking…
        for i in 0..60 {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(i as u32),
                Some(i as u32),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        // …and 30 un-attributed status-3 samples (an Other(0x03) device, or
        // pre-debounce frames): stored, but never part of any session.
        for i in 0..30 {
            db.insert_sample(
                None,
                base + 120.0 + i as f64,
                None,
                None,
                Some(60),
                None,
                None,
                Some(3),
            )
            .unwrap();
        }

        let sum = |db: &Db| -> i64 {
            db.timeseries("duration_running_s", 60, base - 60.0, base + 900.0)
                .unwrap()
                .iter()
                .map(|b| b["value"].as_f64().unwrap())
                .sum::<f64>() as i64
        };

        let before = sum(&db);
        assert_eq!(
            before, 60,
            "raw-path duration_running_s must count IN-SESSION status-3 \
             samples only — the same definition the rollup writer uses for \
             running_samples. If this reads 90, the session filter was \
             dropped from the raw query in `timeseries` (db.rs) and running \
             time will evaporate when the rollup runs"
        );

        db.rollup_samples_at(base + 900.0).unwrap();
        let after = sum(&db);
        assert_eq!(
            after, before,
            "duration_running_s changed value when the rollup crossed it — \
             the raw query in `timeseries` and the rollup writer's \
             running_samples (both in db.rs) must share one in-session \
             definition of running time"
        );
    }

    #[test]
    fn export_import_round_trips() {
        // Use recent timestamps so the raw sample survives the import retention
        // guard — this exercises the full v2 (WITH samples) round-trip.
        let db = mem();
        let t = now_ts() - 100.0;
        let sid = db.open_session(t, "km/h", Some(0), Some(0), None).unwrap();
        db.insert_sample(
            Some(sid),
            t + 1.0,
            Some(5),
            Some(10),
            Some(60),
            Some(2),
            Some(1),
            Some(3),
        )
        .unwrap();
        db.close_session(
            sid,
            t + 2.0,
            Some(5),
            Some(10),
            Some(2),
            Some(1),
            Some(60),
            "stopped",
        )
        .unwrap();
        let dump = db.export_all(true).unwrap();

        let db2 = mem();
        let res = db2.import_dump(&dump, "merge").unwrap();
        assert_eq!(res["sessions"].as_i64().unwrap(), 1);
        assert_eq!(res["samples"].as_i64().unwrap(), 1);
        // Re-importing the same dump is idempotent (skips duplicates).
        let res2 = db2.import_dump(&dump, "merge").unwrap();
        assert_eq!(res2["skipped_sessions"].as_i64().unwrap(), 1);
        assert_eq!(res2["skipped_samples"].as_i64().unwrap(), 1);
        assert_eq!(db2.list_sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn rejects_foreign_dump() {
        let db = mem();
        assert!(db
            .import_dump(&serde_json::json!({"format": "nope"}), "merge")
            .is_err());
    }

    #[test]
    fn incremental_rollups_never_truncate_a_bucket() {
        // The bug this pins: the rollup cutoff was `now - 60`, which lands in the
        // MIDDLE of a minute. That minute was written from only the samples seen
        // so far, then `last_rolled` advanced past its end so the remainder was
        // never rolled — and since the upsert REPLACES steps_delta, the partial
        // value was permanent. Every rollup run silently lost most of one minute.
        //
        // Reproduced by rolling repeatedly at mid-bucket instants, exactly as the
        // engine's 5-minute loop does, then comparing the stored total against
        // the de-glitched truth over the same raw samples.
        let db = mem();
        // 10 minutes of walking, one sample per second, one step per second.
        let base = (((now_ts() as i64) / 60) * 60 - 1200) as f64;
        let sid = db
            .open_session(base, "km/h", Some(0), Some(0), None)
            .unwrap();
        let total_secs = 600;
        for i in 0..total_secs {
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(i as u32),
                Some(i as u32),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }

        // Roll every 90s at instants deliberately offset 23s into a minute.
        let mut t = base + 120.0 + 23.0;
        while t < base + (total_secs as f64) + 300.0 {
            db.rollup_samples_at(t).unwrap();
            t += 90.0;
        }

        let rolled: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(steps_delta),0) FROM sample_rollups_1m WHERE session_id=?",
                params![sid],
                |r| r.get(0),
            )
            .unwrap();

        // Truth: one step per second for 600s, starting at 0 => 599 increments.
        assert_eq!(
            rolled, 599,
            "rollups lost steps: stored {rolled}, expected 599. A bucket was \
             written before it finished and never revisited."
        );

        // And no bucket may hold fewer samples than the minute actually contains.
        let thin: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sample_rollups_1m WHERE session_id=? AND total_samples < 60",
                params![sid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            thin, 0,
            "{thin} bucket(s) were rolled while still incomplete"
        );
    }

    #[test]
    fn rollups_bank_the_first_reading_baseline() {
        // Someone walks before the app connects: the day's first sample already
        // reads 340. The raw path counts it; if the rollup path does not, the
        // displayed total silently drops by the pre-connect walk as soon as the
        // rollup loop runs — and becomes unrecoverable once raw is pruned.
        let db = mem();
        let steps: Vec<u32> = (340..=400).step_by(10).collect();
        let (_sid, date) = seed_rollable_session(&db, 600, &steps);

        let before = db.day_totals(&date).unwrap()["steps"].as_i64().unwrap();
        assert_eq!(
            before, 400,
            "the raw day total includes the pre-connect walk"
        );

        db.rollup_samples().unwrap();
        assert_eq!(
            db.day_totals(&date).unwrap()["steps"].as_i64().unwrap(),
            before,
            "the day total must not shrink when the rollup loop runs"
        );

        db.conn().execute("DELETE FROM samples", []).unwrap();
        assert_eq!(
            db.day_totals(&date).unwrap()["steps"].as_i64().unwrap(),
            before,
            "and must survive raw pruning"
        );
    }

    #[test]
    fn rollups_keep_the_increment_across_a_long_sample_gap() {
        // A 10-minute outage, far beyond the 180 s de-glitch lookback, while the
        // belt keeps counting. Without a seed the walk restarts blind and the
        // steps accrued during the gap are never banked.
        let db = mem();
        let base = (((now_ts() as i64 - 1500) / 60) * 60) as f64 + 1.0;
        let sid = db
            .open_session(base, "km/h", Some(0), Some(0), None)
            .unwrap();

        let mut all: Vec<i64> = Vec::new();
        for i in 0..50 {
            let v = i as u32 * 2;
            all.push(v as i64);
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(v),
                Some(i as u32),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        db.rollup_samples_at(base + 120.0).unwrap();

        let b2 = base + 660.0;
        for i in 0..50 {
            let v = 400 + i as u32;
            all.push(v as i64);
            db.insert_sample(
                Some(sid),
                b2 + i as f64,
                Some(v),
                Some(600 + i as u32),
                Some(60),
                Some(0),
                Some(0),
                Some(3),
            )
            .unwrap();
        }
        db.rollup_samples_at(b2 + 120.0).unwrap();

        let rolled: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(steps_delta),0) FROM sample_rollups_1m WHERE session_id=?",
                params![sid],
                |r| r.get(0),
            )
            .unwrap();
        // Single-source the truth: the same de-glitch over the same values.
        let truth = deglitch_total(&all, 50, 10);
        assert_eq!(
            rolled, truth,
            "the increment accrued across a gap longer than the lookback must be banked"
        );
    }

    #[test]
    fn stale_start_steps_self_heals_on_the_first_fresh_sample() {
        let db = mem();
        // The belt just started; the opening telemetry still carries the
        // previous session's counter. The reset arrives with the next sample.
        let sid = db
            .open_session(now_ts(), "km/h", Some(765), Some(764), None)
            .unwrap();
        db.update_active_session(sid, Some(2), Some(1), Some(0), Some(0), Some(60))
            .unwrap();

        let got: (Option<i64>, Option<i64>) = db
            .conn()
            .query_row(
                "SELECT start_steps, start_duration_s FROM sessions WHERE id=?",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            got.0,
            Some(2),
            "a stale baseline must be replaced by the post-reset reading"
        );
        assert_eq!(got.1, Some(1), "and the duration baseline with it");
    }

    #[test]
    fn a_genuinely_adopted_walk_is_not_rebaselined() {
        // The engine adopted a walk already in progress: the counter reads 500
        // and keeps climbing. That baseline is correct and must be left alone.
        let db = mem();
        let sid = db
            .open_session(now_ts(), "km/h", Some(500), Some(300), None)
            .unwrap();
        db.update_active_session(sid, Some(501), Some(301), Some(0), Some(0), Some(60))
            .unwrap();
        let start: Option<i64> = db
            .conn()
            .query_row(
                "SELECT start_steps FROM sessions WHERE id=?",
                params![sid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(start, Some(500));
    }

    #[test]
    fn summary_only_day_totals_survive_a_stale_baseline() {
        // No rollups and no raw — the legacy summary-only import path. Session B
        // reset mid-session, so its recorded start is the previous total; the
        // unclamped subtraction made it negative and ate session A's steps.
        let db = mem();
        let today = local_date(now_ts());
        for (start, end) in [(0i64, 400i64), (432, 99)] {
            db.conn()
                .execute(
                    "INSERT INTO sessions(started_ts, ended_ts, local_date, display_unit,
                                      start_steps, steps_end)
                 VALUES (?,?,?,'km/h',?,?)",
                    params![now_ts(), now_ts() + 60.0, today, start, end],
                )
                .unwrap();
        }
        assert_eq!(
            db.day_totals(&today).unwrap()["steps"].as_i64().unwrap(),
            499,
            "a session that ends below its recorded start contributes its end value"
        );
    }

    #[test]
    fn migration_v2_repairs_truncated_rollups_and_stale_baselines() {
        let db = mem();
        let steps: Vec<u32> = (0..=600).step_by(10).collect();
        let (sid, date) = seed_rollable_session(&db, 900, &steps);
        // An existing 0.3.1 install: phase 0 already done.
        db.conn().pragma_update(None, "user_version", 1).unwrap();
        db.rollup_samples().unwrap();
        let healthy = db.day_totals(&date).unwrap()["steps"].as_i64().unwrap();

        // Damage a bucket exactly as the pre-0.3.1 truncation did, and stale the
        // baseline the way ingest captured it.
        db.conn()
            .execute(
                "UPDATE sample_rollups_1m SET steps_delta = steps_delta - 100
             WHERE bucket_ts = (SELECT MIN(bucket_ts) FROM sample_rollups_1m WHERE session_id=?)",
                params![sid],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE sessions SET start_steps = 9999 WHERE id=?",
                params![sid],
            )
            .unwrap();
        assert!(
            db.day_totals(&date).unwrap()["steps"].as_i64().unwrap() < healthy,
            "the damage must actually register first"
        );

        let res = db.run_startup_migration(7.0 * 86400.0).unwrap();
        assert_eq!(res["ran"], json!(true));
        assert_eq!(
            db.day_totals(&date).unwrap()["steps"].as_i64().unwrap(),
            healthy,
            "the migration must recompute the truncated bucket from raw"
        );
        let start: Option<i64> = db
            .conn()
            .query_row(
                "SELECT start_steps FROM sessions WHERE id=?",
                params![sid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            start,
            Some(0),
            "the stale baseline must be repaired from raw"
        );

        // Idempotent: a second boot is a no-op.
        assert_eq!(
            db.run_startup_migration(7.0 * 86400.0).unwrap()["ran"],
            json!(false)
        );
    }

    #[test]
    fn steps_by_device_groups_by_source() {
        // Per-device history is built from ROLLUPS, and a device only ever rolls
        // up its own samples — a follower must not re-derive a peer's buckets
        // from a partial copy. So the peers' buckets have to arrive the way they
        // do in production: banked by their recorder and carried over sync.
        let base = (((now_ts() as i64 - 600) / 60) * 60) as f64 + 1.0;

        let record = |name: Option<&str>, start: f64, steps: &[u32]| -> Value {
            set_test_device(name.unwrap_or(""));
            let db = mem();
            let sid = db
                .open_session(start, "km/h", Some(0), Some(0), name)
                .unwrap();
            for (i, st) in steps.iter().enumerate() {
                db.insert_sample(
                    Some(sid),
                    start + i as f64,
                    Some(*st),
                    Some(i as u32),
                    Some(60),
                    Some(0),
                    Some(0),
                    Some(3),
                )
                .unwrap();
            }
            db.rollup_samples().unwrap();
            let mut dump = db.export_since(true, Some(0.0)).unwrap();
            dump["origin"] = json!(name.unwrap_or(""));
            dump
        };

        let mac = record(Some("Mac"), base, &[0, 10, 20, 30]); // 30 steps
        let iphone = record(Some("iPhone"), base + 5.0, &[0, 5, 10]); // 10 steps
        let legacy = record(None, base + 10.0, &[0, 40]); // 40 steps, no source

        // A fourth install that recorded nothing and only syncs.
        set_test_device("Viewer");
        let db = mem();
        for dump in [&mac, &iphone, &legacy] {
            db.import_dump(dump, "merge").unwrap();
        }

        let rows = db.steps_by_device(&local_date(base)).unwrap();
        let mut got = std::collections::HashMap::new();
        for r in &rows {
            got.insert(
                r["source"].as_str().unwrap().to_string(),
                r["steps"].as_i64().unwrap(),
            );
        }
        assert_eq!(got.get("Mac"), Some(&30));
        assert_eq!(got.get("iPhone"), Some(&10));
        assert_eq!(
            got.get(""),
            Some(&40),
            "sessions with no source group under empty string"
        );
    }

    // --- Phase 0: retention refactor -------------------------------------

    /// Insert a session ~10 min ago with samples on a minute boundary so they all
    /// fall before the rollup cutoff (now-60) and can be rolled. Returns the
    /// session id and the local date the samples belong to.
    fn seed_rollable_session(db: &Db, offset_ago_s: i64, steps: &[u32]) -> (i64, String) {
        let base = (((now_ts() as i64 - offset_ago_s) / 60) * 60) as f64 + 1.0;
        let sid = db
            .open_session(base, "km/h", Some(0), Some(0), None)
            .unwrap();
        for (i, st) in steps.iter().enumerate() {
            // distance tracks steps/4, calories steps/10, duration = seconds.
            db.insert_sample(
                Some(sid),
                base + i as f64,
                Some(*st),
                Some(i as u32),
                Some(60),
                Some(*st / 4),
                Some(*st / 10),
                Some(3),
            )
            .unwrap();
        }
        db.close_session(
            sid,
            base + steps.len() as f64,
            steps.last().copied(),
            Some(steps.len() as u32),
            Some(steps.last().copied().unwrap_or(0) / 4),
            Some(steps.last().copied().unwrap_or(0) / 10),
            Some(60),
            "stopped",
        )
        .unwrap();
        (sid, local_date(base))
    }

    #[test]
    fn export_default_omits_raw_include_raw_adds_it() {
        let db = mem();
        seed_rollable_session(&db, 600, &[0, 10, 20, 30, 40]);

        let default = db.export_all(false).unwrap();
        // No samples key (or empty) on the default export.
        assert!(
            default.get("samples").is_none(),
            "default export must not carry raw samples"
        );
        assert!(default.get("sessions").unwrap().as_array().unwrap().len() == 1);
        assert!(default.get("rollups_1m").is_some());
        assert!(default.get("speed_marks").is_some());

        let full = db.export_all(true).unwrap();
        assert!(
            !full.get("samples").unwrap().as_array().unwrap().is_empty(),
            "include_raw export must carry raw samples"
        );
    }

    #[test]
    fn day_and_hour_totals_from_rollups_match_raw() {
        let db = mem();
        // Monotonic climb 0..90 (no glitches) so the de-glitched total == 90.
        let steps: Vec<u32> = (0..=90).step_by(10).collect(); // 0,10,...,90
        let (_sid, date) = seed_rollable_session(&db, 600, &steps);

        // Golden: raw-only totals (nothing rolled yet, floor == 0).
        let raw_day = db.day_totals(&date).unwrap()["steps"].as_i64().unwrap();
        let raw_hour_sum: i64 = db
            .hourly_steps(&date)
            .unwrap()
            .iter()
            .map(|h| h["steps"].as_i64().unwrap())
            .sum();
        assert_eq!(raw_day, 90);
        assert_eq!(raw_hour_sum, 90);

        // Roll every sample up (all < cutoff → floor advances past them).
        db.rollup_samples().unwrap();
        // With raw still present, the union (rollups + empty tail) must match.
        assert_eq!(
            db.day_totals(&date).unwrap()["steps"].as_i64().unwrap(),
            raw_day
        );
        let rolled_hour_sum: i64 = db
            .hourly_steps(&date)
            .unwrap()
            .iter()
            .map(|h| h["steps"].as_i64().unwrap())
            .sum();
        assert_eq!(rolled_hour_sum, raw_hour_sum);

        // Now DELETE all raw — totals must be unchanged (pure rollup path).
        db.conn().execute("DELETE FROM samples", []).unwrap();
        assert_eq!(
            db.day_totals(&date).unwrap()["steps"].as_i64().unwrap(),
            raw_day
        );
        let pruned_hour_sum: i64 = db
            .hourly_steps(&date)
            .unwrap()
            .iter()
            .map(|h| h["steps"].as_i64().unwrap())
            .sum();
        assert_eq!(pruned_hour_sum, raw_hour_sum);
    }

    #[test]
    fn backfill_is_idempotent_and_never_deletes_orphaned_buckets() {
        let db = mem();
        let steps: Vec<u32> = (0..=100).step_by(10).collect();
        let (_sid, _date) = seed_rollable_session(&db, 600, &steps);

        let count = |db: &Db| -> i64 {
            db.conn()
                .query_row("SELECT COUNT(*) FROM sample_rollups_1m", [], |r| r.get(0))
                .unwrap()
        };
        let sum = |db: &Db| -> i64 {
            db.conn()
                .query_row(
                    "SELECT COALESCE(SUM(steps_delta),0) FROM sample_rollups_1m",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };

        db.backfill_rollups(0.0, now_ts()).unwrap();
        let (c1, s1) = (count(&db), sum(&db));
        assert!(c1 > 0);
        assert_eq!(s1, 100, "backfilled deltas sum to the climb");

        // Idempotent: a second full backfill changes nothing.
        db.backfill_rollups(0.0, now_ts()).unwrap();
        assert_eq!(count(&db), c1);
        assert_eq!(sum(&db), s1);

        // Simulate a pruned past: raw gone, rollups remain. Backfill must NOT
        // delete the now-orphaned buckets.
        db.conn().execute("DELETE FROM samples", []).unwrap();
        let res = db.backfill_rollups(0.0, now_ts()).unwrap();
        assert_eq!(res["buckets_written"].as_i64().unwrap(), 0);
        assert_eq!(
            count(&db),
            c1,
            "buckets whose raw is gone must survive backfill"
        );
        assert_eq!(sum(&db), s1);
    }

    #[test]
    fn import_skips_ancient_raw_but_keeps_sessions_and_rollups() {
        let db = mem();
        let now = now_ts();
        let recent_ts = now - 100.0;
        let ancient_ts = now - 30.0 * 86400.0; // 30 days old
        let dump = json!({
            "format": "lifespan-sc110-dump",
            "version": 2,
            "sessions": [{
                "id": 1, "started_ts": recent_ts, "ended_ts": recent_ts + 5.0,
                "local_date": local_date(recent_ts), "display_unit": "km/h",
                "start_steps": 0, "steps_end": 50,
            }],
            "samples": [
                {"id": 1, "session_id": 1, "ts": recent_ts, "steps": 10},
                {"id": 2, "session_id": 1, "ts": ancient_ts, "steps": 20},
            ],
            "rollups_1m": [{
                "bucket_ts": (recent_ts as i64 / 60) * 60, "session_id": 1,
                "steps_delta": 40, "distance_raw_delta": 10, "calories_delta": 4,
                "duration_s_delta": 60, "running_samples": 24, "total_samples": 24,
            }],
        });
        let res = db.import_dump(&dump, "merge").unwrap();
        assert_eq!(res["sessions"].as_i64().unwrap(), 1);
        assert_eq!(res["rollups"].as_i64().unwrap(), 1);
        assert_eq!(res["samples"].as_i64().unwrap(), 1, "recent raw kept");
        assert_eq!(
            res["skipped_old_samples"].as_i64().unwrap(),
            1,
            "ancient raw dropped"
        );
    }

    #[test]
    fn migration_runs_once_prunes_old_raw_keeps_totals() {
        let db = mem();
        // An OLD day (10 days ago) that will be pruned, plus a recent day.
        let old_steps: Vec<u32> = (0..=80).step_by(10).collect();
        let (_o, old_date) = seed_rollable_session(&db, 10 * 86400, &old_steps);
        let recent_steps: Vec<u32> = (0..=50).step_by(10).collect();
        let (_r, _recent_date) = seed_rollable_session(&db, 600, &recent_steps);

        // Golden day total for the old date (raw present, floor 0).
        let old_total_before = db.day_totals(&old_date).unwrap()["steps"].as_i64().unwrap();
        assert_eq!(old_total_before, 80);

        let m1 = db.run_startup_migration(7.0 * 86400.0).unwrap();
        assert!(
            m1["ran"].as_bool().unwrap(),
            "first run must perform the migration"
        );
        assert!(m1["pruned_samples"].as_i64().unwrap() >= old_steps.len() as i64);

        // Old raw is gone...
        let old_raw: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM samples s JOIN sessions se ON se.id=s.session_id WHERE se.local_date=?",
            params![old_date], |r| r.get(0)).unwrap();
        assert_eq!(old_raw, 0, "old raw pruned");
        // ...but the day total survives, served from rollups.
        assert_eq!(
            db.day_totals(&old_date).unwrap()["steps"].as_i64().unwrap(),
            old_total_before
        );

        // Idempotent: a second run is a no-op (no further prune / vacuum).
        let m2 = db.run_startup_migration(7.0 * 86400.0).unwrap();
        assert!(!m2["ran"].as_bool().unwrap(), "second run must be a no-op");
    }

    #[test]
    fn timeofday_cuts_cumulative_at_sod() {
        let db = mem();
        // Build a day two hours ago: hour A gets +30 steps, hour B gets +40.
        let midnight = local_midnight(&local_date(now_ts())).unwrap();
        // Place buckets at 09:00 (sod 32400) and 10:00 (sod 36000) local.
        let sid = db
            .open_session(midnight + 100.0, "km/h", Some(0), Some(0), None)
            .unwrap();
        db.close_session(
            sid,
            midnight + 4000.0,
            Some(70),
            Some(60),
            Some(17),
            Some(7),
            Some(60),
            "stopped",
        )
        .unwrap();
        let insert_bucket = |sod: i64, steps: i64, dist: i64| {
            db.conn().execute(
                "INSERT INTO sample_rollups_1m(bucket_ts, session_id, steps_delta, distance_raw_delta,
                    calories_delta, duration_s_delta, running_samples, total_samples)
                 VALUES (?,?,?,?,0,60,24,24)",
                params![midnight as i64 + sod, sid, steps, dist],
            ).unwrap();
        };
        insert_bucket(32_400, 30, 8); // 09:00
        insert_bucket(36_000, 40, 10); // 10:00
        let date = local_date(now_ts());

        // Before 09:00 → nothing.
        let a = db.timeofday_totals(&date, 30_000).unwrap();
        assert_eq!(a["steps"].as_i64().unwrap(), 0);
        // Through 09:30 → only the 09:00 bucket.
        let b = db.timeofday_totals(&date, 34_200).unwrap();
        assert_eq!(b["steps"].as_i64().unwrap(), 30);
        assert_eq!(b["distance_raw"].as_i64().unwrap(), 8);
        // Through 10:30 → both buckets.
        let c = db.timeofday_totals(&date, 37_800).unwrap();
        assert_eq!(c["steps"].as_i64().unwrap(), 70);
        assert_eq!(c["distance_raw"].as_i64().unwrap(), 18);

        // A date with no data is zero, not an error.
        let empty = db.timeofday_totals("2000-01-01", 86_400).unwrap();
        assert_eq!(empty["steps"].as_i64().unwrap(), 0);
    }
}
