//! BLE engine: owns the connection to the one paired treadmill, picks the
//! driver for it from the registry, and does everything a driver must not —
//! scanning, connect/reconnect with backoff, give-up-after-N-failures,
//! cancellation (pause / device switch / shutdown), session detection,
//! throttled persistence, and the WebSocket broadcast.
//!
//! Drivers (see `drivers/`) only translate a device's Bluetooth traffic into
//! neutral `Sample`s; the conversion to the presentation `Telemetry` happens
//! once, here, at `Telemetry::from_sample`.

use crate::app::{state_dict, unix_now, AppState};
use crate::drivers::{self, Advertisement, DriverHost, Sample};
use crate::telemetry::Telemetry;
use anyhow::{anyhow, Result};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Consecutive failed connect attempts (never reaching the treadmill) before the
/// worker gives up and waits for a manual reconnect, instead of scanning forever.
/// Each attempt scans up to ~10s, so this is roughly a minute of trying.
const MAX_CONNECT_ATTEMPTS: u32 = 6;
/// How long a belt state (running / not running) must hold before it opens or
/// closes a session.
///
/// A DURATION, deliberately not a frame count. Frame rate varies ~20× across
/// drivers (LifeSpan answers a status opcode every ~0.5 s, FTMS pushes at
/// 1 Hz, the props transport streams sub-second), so "two frames" meant
/// anything from 100 ms to 2 s depending on the treadmill. Worse, a frame
/// count amplifies a real failure shape: three drivers derive state from a
/// speed threshold, and a belt sitting at that threshold can alternate
/// running/not-running in PAIRS by chance — with a per-frame debounce each
/// pair opened and closed a session every four frames, each cycle
/// invalidating the today-cache and forcing a full `day_totals` de-glitch
/// walk under the DB mutex (quadratic as the day grows; single-frame
/// alternation was always harmless — only pairs fired). Requiring the state
/// to HOLD for this long is uniform across the frame-rate spread and kills
/// the pair-alternation path outright: an alternating state never holds.
const SESSION_DEBOUNCE: Duration = Duration::from_secs(3);

async fn first_adapter() -> Result<Adapter> {
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter found"))
}

/// What a peripheral advertised, in the neutral shape drivers match against.
fn advertisement(name: &str, services: &[uuid::Uuid]) -> Advertisement {
    Advertisement {
        name: name.to_string(),
        services: services.to_vec(),
    }
}

/// Active scan returning treadmill-looking candidates (or everything if
/// all_devices). "Treadmill-looking" is decided by the driver registry, so a
/// newly added driver's devices show up here with no extra wiring.
pub async fn scan(seconds: f64, all_devices: bool) -> Result<serde_json::Value> {
    let seconds = seconds.clamp(1.0, 15.0);
    let adapter = first_adapter().await?;
    adapter.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
    let peripherals = adapter.peripherals().await?;

    let mut rows = Vec::new();
    for p in peripherals {
        // Read properties while discovery is still running. BlueZ deletes
        // temporary (unpaired LE) device objects on StopDiscovery, so a device
        // read only after stop_scan aborts the whole scan with a D-Bus
        // MethodNotFound ("GetAll ... doesn't exist"). Reading first keeps a
        // just-found treadmill visible; a device that vanishes mid-scan just
        // skips, it never fails the scan.
        let props = match p.properties().await {
            Ok(Some(props)) => props,
            _ => continue,
        };
        let name = props.local_name.clone().unwrap_or_default();
        let is_match = drivers::any_match(&advertisement(&name, &props.services));
        if !all_devices && !is_match {
            continue;
        }
        rows.push(json!({
            "device_id": p.id().to_string(),
            "name": name,
            "rssi": props.rssi,
            "service_uuids": props.services.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            "match": is_match,
        }));
    }
    let _ = adapter.stop_scan().await;
    rows.sort_by(|a, b| {
        let ra = a["rssi"].as_i64().unwrap_or(-999);
        let rb = b["rssi"].as_i64().unwrap_or(-999);
        rb.cmp(&ra)
    });
    Ok(json!({"devices": rows, "scanned_s": seconds}))
}

