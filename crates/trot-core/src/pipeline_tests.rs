//! End-to-end pipeline tests: real driver frames → `Sample` → `Telemetry` →
//! session detection (`ble::ingest_sample`) → storage (`Db`) → `/api` output.
//!
//! Every driver's own suite stops at the `Sample`/`Telemetry` boundary; the
//! seam that has shipped a real bug (the 0.3.2 −16% step undercount) is the
//! one BELOW that boundary — session open/close, the de-glitch accumulator,
//! the rollup floor. These tests push whole *sessions* (start, run with
//! rising counters, pause, resume, stop) through the actual ingest path and
//! assert the recorded totals are exactly right.
//!
//! Time note: `ingest_sample` takes the clock as a parameter, and the rig
//! below drives it synthetically — each pushed frame advances a virtual
//! clock by 5 s (comfortably past `ble::SESSION_DEBOUNCE`, so "the 2nd
//! frame of a held state confirms" reads exactly as it did when the
//! debounce counted frames). Timestamps therefore run up to a minute or two
//! into the real future; `db::update_active_session`'s 5-second baseline
//! self-heal window compares the REAL wall clock against those future
//! `started_ts` values, so the window is *always open* during these tests —
//! a mid-session counter reset here rebaselines the session row where on
//! real hardware (reset arriving minutes in) it would not. The persistence
//! throttle is defeated by resetting `last_persist` before each push
//! (throttling itself is pinned in `ble.rs`). Day totals are unaffected
//! either way; tests below note it where it shows.

use crate::app::AppState;
use crate::ble;
use crate::db::Db;
use crate::drivers::util::checksum_sum;
use crate::drivers::{ftms, kingsmith_wilink, lifespan, pitpat};
use crate::telemetry::Telemetry;
use std::sync::Arc;

/// Virtual seconds between pushed frames: must exceed `ble::SESSION_DEBOUNCE`
/// so that the second frame of a held state confirms it.
const FRAME_SPACING_S: f64 = 5.0;

/// The real ingest path with its loop-local state, as `connect_and_poll`
/// holds it, on a synthetic clock. `IngestState` includes the plausibility
/// gate, so every fixture stream here also proves itself plausible to the
/// gate — a fixture these tests accept is one the shipped ingest path
/// accepts whole.
struct Rig {
    db: Arc<Db>,
    state: Arc<AppState>,
    ing: ble::IngestState,
    clock: f64,
}

impl Rig {
    fn new(unit: &str) -> Self {
        let db = Arc::new(Db::open(":memory:").unwrap());
        let state = AppState::new(db.clone(), unit.into(), None, "test-token".into());
        Rig {
            db,
            state,
            ing: ble::IngestState::default(),
            clock: crate::db::now_ts(),
        }
    }

    fn push(&mut self, telem: &Telemetry) {
        self.clock += FRAME_SPACING_S;
        self.ing.last_persist = 0.0; // defeat the 1 Hz throttle (see module docs)
        ble::ingest_sample(&self.state, telem, self.clock, &mut self.ing);
    }

    fn today(&self) -> serde_json::Value {
        self.db.day_totals(&today_local()).unwrap()
    }

    fn sessions(&self) -> Vec<crate::db::Session> {
        let mut s = self.db.list_sessions(50).unwrap();
        s.sort_by_key(|s| s.id); // oldest first, for readable assertions
        s
    }
}

fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

// ---- LifeSpan: request/response frames through the Reader --------------------

