//! KingSmith WiLink driver — the legacy WalkingPad protocol on service 0xFE00
//! (the pre-app-cipher generation: WalkingPad A1, A1 Pro, C2, R1 Pro, P1 and
//! friends).
//!
//! Protocol knowledge was ported from and cross-checked against three
//! independent open-source implementations (see THIRD-PARTY-NOTICES.md):
//!
//! * **ph4r05/ph4-walkingpad** (MIT, © 2017 CRoCS, Dusan Klinec) — the canonical
//!   reverse engineering (`ph4_walkingpad/pad.py`): status-frame field
//!   offsets, the additive checksum, and the ≥690 ms minimum command spacing
//!   (`minimal_cmd_space`). Its README's captured frames are this module's
//!   test fixtures. <https://github.com/ph4r05/ph4-walkingpad>
//! * **cagnulein/qdomyos-zwift** (GPL-3.0) —
//!   `src/devices/kingsmithr1protreadmill/kingsmithr1protreadmill.cpp`: the
//!   20-byte status-frame length requirement and the advertised-name list
//!   with its `KS-HD-Z1D` FTMS carve-out (`src/devices/bluetooth.cpp`).
//!   <https://github.com/cagnulein/qdomyos-zwift>
//! * **DorianRudolph/QWalkingPad** (GPL-3.0, © 2021 Dorian Rudolph) —
//!   `Protocol.cpp`: independent confirmation of the field offsets and the
//!   belt-state semantics (states 0 and 5 are "not running").
//!   <https://github.com/DorianRudolph/QWalkingPad>
//!
//! Interaction model: **request/response polling**, like LifeSpan but slower.
//! Write the 6-byte status query `F7 A2 00 00 A2 FD` to 0xFE02; the pad
//! answers with a 20-byte status notification on 0xFE01. Two hardware quirks
//! shape the loop:
//!
//! * The firmware drops or garbles writes that arrive **closer than ~690 ms**
//!   apart (ph4-walkingpad's measured `minimal_cmd_space = 0.69 s`), so every
//!   write — init frames included — is paced by [`util::CommandSpacer`]-style
//!   gaps.
//! * qdomyos-zwift runs a 5-frame init handshake before polling, three frames
//!   of which are writes nobody has documented. Trot sends only the two that
//!   are provably queries — see the note by `BODY_PARAMS_QUERY`. ph4-walkingpad
//!   and QWalkingPad both read status without any of the three.
//!
//! Status frame format (20 bytes, confirmed on real captures in
//! ph4-walkingpad's README — all multi-byte integers **big-endian**):
//!   byte  0:      0xF8 (response prefix)
//!   byte  1:      0xA2 (message type: current status)
//!   byte  2:      belt state (0 idle, 1 running, 5 asleep/locked)
//!   byte  3:      belt speed, 0.1 km/h
//!   byte  4:      mode (0 auto, 1 manual, 2 sleep)
//!   bytes 5..8:   elapsed time, seconds (u24 BE)
//!   bytes 8..11:  distance, units of 10 m (u24 BE)
//!   bytes 11..14: steps (u24 BE)
//!   byte  14:     app target speed, km/h = raw / 30 (the real capture shows
//!                 raw 180 alongside a 6.0 km/h belt; 18 km/h is impossible
//!                 on this hardware, so 0.1 km/h it is not)
//!   byte  15:     (unknown)
//!   byte  16:     controller button
//!   byte  17:     (unknown)
//!   byte  18:     checksum: sum(bytes 1..18) mod 256
//!   byte  19:     0xFD (terminator)
//!
//! The WiLink status frame carries no energy field, so `calories` stays
//! `None` — absent, not zero.

use super::util::{run_init_sequence, CommandSpacer, InitStep};
use super::{Advertisement, BeltState, Driver, DriverHost, Emit, Sample};
use crate::diagnostics::Link as Peripheral;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use futures::{FutureExt, StreamExt};
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;

// ---- UUIDs (service 0xFE00) -------------------------------------------------
pub const SERVICE_UUID: Uuid = super::sig_uuid(0xfe00);
pub const NOTIFY_CHAR_UUID: Uuid = super::sig_uuid(0xfe01);
pub const WRITE_CHAR_UUID: Uuid = super::sig_uuid(0xfe02);

// ---- Advertised names -------------------------------------------------------
//
// Ported from qdomyos-zwift's device matcher (bluetooth.cpp), which is the
// widest-deployed list of real-world WiLink advertised names. Comparison is
// case-insensitive (qdomyos upper-cases; real pads advertise mixed case like
// "WalkingPad").

/// Name prefixes that identify a WiLink-generation pad.
pub const ADV_NAME_PREFIXES: &[&str] = &[
    "WALKINGPAD",
    "KINGSMITH",
    "R1 PRO",
    "KS-ST-A1P",   // WalkingPad A1 Pro
    "KS-SC-BLR2C", // Poland-distributed WalkingPad R2 (WiLink firmware)
    "KS-BLC",      // WalkingPad C2
    "KS-BLR",      // WalkingPad R2 Pro
    "KS-H",
    "KS-F0",
];