/// Worker entry point — runs forever, reconnecting with backoff.
pub async fn run(state: Arc<AppState>) {
    // Only our own — a follower must not stamp an end on a walk still running
    // on another device (see close_stale_active).
    let mine = crate::config::device_name();
    if let Ok(closed) = state
        .db
        .close_stale_active("backend_restart", Some(mine.as_str()))
    {
        if closed > 0 {
            tracing::warn!("closed {closed} stale active session(s) at startup");
        }
    }

    let mut fails: u32 = 0;
    while !state.stop.load(Ordering::Relaxed) {
        // Register interest in `wake` BEFORE reading any of the state it guards.
        //
        // `Notify::notify_waiters()` (what set_paused / set_device_id call) only
        // wakes waiters that are ALREADY registered — unlike `notify_one()` it
        // stores no permit. Checking a flag and only then awaiting would drop a
        // wake that lands in between, parking the worker forever while
        // `/api/connect` cheerfully returned {"ok":true}. `enable()` registers us
        // up front, so such a wake is delivered to this future and the await
        // returns immediately. The same future is reused for the reconnect
        // backoff below, so no wake can be swallowed mid-iteration either.
        let wake = state.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();

        let device_id = state.device_id();
        if device_id.is_none() {
            state.connected.store(false, Ordering::Relaxed);
            state.broadcast(json!({"type": "status", "connected": false, "paired": false}));
            tracing::info!("no device paired; waiting for /api/pair");
            wake.as_mut().await;
            continue;
        }
        let device_id = device_id.unwrap();

        // Idle: manually disconnected, or gave up after repeated failures. Either
        // way stop trying and wait for a manual reconnect — the engine (and cloud
        // sync) keep running throughout.
        if state.is_paused() || state.is_connect_failed() {
            state.connected.store(false, Ordering::Relaxed);
            state.broadcast(json!({
                "type": "status", "connected": false, "paired": true,
                "paused": state.is_paused(), "connect_failed": state.is_connect_failed()
            }));
            wake.as_mut().await;
            continue;
        }

        // A drop after a successful connect is a normal reconnect (resets the
        // counter); never reaching the treadmill counts toward giving up.
        let was_connected = match connect_and_poll(&state, &device_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("BLE error: {e:#}");
                false
            }
        };
        if was_connected {
            fails = 0;
        } else {
            fails += 1;
            tracing::warn!("connect attempt {fails}/{MAX_CONNECT_ATTEMPTS} failed for {device_id}");
            if fails >= MAX_CONNECT_ATTEMPTS {
                fails = 0;
                state.set_connect_failed(true);
                tracing::warn!("giving up auto-connect; waiting for manual reconnect");
                continue; // → idle branch broadcasts the failure and waits
            }
        }

        state.connected.store(false, Ordering::Relaxed);
        state.broadcast(json!({
            "type": "status", "connected": false, "paired": state.device_id().is_some(),
            "paused": state.is_paused(), "connect_failed": state.is_connect_failed()
        }));

        // Close any open session on link loss.
        let active = state.active_session();
        if let Some(sid) = active {
            let last = state.last_state();
            persist_close(&state, sid, last.as_ref(), "ble_disconnect");
            state.invalidate_today();
            state.broadcast(json!({"type": "session_end", "id": sid}));
            state.set_active_session(None);
        }

        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = wake.as_mut() => {}
        }
    }

    // Loop exited (stop requested). The peripheral was disconnected on the way
    // out of connect_and_poll; tell shutdown() we're done so it can stop waiting.
    tracing::info!("BLE worker stopped");
    state.ble_done.notify_one();
}

async fn find_peripheral(adapter: &Adapter, device_id: &str) -> Result<Peripheral> {
    adapter.start_scan(ScanFilter::default()).await?;
    // Poll for up to 10s for a peripheral whose id matches the saved one.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        for p in adapter.peripherals().await? {
            if p.id().to_string() == device_id {
                let _ = adapter.stop_scan().await;
                return Ok(p);
            }
        }
    }
    let _ = adapter.stop_scan().await;
    Err(anyhow!("device {device_id} not found in scan"))
}

/// Resolves when the driver must be torn down: shutdown, a manual disconnect
/// (pause), or the paired device changing under us. Registered-before-check on
/// `wake`, for the same lost-wake reason as the worker loop above.
async fn cancelled(state: &Arc<AppState>, device_id: &str) {
    loop {
        let wake = state.wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        if state.is_paused() {
            tracing::info!("manual disconnect requested; dropping link");
            return;
        }
        if state.device_id().as_deref() != Some(device_id) {
            tracing::info!("device_id changed; dropping connection");
            return;
        }
        wake.as_mut().await;
    }
}

/// Returns `Ok(true)` once a link was established (a later mid-session drop is
/// still `true` — a normal reconnect, not a failure), `Ok(false)` if we never
/// reached the treadmill (counts toward the give-up limit).
async fn connect_and_poll(state: &Arc<AppState>, device_id: &str) -> Result<bool> {
    tracing::info!("connecting to {device_id}...");
    let adapter = first_adapter().await?;
    let peripheral = match find_peripheral(&adapter, device_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::info!("{device_id} not found in scan: {e:#}");
            return Ok(false);
        }
    };
    if let Err(e) = peripheral.connect().await {
        tracing::warn!("connect to {device_id} failed: {e:#}");
        return Ok(false);
    }
    if let Err(e) = peripheral.discover_services().await {
        tracing::warn!("service discovery failed: {e:#}");
        let _ = peripheral.disconnect().await;
        return Ok(false);
    }

    // Pick the driver: first registry entry whose `supports()` accepts this
    // device's GATT table (and advertisement). Registry order is the tiebreak —
    // see `drivers::DRIVERS` for why LifeSpan outranks FTMS.
    let adv = match peripheral.properties().await {
        Ok(Some(props)) => advertisement(&props.local_name.unwrap_or_default(), &props.services),
        _ => advertisement("", &[]),
    };
    let gatt = peripheral.characteristics();
    // The full supporter set, not just the winner: registry order is
    // load-bearing and untestable against real hardware, so the dispatch
    // line below records every driver that would have claimed this device.
    let supporters = drivers::supporters(&adv, &gatt);
    let driver = match drivers::for_device(&adv, &gatt) {
        Some(d) => d,
        None => {
            let _ = peripheral.disconnect().await;
            return Err(anyhow!(
                "no driver supports this device's characteristics (drivers: {})",
                drivers::ids().join(", ")
            ));
        }
    };

    announce_connected(state, device_id, driver.id(), &supporters);

    // The display unit is captured once per connection: the driver interprets
    // its wire format with it and `from_sample` re-encodes with it, and the two
    // MUST agree or raw values would drift on a mid-session unit change.
    let unit = state.display_unit();
    let recorder = |tag: u8, frame: &[u8]| state.record_frame(tag, frame);
    let host = DriverHost::new(unit.clone(), &recorder);

    let mut ing = IngestState::default();
    let mut emit = |sample: Sample| {
        let telem = Telemetry::from_sample(&sample, &unit);
        // `ingest_sample` returns the GATED telemetry; broadcast that, so a
        // field the plausibility gate stripped never reaches /ws either.
        let telem = ingest_sample(state, &telem, unix_now(), &mut ing);
        broadcast_state(state, &telem);
    };

    // The driver runs until the link errors; we cancel it on shutdown, pause,
    // or device switch. Either way the disconnect below is ours, not the
    // driver's — a driver never manages the link's lifecycle.
    let outcome = tokio::select! {
        r = driver.run(&peripheral, &host, &mut emit) => r,
        _ = cancelled(state, device_id) => Ok(()),
    };
    let _ = peripheral.disconnect().await;
    if let Err(e) = outcome {
        tracing::warn!("BLE session ended: {e:#}");
    }
    Ok(true) // we did connect; a drop here is a normal reconnect
}

