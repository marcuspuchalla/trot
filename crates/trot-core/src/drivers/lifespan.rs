//! LifeSpan (Omni console) driver — the native protocol on service 0xFFF0.
//!
//! The decoders are a faithful port of the author's own earlier Python package
//! `lifespan_sc110` (`parser.py`, `__init__.py`), relicensed here under GPLv3
//! by its sole copyright holder. It is not third-party work and so has no
//! entry in THIRD-PARTY-NOTICES.md.
//!
//! Protocol knowledge was bootstrapped from and cross-checked against
//! blak3r/treadspan (MIT, © 2025 Blake Robertson). See THIRD-PARTY-NOTICES.md.
//!
//! Interaction model: **request/response polling.** The console answers one
//! value per request — write a 6-byte opcode frame to 0xFFF2, read the reply
//! as a notification on 0xFFF1 — so the driver rotates through the opcodes
//! (~50 ms apart) and accumulates the answers into one cumulative state.
//!
//! Frame format (confirmed on SC110):
//!   byte 0: 0xA1 (prefix)
//!   byte 1: 0xAA (ok) or 0xFF (unknown opcode)
//!   bytes 2..5: opcode-dependent payload
//!
//! The response does NOT echo the request opcode, so decoding requires knowing
//! which opcode we polled last — `Reader` tracks that.

use super::{Advertisement, BeltState, Driver, DriverHost, Emit, Sample};
use crate::diagnostics::Link as Peripheral;
use crate::telemetry::{speed_kmh, STATUS_PAUSED, STATUS_RUNNING, STATUS_STANDBY, STATUS_SUMMARY};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use futures::{FutureExt, StreamExt};
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;

// ---- UUIDs (service 0xFFF0) -------------------------------------------------
pub const SERVICE_UUID: Uuid = super::sig_uuid(0xfff0);
pub const NOTIFY_CHAR_UUID: Uuid = super::sig_uuid(0xfff1);
pub const WRITE_CHAR_UUID: Uuid = super::sig_uuid(0xfff2);

// Match any LifeSpan console (TR1200/TR5000/SC/DT… all share the Omni protocol),
// not just "LifeSpan-TM". "ESP32" is kept as a fallback for units whose BLE
// module advertises only its generic chipset name.
pub const ADV_NAME_PREFIXES: &[&str] = &["LifeSpan", "ESP32"];

pub const REQ_PREFIX: u8 = 0xA1;
pub const RESP_OK: u8 = 0xAA;
pub const RESP_ERR: u8 = 0xFF;

pub const OPCODE_SPEED: u8 = 0x82;
pub const OPCODE_DISTANCE: u8 = 0x85;
pub const OPCODE_CALORIES: u8 = 0x87;
pub const OPCODE_STEPS: u8 = 0x88;
pub const OPCODE_DURATION: u8 = 0x89;
pub const OPCODE_STATUS: u8 = 0x91;

/// SPEED is interleaved 5x so the belt speed updates feel live.
pub const DEFAULT_POLL_ROTATION: &[u8] = &[
    OPCODE_SPEED,
    OPCODE_STATUS,
    OPCODE_SPEED,
    OPCODE_STEPS,
    OPCODE_SPEED,
    OPCODE_DISTANCE,
    OPCODE_SPEED,
    OPCODE_DURATION,
    OPCODE_SPEED,
    OPCODE_CALORIES,
];

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Consecutive unanswered polls (~2s each) before we treat a seemingly-"connected"
/// link as dead and force a full reconnect. macOS can leave a peripheral handle
/// open with no disconnect event after the treadmill sleeps/powers off; without
/// this the worker would poll a stale link forever and never re-scan when the
/// belt comes back.
const MAX_DEAD_POLLS: u32 = 15;