/// Names that match only exactly (an over-broad prefix here would swallow
/// unrelated devices — qdomyos compares "RE" with equality, not prefix).
pub const ADV_NAME_EXACT: &[&str] = &["RE"];

/// KingSmith-named devices that do NOT speak WiLink and must never be claimed,
/// even though they collide with a prefix above (all collide with "KS-H"):
///
/// * `KS-HD-Z1D` — WalkingPad Z1: advertises a KS- name but is an FTMS device;
///   qdomyos-zwift carves it out explicitly and routes it to FTMS.
/// * `KS-HC-R1A…`, `KS-HDSC-X21C`, `KS-HDSY-X21C` — the "R2" app-cipher
///   generation (base64 + substitution-cipher transport on service 0x1234),
///   a different protocol. Trot **deliberately does not support** that
///   generation (its driver was removed — see docs/provenance.md), but the
///   carve-outs stay: a WiLink driver must still never poll an app-cipher
///   pad, so these names now fall through to **no driver** rather than to a
///   sibling.
///
/// The other app-cipher/FTMS carve-outs in qdomyos (`KS-ST-K12PRO`,
/// `KS-X21`, `KS-NACH-…`, `KS-NGCH-…`, `KS-NG-`, `KS-AP-`, `KS-MC`) don't
/// collide with any prefix above and need no entry here.
pub const ADV_NAME_EXCLUDE_PREFIXES: &[&str] =
    &["KS-HD-Z1D", "KS-HC-R1A", "KS-HDSC-X21C", "KS-HDSY-X21C"];

// ---- Wire constants ---------------------------------------------------------

pub const REQ_PREFIX: u8 = 0xF7;
pub const RESP_PREFIX: u8 = 0xF8;
pub const TERMINATOR: u8 = 0xFD;
/// Message type of the current-status request/response pair.
pub const MSG_STATUS: u8 = 0xA2;
/// A status response is exactly 20 bytes; qdomyos-zwift discards any other
/// length, and every observed capture agrees.
pub const STATUS_FRAME_LEN: usize = 20;

/// The firmware drops writes spaced closer than this
/// (ph4-walkingpad's measured `minimal_cmd_space = 0.69 s`).
pub const WRITE_MIN_GAP_MS: u64 = 690;
pub const WRITE_MIN_GAP: Duration = Duration::from_millis(WRITE_MIN_GAP_MS);

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive unanswered polls before the link is declared dead — same
/// rationale as the LifeSpan driver: macOS can hold a stale handle open with
/// no disconnect event after the pad sleeps.
const MAX_DEAD_POLLS: u32 = 15;

// ---- Frame building ---------------------------------------------------------
//
// Every request is `F7 <body…> <checksum> FD` where the checksum is the
// additive sum of the body bytes mod 256 (ph4-walkingpad's `fix_crc`:
// `cmd[-2] = sum(cmd[1:-2]) % 256`).

/// Body of the status query: `F7 A2 00 00 A2 FD` — ph4's `ask_stats`,
/// QWalkingPad's `query()`, qdomyos' `noOpData`, byte-identical in all three.
pub const BODY_STATUS_QUERY: &[u8] = &[MSG_STATUS, 0x00, 0x00];
/// Params query: `F7 A6 00 …` — QWalkingPad's `queryParams()`, whose `0xA6`
/// reply it parses as the pad's stored preferences.
///
/// Key `0` with an empty payload is a *read*. Keys ≥ 1 in the same `A6` family
/// **set** preferences (max speed, child lock, units), so only key 0 belongs in
/// an observe-only driver — the same subtype-is-the-verb pattern as `A2`.
pub const BODY_PARAMS_QUERY: &[u8] = &[0xA6, 0x00, 0x00, 0x00, 0x00, 0x00];

// DELIBERATELY ABSENT: qdomyos-zwift's `initData1`/`1b` (`F7 A5 61 …`),
// `initData4` (`F7 B1 …`) and `initData5` (`F7 B3 …`).
//
// Trot only writes frames it can prove are reads. Those three are writes of
// unknown effect: `B1`/`B3` appear in no public source but qdomyos, which sends
// them verbatim and uncommented, and `A5` subtype `61` is undocumented anywhere
// (ph4's `A5` frames use subtype `60`). ph4-walkingpad and QWalkingPad both
// read status perfectly well without any of them, which is the evidence that
// they are not required to observe a pad.
//
// If a model someday won't answer without one, reopen this with a capture
// showing what the frame actually does — not with "qdomyos sends it".

/// `F7 <body…> <sum(body) % 256> FD`.
pub fn build_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 3);
    frame.push(REQ_PREFIX);
    frame.extend_from_slice(body);
    frame.push(super::util::checksum_sum(body));
    frame.push(TERMINATOR);
    frame
}