/// One full poll rotation worth of raw LifeSpan responses, decoded by the
/// real `Reader`, converted by the real `to_sample`.
fn ls_cycle(
    reader: &mut lifespan::Reader,
    status: u8,
    steps: u32,
    dur_s: u32,
    dist_raw: u32,
    cal: u32,
    speed_raw: u32,
) -> Telemetry {
    let be = |v: u32| ((v >> 8) as u8, (v & 0xFF) as u8);
    let frame = |b2: u8, b3: u8, b4: u8| vec![0xA1, 0xAA, b2, b3, b4, 0x00];

    reader
        .feed(lifespan::OPCODE_STATUS, &frame(status, 0, 0))
        .unwrap();
    let (h, l) = be(steps);
    reader
        .feed(lifespan::OPCODE_STEPS, &frame(h, l, 0))
        .unwrap();
    let (m, s) = ((dur_s / 60) as u8, (dur_s % 60) as u8);
    reader
        .feed(lifespan::OPCODE_DURATION, &frame(0, m, s))
        .unwrap();
    let (h, l) = be(dist_raw);
    reader
        .feed(lifespan::OPCODE_DISTANCE, &frame(h, l, 0))
        .unwrap();
    let (h, l) = be(cal);
    reader
        .feed(lifespan::OPCODE_CALORIES, &frame(h, l, 0))
        .unwrap();
    let (h, l) = be(speed_raw);
    reader
        .feed(lifespan::OPCODE_SPEED, &frame(h, l, 0))
        .unwrap();

    Telemetry::from_sample(&lifespan::to_sample(reader.state(), "km/h"), "km/h")
}

/// A whole LifeSpan session — standby, start, walk, pause, resume, stop —
/// through the real pipeline, with the totals asserted exactly, then again
/// through the `/api/today` route, then again after the rollup has banked
/// the raw samples (twice — the second run must be a no-op).
#[tokio::test]
async fn lifespan_session_end_to_end_through_storage_api_and_rollup() {
    let mut rig = Rig::new("km/h");
    let mut reader = lifespan::Reader::new();
    let mut cyc = |status, steps, dur, dist, cal, speed| {
        ls_cycle(&mut reader, status, steps, dur, dist, cal, speed)
    };

    use crate::telemetry::{STATUS_PAUSED, STATUS_RUNNING, STATUS_STANDBY};

    // Standby: nothing opens.
    let frames = [
        cyc(STATUS_STANDBY, 0, 0, 0, 0, 0),
        cyc(STATUS_STANDBY, 0, 0, 0, 0, 0),
        // Belt starts. The 2nd running frame confirms and opens the session.
        cyc(STATUS_RUNNING, 12, 5, 1, 1, 300),
        cyc(STATUS_RUNNING, 30, 10, 2, 2, 300),
        cyc(STATUS_RUNNING, 55, 20, 3, 3, 300),
        cyc(STATUS_RUNNING, 80, 30, 4, 4, 300),
        cyc(STATUS_RUNNING, 110, 40, 5, 5, 300),
        // Pause: counters still creep during deceleration; the 2nd paused
        // frame confirms and closes.
        cyc(STATUS_PAUSED, 112, 41, 5, 5, 0),
        cyc(STATUS_PAUSED, 113, 42, 5, 5, 0),
        // Resume: a new session opens on the 2nd running frame.
        cyc(STATUS_RUNNING, 113, 42, 5, 5, 300),
        cyc(STATUS_RUNNING, 120, 50, 6, 6, 300),
        cyc(STATUS_RUNNING, 150, 65, 7, 7, 300),
        // Stop.
        cyc(STATUS_STANDBY, 150, 65, 7, 7, 0),
        cyc(STATUS_STANDBY, 150, 65, 7, 7, 0),
    ];
    for t in &frames {
        rig.push(t);
    }
    assert_eq!(rig.state.active_session(), None, "final stop must close");

    // Sessions: pause/resume = two sessions, with the exact boundary values.
    let sessions = rig.sessions();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].start_steps, Some(30));
    assert_eq!(sessions[0].steps_end, Some(113));
    assert_eq!(sessions[0].duration_s_end, Some(42));
    assert!(sessions[0].ended_ts.is_some());
    assert_eq!(sessions[1].start_steps, Some(120));
    assert_eq!(sessions[1].steps_end, Some(150));
    assert!(sessions[1].ended_ts.is_some());

    // Day totals, exactly: the console's counter ran 0→150 today and the
    // first in-session reading (30) banks the pre-session walk.
    let day = rig.today();
    assert_eq!(day["steps"], 150, "steps lost or invented: {day}");
    assert_eq!(day["duration_s"], 65);
    assert_eq!(day["distance_raw"], 7);
    assert_eq!(day["calories"], 7);
    assert_eq!(day["sessions"], 2);

    // The same numbers through the real /api/today route.
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let router = crate::api::router(rig.state.clone());
    let resp = router
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/today")
                .header("host", "127.0.0.1:1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["totals"]["steps"], 150, "API disagrees: {json}");
    assert_eq!(json["totals"]["total_steps_live"], 150);
    assert_eq!(json["totals"]["duration_s"], 65);

    // The rollup boundary: bank everything into per-minute rollups (forcing
    // every sample's minute complete — the rollup instant is based on the
    // rig's virtual clock, which the samples were stamped with), and the day
    // must not move by a single step — the 0.3.2 bug class was exactly a
    // total that shrank when the rollup ran. A second run must be a no-op.
    let now = rig.clock;
    rig.db.rollup_samples_at(now + 120.0).unwrap();
    let rolled = rig.today();
    assert_eq!(rolled["steps"], 150, "rollup changed the step total");
    assert_eq!(rolled["duration_s"], 65);
    assert_eq!(rolled["distance_raw"], 7);
    assert_eq!(rolled["calories"], 7);
    let res = rig.db.rollup_samples_at(now + 121.0).unwrap();
    assert_eq!(res["buckets_written"], 0, "second rollup must be a no-op");
    assert_eq!(rig.today()["steps"], 150);
}