/// 6-byte `A1 OPCODE 00 00 00 00` request frame.
pub fn build_request(opcode: u8) -> [u8; 6] {
    [REQ_PREFIX, opcode, 0, 0, 0, 0]
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("expected 6 bytes, got {0}")]
    BadLength(usize),
    #[error("bad prefix")]
    BadPrefix,
    #[error("device reported unknown-opcode (A1 FF ...)")]
    UnknownOpcode,
    #[error("unexpected response flag 0x{0:02x}")]
    UnexpectedFlag(u8),
    #[error("out-of-range duration bytes")]
    BadDuration,
}

fn validate(frame: &[u8]) -> Result<(), ProtocolError> {
    if frame.len() != 6 {
        return Err(ProtocolError::BadLength(frame.len()));
    }
    if frame[0] != REQ_PREFIX {
        return Err(ProtocolError::BadPrefix);
    }
    if frame[1] == RESP_ERR {
        return Err(ProtocolError::UnknownOpcode);
    }
    if frame[1] != RESP_OK {
        return Err(ProtocolError::UnexpectedFlag(frame[1]));
    }
    Ok(())
}

fn u16_be(frame: &[u8]) -> u32 {
    ((frame[2] as u32) << 8) | frame[3] as u32
}

pub fn decode_steps(frame: &[u8]) -> Result<u32, ProtocolError> {
    validate(frame)?;
    Ok(u16_be(frame))
}

pub fn decode_calories(frame: &[u8]) -> Result<u32, ProtocolError> {
    validate(frame)?;
    Ok(u16_be(frame))
}

/// Raw u16 big-endian in units of 10 meters (decameters). `displayed_m = raw * 10`.
pub fn decode_distance_raw(frame: &[u8]) -> Result<u32, ProtocolError> {
    validate(frame)?;
    Ok(u16_be(frame))
}

/// Returns hundredths of the displayed speed unit: byte 2 = whole units,
/// byte 3 = hundredths (0..99). Callers divide by 100 for displayed value.
pub fn decode_speed_raw(frame: &[u8]) -> Result<u32, ProtocolError> {
    validate(frame)?;
    Ok(frame[2] as u32 * 100 + frame[3] as u32)
}

pub fn decode_duration_seconds(frame: &[u8]) -> Result<u32, ProtocolError> {
    validate(frame)?;
    let (h, m, s) = (frame[2] as u32, frame[3] as u32, frame[4] as u32);
    if m >= 60 || s >= 60 {
        return Err(ProtocolError::BadDuration);
    }
    Ok(h * 3600 + m * 60 + s)
}

pub fn decode_status(frame: &[u8]) -> Result<u8, ProtocolError> {
    validate(frame)?;
    Ok(frame[2])
}

/// The console's status byte as a neutral [`BeltState`]. The numeric values
/// double as the API's presentation codes (they were inherited from this very
/// protocol), so `telemetry::status_code` is the exact inverse — a test below
/// pins that round trip for every byte.
pub(crate) fn belt_state(v: u8) -> BeltState {
    match v {
        STATUS_STANDBY => BeltState::Standby,
        STATUS_RUNNING => BeltState::Running,
        STATUS_SUMMARY => BeltState::Summary,
        STATUS_PAUSED => BeltState::Paused,
        other => BeltState::Other(other),
    }
}

/// Latest known raw state, assembled across multiple polls — the console
/// answers one field per request, so a single response never carries the whole
/// picture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readout {
    pub steps: Option<u32>,
    pub duration_s: Option<u32>,
    pub distance_raw: Option<u32>,
    pub calories: Option<u32>,
    pub speed_raw: Option<u32>,
    pub status: Option<u8>,
}

/// Incremental decoder: feed (opcode_of_last_poll, response_frame) pairs.
#[derive(Default)]
pub struct Reader {
    state: Readout,
}

impl Reader {
    pub fn new() -> Self {
        Reader::default()
    }

    pub fn state(&self) -> &Readout {
        &self.state
    }