/// The opening queries, spaced by the same ≥690 ms gap as every other write
/// (the delay after the last step doubles as the gap before the first poll).
///
/// Both frames are reads. qdomyos-zwift sends three further frames here whose
/// effect nobody has documented; see the note above for why we don't.
pub fn init_steps() -> Vec<InitStep> {
    [BODY_STATUS_QUERY, BODY_PARAMS_QUERY]
        .iter()
        .map(|body| {
            InitStep::write(WRITE_CHAR_UUID, build_frame(body)).then_wait_ms(WRITE_MIN_GAP_MS)
        })
        .collect()
}

// ---- Status-frame parsing ---------------------------------------------------

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("expected {STATUS_FRAME_LEN} bytes, got {0}")]
    BadLength(usize),
    #[error("bad prefix 0x{0:02x}")]
    BadPrefix(u8),
    /// A well-prefixed frame of another message type (0xA6 params, 0xA7
    /// stored record, …). Expected traffic — skip it, don't warn.
    #[error("not a status frame (message type 0x{0:02x})")]
    NotStatus(u8),
    #[error("missing 0xFD terminator")]
    BadTerminator,
    #[error("checksum mismatch: computed 0x{computed:02x}, frame carries 0x{found:02x}")]
    BadChecksum { computed: u8, found: u8 },
}

/// One decoded 20-byte status frame, fields as the wire reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Belt state byte: 0 idle, 1 running, 5 asleep/locked (see [`belt_state`]).
    pub belt_state: u8,
    /// Belt speed in 0.1 km/h.
    pub speed_raw: u8,
    /// Mode byte: 0 auto, 1 manual, 2 sleep (QWalkingPad's `MODE_*`).
    pub mode: u8,
    /// Elapsed time, seconds.
    pub time_s: u32,
    /// Distance in units of 10 m.
    pub distance_raw: u32,
    /// Cumulative steps.
    pub steps: u32,
    /// The app-requested target speed (not the belt speed), km/h = raw / 30
    /// (see the frame-format note in the module docs). Unparsed beyond the
    /// raw byte — Trot never consumes a target speed.
    pub app_speed_raw: u8,
    /// Controller button byte.
    pub button: u8,
}

fn u24_be(frame: &[u8], at: usize) -> u32 {
    ((frame[at] as u32) << 16) | ((frame[at + 1] as u32) << 8) | frame[at + 2] as u32
}

/// Parse a notification into a [`Status`]. Pure function of the bytes; never
/// panics on malformed input.
///
/// Stricter than the upstream implementations, deliberately: none of the
/// three verifies the checksum on inbound frames, but a corrupt counter that
/// parses cleanly silently poisons someone's step totals, so we reject any
/// frame whose trailer doesn't add up (verified against real captured frames
/// in the tests below).
pub fn parse_status(frame: &[u8]) -> Result<Status, ProtocolError> {
    if frame.len() < 2 {
        return Err(ProtocolError::BadLength(frame.len()));
    }
    if frame[0] != RESP_PREFIX {
        return Err(ProtocolError::BadPrefix(frame[0]));
    }
    if frame[1] != MSG_STATUS {
        return Err(ProtocolError::NotStatus(frame[1]));
    }
    if frame.len() != STATUS_FRAME_LEN {
        return Err(ProtocolError::BadLength(frame.len()));
    }
    if frame[STATUS_FRAME_LEN - 1] != TERMINATOR {
        return Err(ProtocolError::BadTerminator);
    }
    let computed = super::util::checksum_sum(&frame[1..STATUS_FRAME_LEN - 2]);
    let found = frame[STATUS_FRAME_LEN - 2];
    if computed != found {
        return Err(ProtocolError::BadChecksum { computed, found });
    }
    Ok(Status {
        belt_state: frame[2],
        speed_raw: frame[3],
        mode: frame[4],
        time_s: u24_be(frame, 5),
        distance_raw: u24_be(frame, 8),
        steps: u24_be(frame, 11),
        app_speed_raw: frame[14],
        button: frame[16],
    })
}

/// The wire's belt-state byte as a neutral [`BeltState`] — **total**: every
/// byte maps to `Standby` or `Running`, none to [`BeltState::Other`].
///
/// What the sources establish: real captures show `1` while the belt moves
/// (ph4-walkingpad README); QWalkingPad treats exactly `0` and `5` as "not
/// running" (`padRunning = s != 0 && s != 5`) and `5` is the asleep/locked
/// state (qdomyos calls it the "lock byte"; ph4's demo shows it alongside
/// mode 2, sleep). `5` maps to `Standby` rather than `Other(5)` **on
/// purpose**: it is a known not-running state, and the raw passthrough byte
/// would collide with the API contract's `PAUSED` code (0x05), presenting a
/// sleeping pad as paused.
///
/// Every other byte maps to `Running` because that is QWalkingPad's
/// hardware-tested predicate taken *whole*: its author verified on real pads
/// that any state outside {0, 5} means the belt moves. Mapping those bytes
/// to `Other` instead — as this driver once did — meant a pad reporting a
/// state outside {0, 1, 5} mid-walk recorded zero session time and zero
/// steps for the entire walk (`Other` never opens a session).
///
/// ⚠️ **Do not port this shape to another protocol.** "Everything else ⇒
/// Running" is justified here *only* by an upstream predicate tested on real
/// hardware. For a protocol without that evidence, unknown-⇒-`Running` is
/// strictly worse than unknown-⇒-`Other`: it would open sessions (and accrue
/// walking time) on bytes nobody has ever observed. Urevo and PitPat both
/// correctly map unknown bytes to `Other` — the divergence is deliberate,
/// per-protocol, evidence-driven.
pub(crate) fn belt_state(v: u8) -> BeltState {
    match v {
        0 | 5 => BeltState::Standby, // 5 = asleep / locked — not running
        _ => BeltState::Running,
    }
}