fn announce_connected(state: &Arc<AppState>, device_id: &str, kind: &str, supporters: &[&str]) {
    state.connected.store(true, Ordering::Relaxed);
    state.set_driver(kind); // `/api/state`'s `driver` field
    state.broadcast(json!({
        "type": "status", "connected": true, "paired": true,
        "display_unit": state.display_unit(), "device_id": device_id,
    }));
    // The dispatch record, one line, stable format — keep it greppable as
    // "driver dispatch:". `accepted` is EVERY driver whose supports() took
    // the device, in registry order (the winner is the first); when a future
    // driver starts shadowing another on real hardware, this line is the
    // whole bug report. WARN, deliberately, so it survives default log
    // filtering — it prints once per connect.
    tracing::warn!(
        "driver dispatch: device={device_id} claimed={kind} accepted=[{}]",
        supporters.join(", ")
    );
    tracing::info!("connected ({kind})");
}

fn broadcast_state(state: &Arc<AppState>, telem: &Telemetry) {
    let mut msg = json!({"type": "state", "state": state_dict(telem)});
    if let Value::Object(ref mut m) = msg {
        m.insert("today".into(), state.today_payload());
        m.insert("active_session_id".into(), json!(state.active_session()));
    }
    state.broadcast(msg);
}

/// Minimum spacing between PERSISTED raw samples. We poll far faster than this
/// (~50 ms plus the radio round trip, so 10–15 telemetry updates a second) but
/// storing every one of them wrote ~1M rows per day of walking — bloating the
/// database and making every day-total aggregation proportionally slower — for no
/// extra fidelity: the rollups are per-minute and the UI ticks about once a
/// second. Mirrors `db::SAMPLE_INTERVAL_S`, which converts a count of running
/// samples back into seconds; the two MUST stay in step.
const SAMPLE_MIN_INTERVAL_S: f64 = crate::db::SAMPLE_INTERVAL_S;

// ---- PLAUSIBILITY GATE -------------------------------------------------------
//
// A rate-sanity gate on the counters a driver reports, applied at the very
// top of `ingest_sample` — before `set_last_state`, deliberately:
// `persist_close` reads `last_state()` on link loss to write the session's
// closing values, so a gate any lower would still let a poisoned final frame
// into the session row. Gating here covers the raw insert, the live session
// update, the session close values, the broadcast (the caller broadcasts the
// gated return value) and — because rollups derive from raw — the rollup.
//
// WHAT IT IS: a development tool and damage limiter for unit-scale errors.
// A new driver decoding a field at 10–100× its true scale used to produce
// *visibly wrong* numbers during hardware testing; this gate strips such a
// field (sets it absent) instead of storing it, counts the strip
// (`rejected_samples` in /api/diag) and WARNs with the field and the rate —
// quietly-missing-but-loudly-counted, never silently wrong AND never
// silently missing.
//
// WHAT IT IS NOT — read this before trusting it:
// * NOT a security boundary. A mis-decode (or a hostile device) that lands
//   in plausible range passes — and most mis-decodes on unverified hardware
//   land in plausible range. SECURITY.md says the same.
// * Sustained grinding just inside the ceilings passes by construction
//   (~690k steps/day at 8 steps/s).
// * RESET LAUNDERING passes: decreases must pass untouched (below), so a
//   device cycling 0 → 700 → 0 → 700 stays inside every rate ceiling and
//   banks each ramp — structurally indistinguishable from a real power
//   cycle. Bounding how often a reset is HONOURED was considered and
//   rejected: a causal gate cannot tell a reset from a one-frame stale-low
//   read (db.rs's `deglitch_walk` distinguishes them using the FOLLOWING
//   frame, which a streaming gate does not have), so a reset-frequency cap
//   would misfire on the documented stale-frame behaviour of real consoles
//   and start dropping good data. See the deferral note on `FieldGate`.
//
// DIVISION OF LABOUR with `db.rs`: this gate is per-connection rate sanity
// on LIVE samples (units-scale errors, absurd single frames); `db.rs`'s
// de-glitch accumulator is cross-frame glitch REPAIR on STORED ones
// (isolated spikes, resets, stale reads). The gate must never intercept
// what the de-glitcher can repair — that is why decreases always pass and
// why the burst allowances below are generous: a value the gate strips is
// invisible to the de-glitcher forever, so the gate only strips what no
// treadmill can physically report.
//
// RULES:
// * Rate-based, never absolute (except speed): the counters are cumulative,
//   so absolute bounds are useless (a real 60k-step day is legitimate) and
//   dangerous. What is gated is the INCREASE per elapsed second.
// * Every decrease passes untouched, so `deglitch_walk`'s reset handling
//   keeps working on exactly the stream it always saw.
// * Driver-agnostic: one set of ceilings for every driver, named below with
//   their rationale. No per-driver knobs — a knob here would be one more
//   obligation for the next driver author.
//
// The ceilings bound what any treadmill can physically produce; the burst
// allowances absorb quantisation, BLE notification coalescing and
// subscribe-time replay (a stack can deliver a queued burst in one
// instant). Sizing: each allowance is small enough that a 10× unit-scale
// error at walking pace exceeds `ceiling × dt + allowance` within
// [`GATE_ANCHOR_WINDOW_S`] (so it IS caught), and large enough that every
// legitimate stream in the pipeline test corpus passes with margin. The
// per-field envelope re-anchors at most once per window — per-frame
// re-anchoring would let a modest sustained over-rate slip under the
// allowance frame by frame.