    /// Decode one response in the context of the opcode last polled.
    /// Returns a clone of the updated state, or the error if decoding failed
    /// (in which case state is left unchanged, mirroring the Python client).
    pub fn feed(&mut self, last_opcode: u8, response: &[u8]) -> Result<Readout, ProtocolError> {
        match last_opcode {
            OPCODE_STEPS => self.state.steps = Some(decode_steps(response)?),
            OPCODE_DURATION => self.state.duration_s = Some(decode_duration_seconds(response)?),
            OPCODE_DISTANCE => self.state.distance_raw = Some(decode_distance_raw(response)?),
            OPCODE_CALORIES => self.state.calories = Some(decode_calories(response)?),
            OPCODE_SPEED => self.state.speed_raw = Some(decode_speed_raw(response)?),
            OPCODE_STATUS => self.state.status = Some(decode_status(response)?),
            _ => {}
        }
        Ok(self.state.clone())
    }
}

/// A [`Readout`] as a neutral SI sample. `display_unit` is needed because the
/// console reports speed in hundredths of whatever unit it *displays* — the
/// one place this driver's wire format depends on user configuration.
pub(crate) fn to_sample(r: &Readout, display_unit: &str) -> Sample {
    Sample {
        speed_kmh: r.speed_raw.map(|raw| speed_kmh(raw, display_unit)),
        distance_m: r.distance_raw.map(|raw| (raw * 10) as f64),
        steps: r.steps,
        duration_s: r.duration_s,
        calories: r.calories,
        state: r.status.map(belt_state),
    }
}

// ---- The driver -------------------------------------------------------------

/// Does the advertised name look like a LifeSpan console?
fn matches_name(adv: &Advertisement) -> bool {
    ADV_NAME_PREFIXES
        .iter()
        .any(|pfx| adv.name.starts_with(pfx))
}

/// Does the GATT table have exactly the characteristic roles this driver
/// uses — notify on FFF1, write on FFF2? Roles, not just UUIDs: Deerrun
/// exposes the same two UUIDs with the roles swapped, and subscribing/writing
/// at it with this protocol would at best fail and at worst mis-drive it.
fn gatt_shape_is_lifespan(gatt: &BTreeSet<Characteristic>) -> bool {
    super::util::has_notify(gatt, NOTIFY_CHAR_UUID) && super::util::has_write(gatt, WRITE_CHAR_UUID)
}

pub struct LifeSpan;