/// A [`Status`] as a neutral SI sample. WiLink reports SI-adjacent units
/// natively (0.1 km/h, 10 m, seconds), so this is pure scaling; the console's
/// display unit is irrelevant to the wire format.
pub(crate) fn to_sample(s: &Status) -> Sample {
    Sample {
        speed_kmh: Some(s.speed_raw as f64 / 10.0),
        distance_m: Some(s.distance_raw as f64 * 10.0),
        steps: Some(s.steps),
        duration_s: Some(s.time_s),
        calories: None, // the status frame has no energy field — absent, not zero
        state: Some(belt_state(s.belt_state)),
    }
}

// ---- The driver -------------------------------------------------------------

fn normalized(name: &str) -> String {
    name.trim().to_ascii_uppercase()
}

/// Is this advertised name one of the known non-WiLink KingSmith devices?
fn excluded_name(name: &str) -> bool {
    let name = normalized(name);
    ADV_NAME_EXCLUDE_PREFIXES
        .iter()
        .any(|pfx| name.starts_with(pfx))
}

/// Does the advertised name look like a WiLink pad (and not a carve-out)?
fn matches_name(name: &str) -> bool {
    let n = normalized(name);
    !excluded_name(name)
        && (ADV_NAME_PREFIXES.iter().any(|pfx| n.starts_with(pfx))
            || ADV_NAME_EXACT.iter().any(|exact| n == *exact))
}

/// Notify on FE01, write on FE02 — roles verified, not just UUIDs. FE00 is
/// less contested than the FFF0 block, but the discipline is the same.
fn gatt_shape_is_wilink(gatt: &BTreeSet<Characteristic>) -> bool {
    super::util::has_notify(gatt, NOTIFY_CHAR_UUID) && super::util::has_write(gatt, WRITE_CHAR_UUID)
}

pub struct KingSmithWiLink;