/// Steps ceiling: elite running cadence is ~5 steps/s; 8/s is beyond any
/// treadmill gait.
const GATE_MAX_STEPS_PER_S: f64 = 8.0;
/// Steps burst allowance. Strictly above db.rs's step spike threshold (50),
/// so any single-frame glitch small enough for the de-glitcher to judge is
/// never intercepted here.
const GATE_STEPS_BURST: f64 = 60.0;
/// Distance ceiling, in raw decameters/s: 1 raw/s = 10 m/s = 36 km/h,
/// faster than any treadmill sold.
const GATE_MAX_DISTANCE_RAW_PER_S: f64 = 1.0;
/// Distance burst allowance: 20 raw = 200 m, one decameter-quantised burst.
const GATE_DISTANCE_RAW_BURST: f64 = 20.0;
/// Duration ceiling: elapsed workout time cannot advance faster than the
/// wall clock; 2× covers clock skew between console and host.
const GATE_MAX_DURATION_PER_S: f64 = 2.0;
/// Duration burst allowance: subscribe-time replay can deliver minutes of
/// backlog at once, and duration is the least corruptible field (its
/// long-run rate is wall-clock-bounded), so the allowance is generous —
/// a 10× duration mis-scale still exceeds the envelope within ~20 s.
const GATE_DURATION_BURST: f64 = 150.0;
/// Calories ceiling: 1 kcal/s = 3600 kcal/h, several times any walking
/// workload.
const GATE_MAX_CALORIES_PER_S: f64 = 1.0;
/// Calories burst allowance: 60 kcal ≈ an hour of walking delivered as one
/// coalesced burst; a 10× kcal mis-scale still trips within ~10 s.
const GATE_CALORIES_BURST: f64 = 60.0;
/// Speed is a live reading, not a cumulative counter, so it takes the one
/// absolute bound: 3000 centi-units = 30 km/h or 30 mph by console unit —
/// beyond either interpretation of any under-desk belt.
const GATE_MAX_SPEED_RAW: u32 = 3000;
/// How long a field's envelope anchor holds before it may re-anchor to the
/// current value. Long enough that a sustained over-rate must reveal itself
/// against one anchor; short enough that the envelope tracks a real day.
const GATE_ANCHOR_WINDOW_S: f64 = 60.0;
/// Rate limit for the gate's WARN line (the counter in /api/diag is exact;
/// the log is a hint, not a ledger).
const GATE_WARN_INTERVAL_S: f64 = 30.0;

/// Per-field envelope state for the plausibility gate.
#[derive(Debug, Default, Clone)]
struct FieldGate {
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
    /// Admit or refuse `v` at `now`. Refusal means "strip the field from
    /// this sample"; the anchor is left untouched so a genuinely absurd
    /// stream stays refused (and counted) instead of ratcheting the
    /// envelope up.
    fn admit(&mut self, v: u32, now: f64, ceiling: f64, burst: f64) -> bool {
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
        } else if now - ats >= GATE_ANCHOR_WINDOW_S {
            self.anchor = Some((v, now));
        }
        true
    }
}

/// Per-connection plausibility-gate state, part of [`IngestState`].
#[derive(Debug, Default)]
pub(crate) struct GateState {
    steps: FieldGate,
    distance: FieldGate,
    duration: FieldGate,
    calories: FieldGate,
    last_warn: f64,
}