// ---- KingSmith WiLink: push frames with a stale frame and a reset ------------

fn wl_telem(state: u8, speed: u8, time_s: u32, dist: u32, steps: u32) -> Telemetry {
    let mut f = vec![0u8; 20];
    f[0] = 0xF8;
    f[1] = 0xA2;
    f[2] = state;
    f[3] = speed;
    f[4] = 1; // manual mode
    f[5] = (time_s >> 16) as u8;
    f[6] = (time_s >> 8) as u8;
    f[7] = time_s as u8;
    f[8] = (dist >> 16) as u8;
    f[9] = (dist >> 8) as u8;
    f[10] = dist as u8;
    f[11] = (steps >> 16) as u8;
    f[12] = (steps >> 8) as u8;
    f[13] = steps as u8;
    f[19] = 0xFD;
    f[18] = checksum_sum(&f[1..18]);
    let status = kingsmith_wilink::parse_status(&f).expect("test frame must be valid");
    Telemetry::from_sample(&kingsmith_wilink::to_sample(&status), "km/h")
}

/// A steps-reporting session with the two classic counter pathologies —
/// a stale frame (old value wedged into the stream after a radio hiccup)
/// and a mid-session power-blip reset — followed by an FTMS device (no
/// steps at all) sharing the same day. The stale frame must not subtract,
/// the reset must keep both halves, and the step-less device must leave
/// the day's steps exactly where they were.
#[test]
fn wilink_session_with_reset_then_a_stepless_ftms_session() {
    let mut rig = Rig::new("km/h");

    let frames = [
        wl_telem(1, 60, 10, 5, 500), // running; unconfirmed (no session yet)
        wl_telem(1, 60, 20, 6, 510), // confirms → session opens, start 510
        wl_telem(1, 60, 30, 7, 520),
        wl_telem(1, 60, 31, 7, 300), // STALE frame: old counter value replayed
        wl_telem(1, 60, 32, 7, 530),
        wl_telem(1, 60, 40, 8, 540),
        wl_telem(1, 60, 41, 8, 3), // power blip: counter reset to ~0
        wl_telem(1, 60, 50, 9, 20),
        wl_telem(1, 60, 60, 10, 60),
        wl_telem(0, 0, 60, 10, 60), // stopping…
        wl_telem(0, 0, 60, 10, 60), // …confirmed → session closes
    ];
    for t in &frames {
        rig.push(t);
    }
    assert_eq!(rig.state.active_session(), None);

    // 510 banked as baseline, +10, stale 300 dropped (spike), +10, +10,
    // reset kept (+17 +40) → 597. Anything else is a lost or invented step.
    let day = rig.today();
    assert_eq!(day["steps"], 597, "de-glitch mis-handled the stream: {day}");
    assert_eq!(day["distance_raw"], 10);
    assert_eq!(day["duration_s"], 60);
    assert_eq!(
        day["calories"], 0,
        "WiLink reports no energy; a fabricated calorie count would show here"
    );

    let sessions = rig.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].steps_end, Some(60), "the closing counter value");
    // (start_steps was self-healed to 3 by the reset because the rig's
    // virtual timestamps keep the 5 s baseline window open — see the module
    // docs. On real hardware a reset minutes in leaves start_steps at 510.)

    // Now an FTMS walking pad (no step counter) does a session on the same
    // day: speed-only frames, so the belt state is derived from speed.
    let ftms_speed = |kmh_centi: u16| {
        let mut buf = vec![0x00, 0x00];
        buf.extend_from_slice(&kmh_centi.to_le_bytes());
        let d = ftms::parse_treadmill_data(&buf).unwrap();
        Telemetry::from_sample(&ftms::to_sample(&d), "km/h")
    };
    for t in [
        ftms_speed(300),
        ftms_speed(300), // confirms → session opens (steps absent)
        ftms_speed(310),
        ftms_speed(0),
        ftms_speed(0), // confirms → closes
    ] {
        rig.push(&t);
    }

    let sessions = rig.sessions();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions[1].start_steps, None,
        "a step-less device must open a session with steps ABSENT, not 0"
    );
    assert_eq!(sessions[1].steps_end, None);

    // The step-less session must leave every total exactly untouched.
    let day = rig.today();
    assert_eq!(
        day["steps"], 597,
        "an FTMS session corrupted the day's steps: {day}"
    );
    assert_eq!(day["distance_raw"], 10);
    assert_eq!(day["sessions"], 2);
}