#[async_trait]
impl Driver for LifeSpan {
    fn id(&self) -> &'static str {
        "lifespan"
    }

    fn matches(&self, adv: &Advertisement) -> bool {
        // Scan-time stays permissive (properties aren't known yet): a likely
        // name OR the service UUID is enough to list the device. supports()
        // is where the strictness lives.
        matches_name(adv) || adv.services.contains(&SERVICE_UUID)
    }

    fn supports(&self, adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        // Strict on purpose. 0xFFF0/FFF1/FFF2 is a generic vendor-module
        // layout that at least six mutually incompatible treadmill protocols
        // (LifeSpan, Urevo, Sperax, FitShow, Deerrun, Zipro, Focus) squat on
        // — and Deerrun swaps the notify/write roles relative to us. Claiming a
        // device on UUIDs alone would write LifeSpan opcodes at hardware
        // speaking a different protocol, so we require BOTH a recognised
        // advertised name AND the exact characteristic roles we use. A device
        // with the right roles but an unrecognised name is caught by
        // [`LifeSpanFallback`] at the END of the registry, after every
        // stricter driver has had its chance.
        matches_name(adv) && gatt_shape_is_lifespan(gatt)
    }

    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()> {
        let chars = link.characteristics();
        let notify_char = chars
            .iter()
            .find(|c| c.uuid == NOTIFY_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("notify characteristic (FFF1) missing"))?;
        let write_char = chars
            .iter()
            .find(|c| c.uuid == WRITE_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("write characteristic (FFF2) missing"))?;
        link.subscribe(&notify_char).await?;
        let mut notifications = link.notifications().await?;

        let mut reader = Reader::new();
        let mut idx = 0usize;
        let mut dead_polls: u32 = 0;

        loop {
            let opcode = DEFAULT_POLL_ROTATION[idx % DEFAULT_POLL_ROTATION.len()];
            idx += 1;

            // Drain any stale buffered notifications so the response we read below is
            // the one for THIS request. Responses don't echo their opcode, so a single
            // buffered/lagging frame would otherwise mis-assign every field (speed
            // reading as steps, etc.).
            while let Some(n) = notifications.next().now_or_never().flatten() {
                link.record("discard_stale", serde_json::json!({"characteristic":n.uuid,"payload":crate::diagnostics::frame(&n.value)}));
            }

            // Bound the write: a stale link can block the write forever with no
            // disconnect event, which would wedge the worker.
            match tokio::time::timeout(
                RESPONSE_TIMEOUT,
                link.write(&write_char, &build_request(opcode), WriteType::WithResponse),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e.into()), // real BLE error → reconnect
                Err(_) => {
                    dead_polls += 1;
                    tracing::warn!(
                        "timeout writing opcode 0x{opcode:02x} ({dead_polls}/{MAX_DEAD_POLLS})"
                    );
                    if dead_polls >= MAX_DEAD_POLLS {
                        return Err(anyhow!("link unresponsive; forcing reconnect"));
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            }

            // Await the next notification with a timeout (responses don't echo opcode).
            let frame = match tokio::time::timeout(RESPONSE_TIMEOUT, notifications.next()).await {
                Ok(Some(n)) => n.value,
                Ok(None) => return Err(anyhow!("notification stream ended")),
                Err(_) => {
                    dead_polls += 1;
                    tracing::warn!(
                        "timeout waiting for response to opcode 0x{opcode:02x} ({dead_polls}/{MAX_DEAD_POLLS})"
                    );
                    if dead_polls >= MAX_DEAD_POLLS {
                        return Err(anyhow!("link unresponsive; forcing reconnect"));
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };
            dead_polls = 0; // a frame arrived → the link is alive
            host.record_frame(opcode, &frame); // raw capture for protocol diagnostics

            match reader.feed(opcode, &frame) {
                Ok(readout) => emit(to_sample(&readout, &host.display_unit)),
                Err(e) => {
                    link.record("decode_error",serde_json::json!({"request_opcode":opcode,"association":"inferred","error":e.to_string(),"expected_prefix":REQ_PREFIX,"actual_prefix":frame.first(),"length":frame.len()}));
                    tracing::warn!("decode error opcode 0x{opcode:02x}: {e}");
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

// ---- The last-resort fallback -----------------------------------------------

/// LifeSpan matching without the name requirement — registered LAST, and it
/// must stay last.
///
/// Why it exists: [`LifeSpan::supports`] requires a recognised advertised
/// name, but some consoles reach connect time with a name we've never seen
/// (models outside the known prefixes, or a platform that simply doesn't
/// surface the name after reconnect). Before matching became strict, Trot
/// claimed any `FFF1`+`FFF2` device as LifeSpan — so a paired console that
/// suddenly stopped matching after an upgrade would strand its user with no
/// driver at all. This entry preserves those users: if **no other driver**
/// wants the device and its characteristic roles are exactly LifeSpan-shaped
/// (notify on FFF1, write on FFF2 — which already excludes role-swapped
/// protocols like Deerrun), drive it as LifeSpan.
///
/// The residual risk is a same-shaped foreign protocol (e.g. Urevo) with an
/// unrecognised name reaching the fallback and being polled with LifeSpan
/// opcodes. That is exactly what happened before strict matching too — and
/// the failure is benign: unanswered polls, dead-link detection, reconnect,
/// eventually give-up. The fix for those devices is their own driver
/// registered ABOVE this entry, at which point they never get here.
pub struct LifeSpanFallback;

#[async_trait]
impl Driver for LifeSpanFallback {
    fn id(&self) -> &'static str {
        "lifespan-fallback"
    }

    fn matches(&self, _adv: &Advertisement) -> bool {
        // Scan matching is already covered by LifeSpan's permissive matches();
        // the fallback is a connect-time safety net, not a scan category.
        false
    }

    fn supports(&self, _adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        gatt_shape_is_lifespan(gatt)
    }

    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()> {
        LifeSpan.run(link, host, emit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{status_code, status_name, Telemetry};

    fn hx(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn build_request_frames() {
        assert_eq!(build_request(OPCODE_STEPS), hx("a1 88 00 00 00 00")[..]);
        assert_eq!(build_request(OPCODE_STATUS), hx("a1 91 00 00 00 00")[..]);
    }

    #[test]
    fn validation_errors() {
        assert!(matches!(
            decode_steps(&hx("a1 aa 00 06")),
            Err(ProtocolError::BadLength(4))
        ));
        assert!(matches!(
            decode_steps(&hx("a2 aa 00 06 00 00")),
            Err(ProtocolError::BadPrefix)
        ));
        assert!(matches!(
            decode_steps(&hx("a1 ff 00 00 00 00")),
            Err(ProtocolError::UnknownOpcode)
        ));
    }

    #[test]
    fn decoders() {
        assert_eq!(decode_steps(&hx("a1 aa 00 23 00 00")).unwrap(), 35);
        assert_eq!(decode_steps(&hx("a1 aa 01 00 00 00")).unwrap(), 256);
        assert_eq!(decode_steps(&hx("a1 aa ff ff 00 00")).unwrap(), 65535);
        assert_eq!(decode_calories(&hx("a1 aa 00 02 00 00")).unwrap(), 2);
        assert_eq!(decode_distance_raw(&hx("a1 aa 01 1f 00 00")).unwrap(), 287);
        assert_eq!(decode_speed_raw(&hx("a1 aa 00 3c 00 00")).unwrap(), 60);
        assert_eq!(
            decode_duration_seconds(&hx("a1 aa 00 01 0b 00")).unwrap(),
            71
        );
        assert_eq!(
            decode_duration_seconds(&hx("a1 aa 00 00 0e 00")).unwrap(),
            14
        );
        assert_eq!(
            decode_duration_seconds(&hx("a1 aa 00 3b 3b 00")).unwrap(),
            59 * 60 + 59
        );
        assert_eq!(
            decode_duration_seconds(&hx("a1 aa 01 00 00 00")).unwrap(),
            3600
        );
    }

    #[test]
    fn duration_out_of_range() {
        assert!(decode_duration_seconds(&hx("a1 aa 00 3c 00 00")).is_err());
        assert!(decode_duration_seconds(&hx("a1 aa 00 00 3c 00")).is_err());
    }

    #[test]
    fn status_decode() {
        assert_eq!(
            decode_status(&hx("a1 aa 03 00 00 00")).unwrap(),
            STATUS_RUNNING
        );
        assert_eq!(
            decode_status(&hx("a1 aa 01 00 00 00")).unwrap(),
            STATUS_STANDBY
        );
    }

    /// The wire byte → BeltState → presentation code round trip must be the
    /// identity for EVERY byte, or a LifeSpan console's `status`/`status_name`
    /// on the wire would change under the driver refactor.
    #[test]
    fn belt_state_round_trips_every_status_byte() {
        for v in 0..=255u8 {
            assert_eq!(status_code(belt_state(v)), v, "byte 0x{v:02x}");
        }
    }

    #[test]
    fn reader_aggregates_across_opcodes() {
        let mut r = Reader::new();
        r.feed(OPCODE_STEPS, &hx("a1 aa 00 23 00 00")).unwrap();
        r.feed(OPCODE_STATUS, &hx("a1 aa 03 00 00 00")).unwrap();
        r.feed(OPCODE_DURATION, &hx("a1 aa 00 01 0b 00")).unwrap();
        r.feed(OPCODE_SPEED, &hx("a1 aa 00 3c 00 00")).unwrap();
        r.feed(OPCODE_CALORIES, &hx("a1 aa 00 02 00 00")).unwrap();
        r.feed(OPCODE_DISTANCE, &hx("a1 aa 00 00 00 00")).unwrap();
        let s = r.state();
        assert_eq!(s.steps, Some(35));
        assert_eq!(s.duration_s, Some(71));
        assert_eq!(s.calories, Some(2));
        assert_eq!(s.speed_raw, Some(60));
        assert_eq!(s.status, Some(STATUS_RUNNING));
    }

    /// A decode error must leave the accumulated state untouched.
    #[test]
    fn reader_keeps_state_on_a_bad_frame() {
        let mut r = Reader::new();
        r.feed(OPCODE_STEPS, &hx("a1 aa 00 23 00 00")).unwrap();
        assert!(r.feed(OPCODE_STEPS, &hx("a1 ff 00 00 00 00")).is_err());
        assert_eq!(r.state().steps, Some(35));
    }

    /// Golden end-to-end pin: raw frames → Reader → Sample → Telemetry must
    /// reproduce exactly what the pre-refactor pipeline put on the wire.
    /// Values were captured against the old code (Reader emitting Telemetry
    /// directly); the raw fields are asserted exactly — they are what
    /// storage and the de-glitch accumulator consume.
    #[test]
    fn golden_frames_to_telemetry_mph_console() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let mut r = Reader::new();
        for (opcode, frame) in [
            (OPCODE_SPEED, "a1 aa 00 3c 00 00"),    // raw 60
            (OPCODE_STATUS, "a1 aa 03 00 00 00"),   // RUNNING
            (OPCODE_STEPS, "a1 aa 00 23 00 00"),    // 35
            (OPCODE_DURATION, "a1 aa 00 01 0b 00"), // 71 s
            (OPCODE_DISTANCE, "a1 aa 01 1f 00 00"), // raw 287
            (OPCODE_CALORIES, "a1 aa 00 02 00 00"), // 2
        ] {
            r.feed(opcode, &hx(frame)).unwrap();
        }
        let t = Telemetry::from_sample(&to_sample(r.state(), "mph"), "mph");
        assert_eq!(t.steps, Some(35));
        assert_eq!(t.duration_s, Some(71));
        assert_eq!(t.distance_raw, Some(287));
        assert_eq!(t.calories, Some(2));
        assert_eq!(t.speed_raw, Some(60));
        assert_eq!(t.status, Some(STATUS_RUNNING));
        assert_eq!(t.status_name.as_deref(), Some("RUNNING"));
        assert_eq!(t.display_unit, "mph");
        assert!(t.is_running);
        assert!(approx(t.speed_mph.unwrap(), 0.6));
        assert!(approx(t.speed_kmh.unwrap(), 0.9656064));
        assert_eq!(t.distance_m, Some(2870));
        assert!(approx(t.distance_km.unwrap(), 2.87));
        assert!(approx(t.distance_mi.unwrap(), 2.87 / 1.609344));
        // And the same status byte produces the same name for unknown values.
        assert_eq!(status_name(0x7f), "UNKNOWN_0x7f");
    }

    // ---- supports(): strict vs fallback -------------------------------------

    use btleplug::api::CharPropFlags;

    fn adv(name: &str) -> Advertisement {
        Advertisement {
            name: name.into(),
            services: vec![],
        }
    }

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

    fn lifespan_shaped() -> BTreeSet<Characteristic> {
        gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
            (WRITE_CHAR_UUID, CharPropFlags::WRITE),
        ])
    }

    /// The Deerrun shape: same UUIDs as LifeSpan, notify/write roles swapped.
    fn deerrun_shaped() -> BTreeSet<Characteristic> {
        gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::WRITE_WITHOUT_RESPONSE),
            (WRITE_CHAR_UUID, CharPropFlags::NOTIFY),
        ])
    }

    /// Strict supports(): a recognised name AND the exact roles, both
    /// required. UUIDs without the right properties prove nothing — five
    /// incompatible protocols share this UUID block.
    #[test]
    fn strict_supports_needs_name_and_roles() {
        for name in ["LifeSpan-TM", "LifeSpan TR1200", "ESP32-treadmill"] {
            assert!(LifeSpan.supports(&adv(name), &lifespan_shaped()), "{name}");
        }
        // Right roles, unrecognised name → not the strict driver's call.
        assert!(!LifeSpan.supports(&adv(""), &lifespan_shaped()));
        assert!(!LifeSpan.supports(&adv("Urevo E1L"), &lifespan_shaped()));
        // Recognised name, wrong roles → refuse; the table isn't ours.
        assert!(!LifeSpan.supports(&adv("LifeSpan-TM"), &deerrun_shaped()));
        // Recognised name, UUIDs present but no properties at all → refuse.
        // DELIBERATE, not incidental: the role checks accept either
        // subscription flavour (NOTIFY or INDICATE — see util::has_notify),
        // but a table too broken to declare a single property is no evidence
        // of a role, and claiming it would write LifeSpan opcodes at an
        // unknown table. If a real console ever ships an all-zero property
        // bitmap, that is the finding that reopens this — with a capture.
        assert!(!LifeSpan.supports(
            &adv("LifeSpan-TM"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::default()),
                (WRITE_CHAR_UUID, CharPropFlags::default()),
            ])
        ));
    }

    /// An indicate-only FFF1 also satisfies the notify role: btleplug's
    /// subscribe() handles INDICATE exactly like NOTIFY, and a console
    /// refused here would match no driver at all — not even the fallback —
    /// and present as connect_failed (util::has_notify documents the rule).
    #[test]
    fn indicate_only_notify_satisfies_the_notify_role() {
        let table = gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::INDICATE),
            (WRITE_CHAR_UUID, CharPropFlags::WRITE),
        ]);
        assert!(
            LifeSpan.supports(&adv("LifeSpan-TM"), &table),
            "an indicate-only console must stay connectable — has_notify \
             accepts NOTIFY | INDICATE (drivers/util.rs)"
        );
        assert!(LifeSpanFallback.supports(&adv(""), &table));
    }

    /// The fallback ignores the name but still verifies roles — it exists to
    /// keep unrecognised-name LifeSpan consoles connectable, not to claim
    /// role-swapped foreign protocols.
    #[test]
    fn fallback_supports_roles_only() {
        assert!(LifeSpanFallback.supports(&adv(""), &lifespan_shaped()));
        assert!(LifeSpanFallback.supports(&adv("Mystery Pad 3000"), &lifespan_shaped()));
        assert!(!LifeSpanFallback.supports(&adv(""), &deerrun_shaped()));
        assert!(!LifeSpanFallback.supports(
            &adv(""),
            &gatt(&[(NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY)])
        ));
    }

    /// Write-without-response also counts as writable — some vendor modules
    /// only expose that flavour on FFF2.
    #[test]
    fn write_without_response_satisfies_the_write_role() {
        let table = gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
            (WRITE_CHAR_UUID, CharPropFlags::WRITE_WITHOUT_RESPONSE),
        ]);
        assert!(LifeSpan.supports(&adv("LifeSpan-TM"), &table));
        assert!(LifeSpanFallback.supports(&adv(""), &table));
    }

    /// Scan-time matching stays permissive (no properties exist yet): name
    /// prefix or service UUID lists the device; the fallback adds no scan
    /// category of its own.
    #[test]
    fn scan_matching_is_permissive_and_the_fallback_adds_none() {
        assert!(LifeSpan.matches(&adv("LifeSpan-TM")));
        assert!(LifeSpan.matches(&Advertisement {
            name: String::new(),
            services: vec![SERVICE_UUID],
        }));
        assert!(!LifeSpan.matches(&adv("Some Headphones")));
        assert!(!LifeSpanFallback.matches(&adv("LifeSpan-TM")));
    }
}