/// Apply the plausibility gate: returns the (possibly field-stripped) copy
/// of `telem` to ingest, plus a description of each stripped field for the
/// WARN line. A stripped field becomes ABSENT — the same value it would
/// have carried had the device not reported it — never zero.
fn gate_telemetry(telem: &Telemetry, now: f64, gate: &mut GateState) -> (Telemetry, Vec<String>) {
    let mut t = telem.clone();
    let mut dropped: Vec<String> = Vec::new();

    let mut counter =
        |field: &mut FieldGate, name: &str, value: Option<u32>, ceiling: f64, burst: f64| -> bool {
            let Some(v) = value else { return true };
            let anchor = field.anchor;
            if field.admit(v, now, ceiling, burst) {
                return true;
            }
            let (av, ats) = anchor.expect("refusal implies an anchor");
            dropped.push(format!(
                "{name} +{} in {:.1}s (ceiling {ceiling}/s + {burst})",
                v.saturating_sub(av),
                now - ats
            ));
            false
        };

    if !counter(
        &mut gate.steps,
        "steps",
        t.steps,
        GATE_MAX_STEPS_PER_S,
        GATE_STEPS_BURST,
    ) {
        t.steps = None;
    }
    if !counter(
        &mut gate.distance,
        "distance_raw",
        t.distance_raw,
        GATE_MAX_DISTANCE_RAW_PER_S,
        GATE_DISTANCE_RAW_BURST,
    ) {
        t.distance_raw = None;
        t.distance_m = None;
        t.distance_km = None;
        t.distance_mi = None;
    }
    if !counter(
        &mut gate.duration,
        "duration_s",
        t.duration_s,
        GATE_MAX_DURATION_PER_S,
        GATE_DURATION_BURST,
    ) {
        t.duration_s = None;
    }
    if !counter(
        &mut gate.calories,
        "calories",
        t.calories,
        GATE_MAX_CALORIES_PER_S,
        GATE_CALORIES_BURST,
    ) {
        t.calories = None;
    }
    if let Some(sr) = t.speed_raw {
        if sr > GATE_MAX_SPEED_RAW {
            dropped.push(format!(
                "speed_raw {sr} (absolute cap {GATE_MAX_SPEED_RAW})"
            ));
            t.speed_raw = None;
            t.speed_kmh = None;
            t.speed_mph = None;
        }
    }
    (t, dropped)
}

/// The per-connection loop state `ingest_sample` mutates: the plausibility
/// gate, the session debounce and the persistence throttle. One instance per
/// connection, created in `connect_and_poll` (and per test rig).
#[derive(Debug, Default)]
pub(crate) struct IngestState {
    pub(crate) gate: GateState,
    pub(crate) last_status: Option<u8>,
    /// Session-debounce state: the `is_running` value the stream is
    /// currently holding and when it started holding it — a session opens
    /// (closes) only once `is_running` has held true (false) for
    /// [`SESSION_DEBOUNCE`], regardless of how many frames arrived between.
    pub(crate) run_held: Option<(bool, f64)>,
    pub(crate) last_persist: f64,
}

/// Ingest one telemetry update: plausibility gate, session detection,
/// throttled persistence. Returns the GATED telemetry — the caller must
/// broadcast that, not its input, or a stripped field would still reach
/// `/ws` clients.
///
/// `now` is the wall clock, injected so the time-based session debounce and
/// the gate are testable.
pub(crate) fn ingest_sample(
    state: &Arc<AppState>,
    telem: &Telemetry,
    now: f64,
    ing: &mut IngestState,
) -> Telemetry {
    // The gate runs FIRST — before `set_last_state` — because
    // `persist_close` reads `last_state()` on link loss for the session's
    // closing values (see the PLAUSIBILITY GATE comment above).
    let (telem, dropped) = gate_telemetry(telem, now, &mut ing.gate);
    if !dropped.is_empty() {
        state.count_rejected_sample();
        if now - ing.gate.last_warn >= GATE_WARN_INTERVAL_S {
            ing.gate.last_warn = now;
            tracing::warn!(
                "plausibility gate: dropped {} — if this climbs during a \
                 hardware test the driver's unit scale is wrong (constants \
                 and rationale: PLAUSIBILITY GATE in ble.rs; running count \
                 in /api/diag rejected_samples: {})",
                dropped.join(", "),
                state.rejected_samples()
            );
        }
    }
    // `steps_supported` is observed from the stream, post-gate: the first
    // sample carrying steps concludes `true`, and nothing ever concludes
    // `false` (see AppState::steps_supported_json).
    if telem.steps.is_some() {
        state.note_steps_seen();
    }
    let (last_status, run_held, last_persist) = (
        &mut ing.last_status,
        &mut ing.run_held,
        &mut ing.last_persist,
    );
    state.set_last_state(Some(telem.clone()));

    // The persistence throttle needs to know whether this telemetry
    // represents a status transition (transitions always persist).
    let status_changed = telem.status.is_some() && telem.status != *last_status;
    if telem.status.is_some() {
        *last_status = telem.status;
    }

    // Session debounce, by TIME held, not frames seen (see SESSION_DEBOUNCE):
    // any flip of is_running restarts the clock, so an alternating stream
    // never confirms anything.
    let held_s = match *run_held {
        Some((running, since)) if running == telem.is_running => now - since,
        _ => {
            *run_held = Some((telem.is_running, now));
            0.0
        }
    };
    let confirmed = held_s >= SESSION_DEBOUNCE.as_secs_f64();
    let active = state.active_session();

    if confirmed && telem.is_running && active.is_none() {
        // Attribute the session to this install so the multi-device breakdown can
        // split steps by device. Empty name → NULL (surfaced as "Unknown").
        let source = crate::config::device_name();
        let source = (!source.is_empty()).then_some(source);
        if let Ok(sid) = state.db.open_session(
            now,
            &state.display_unit(),
            telem.steps,
            telem.duration_s,
            source.as_deref(),
        ) {
            state.set_active_session(Some(sid));
            state.invalidate_today();
            state.broadcast(json!({"type": "session_start", "id": sid}));
            tracing::info!("session {sid} started (start_steps={:?})", telem.steps);
        }
    } else if confirmed && !telem.is_running && active.is_some() {
        let sid = active.unwrap();
        let reason = telem
            .status_name
            .clone()
            .unwrap_or_else(|| "stopped".into());
        persist_close(state, sid, Some(&telem), &reason);
        state.invalidate_today();
        tracing::info!("session {sid} closed");
        state.broadcast(json!({"type": "session_end", "id": sid}));
        state.set_active_session(None);
    }

    // Persist at most one row per SAMPLE_MIN_INTERVAL_S. A status change is always
    // written through, so a start/stop transition is never quietly dropped by the
    // throttle. Session detection above runs off live telemetry, not stored rows,
    // so throttling cannot affect it.
    if !status_changed && now - *last_persist < SAMPLE_MIN_INTERVAL_S {
        return telem;
    }
    *last_persist = now;

    if let Some(sid) = state.active_session() {
        if let Err(e) = state.db.update_active_session(
            sid,
            telem.steps,
            telem.duration_s,
            telem.distance_raw,
            telem.calories,
            telem.speed_raw,
        ) {
            tracing::warn!("could not update session {sid}: {e}");
        }
    }

    // Persist the raw sample. Never silently swallow the error: a dropped write
    // is lost walking, and when the cause is contention (a second daemon on the
    // same database) the log line is the only way to notice.
    if let Err(e) = state.db.insert_sample(
        state.active_session(),
        now,
        telem.steps,
        telem.duration_s,
        telem.speed_raw,
        telem.distance_raw,
        telem.calories,
        telem.status,
    ) {
        tracing::warn!("could not persist sample: {e}");
    }
    telem
}