// ---- FTMS: a full session with the fields FTMS actually reports --------------

fn ftms_telem(speed_centi: u16, dist_m: u32, kcal: u16, secs: u16) -> Telemetry {
    // flags 0x0484: speed + total distance + expended energy + elapsed time
    // (the exact flag shape a real Urevo E1L notifies).
    let mut buf = Vec::new();
    buf.extend_from_slice(&0x0484u16.to_le_bytes());
    buf.extend_from_slice(&speed_centi.to_le_bytes());
    buf.extend_from_slice(&[dist_m as u8, (dist_m >> 8) as u8, (dist_m >> 16) as u8]);
    buf.extend_from_slice(&kcal.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // kcal/h — unused
    buf.push(0); // kcal/min — unused
    buf.extend_from_slice(&secs.to_le_bytes());
    let d = ftms::parse_treadmill_data(&buf).unwrap();
    Telemetry::from_sample(&ftms::to_sample(&d), "km/h")
}

/// An FTMS-only day: distance/energy/time accrue, steps stay 0 (absent on
/// the wire, never fabricated), and the totals survive the rollup exactly.
#[test]
fn ftms_session_accrues_distance_but_never_steps() {
    let mut rig = Rig::new("km/h");

    let frames = [
        ftms_telem(0, 0, 0, 0),
        ftms_telem(0, 0, 0, 0),
        ftms_telem(300, 50, 1, 30),  // belt moving; unconfirmed
        ftms_telem(300, 100, 2, 60), // confirms → session opens
        ftms_telem(300, 200, 3, 120),
        ftms_telem(300, 350, 5, 180),
        ftms_telem(0, 350, 5, 180), // stopping…
        ftms_telem(0, 350, 5, 180), // …confirmed → closes
    ];
    for t in &frames {
        rig.push(t);
    }
    assert_eq!(rig.state.active_session(), None);

    let day = rig.today();
    assert_eq!(
        day["steps"], 0,
        "FTMS has no step counter; steps must be zero, not fabricated: {day}"
    );
    // First in-session reading (100 m = 10 raw) banks the pre-session belt
    // travel; the rest accrues incrementally to the final odometer value.
    assert_eq!(day["distance_raw"], 35, "350 m in decameters: {day}");
    assert_eq!(day["duration_s"], 180);
    assert_eq!(day["calories"], 5);
    assert_eq!(day["sessions"], 1);

    let sessions = rig.sessions();
    assert_eq!(sessions[0].start_steps, None);
    assert_eq!(sessions[0].steps_end, None);
    assert_eq!(sessions[0].distance_raw_end, Some(35));

    // The rollup must not move a single meter. (Rolled relative to the
    // rig's virtual clock so every sample's minute is complete.)
    rig.db.rollup_samples_at(rig.clock + 120.0).unwrap();
    let rolled = rig.today();
    assert_eq!(rolled["steps"], 0);
    assert_eq!(rolled["distance_raw"], 35);
    assert_eq!(rolled["duration_s"], 180);
    assert_eq!(rolled["calories"], 5);
}

/// Characterisation of the close boundary — a finding of this audit, pinned
/// so any change to it is a conscious decision.
///
/// In `ingest_sample` the session is closed BEFORE the closing sample is
/// inserted, so the sample carrying the confirmed close is stored with
/// `session_id = NULL` — and day totals only walk in-session samples.
/// Counter movement on that one frame (belt coasting to a stop after the
/// pause/stop was confirmed) is therefore:
///
/// * kept in the session record (`steps_end` — `persist_close` uses the
///   closing telemetry), and
/// * recovered into the day total by the NEXT session's first sample (the
///   counter is cumulative, so the delta picks it up), but
/// * absent from the day total if no further session opens that day.
///
/// The permanent-loss case is bounded by one frame interval of coasting —
/// single-digit steps on the last stop of a day. Not fixed here (the fix
/// would reorder close/insert in the frozen ingest path); reported instead.
#[test]
fn steps_on_the_closing_frame_land_in_the_session_and_the_next_delta() {
    let mut rig = Rig::new("km/h");
    for t in [
        wl_telem(1, 60, 10, 1, 80),
        wl_telem(1, 60, 20, 2, 90), // confirms → opens (start 90)
        wl_telem(1, 60, 30, 3, 100),
        wl_telem(0, 0, 31, 3, 101), // first stop frame — still in-session
        wl_telem(0, 0, 32, 3, 103), // confirmed close — stored OUTSIDE the session
    ] {
        rig.push(&t);
    }
    let sessions = rig.sessions();
    assert_eq!(
        sessions[0].steps_end,
        Some(103),
        "the closing counter value lands in the session record"
    );
    assert_eq!(
        rig.today()["steps"],
        101,
        "…but the 2 coasting steps on the closing frame are not in the day \
         total while no later session exists (the characterised boundary loss)"
    );

    // A later session recovers them through the cumulative counter's delta.
    for t in [
        wl_telem(1, 60, 40, 4, 103),
        wl_telem(1, 60, 50, 5, 110), // confirms → opens
        wl_telem(0, 0, 51, 5, 110),
        wl_telem(0, 0, 52, 5, 110), // closes
    ] {
        rig.push(&t);
    }
    assert_eq!(
        rig.today()["steps"],
        110,
        "the next session's delta must recover the boundary steps exactly"
    );
}

// ---- The 0.3.2 bug class: baselines across the session/rollup boundary ------

/// Connecting to a belt that is already mid-walk: the opening counter value
/// is steps already walked today and must be banked — and must STAY banked
/// once the rollup loop runs (the 0.3.2 undercount was the rollups dropping
/// the baseline the raw walk had counted).
#[test]
fn a_mid_walk_connect_keeps_its_baseline_through_the_rollup() {
    let mut rig = Rig::new("km/h");
    let frames = [
        wl_telem(1, 60, 300, 30, 1800), // already walking; unconfirmed
        wl_telem(1, 60, 310, 31, 1820), // confirms → opens, baseline banked
        wl_telem(1, 60, 320, 32, 1850),
        wl_telem(1, 60, 330, 33, 1900),
        wl_telem(0, 0, 330, 33, 1900),
        wl_telem(0, 0, 330, 33, 1900), // closes
    ];
    for t in &frames {
        rig.push(t);
    }

    let day = rig.today();
    assert_eq!(day["steps"], 1900, "the pre-connect walk must be banked");

    rig.db.rollup_samples_at(rig.clock + 120.0).unwrap();
    assert_eq!(
        rig.today()["steps"],
        1900,
        "the rollup dropped the first-reading baseline — the 0.3.2 bug"
    );
}

/// The SC110 zeroes its counters a moment AFTER the belt starts, so the
/// session-opening telemetry still carries the PREVIOUS session's total.
/// The stored baseline must self-heal to the true post-reset value, and the
/// day total must count both sessions exactly once.
#[test]
fn a_stale_session_baseline_self_heals_and_the_day_reconciles() {
    let mut rig = Rig::new("km/h");
    use crate::telemetry::{STATUS_PAUSED, STATUS_RUNNING, STATUS_STANDBY};
    let mut reader = lifespan::Reader::new();
    let mut cyc = |status, steps| ls_cycle(&mut reader, status, steps, 0, 0, 0, 300);

    // Session A ends at 765 on the console.
    for t in [
        cyc(STATUS_RUNNING, 700),
        cyc(STATUS_RUNNING, 720), // opens (start 720)
        cyc(STATUS_RUNNING, 765),
        cyc(STATUS_PAUSED, 765),
        cyc(STATUS_PAUSED, 765), // closes (end 765)
    ] {
        rig.push(&t);
    }
    // Session B: opens on the stale 765, then the console zeroes to 2.
    for t in [
        cyc(STATUS_RUNNING, 765),
        cyc(STATUS_RUNNING, 765), // opens with the stale baseline 765
        cyc(STATUS_RUNNING, 2),   // the console's delayed zeroing
        cyc(STATUS_RUNNING, 30),
        cyc(STATUS_RUNNING, 87),
        cyc(STATUS_STANDBY, 87),
        cyc(STATUS_STANDBY, 87), // closes
    ] {
        rig.push(&t);
    }

    let sessions = rig.sessions();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].steps_end, Some(765));
    assert_eq!(
        sessions[1].start_steps,
        Some(2),
        "the stale 765 baseline must self-heal to the post-reset value"
    );
    assert_eq!(sessions[1].steps_end, Some(87));

    // 765 walked before the reset + 85 after = 850, counted exactly once.
    assert_eq!(rig.today()["steps"], 850);
}