#[async_trait]
impl Driver for KingSmithWiLink {
    fn id(&self) -> &'static str {
        "kingsmith-wilink"
    }

    fn matches(&self, adv: &Advertisement) -> bool {
        // Scan-time stays permissive: a known name OR the WiLink service is
        // enough to list the device. The carve-outs apply even here — a
        // KS-HD-Z1D is FTMS hardware and should surface via that driver.
        matches_name(&adv.name) || adv.services.contains(&SERVICE_UUID)
    }

    fn supports(&self, adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        // Connect-time: the characteristic roles must check out, and the name
        // must be either recognised or absent. A *nameless* device with the
        // exact FE01-notify/FE02-write shape is accepted — platforms sometimes
        // fail to surface a name at connect time, and stranding an
        // already-paired pad over a missing string is the failure mode the
        // LifeSpan fallback exists to prevent. A device that *does* have a
        // name we don't recognise (or one on the carve-out list) is refused
        // and falls through — the R2/app-cipher generation and the FTMS
        // WalkingPads are exactly such devices.
        if excluded_name(&adv.name) {
            return false;
        }
        (matches_name(&adv.name) || normalized(&adv.name).is_empty()) && gatt_shape_is_wilink(gatt)
    }

    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()> {
        let chars = link.characteristics();
        let notify_char = chars
            .iter()
            .find(|c| c.uuid == NOTIFY_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("notify characteristic (FE01) missing"))?;
        let write_char = chars
            .iter()
            .find(|c| c.uuid == WRITE_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("write characteristic (FE02) missing"))?;

        // Subscribe first — the pad answers the init frames immediately.
        link.subscribe(&notify_char).await?;
        let mut notifications = link.notifications().await?;

        run_init_sequence(link, &init_steps()).await?;

        let poll = build_frame(BODY_STATUS_QUERY);
        let mut spacer = CommandSpacer::new(WRITE_MIN_GAP);
        let mut dead_polls: u32 = 0;

        loop {
            // ≥690 ms since the previous write, or the firmware drops it. The
            // init sequence's trailing delay already covered the first gap.
            spacer.pace().await;

            // Drain stale buffered notifications (init replies, late frames)
            // so the response read below answers THIS request.
            while notifications.next().now_or_never().flatten().is_some() {}

            // Bound the write: a stale link can block forever with no
            // disconnect event, which would wedge the worker.
            match tokio::time::timeout(
                RESPONSE_TIMEOUT,
                link.write(&write_char, &poll, WriteType::WithResponse),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e.into()), // real BLE error → reconnect
                Err(_) => {
                    dead_polls += 1;
                    tracing::warn!("timeout writing status query ({dead_polls}/{MAX_DEAD_POLLS})");
                    if dead_polls >= MAX_DEAD_POLLS {
                        return Err(anyhow!("link unresponsive; forcing reconnect"));
                    }
                    continue;
                }
            }

            let frame = match tokio::time::timeout(RESPONSE_TIMEOUT, notifications.next()).await {
                Ok(Some(n)) => n.value,
                Ok(None) => return Err(anyhow!("notification stream ended")),
                Err(_) => {
                    dead_polls += 1;
                    tracing::warn!(
                        "timeout waiting for status response ({dead_polls}/{MAX_DEAD_POLLS})"
                    );
                    if dead_polls >= MAX_DEAD_POLLS {
                        return Err(anyhow!("link unresponsive; forcing reconnect"));
                    }
                    continue;
                }
            };
            dead_polls = 0; // a frame arrived → the link is alive
            host.record_frame(MSG_STATUS, &frame); // raw capture for /api/diag

            match parse_status(&frame) {
                Ok(status) => emit(to_sample(&status)),
                Err(ProtocolError::NotStatus(t)) => {
                    tracing::debug!("ignoring non-status frame type 0x{t:02x}");
                }
                Err(e) => tracing::warn!("wilink decode error: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::util::GattIo;
    use super::*;
    use crate::telemetry::Telemetry;
    use std::sync::Mutex;
    use tokio::time::Instant;

    fn hx(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // ---- Frame building ------------------------------------------------------

    /// The builder must reproduce the exact wire frames the upstream projects
    /// send — the status query is byte-identical in all three sources.
    #[test]
    fn build_frame_reproduces_the_upstream_request_frames() {
        // ph4 `ask_stats` / QWalkingPad `query()` / qdomyos `noOpData`.
        assert_eq!(build_frame(BODY_STATUS_QUERY), hx("f7 a2 00 00 a2 fd"));
        // qdomyos `initData3` / QWalkingPad `queryParams()`.
        assert_eq!(
            build_frame(BODY_PARAMS_QUERY),
            hx("f7 a6 00 00 00 00 00 a6 fd")
        );
        // qdomyos speed frame from `forceSpeedOrIncline` (2.5 km/h example).
        assert_eq!(
            build_frame(&[0xA2, 0x01, 0x19]),
            hx("f7 a2 01 19 bc fd"),
            "checksum must match qdomyos' hand-computed value"
        );
    }

    /// Every byte this driver writes must be a read. Pinning the whole init
    /// sequence — not just its individual frames — is what makes an added
    /// write show up as a failing test rather than as silent new traffic.
    #[test]
    fn the_driver_only_ever_writes_queries() {
        let frames: Vec<Vec<u8>> = init_steps().iter().map(|s| s.payload.clone()).collect();
        assert_eq!(
            frames,
            vec![hx("f7 a2 00 00 a2 fd"), hx("f7 a6 00 00 00 00 00 a6 fd")],
            "init must be the status query and the params query, nothing else"
        );

        // The poll loop's only frame, and the subtype that makes it a read.
        assert_eq!(build_frame(BODY_STATUS_QUERY), hx("f7 a2 00 00 a2 fd"));

        // `A2` carries both reads and commands; the subtype is the verb.
        // Subtype 00 queries, 01 sets speed, 02 sets mode, 04 starts the belt.
        // Nothing we send may use a non-zero subtype.
        for frame in frames
            .iter()
            .chain(std::iter::once(&build_frame(BODY_STATUS_QUERY)))
        {
            if frame[1] == MSG_STATUS {
                assert_eq!(
                    frame[2], 0x00,
                    "A2 subtype must be 00 (query), got {frame:02x?}"
                );
            }
        }
    }

    // ---- Status parsing: real captured fixtures ------------------------------
    //
    // The five frames below are real WalkingPad captures published in
    // ph4-walkingpad's README (rec_time ≈ 2021-03-13), together with the
    // decoded values its parser reported for them. They pin the field
    // offsets, the big-endian u24s AND the inbound checksum in one go.

    const FIXTURES: &[(&str, u32, u32, u32)] = &[
        // (raw frame, time_s, distance_raw, steps)
        ("f8a2013c0100022a00004f0003d1b4000000e3fd", 554, 79, 977),
        ("f8a2013c0100022a00004f0003d2b4000000e4fd", 554, 79, 978),
        ("f8a2013c0100022b00004f0003d4b4000000e7fd", 555, 79, 980),
        ("f8a2013c0100022c00004f0003d5b4000000e9fd", 556, 79, 981),
        ("f8a2013c0100022d0000500003d6b4000000ecfd", 557, 80, 982),
    ];

    #[test]
    fn decodes_the_ph4_captured_frames() {
        for (raw, time_s, distance_raw, steps) in FIXTURES {
            let s = parse_status(&hx(raw)).unwrap();
            assert_eq!(s.time_s, *time_s, "{raw}");
            assert_eq!(s.distance_raw, *distance_raw, "{raw}");
            assert_eq!(s.steps, *steps, "{raw}");
            // Constant across this capture session:
            assert_eq!(s.belt_state, 1, "belt running");
            assert_eq!(s.speed_raw, 60, "6.0 km/h");
            assert_eq!(s.mode, 1, "manual mode");
            assert_eq!(s.app_speed_raw, 180, "app target 6.0 km/h (raw/30)");
            assert_eq!(s.button, 0);
        }
    }

    /// Large counters exercise the full u24 big-endian width (a wrong
    /// endianness or width would silently corrupt step data — the exact bug
    /// class this project just spent a release fixing).
    #[test]
    fn u24_fields_are_big_endian_across_the_full_width() {
        // state 1, speed 30, mode 1, time 0x012345, dist 0x000102, steps 0x0a0b0c.
        let mut frame = hx("f8 a2 01 1e 01 01 23 45 00 01 02 0a 0b 0c 1e 00 00 00 00 fd");
        frame[18] = super::super::util::checksum_sum(&frame[1..18]);
        let s = parse_status(&frame).unwrap();
        assert_eq!(s.time_s, 0x012345);
        assert_eq!(s.distance_raw, 0x000102);
        assert_eq!(s.steps, 0x0a0b0c);
    }

    // ---- Checksum ------------------------------------------------------------

    /// The checksum trailer on the real fixtures must verify with the shared
    /// additive checksum over bytes 1..18 — and a single corrupted counter
    /// byte must be rejected, not parsed into someone's step history.
    #[test]
    fn checksum_verifies_on_real_frames_and_rejects_corruption() {
        for (raw, ..) in FIXTURES {
            let frame = hx(raw);
            assert_eq!(
                super::super::util::checksum_sum(&frame[1..18]),
                frame[18],
                "{raw}"
            );
        }
        // Flip one steps byte: frame must be rejected.
        let mut corrupt = hx(FIXTURES[0].0);
        corrupt[13] ^= 0x01;
        assert!(matches!(
            parse_status(&corrupt),
            Err(ProtocolError::BadChecksum { .. })
        ));
        // Fixing the trailer makes the same bytes parse again.
        corrupt[18] = super::super::util::checksum_sum(&corrupt[1..18]);
        assert_eq!(parse_status(&corrupt).unwrap().steps, 976);
    }

    // ---- Malformed input -----------------------------------------------------

    #[test]
    fn malformed_frames_error_without_panicking() {
        assert_eq!(parse_status(&[]), Err(ProtocolError::BadLength(0)));
        assert_eq!(parse_status(&[0xF8]), Err(ProtocolError::BadLength(1)));
        // Truncated mid-frame (a sleepy pad or a lossy link).
        assert_eq!(
            parse_status(&hx("f8 a2 01 3c 01 00 02 2a")),
            Err(ProtocolError::BadLength(8))
        );
        // 19 bytes — one short of a status frame.
        assert_eq!(
            parse_status(&hx(FIXTURES[0].0)[..19]),
            Err(ProtocolError::BadLength(19))
        );
        // Wrong prefix entirely (a LifeSpan-style frame, say).
        assert_eq!(
            parse_status(&hx("a1 aa 00 23 00 00")),
            Err(ProtocolError::BadPrefix(0xA1))
        );
        // Right length, missing terminator.
        let mut no_term = hx(FIXTURES[0].0);
        no_term[19] = 0x00;
        assert_eq!(parse_status(&no_term), Err(ProtocolError::BadTerminator));
    }

    /// Other well-formed message types (stored records 0xA7, params 0xA6) are
    /// expected traffic and must be distinguishable from corruption.
    #[test]
    fn non_status_messages_are_identified_not_mangled() {
        assert_eq!(
            parse_status(&hx(
                "f8 a7 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 a7 fd"
            )),
            Err(ProtocolError::NotStatus(0xA7))
        );
        assert_eq!(
            parse_status(&hx("f8 a6 00 00")),
            Err(ProtocolError::NotStatus(0xA6))
        );
    }

    // ---- Belt state ----------------------------------------------------------

    /// The mapping is TOTAL, taken whole from QWalkingPad's hardware-tested
    /// predicate `padRunning = s != 0 && s != 5`: 0 and 5 are the only
    /// not-running states, everything else means the belt moves. Routing the
    /// "everything else" bytes to `Other` instead would record zero session
    /// time and zero steps for a whole walk on a pad reporting a state
    /// outside {0, 1, 5} — `Other` never opens a session.
    #[test]
    fn belt_state_is_total_per_the_upstream_predicate() {
        assert_eq!(belt_state(0), BeltState::Standby);
        assert_eq!(belt_state(1), BeltState::Running);
        assert_eq!(belt_state(5), BeltState::Standby, "asleep/locked");
        for v in [2u8, 3, 4, 6, 9, 0x7f, 0xff] {
            assert_eq!(
                belt_state(v),
                BeltState::Running,
                "byte {v}: WiLink takes QWalkingPad's predicate WHOLE — any \
                 state outside {{0, 5}} is Running on this hardware. If you \
                 changed this to Other(v), a moving pad in state {v} records \
                 no session at all; if you are porting this shape to another \
                 protocol, don't — see belt_state's rustdoc in \
                 kingsmith_wilink.rs for why the evidence is WiLink-only"
            );
        }
    }

    // ---- Sample / Telemetry golden pins --------------------------------------

    /// Fixture frame → Sample → Telemetry: the SI conversion and the
    /// presentation re-encoding must land on exact raw values (steps and
    /// distance feed the storage accumulator).
    #[test]
    fn golden_fixture_to_telemetry() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let s = parse_status(&hx(FIXTURES[0].0)).unwrap();
        let sample = to_sample(&s);
        assert!(approx(sample.speed_kmh.unwrap(), 6.0));
        assert!(approx(sample.distance_m.unwrap(), 790.0));
        assert_eq!(sample.steps, Some(977));
        assert_eq!(sample.duration_s, Some(554));
        assert_eq!(sample.calories, None, "WiLink reports no energy — absent");
        assert_eq!(sample.state, Some(BeltState::Running));

        let t = Telemetry::from_sample(&sample, "km/h");
        assert_eq!(t.speed_raw, Some(600), "6.00 km/h in centi-units");
        assert_eq!(
            t.distance_raw,
            Some(79),
            "decameters — the wire unit, exactly"
        );
        assert_eq!(t.distance_m, Some(790));
        assert_eq!(t.steps, Some(977));
        assert_eq!(t.duration_s, Some(554));
        assert_eq!(t.calories, None);
        assert_eq!(t.status_name.as_deref(), Some("RUNNING"));
        assert!(t.is_running);
    }

    /// An idle pad (state 5, everything zero — the shape ph4's demo shows on
    /// wake) must present as standby, NOT as the contract's PAUSED code 0x05.
    #[test]
    fn a_sleeping_pad_is_standby_not_paused() {
        let mut frame = hx("f8 a2 05 00 02 00 00 00 00 00 00 00 00 00 00 00 02 00 00 fd");
        frame[18] = super::super::util::checksum_sum(&frame[1..18]);
        let s = parse_status(&frame).unwrap();
        assert_eq!(s.belt_state, 5);
        let t = Telemetry::from_sample(&to_sample(&s), "km/h");
        assert_eq!(t.status, Some(0x01), "STANDBY");
        assert_eq!(t.status_name.as_deref(), Some("STANDBY"));
        assert!(!t.is_running);
    }

    // ---- Name matching -------------------------------------------------------

    fn adv(name: &str) -> Advertisement {
        Advertisement {
            name: name.into(),
            services: vec![],
        }
    }

    #[test]
    fn known_wilink_names_match_case_insensitively() {
        for name in [
            "WalkingPad A1",
            "WALKINGPAD",
            "KingSmith WalkingPad",
            "R1 PRO",
            "KS-ST-A1P",
            "KS-SC-BLR2C",
            "KS-BLC-something", // C2
            "KS-BLR2",          // R2 Pro on WiLink firmware
            "KS-H101",
            "KS-F0-A1",
            "RE", // exact-match only
        ] {
            assert!(KingSmithWiLink.matches(&adv(name)), "{name}");
        }
    }

    /// `KS-HD-Z1D` advertises a KingSmith name but is an FTMS device — the
    /// upstream project carves it out explicitly, and so must we, or we'd
    /// steal it from the FTMS driver and poll it with garbage. Same for the
    /// R2/app-cipher generation names that collide with the `KS-H` prefix.
    #[test]
    fn non_wilink_kingsmith_names_are_carved_out() {
        for name in [
            "KS-HD-Z1D",      // WalkingPad Z1 — FTMS
            "KS-HD-Z1D-1234", // suffixed variants too
            "ks-hd-z1d",
            "KS-HC-R1AA", // R2 generation
            "KS-HC-R1AC",
            "KS-HDSC-X21C", // X21 generation
            "KS-HDSY-X21C",
        ] {
            assert!(!KingSmithWiLink.matches(&adv(name)), "{name}");
            assert!(!matches_name(name), "{name}");
        }
        // And names that never were KingSmith WiLink.
        for name in [
            "",
            "LifeSpan-TM",
            "Some Headphones",
            "REDMI Band",
            "KS-MC21",
        ] {
            assert!(!matches_name(name), "{name}");
        }
    }

    #[test]
    fn scan_matches_on_the_advertised_service_too() {
        assert!(KingSmithWiLink.matches(&Advertisement {
            name: String::new(),
            services: vec![SERVICE_UUID],
        }));
        assert!(!KingSmithWiLink.matches(&adv("")));
    }

    // ---- supports(): roles, names, carve-outs --------------------------------

    use btleplug::api::CharPropFlags;

    fn gatt(chars: &[(Uuid, CharPropFlags)]) -> BTreeSet<Characteristic> {
        chars
            .iter()
            .map(|(uuid, properties)| Characteristic {
                uuid: *uuid,
                service_uuid: SERVICE_UUID,
                properties: *properties,
                descriptors: BTreeSet::new(),
            })
            .collect()
    }

    fn wilink_shaped() -> BTreeSet<Characteristic> {
        gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
            (WRITE_CHAR_UUID, CharPropFlags::WRITE),
        ])
    }

    #[test]
    fn supports_needs_the_exact_roles() {
        assert!(KingSmithWiLink.supports(&adv("WalkingPad A1"), &wilink_shaped()));
        // Write-without-response also satisfies the write role.
        assert!(KingSmithWiLink.supports(
            &adv("WalkingPad A1"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
                (WRITE_CHAR_UUID, CharPropFlags::WRITE_WITHOUT_RESPONSE),
            ])
        ));
        // Roles swapped → not our protocol, whatever the name says.
        assert!(!KingSmithWiLink.supports(
            &adv("WalkingPad A1"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::WRITE),
                (WRITE_CHAR_UUID, CharPropFlags::NOTIFY),
            ])
        ));
        // UUIDs present but no properties → refuse.
        assert!(!KingSmithWiLink.supports(
            &adv("WalkingPad A1"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::default()),
                (WRITE_CHAR_UUID, CharPropFlags::default()),
            ])
        ));
        // Half a table → refuse.
        assert!(!KingSmithWiLink.supports(
            &adv("WalkingPad A1"),
            &gatt(&[(NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY)])
        ));
    }

    #[test]
    fn supports_accepts_nameless_but_refuses_foreign_and_carved_out_names() {
        // Nameless + exact shape: accepted (platforms lose names at connect
        // time; stranding a paired pad over a string is the known failure).
        assert!(KingSmithWiLink.supports(&adv(""), &wilink_shaped()));
        // A name we know isn't WiLink: refused even with the right shape.
        assert!(!KingSmithWiLink.supports(&adv("KS-HD-Z1D"), &wilink_shaped()));
        assert!(!KingSmithWiLink.supports(&adv("KS-HC-R1AA"), &wilink_shaped()));
        // An unrecognised name: refused — falls through to FTMS/fallback.
        assert!(!KingSmithWiLink.supports(&adv("Mystery Pad 3000"), &wilink_shaped()));
    }

    // ---- Init sequence: order, payloads, 690 ms spacing ----------------------

    #[derive(Default)]
    struct MockLink {
        writes: Mutex<Vec<(Uuid, Vec<u8>, Instant)>>,
    }

    #[async_trait]
    impl GattIo for MockLink {
        async fn write_uuid(
            &self,
            char_uuid: Uuid,
            payload: &[u8],
            _with_response: bool,
        ) -> Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push((char_uuid, payload.to_vec(), Instant::now()));
            Ok(())
        }

        async fn subscribe_uuid(&self, _char_uuid: Uuid) -> Result<()> {
            Ok(())
        }
    }

    /// The handshake on the virtual clock: exactly two writes to FE02, both
    /// queries, each ≥690 ms after the previous — the pad drops faster writes
    /// on the floor. The count is part of the assertion: qdomyos-zwift sends
    /// five here, and the three we leave out are undocumented writes that an
    /// observe-only driver has no business making.
    #[tokio::test(start_paused = true)]
    async fn init_sequence_writes_only_the_two_queries_with_wilink_spacing() {
        let link = MockLink::default();
        let start = Instant::now();
        run_init_sequence(&link, &init_steps()).await.unwrap();
        // The trailing delay also runs — it is the gap before the first poll.
        assert_eq!(Instant::now() - start, Duration::from_millis(2 * 690));

        let writes = link.writes.lock().unwrap().clone();
        let expected = [hx("f7 a2 00 00 a2 fd"), hx("f7 a6 00 00 00 00 00 a6 fd")];
        assert_eq!(
            writes.len(),
            expected.len(),
            "init must write exactly the two query frames"
        );
        for (i, ((uuid, payload, at), want)) in writes.iter().zip(&expected).enumerate() {
            assert_eq!(*uuid, WRITE_CHAR_UUID, "frame {i}");
            assert_eq!(payload, want, "frame {i}");
            assert_eq!(
                *at - start,
                Duration::from_millis(690 * i as u64),
                "frame {i} must wait out the 690 ms gap"
            );
        }
    }

    /// The poll-loop spacing contract on the virtual clock: consecutive
    /// paced writes are exactly 690 ms apart, and time spent waiting for the
    /// response counts toward the gap.
    #[tokio::test(start_paused = true)]
    async fn poll_writes_are_paced_at_least_690ms_apart() {
        let mut spacer = CommandSpacer::new(WRITE_MIN_GAP);
        let start = Instant::now();
        spacer.pace().await;
        assert_eq!(Instant::now() - start, Duration::ZERO, "first poll is free");
        spacer.pace().await;
        assert_eq!(Instant::now() - start, Duration::from_millis(690));
        // A response that took 400 ms to arrive leaves only 290 ms to wait.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let before = Instant::now();
        spacer.pace().await;
        assert_eq!(Instant::now() - before, Duration::from_millis(290));
    }
}