fn persist_close(state: &Arc<AppState>, sid: i64, telem: Option<&Telemetry>, reason: &str) {
    let _ = state.db.close_session(
        sid,
        unix_now(),
        telem.and_then(|t| t.steps),
        telem.and_then(|t| t.duration_s),
        telem.and_then(|t| t.distance_raw),
        telem.and_then(|t| t.calories),
        telem.and_then(|t| t.speed_raw),
        reason,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::telemetry::{STATUS_RUNNING, STATUS_STANDBY};

    fn running(steps: u32) -> Telemetry {
        let mut t = Telemetry::new("km/h");
        t.status = Some(STATUS_RUNNING);
        t.status_name = Some("RUNNING".into());
        t.is_running = true;
        t.steps = Some(steps);
        t
    }

    fn raw_count(db: &Db) -> i64 {
        db.rollup_status().unwrap()["raw_samples"].as_i64().unwrap()
    }

    /// The worker produces 10-15 telemetry updates a second; storing every one of
    /// them wrote ~1M rows per day of walking. At most one row per interval should
    /// land — but a status transition must always be written through, so a
    /// start/stop edge is never lost to the throttle.
    #[test]
    fn throttles_raw_writes_but_never_drops_a_transition() {
        let db = Arc::new(Db::open(":memory:").unwrap());
        let state = AppState::new(db.clone(), "km/h".into(), None, "tok".into());
        let mut ing = IngestState::default();

        // A burst of same-status telemetry, all well inside one interval.
        for i in 0..50 {
            ingest_sample(&state, &running(i), unix_now(), &mut ing);
        }
        assert_eq!(
            raw_count(&db),
            1,
            "a burst within one interval must collapse to a single stored sample"
        );

        // Stopping is a transition: it must be persisted immediately, even though
        // we are still inside the throttle window.
        let mut stopped = running(50);
        stopped.status = Some(STATUS_STANDBY);
        stopped.status_name = Some("STANDBY".into());
        stopped.is_running = false;
        ingest_sample(&state, &stopped, unix_now(), &mut ing);
        assert_eq!(
            raw_count(&db),
            2,
            "a status change must be written through the throttle"
        );
    }

    /// The real ingest loop-state, driven on a synthetic clock.
    struct DebounceRig {
        state: Arc<AppState>,
        ing: IngestState,
        base: f64,
    }

    impl DebounceRig {
        fn new() -> Self {
            let db = Arc::new(Db::open(":memory:").unwrap());
            DebounceRig {
                state: AppState::new(db, "km/h".into(), None, "tok".into()),
                ing: IngestState::default(),
                base: unix_now(),
            }
        }

        fn push_at(&mut self, offset_s: f64, telem: &Telemetry) -> Telemetry {
            self.ing.last_persist = 0.0; // the throttle is pinned elsewhere
            ingest_sample(&self.state, telem, self.base + offset_s, &mut self.ing)
        }
    }

    fn stopped() -> Telemetry {
        let mut t = Telemetry::new("km/h");
        t.status = Some(STATUS_STANDBY);
        t.status_name = Some("STANDBY".into());
        t.is_running = false;
        t
    }

    /// The debounce is a DURATION, not a frame count: frame rate varies ~20×
    /// across drivers, so only "state held for SESSION_DEBOUNCE" means the
    /// same thing on every treadmill. Many fast frames must not confirm; a
    /// held state must, at any frame rate.
    #[test]
    fn session_debounce_counts_time_held_not_frames_seen() {
        let mut rig = DebounceRig::new();

        // 20 running frames in under a second — a fast driver. A frame-count
        // debounce would have opened on the second frame.
        for i in 0..20 {
            rig.push_at(i as f64 * 0.05, &running(i));
        }
        assert_eq!(
            rig.state.active_session(),
            None,
            "20 running frames inside 1 s opened a session — the debounce \
             must count time held, not frames seen (SESSION_DEBOUNCE in \
             ble.rs is a Duration; drivers emit at wildly different rates)"
        );

        // The same state still held after SESSION_DEBOUNCE: opens.
        rig.push_at(3.5, &running(30));
        assert!(
            rig.state.active_session().is_some(),
            "running held past SESSION_DEBOUNCE must open a session"
        );

        // Closing mirrors opening: a fresh stop frame doesn't close…
        rig.push_at(4.0, &stopped());
        rig.push_at(5.0, &stopped());
        assert!(
            rig.state.active_session().is_some(),
            "a stop held less than SESSION_DEBOUNCE must not close the session"
        );
        // …but a stop held past the debounce does.
        rig.push_at(7.5, &stopped());
        assert_eq!(
            rig.state.active_session(),
            None,
            "not-running held past SESSION_DEBOUNCE must close the session"
        );
    }

    // ---- Plausibility gate ---------------------------------------------------

    /// An implausible step INCREASE is stripped (absent, not zero), counted,
    /// and the rest of the sample survives — the gate is per field, not per
    /// sample, so session detection and speed keep flowing.
    #[test]
    fn gate_strips_an_implausible_step_rate_and_counts_it() {
        let mut rig = DebounceRig::new();
        rig.push_at(0.0, &running(100)); // baseline: always accepted
        let mut hostile = running(100_000); // +99 900 steps in one second
        hostile.speed_raw = Some(300);
        let got = rig.push_at(1.0, &hostile);
        assert_eq!(
            got.steps, None,
            "+99900 steps/s must be stripped — the rate ceiling is \
             GATE_MAX_STEPS_PER_S (PLAUSIBILITY GATE constants in ble.rs)"
        );
        assert_eq!(got.speed_raw, Some(300), "unrelated fields must survive");
        assert!(got.is_running, "the gate must never touch session state");
        assert_eq!(
            rig.state.last_state().unwrap().steps,
            None,
            "the gate must run BEFORE set_last_state, or persist_close would \
             write the poisoned value into the session row on link loss"
        );
        assert_eq!(
            rig.state.rejected_samples(),
            1,
            "a stripped sample must be counted — /api/diag's \
             rejected_samples is the loud half of the gate"
        );
    }

    /// Every decrease passes untouched: resets are the STORAGE layer's to
    /// judge (`deglitch_walk` in db.rs), and a gate that swallowed them
    /// would break its reset handling. The post-reset climb passes too.
    #[test]
    fn gate_lets_every_decrease_pass_and_the_reset_climb_after_it() {
        let mut rig = DebounceRig::new();
        for (t, steps) in [(0.0, 500u32), (5.0, 3), (10.0, 20), (15.0, 60)] {
            let got = rig.push_at(t, &running(steps));
            assert_eq!(
                got.steps,
                Some(steps),
                "t={t}: decreases and the climb after a counter reset must \
                 pass the gate untouched (rate-based, increases only — \
                 PLAUSIBILITY GATE in ble.rs; reset repair belongs to \
                 db.rs's deglitch_walk)"
            );
        }
        assert_eq!(rig.state.rejected_samples(), 0);
    }

    /// The one-frame deferral after a deep drop: a stale-low read (the
    /// documented `…1800, 346, 1891…` console behaviour) must not wedge the
    /// gate's envelope at the dip value and reject the recovery — that
    /// would starve the de-glitcher of the very stream it repairs.
    #[test]
    fn gate_defers_reset_judgement_so_a_stale_low_dip_cannot_wedge_it() {
        let mut rig = DebounceRig::new();
        rig.push_at(0.0, &running(1800));
        let dip = rig.push_at(5.0, &running(346)); // stale-low: passes (decrease)
        assert_eq!(dip.steps, Some(346));
        let recovery = rig.push_at(10.0, &running(1830));
        assert_eq!(
            recovery.steps,
            Some(1830),
            "the recovery after a one-frame stale-low dip was rejected — the \
             gate must defer the reset-vs-stale judgement one frame \
             (FieldGate::pending_reset in ble.rs); anchoring on the dip \
             value rejects minutes of good samples and starves \
             deglitch_walk of the stream it exists to repair"
        );
        assert_eq!(rig.state.rejected_samples(), 0);
    }

    /// A sustained unit-scale error (the 10–100× decode bug the gate exists
    /// for) stays rejected: the envelope must NOT ratchet up on refused
    /// values, so the counter keeps climbing and the tester sees it.
    #[test]
    fn gate_rejects_a_sustained_unit_scale_error_persistently() {
        let mut rig = DebounceRig::new();
        rig.push_at(0.0, &running(0));
        // A driver decoding centi-steps as steps: ~200 "steps"/s.
        for i in 1..=30u32 {
            let got = rig.push_at(i as f64, &running(200 * i));
            assert_eq!(
                got.steps, None,
                "second {i}: a 200/s stream must stay rejected — if this is \
                 Some, the gate ratcheted its envelope up on refused values \
                 and a unit-scale error is flowing into stored history \
                 (FieldGate::admit in ble.rs leaves the anchor untouched on \
                 refusal)"
            );
        }
        assert_eq!(
            rig.state.rejected_samples(),
            30,
            "every rejected sample must be counted: rejected_samples is how \
             a contributor's hardware test surfaces a wrong unit scale \
             (docs/drivers/README.md, Step 4)"
        );
    }

    /// The other gated fields: speed takes the one ABSOLUTE cap (it is a
    /// live reading, not cumulative), duration is wall-clock-bounded, and
    /// distance/calories are rate-bounded like steps.
    #[test]
    fn gate_bounds_speed_absolutely_and_the_other_counters_by_rate() {
        let mut rig = DebounceRig::new();
        let telem = |speed: u32, dur: u32, dist: u32, cal: u32| {
            let mut t = Telemetry::new("km/h");
            t.speed_raw = Some(speed);
            t.duration_s = Some(dur);
            t.distance_raw = Some(dist);
            t.calories = Some(cal);
            t.refresh_derived();
            t
        };
        // Baselines are always accepted.
        rig.push_at(0.0, &telem(300, 10, 10, 5));
        // One second later: everything implausible at once.
        let got = rig.push_at(1.0, &telem(65_000, 800, 6_000, 600));
        assert_eq!(
            got.speed_raw, None,
            "speed_raw above GATE_MAX_SPEED_RAW must be stripped (absolute \
             cap — speed is not a cumulative counter; ble.rs)"
        );
        assert_eq!(got.speed_kmh, None, "derived speed must go with the raw");
        assert_eq!(
            got.duration_s, None,
            "elapsed time cannot advance ~800 s in 1 s of wall clock \
             (GATE_MAX_DURATION_PER_S in ble.rs)"
        );
        assert_eq!(
            got.distance_raw, None,
            "+59.9 km in one second exceeds GATE_MAX_DISTANCE_RAW_PER_S \
             (ble.rs)"
        );
        assert_eq!(got.distance_m, None, "derived distance goes with the raw");
        assert_eq!(
            got.calories, None,
            "+595 kcal in one second exceeds GATE_MAX_CALORIES_PER_S (ble.rs)"
        );
        // One sample, one count — the counter counts samples, not fields.
        assert_eq!(rig.state.rejected_samples(), 1);
        // Plausible values keep flowing afterwards.
        let ok = rig.push_at(2.0, &telem(310, 12, 10, 5));
        assert_eq!(ok.speed_raw, Some(310));
        assert_eq!(ok.duration_s, Some(12));
    }

    /// `steps_supported` is observed from the ingested stream: null until a
    /// sample carries steps, true from then on, and NEVER false — a
    /// steps-None sample later (an accumulating driver's first poll cycle
    /// after reconnect, a gated frame) must not un-conclude it.
    #[test]
    fn steps_supported_is_observed_from_the_stream_and_never_false() {
        let mut rig = DebounceRig::new();
        let mut stepless = Telemetry::new("km/h");
        stepless.speed_raw = Some(300);
        rig.push_at(0.0, &stepless);
        assert_eq!(
            rig.state.snapshot()["steps_supported"],
            serde_json::Value::Null,
            "no steps seen yet: steps_supported must be null, not false \
             (tri-state rule on AppState::steps_supported_json)"
        );
        rig.push_at(1.0, &running(10));
        assert_eq!(rig.state.snapshot()["steps_supported"], json!(true));
        rig.push_at(2.0, &stepless);
        assert_eq!(
            rig.state.snapshot()["steps_supported"],
            json!(true),
            "a later steps-None sample must not flip steps_supported back — \
             None is 'not reported this frame', not 'cannot report' \
             (AppState::steps_supported_json)"
        );
    }

    /// The amplifier the duration killed: a status stream alternating in
    /// PAIRS (a belt sitting at a speed-threshold boundary — ftms.rs and
    /// sperax.rs derive state from speed). With a frame-count debounce each
    /// pair confirmed, so a session opened and closed every four frames,
    /// invalidating the today-cache and walking day_totals under the DB
    /// mutex each time. An alternating state never HOLDS, so a time-based
    /// debounce confirms nothing.
    #[test]
    fn pair_alternation_never_opens_or_closes_sessions() {
        let mut rig = DebounceRig::new();
        for i in 0..100u32 {
            // R R S S R R S S …, one frame per second — each state holds
            // only 1 s, well under SESSION_DEBOUNCE.
            let telem = if (i / 2) % 2 == 0 {
                running(i)
            } else {
                stopped()
            };
            rig.push_at(i as f64, &telem);
            assert_eq!(
                rig.state.active_session(),
                None,
                "frame {i}: a pair-alternating status stream opened a \
                 session — this is the flapping amplifier SESSION_DEBOUNCE's \
                 time basis exists to kill (see its comment in ble.rs); no \
                 state in this stream ever holds for the debounce duration"
            );
        }
    }
}