// ---- PitPat BA09-B: steps in the extended tail, counters reset on stop -------

/// A SupeRun BA09-B status frame (60 bytes, fw 37) on the layout of the real
/// capture, decoded by the real parser and converted by the real `to_sample`.
fn ba09_telem(
    running: bool,
    speed_raw: u16,
    dist_m: u32,
    steps: u16,
    kcal: u16,
    dur_ms: u32,
) -> Telemetry {
    let mut f = vec![0u8; 60];
    f[0] = 0x67;
    f[1] = 60;
    f[3..5].copy_from_slice(&speed_raw.to_be_bytes());
    f[7..11].copy_from_slice(&dist_m.to_be_bytes());
    f[18..20].copy_from_slice(&kcal.to_be_bytes());
    f[20..24].copy_from_slice(&dur_ms.to_be_bytes());
    f[25] = 0x25;
    f[26] = if running {
        pitpat::STATE_RUNNING
    } else {
        pitpat::STATE_STOPPED
    };
    f[43..45].copy_from_slice(&steps.to_be_bytes());
    f[59] = pitpat::TERMINATOR;
    f[58] = pitpat::frame_checksum(&f);
    let status = pitpat::parse_status(&f).unwrap();
    Telemetry::from_sample(&pitpat::to_sample(&status), "km/h")
}

#[test]
fn ba09_walk_keeps_its_steps_when_the_counter_resets_on_stop() {
    let mut rig = Rig::new("km/h");
    rig.push(&ba09_telem(false, 0, 0, 0, 0, 0));
    for i in 1..=10u32 {
        rig.push(&ba09_telem(
            true,
            4000,
            i * 8,
            (i * 10) as u16,
            (i / 3) as u16,
            i * 5000,
        ));
    }
    // Belt stops: steps/kcal/duration drop to 0, distance stays.
    rig.push(&ba09_telem(false, 0, 80, 0, 0, 0));
    rig.push(&ba09_telem(false, 0, 80, 0, 0, 0));

    assert_eq!(rig.sessions().len(), 1);
    assert_eq!(
        rig.today()["steps"],
        100,
        "the day keeps the last counter value before the stop reset"
    );
}
