//! Bluetooth SIG Fitness Machine Service (FTMS) driver — Treadmill Data.
//!
//! Interaction model: **subscribe-and-push.** The treadmill notifies a
//! Treadmill Data record (~1 Hz while the belt moves); we subscribe once and
//! decode each push. We also subscribe to Fitness Machine Status (0x2ADA)
//! where the device exposes it — on some hardware it is the only reliable
//! signal for state transitions (see "Hardening" below).
//!
//! The standard-frame parser implements the Treadmill Data characteristic
//! (0x2ACD) byte layout directly from the official FTMS v1.0.1 service
//! specification and the Bluetooth SIG Assigned Numbers / GATT Specification
//! Supplement field definitions — the published field order, sizes,
//! signedness, and resolutions. (Not a formal clean room: the same author
//! also read the implementations credited below, for the hardening.)
//!
//! **Hardening beyond the spec** — real walking-pad firmware deviates from
//! the paper in verified, recurring ways. This knowledge is ported from and
//! cross-checked against (see THIRD-PARTY-NOTICES.md):
//!
//! * **mcdax/walkingpad-controller** (MIT, © 2026 mcdax) —
//!   `docs/ftms-protocol-reference.md`, derived from KS Fit vendor-app
//!   analysis plus four real `btsnoop_hci` captures of a KingSmith KS-MC21:
//!   the mandatory staggered CCCD enables (§2.2: firmware silently drops
//!   notification-enable writes ~30 ms apart; the vendor app spaces them
//!   100/200/300 ms), the bit-13 step extension (§6.2), the 0x2ADA Fitness
//!   Machine Status behaviour (§3.2), and the no-keepalive finding (§2.7).
//!   <https://github.com/mcdax/walkingpad-controller>
//! * **cagnulein/qdomyos-zwift** (GPL-3.0) — the advertised-name list for
//!   real-world FTMS walking pads ([`ADV_NAME_PREFIXES`], from
//!   `src/devices/bluetooth.cpp`, including the SPERAX_RM-01/SPERAX_RM01
//!   carve-out).
//!   <https://github.com/cagnulein/qdomyos-zwift>
//! * **dudanov/python-pyftms** (Apache-2.0) — used as a cross-check of the
//!   Fitness Machine Status (0x2ADA) opcode map against the FTMS v1.0 spec.
//!   <https://github.com/dudanov/python-pyftms>
//!
//! The hardening lessons, verified on real captures:
//!
//! * **State changes arrive on 0x2ADA Fitness Machine Status, not anywhere
//!   else.** The KS-MC21 captures show start/stop/pause transitions signalled
//!   only there — which is why this driver subscribes to it: a paused belt
//!   and a stopped belt both read 0 km/h, and 0x2ADA is what tells them
//!   apart.
//! * **Trot never writes to the FTMS Control Point.** Trot observes
//!   treadmills; it does not control them — a permanent product commitment,
//!   not a current limitation (see `docs/drivers/README.md`). The upstream
//!   references implement speed/start/stop commands, including the vendor
//!   "unlock" writes that gate the Control Point on shared ODM modules
//!   (KingSmith MC-21, Merach); none of that is ported, deliberately. The
//!   captures confirm the unlock gates *commands only* — telemetry
//!   notifications flow without it — so an observe-only driver never needs
//!   one.
//! * **No application-level keepalive is needed** — none was observed in any
//!   of the four captures; the link lives on link-layer keepalives plus the
//!   notification stream. This driver deliberately writes nothing.
//! * A widely-circulated third-party parser reads Total Distance as u16;
//!   the spec field is **u24** and this parser reads u24 (pinned by test).
//!
//! Frame format (little-endian throughout):
//!   bytes 0..2: Flags (uint16). Each bit signals presence of an optional field.
//!   then, in strict spec order, the present fields follow.
//!
//! Flags bit map (Table 4.5, FTMS v1.0.1):
//!   bit 0  More Data            — when 0, Instantaneous Speed IS present
//!   bit 1  Average Speed
//!   bit 2  Total Distance
//!   bit 3  Inclination + Ramp Angle Setting (pair)
//!   bit 4  Elevation Gain (positive + negative pair)
//!   bit 5  Instantaneous Pace
//!   bit 6  Average Pace
//!   bit 7  Expended Energy (total + per-hour + per-minute triple)
//!   bit 8  Heart Rate
//!   bit 9  Metabolic Equivalent
//!   bit 10 Elapsed Time
//!   bit 11 Remaining Time
//!   bit 12 Force on Belt + Power Output (pair)
//!   bit 13 (non-SIG) KingSmith step-count extension — see below
//!
//! Field sizes / resolutions (GATT Specification Supplement, Treadmill Data):
//!   Instantaneous Speed   uint16  0.01 km/h
//!   Average Speed         uint16  0.01 km/h
//!   Total Distance        uint24  1 m
//!   Inclination           sint16  0.1 %
//!   Ramp Angle Setting    sint16  0.1 deg
//!   Positive Elev. Gain   uint16  0.1 m
//!   Negative Elev. Gain   uint16  0.1 m
//!   Instantaneous Pace    uint8   0.1 km/min
//!   Average Pace          uint8   0.1 km/min
//!   Total Energy          uint16  1 kcal
//!   Energy per Hour       uint16  1 kcal
//!   Energy per Minute     uint8   1 kcal
//!   Heart Rate            uint8   1 bpm
//!   Metabolic Equivalent  uint8   0.1
//!   Elapsed Time          uint16  1 s
//!   Remaining Time        uint16  1 s
//!   Force on Belt         sint16  1 N
//!   Power Output          sint16  1 W
//!
//! KingSmith extension (bit 13, `0x2000` — NOT SIG-defined): three extra
//! bytes after the standard fields — a uint16-LE step count plus one zero
//! pad byte. The counter is pressure-sensor based. Because bit 13 is reserved
//! in the spec, another vendor could set it meaning something else entirely,
//! so the extension is parsed **only** for devices whose advertised name is a
//! known KingSmith family ([`is_kingsmith_name`]) — never blanket.

use super::util::subscribe_staggered;
use super::{Advertisement, BeltState, Driver, DriverHost, Emit, Sample};
use crate::diagnostics::Link as Peripheral;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _};
use futures::StreamExt;
use serde::Serialize;
use std::collections::BTreeSet;
use std::time::Duration;

// ---- UUIDs ------------------------------------------------------------------

/// Build the full 128-bit string form of a 16-bit Bluetooth SIG assigned UUID.
/// e.g. `0x1826` -> `00001826-0000-1000-8000-00805f9b34fb`.
#[allow(dead_code)] // helper kept alongside the UUID set
pub fn uuid16(short: u16) -> String {
    format!("0000{short:04x}-0000-1000-8000-00805f9b34fb")
}

/// Fitness Machine Service.
pub const FITNESS_MACHINE_SERVICE_UUID: &str = "00001826-0000-1000-8000-00805f9b34fb";
/// Treadmill Data characteristic (notify).
pub const TREADMILL_DATA_UUID: &str = "00002acd-0000-1000-8000-00805f9b34fb";
/// Fitness Machine Status characteristic (notify). On some devices (KS-MC21)
/// this is the only reliable signal for start/stop/pause transitions.
pub const FITNESS_MACHINE_STATUS_UUID: &str = "00002ada-0000-1000-8000-00805f9b34fb";
// The remaining *read-only* members of the FTMS UUID set, kept so the set
// lives in one place. The Fitness Machine Control Point (the characteristic
// a client writes speed/start/stop commands to) is deliberately not here:
// Trot observes treadmills, it never controls them, so nothing in this tree
// may address that characteristic.
/// Fitness Machine Feature characteristic (read).
#[allow(dead_code)]
pub const FITNESS_MACHINE_FEATURE_UUID: &str = "00002acc-0000-1000-8000-00805f9b34fb";
/// Training Status characteristic (read/notify).
#[allow(dead_code)]
pub const TRAINING_STATUS_UUID: &str = "00002ad3-0000-1000-8000-00805f9b34fb";

// ---- Advertised names -------------------------------------------------------
//
// FTMS walking pads frequently advertise their name without the 0x1826
// service UUID in the advertisement (the service only shows up after
// connect + discovery), so `matches()` also recognises the real-world name
// prefixes. The list is ported from qdomyos-zwift's device matcher
// (src/devices/bluetooth.cpp), the widest-deployed collection of verified
// FTMS treadmill names; comparison is case-insensitive.

/// Name prefixes of walking pads verified to speak standard FTMS.
pub const ADV_NAME_PREFIXES: &[&str] = &[
    "URTM",              // Urevo (Spacewalk 3S "URTM024", E1L family)
    "MRK-T",             // Merach treadmills (W50) — MRK-S/MRK-R are rowers/bikes
    "SF-T",              // Sunny Health & Fitness treadmills
    "CITYSPORTS-LINKER", // CitySports
    "WELLFIT TM",        // WellFit
    "MOBVOI TM",         // Mobvoi Home Treadmill
    "MOBVOI WMTP",       // Mobvoi walking pad
    "SWALK LITE-",       // Sportstech sWalk Lite
    "ANPLUS-",           // Anplus
    "ANPIUS-",           // Anplus (misspelled firmware variant, real)
    "YPOO-MINI PRO-",    // YPOO Mini Pro
    "THERUN  T15",       // TheRun T15 — the double space is what it advertises
    "FOCUS M3",          // Focus Fitness M3
    "KS-MC",             // KingSmith MC21 family (WalkingPad MC21)
    "KS-HD-Z1D",         // KingSmith WalkingPad Z1 (FTMS despite the KS- name)
    "KS-AP-",            // KingSmith WalkingPad R3 Hybrid+
    "KS-NG-",            // KingSmith X218 / Walking Pad
    // Hyphenated ONLY: "SPERAX_RM01" (no hyphen) and "SPERAX_RM-02" are a
    // different, proprietary Sperax protocol — qdomyos routes those to a
    // dedicated non-FTMS driver. Matching them here would mis-drive them.
    "SPERAX_RM-01",
];

/// KingSmith FTMS families, for gating the non-standard bit-13 step
/// extension. `ZP-ZEALR1` is the OEM Zeal-branded MC-21 variant.
pub const KINGSMITH_FTMS_NAME_PREFIXES: &[&str] = &["KS-", "ZP-ZEALR1"];

fn normalized(name: &str) -> String {
    name.trim().to_ascii_uppercase()
}

fn matches_name(name: &str) -> bool {
    let n = normalized(name);
    ADV_NAME_PREFIXES.iter().any(|p| n.starts_with(p))
}

/// Is this a KingSmith-family FTMS device? Gates the bit-13 step extension:
/// bit 13 is reserved in the SIG spec, so its KingSmith meaning must never be
/// assumed on other vendors' frames. An empty/unknown name safely disables
/// the extension (the frame still parses; steps just stay absent).
pub(crate) fn is_kingsmith_name(name: &str) -> bool {
    let n = normalized(name);
    KINGSMITH_FTMS_NAME_PREFIXES
        .iter()
        .any(|p| n.starts_with(p))
}

// ---- Flag bits --------------------------------------------------------------

const FLAG_MORE_DATA: u16 = 1 << 0;
const FLAG_AVG_SPEED: u16 = 1 << 1;
const FLAG_TOTAL_DISTANCE: u16 = 1 << 2;
const FLAG_INCLINATION: u16 = 1 << 3;
const FLAG_ELEVATION_GAIN: u16 = 1 << 4;
const FLAG_INST_PACE: u16 = 1 << 5;
const FLAG_AVG_PACE: u16 = 1 << 6;
const FLAG_EXPENDED_ENERGY: u16 = 1 << 7;
const FLAG_HEART_RATE: u16 = 1 << 8;
const FLAG_METABOLIC_EQUIV: u16 = 1 << 9;
const FLAG_ELAPSED_TIME: u16 = 1 << 10;
const FLAG_REMAINING_TIME: u16 = 1 << 11;
const FLAG_FORCE_POWER: u16 = 1 << 12;
/// NOT SIG-defined: KingSmith's step-count extension (see module docs).
const FLAG_KINGSMITH_STEPS: u16 = 1 << 13;

// ---- Error ------------------------------------------------------------------

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FtmsError {
    /// The buffer ran out before a required field could be read.
    #[error("buffer too short: needed {expected} bytes, got {got}")]
    TooShort { expected: usize, got: usize },
}

// ---- Decoded data -----------------------------------------------------------

/// Fully decoded Treadmill Data notification, in real-world units.
///
/// Every optional field is `None` unless its flag bit was set. `instantaneous_speed`
/// is always present per the spec (when the More Data bit is 0, which is the only
/// case a single self-contained notification appears in).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FtmsTreadmillData {
    /// Instantaneous belt speed, km/h.
    pub instantaneous_speed: Option<f64>,
    /// Average speed since session start, km/h.
    pub average_speed: Option<f64>,
    /// Total distance since session start, meters.
    pub total_distance_m: Option<u32>,
    /// Current inclination, percent (may be negative).
    pub inclination_pct: Option<f64>,
    /// Current ramp angle setting, degrees (may be negative).
    pub ramp_angle_deg: Option<f64>,
    /// Positive elevation gain since session start, meters.
    pub positive_elevation_gain_m: Option<f64>,
    /// Negative elevation gain since session start, meters.
    pub negative_elevation_gain_m: Option<f64>,
    /// Instantaneous pace, km/min.
    pub instantaneous_pace: Option<f64>,
    /// Average pace, km/min.
    pub average_pace: Option<f64>,
    /// Total expended energy, kcal.
    pub total_energy_kcal: Option<u32>,
    /// Energy per hour, kcal.
    pub energy_per_hour_kcal: Option<u32>,
    /// Energy per minute, kcal.
    pub energy_per_minute_kcal: Option<u32>,
    /// Current heart rate, bpm.
    pub heart_rate_bpm: Option<u8>,
    /// Metabolic equivalent (METs).
    pub metabolic_equivalent: Option<f64>,
    /// Elapsed time, seconds.
    pub elapsed_time_s: Option<u32>,
    /// Remaining time, seconds.
    pub remaining_time_s: Option<u32>,
    /// Force on the belt, Newtons (may be negative).
    pub force_on_belt_n: Option<i32>,
    /// Power output, Watts (may be negative).
    pub power_output_w: Option<i32>,
    /// Step count from the non-standard KingSmith bit-13 extension. Only ever
    /// set when the caller opted into the extension (known KingSmith device).
    pub kingsmith_steps: Option<u32>,
}

// ---- Little-endian cursor ---------------------------------------------------

/// Bounds-checked little-endian reader. Never index-panics: every read returns
/// `Err(TooShort{..})` when fewer bytes remain than the field requires.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn need(&self, n: usize) -> Result<(), FtmsError> {
        if self.pos + n > self.buf.len() {
            Err(FtmsError::TooShort {
                expected: self.pos + n,
                got: self.buf.len(),
            })
        } else {
            Ok(())
        }
    }

    fn u8(&mut self) -> Result<u8, FtmsError> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn u16(&mut self) -> Result<u16, FtmsError> {
        self.need(2)?;
        let v = u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn i16(&mut self) -> Result<i16, FtmsError> {
        Ok(self.u16()? as i16)
    }

    /// 24-bit little-endian unsigned integer.
    fn u24(&mut self) -> Result<u32, FtmsError> {
        self.need(3)?;
        let v = (self.buf[self.pos] as u32)
            | ((self.buf[self.pos + 1] as u32) << 8)
            | ((self.buf[self.pos + 2] as u32) << 16);
        self.pos += 3;
        Ok(v)
    }
}

// ---- Parser -----------------------------------------------------------------

/// Parse a Treadmill Data (0x2ACD) characteristic value into real-world units.
///
/// Reads the u16 LE flags first, then conditionally reads each optional field in
/// the exact order defined by the spec. Returns `FtmsError::TooShort` rather than
/// panicking when the buffer is truncated. Bit 13 is treated as reserved
/// (ignored) — see [`parse_treadmill_data_ext`] for the KingSmith variant.
pub fn parse_treadmill_data(buf: &[u8]) -> Result<FtmsTreadmillData, FtmsError> {
    parse_treadmill_data_ext(buf, false)
}

/// [`parse_treadmill_data`], optionally interpreting the non-standard
/// KingSmith bit-13 extension (u16-LE step count + one pad byte trailing the
/// standard fields).
///
/// `kingsmith_steps` must only be `true` for devices positively identified as
/// KingSmith family ([`is_kingsmith_name`]): bit 13 is *reserved* in the SIG
/// spec, and interpreting it blanket would misread another vendor's frames.
/// With `false`, a set bit 13 is ignored and any trailing bytes are left
/// untouched (the standard fields still decode correctly).
pub fn parse_treadmill_data_ext(
    buf: &[u8],
    kingsmith_steps: bool,
) -> Result<FtmsTreadmillData, FtmsError> {
    let mut cur = Cursor::new(buf);
    let flags = cur.u16()?;
    let mut out = FtmsTreadmillData::default();

    // Bit 0 (More Data): when 0, Instantaneous Speed is present in THIS frame.
    // When 1, the speed lives in a separate notification of the same record.
    if flags & FLAG_MORE_DATA == 0 {
        out.instantaneous_speed = Some(cur.u16()? as f64 * 0.01);
    }
    if flags & FLAG_AVG_SPEED != 0 {
        out.average_speed = Some(cur.u16()? as f64 * 0.01);
    }
    if flags & FLAG_TOTAL_DISTANCE != 0 {
        out.total_distance_m = Some(cur.u24()?);
    }
    if flags & FLAG_INCLINATION != 0 {
        out.inclination_pct = Some(cur.i16()? as f64 * 0.1);
        out.ramp_angle_deg = Some(cur.i16()? as f64 * 0.1);
    }
    if flags & FLAG_ELEVATION_GAIN != 0 {
        out.positive_elevation_gain_m = Some(cur.u16()? as f64 * 0.1);
        out.negative_elevation_gain_m = Some(cur.u16()? as f64 * 0.1);
    }
    if flags & FLAG_INST_PACE != 0 {
        out.instantaneous_pace = Some(cur.u8()? as f64 * 0.1);
    }
    if flags & FLAG_AVG_PACE != 0 {
        out.average_pace = Some(cur.u8()? as f64 * 0.1);
    }
    if flags & FLAG_EXPENDED_ENERGY != 0 {
        out.total_energy_kcal = Some(cur.u16()? as u32);
        out.energy_per_hour_kcal = Some(cur.u16()? as u32);
        out.energy_per_minute_kcal = Some(cur.u8()? as u32);
    }
    if flags & FLAG_HEART_RATE != 0 {
        out.heart_rate_bpm = Some(cur.u8()?);
    }
    if flags & FLAG_METABOLIC_EQUIV != 0 {
        out.metabolic_equivalent = Some(cur.u8()? as f64 * 0.1);
    }
    if flags & FLAG_ELAPSED_TIME != 0 {
        out.elapsed_time_s = Some(cur.u16()? as u32);
    }
    if flags & FLAG_REMAINING_TIME != 0 {
        out.remaining_time_s = Some(cur.u16()? as u32);
    }
    if flags & FLAG_FORCE_POWER != 0 {
        out.force_on_belt_n = Some(cur.i16()? as i32);
        out.power_output_w = Some(cur.i16()? as i32);
    }
    if kingsmith_steps && flags & FLAG_KINGSMITH_STEPS != 0 {
        // KingSmith extension: u16-LE step count + one zero pad byte, trailing
        // the standard fields (mcdax/walkingpad-controller §6.2, confirmed on
        // KS-MC21 captures). The pad byte's value is not validated — only its
        // presence, so a truncated frame still errors instead of misreading.
        out.kingsmith_steps = Some(cur.u16()? as u32);
        let _pad = cur.u8()?;
    }

    Ok(out)
}

// ---- Fitness Machine Status (0x2ADA) ----------------------------------------

/// One Fitness Machine Status event, decoded. Opcode map per FTMS v1.0 §4.17
/// (cross-checked against dudanov/python-pyftms and the on-wire events in
/// mcdax/walkingpad-controller's KS-MC21 captures). Only the treadmill-relevant
/// events are named; everything else passes through as [`Other`].
///
/// [`Other`]: MachineStatus::Other
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MachineStatus {
    /// 0x01 — machine reset.
    Reset,
    /// 0x02 with parameter 0x01 — stopped by the user.
    StoppedByUser,
    /// 0x02 with parameter 0x02 — paused by the user.
    PausedByUser,
    /// 0x03 — stopped by the safety key.
    StoppedBySafetyKey,
    /// 0x04 — started or resumed by the user.
    StartedOrResumedByUser,
    /// 0x05 — target speed changed; the new target, km/h.
    TargetSpeedChanged(f64),
    /// 0xFF — control permission lost.
    ControlPermissionLost,
    /// Any other opcode, passed through raw.
    Other(u8),
}

/// Parse a Fitness Machine Status (0x2ADA) notification.
///
/// Returns `TooShort` for an empty buffer or an event cut off before its
/// required parameter — never panics on malformed input.
pub fn parse_machine_status(buf: &[u8]) -> Result<MachineStatus, FtmsError> {
    let mut cur = Cursor::new(buf);
    Ok(match cur.u8()? {
        0x01 => MachineStatus::Reset,
        0x02 => match cur.u8()? {
            // Spec: 0x01 = stopped, 0x02 = paused. An unknown parameter is
            // still definitely not-running; report it as stopped.
            0x02 => MachineStatus::PausedByUser,
            _ => MachineStatus::StoppedByUser,
        },
        0x03 => MachineStatus::StoppedBySafetyKey,
        0x04 => MachineStatus::StartedOrResumedByUser,
        0x05 => MachineStatus::TargetSpeedChanged(cur.u16()? as f64 * 0.01),
        0xFF => MachineStatus::ControlPermissionLost,
        other => MachineStatus::Other(other),
    })
}

/// What a status event does to the pause overlay: `Some(true)` = the belt is
/// paused, `Some(false)` = any pause is over, `None` = no state claim.
///
/// Only *pause* needs an overlay at all: running and stopped are already
/// visible in the speed itself, but a paused belt and a stopped belt both
/// read 0 km/h — 0x2ADA is what tells them apart.
fn pause_effect(ev: &MachineStatus) -> Option<bool> {
    match ev {
        MachineStatus::PausedByUser => Some(true),
        MachineStatus::Reset
        | MachineStatus::StoppedByUser
        | MachineStatus::StoppedBySafetyKey
        | MachineStatus::StartedOrResumedByUser => Some(false),
        MachineStatus::TargetSpeedChanged(_)
        | MachineStatus::ControlPermissionLost
        | MachineStatus::Other(_) => None,
    }
}

/// The state to report: the speed-derived state, except that a stationary
/// belt under an active pause event presents as `Paused` rather than
/// `Standby`. A moving belt is always authoritative.
fn effective_state(speed_derived: Option<BeltState>, paused: bool) -> Option<BeltState> {
    match speed_derived {
        Some(BeltState::Standby) if paused => Some(BeltState::Paused),
        other => other,
    }
}

// ---- The driver -------------------------------------------------------------

/// FTMS treadmills push Treadmill Data ~1 Hz; tolerate a quiet belt before
/// treating the link as dead.
const FTMS_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// A belt reporting less than this is stopped: FTMS has no explicit status in
/// Treadmill Data, so the running flag is derived from the speed itself.
const RUNNING_THRESHOLD_KMH: f64 = 0.05;

/// A decoded record as a neutral SI sample. FTMS already speaks SI, so this is
/// mostly a field mapping; only the belt state is synthesised (from speed).
/// Standard FTMS has no step counter, so `steps` is only ever set from the
/// name-gated KingSmith extension — absent everywhere else, never zero.
pub(crate) fn to_sample(d: &FtmsTreadmillData) -> Sample {
    Sample {
        speed_kmh: d.instantaneous_speed,
        distance_m: d.total_distance_m.map(|m| m as f64),
        steps: d.kingsmith_steps,
        duration_s: d.elapsed_time_s,
        calories: d.total_energy_kcal,
        state: d.instantaneous_speed.map(|kmh| {
            if kmh > RUNNING_THRESHOLD_KMH {
                BeltState::Running
            } else {
                BeltState::Standby
            }
        }),
    }
}

pub struct Ftms;

#[async_trait]
impl Driver for Ftms {
    fn id(&self) -> &'static str {
        "ftms"
    }

    fn matches(&self, adv: &Advertisement) -> bool {
        adv.services.contains(&super::sig_uuid(0x1826)) || matches_name(&adv.name)
    }

    fn supports(&self, _adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        // Role-verified, not UUID presence: this driver subscribes to
        // Treadmill Data, so the characteristic must be subscribable
        // (NOTIFY or INDICATE — `has_notify` accepts both flavours since
        // btleplug's subscribe() handles either). A table that merely
        // *mentions* 2ACD without the notify role is not an FTMS treadmill
        // we can read; claiming it would subscribe to nothing and jam a
        // device another driver (or the LifeSpan fallback) could serve.
        // Every other driver in the tree verifies roles; this one is not
        // the exception (docs/drivers/README.md, "A service UUID proves
        // nothing").
        super::util::has_notify(gatt, super::sig_uuid(0x2acd))
    }

    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()> {
        let treadmill_data_uuid = super::sig_uuid(0x2acd);
        let machine_status_uuid = super::sig_uuid(0x2ada);

        let chars = link.characteristics();
        if !chars.iter().any(|c| c.uuid == treadmill_data_uuid) {
            return Err(anyhow!("Treadmill Data characteristic (2ACD) missing"));
        }
        // Fitness Machine Status is spec-mandatory but verify the notify role
        // anyway — subscribe only to what actually exists.
        let has_status = chars
            .iter()
            .any(|c| c.uuid == machine_status_uuid && c.properties.contains(CharPropFlags::NOTIFY));

        // Staggered CCCD enables, status first, data last — the vendor app's
        // order. Treadmill firmware silently drops notification-enable writes
        // arriving within ~30 ms of each other; KS Fit spaces them 100/200/300
        // ms (literal constants in its subscription loop). A single-CCCD device
        // has nothing to collide with and pays no delay.
        if has_status {
            subscribe_staggered(
                link,
                &[
                    (machine_status_uuid, Duration::from_millis(100)),
                    (treadmill_data_uuid, Duration::from_millis(200)),
                ],
            )
            .await?;
        } else {
            subscribe_staggered(link, &[(treadmill_data_uuid, Duration::ZERO)]).await?;
        }
        let mut notifications = link.notifications().await?;

        // The bit-13 step extension is gated on the advertised name; when the
        // platform surfaces none, the safe default is off (steps stay absent —
        // never a misread of another vendor's reserved bit).
        let name = link
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|p| p.local_name)
            .unwrap_or_default();
        let kingsmith = is_kingsmith_name(&name);

        // Newest data-frame sample, before the pause overlay is applied.
        let mut latest = Sample::default();
        // The 0x2ADA pause overlay: a paused belt and a stopped belt both read
        // 0 km/h; this is what tells them apart on devices that report it.
        let mut paused = false;

        loop {
            let n = match tokio::time::timeout(FTMS_IDLE_TIMEOUT, notifications.next()).await {
                Ok(Some(n)) => n,
                Ok(None) => return Err(anyhow!("notification stream ended")),
                Err(_) => {
                    // A quiet belt is normal for FTMS (no push when stopped). Only
                    // treat the link as dead if the OS no longer considers us
                    // connected — recovers a stale handle without churning on
                    // legitimate pauses.
                    if !link.is_connected().await.unwrap_or(false) {
                        return Err(anyhow!("FTMS link dropped; reconnecting"));
                    }
                    continue;
                }
            };

            if n.uuid == machine_status_uuid {
                host.record_frame(0xDA, &n.value);
                let ev = match parse_machine_status(&n.value) {
                    Ok(ev) => ev,
                    Err(e) => {
                        tracing::warn!("FTMS machine-status decode error: {e}");
                        continue;
                    }
                };
                let Some(pause) = pause_effect(&ev) else {
                    continue; // no state claim (target changes etc.)
                };
                paused = pause;
                // Re-state the latest known reading under the new overlay —
                // but only once there IS a reading; an all-None sample says
                // nothing.
                if latest.state.is_some() {
                    emit(Sample {
                        state: effective_state(latest.state, paused),
                        ..latest.clone()
                    });
                }
                continue;
            }
            if n.uuid != treadmill_data_uuid {
                continue; // not ours (defensive: only subscribed chars notify)
            }

            host.record_frame(0xCD, &n.value);
            let data = match parse_treadmill_data_ext(&n.value, kingsmith) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("FTMS decode error: {e}");
                    continue;
                }
            };
            let base = to_sample(&data);
            if base.state == Some(BeltState::Running) {
                paused = false; // the belt is factually moving; any pause ended
            }
            latest = base;
            emit(Sample {
                state: effective_state(latest.state, paused),
                ..latest.clone()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// uuid16 helper builds the canonical 128-bit base form.
    #[test]
    fn uuid16_builds_base_form() {
        assert_eq!(uuid16(0x1826), FITNESS_MACHINE_SERVICE_UUID);
        assert_eq!(uuid16(0x2acd), TREADMILL_DATA_UUID);
        assert_eq!(uuid16(0x2acc), FITNESS_MACHINE_FEATURE_UUID);
        assert_eq!(uuid16(0x2ada), FITNESS_MACHINE_STATUS_UUID);
    }

    /// Flags = 0x0000: More Data bit clear, no optional bits.
    /// Exercises only the always-present Instantaneous Speed field.
    #[test]
    fn speed_only_frame() {
        // flags=0x0000, speed=0x05DC -> 1500 * 0.01 = 15.00 km/h
        let buf = [0x00, 0x00, 0xDC, 0x05];
        let d = parse_treadmill_data(&buf).unwrap();
        assert!(approx(d.instantaneous_speed.unwrap(), 15.0));
        assert_eq!(d.average_speed, None);
        assert_eq!(d.total_distance_m, None);
    }

    /// Flags bits: 0 (More Data clear -> speed present), 2 (Total Distance),
    /// 7 (Expended Energy), 10 (Elapsed Time).
    /// flags = 0b0100_1000_0100 = 0x0484 — the exact flag shape a real Urevo
    /// E1L notifies.
    #[test]
    fn speed_distance_energy_time_frame() {
        let flags: u16 = FLAG_TOTAL_DISTANCE | FLAG_EXPENDED_ENERGY | FLAG_ELAPSED_TIME;
        assert_eq!(flags, 0x0484);
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        // instantaneous speed: 0x0258 = 600 -> 6.00 km/h
        buf.extend_from_slice(&600u16.to_le_bytes());
        // total distance u24: 12345 m -> bytes 0x39 0x30 0x00
        buf.extend_from_slice(&[0x39, 0x30, 0x00]);
        // total energy 250 kcal
        buf.extend_from_slice(&250u16.to_le_bytes());
        // energy per hour 480 kcal
        buf.extend_from_slice(&480u16.to_le_bytes());
        // energy per minute 8 kcal
        buf.push(8u8);
        // elapsed time 3661 s
        buf.extend_from_slice(&3661u16.to_le_bytes());

        let d = parse_treadmill_data(&buf).unwrap();
        assert!(approx(d.instantaneous_speed.unwrap(), 6.0));
        assert_eq!(d.total_distance_m, Some(12345));
        assert_eq!(d.total_energy_kcal, Some(250));
        assert_eq!(d.energy_per_hour_kcal, Some(480));
        assert_eq!(d.energy_per_minute_kcal, Some(8));
        assert_eq!(d.elapsed_time_s, Some(3661));
        // Untouched fields stay None.
        assert_eq!(d.average_speed, None);
        assert_eq!(d.heart_rate_bpm, None);
    }

    /// flags 0x0584 = 0x0484 + Heart Rate — the flag shape a real Sportstech
    /// S-Walk / FITHOME pad notifies. Pins the field order around the HR byte.
    #[test]
    fn speed_distance_energy_hr_time_frame() {
        let flags: u16 =
            FLAG_TOTAL_DISTANCE | FLAG_EXPENDED_ENERGY | FLAG_HEART_RATE | FLAG_ELAPSED_TIME;
        assert_eq!(flags, 0x0584);
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&320u16.to_le_bytes()); // 3.20 km/h
        buf.extend_from_slice(&[0xE8, 0x03, 0x00]); // 1000 m
        buf.extend_from_slice(&42u16.to_le_bytes()); // 42 kcal
        buf.extend_from_slice(&180u16.to_le_bytes()); // 180 kcal/h
        buf.push(3u8); // 3 kcal/min
        buf.push(96u8); // 96 bpm — between energy and elapsed time
        buf.extend_from_slice(&1200u16.to_le_bytes()); // 1200 s

        let d = parse_treadmill_data(&buf).unwrap();
        assert!(approx(d.instantaneous_speed.unwrap(), 3.2));
        assert_eq!(d.total_distance_m, Some(1000));
        assert_eq!(d.total_energy_kcal, Some(42));
        assert_eq!(d.heart_rate_bpm, Some(96));
        assert_eq!(d.elapsed_time_s, Some(1200));
    }

    /// Total Distance is u24, not u16 — a widely-circulated third-party parser
    /// gets this wrong. A distance above 65535 m must decode through the third
    /// byte (a u16 misread of this frame would report 34464 m).
    #[test]
    fn total_distance_uses_the_full_u24_width() {
        let flags: u16 = FLAG_TOTAL_DISTANCE;
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&500u16.to_le_bytes()); // speed 5.00 km/h
        buf.extend_from_slice(&[0xA0, 0x86, 0x01]); // 100000 m LE u24
        let d = parse_treadmill_data(&buf).unwrap();
        assert_eq!(d.total_distance_m, Some(100_000));
        assert_ne!(d.total_distance_m, Some(0x86A0), "u16 misread");
    }

    /// Flags bits: 0 (More Data clear -> speed present), 1 (Average Speed),
    /// 3 (Inclination + Ramp Angle, both signed). Tests a NEGATIVE inclination.
    /// flags = 0b1010 = 0x000A.
    #[test]
    fn avg_speed_and_negative_inclination_frame() {
        let flags: u16 = FLAG_AVG_SPEED | FLAG_INCLINATION;
        assert_eq!(flags, 0x000A);
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        // inst speed 5.55 km/h -> 555
        buf.extend_from_slice(&555u16.to_le_bytes());
        // avg speed 5.00 km/h -> 500
        buf.extend_from_slice(&500u16.to_le_bytes());
        // inclination -3.5% -> raw -35 (sint16)
        buf.extend_from_slice(&(-35i16).to_le_bytes());
        // ramp angle +2.0 deg -> raw 20
        buf.extend_from_slice(&20i16.to_le_bytes());

        let d = parse_treadmill_data(&buf).unwrap();
        assert!(approx(d.instantaneous_speed.unwrap(), 5.55));
        assert!(approx(d.average_speed.unwrap(), 5.0));
        assert!(approx(d.inclination_pct.unwrap(), -3.5));
        assert!(approx(d.ramp_angle_deg.unwrap(), 2.0));
    }

    /// Flags bit 12 (Force on Belt + Power Output, both signed). Verifies signed
    /// decoding of a negative force/power and that the More Data bit being SET
    /// (bit 0) correctly suppresses the speed field.
    #[test]
    fn force_and_power_with_more_data_set() {
        let flags: u16 = FLAG_MORE_DATA | FLAG_FORCE_POWER;
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        // force -120 N, power -45 W
        buf.extend_from_slice(&(-120i16).to_le_bytes());
        buf.extend_from_slice(&(-45i16).to_le_bytes());

        let d = parse_treadmill_data(&buf).unwrap();
        // More Data set -> speed NOT present in this frame.
        assert_eq!(d.instantaneous_speed, None);
        assert_eq!(d.force_on_belt_n, Some(-120));
        assert_eq!(d.power_output_w, Some(-45));
    }

    /// Truncated buffer: flags claim Total Distance (u24) present but only 1
    /// distance byte follows. Must return Err, never panic.
    #[test]
    fn truncated_buffer_returns_err() {
        // flags with speed + total distance, but cut off mid-distance.
        let flags: u16 = FLAG_TOTAL_DISTANCE;
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&600u16.to_le_bytes()); // speed
        buf.push(0x39); // only 1 of 3 distance bytes
        let err = parse_treadmill_data(&buf).unwrap_err();
        assert!(matches!(err, FtmsError::TooShort { .. }));
    }

    /// Empty buffer cannot even read the mandatory flags field.
    #[test]
    fn empty_buffer_returns_err() {
        assert_eq!(
            parse_treadmill_data(&[]).unwrap_err(),
            FtmsError::TooShort {
                expected: 2,
                got: 0
            }
        );
    }

    // ---- KingSmith bit-13 step extension ------------------------------------

    /// Builds the MC-21 frame shape: 0x0484 fields + bit 13, with the three
    /// extension bytes (u16-LE steps + zero pad) trailing the standard fields.
    fn kingsmith_frame(steps: u16) -> Vec<u8> {
        let flags: u16 =
            FLAG_TOTAL_DISTANCE | FLAG_EXPENDED_ENERGY | FLAG_ELAPSED_TIME | FLAG_KINGSMITH_STEPS;
        assert_eq!(flags, 0x2484);
        let mut buf = Vec::new();
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&400u16.to_le_bytes()); // 4.00 km/h
        buf.extend_from_slice(&[0x10, 0x27, 0x00]); // 10000 m
        buf.extend_from_slice(&100u16.to_le_bytes()); // 100 kcal
        buf.extend_from_slice(&200u16.to_le_bytes()); // 200 kcal/h
        buf.push(4u8); // 4 kcal/min
        buf.extend_from_slice(&2400u16.to_le_bytes()); // 2400 s
        buf.extend_from_slice(&steps.to_le_bytes()); // extension: steps
        buf.push(0x00); // extension: zero pad
        buf
    }

    /// On a known KingSmith device the extension decodes; the standard fields
    /// are unaffected either way.
    #[test]
    fn kingsmith_extension_decodes_when_enabled() {
        let buf = kingsmith_frame(4321);
        let d = parse_treadmill_data_ext(&buf, true).unwrap();
        assert_eq!(d.kingsmith_steps, Some(4321));
        assert!(approx(d.instantaneous_speed.unwrap(), 4.0));
        assert_eq!(d.total_distance_m, Some(10000));
        assert_eq!(d.elapsed_time_s, Some(2400));
    }

    /// Without the device-family opt-in, bit 13 is reserved: the trailing
    /// bytes are ignored, steps stay absent, and the standard fields still
    /// decode — another vendor setting bit 13 must never be misread.
    #[test]
    fn kingsmith_extension_is_ignored_without_the_family_gate() {
        let buf = kingsmith_frame(4321);
        let d = parse_treadmill_data_ext(&buf, false).unwrap();
        assert_eq!(d.kingsmith_steps, None);
        assert_eq!(d.total_distance_m, Some(10000));
        // The plain entry point behaves identically.
        assert_eq!(parse_treadmill_data(&buf).unwrap(), d);
    }

    /// A KingSmith frame with bit 13 set but the extension bytes cut off must
    /// error, not misread.
    #[test]
    fn truncated_kingsmith_extension_returns_err() {
        let full = kingsmith_frame(4321);
        for cut in 1..=3 {
            let err = parse_treadmill_data_ext(&full[..full.len() - cut], true).unwrap_err();
            assert!(matches!(err, FtmsError::TooShort { .. }), "cut {cut}");
        }
    }

    /// Bit 13 clear + extension enabled: no extension bytes are expected and
    /// steps stay absent.
    #[test]
    fn extension_flag_clear_reads_no_extension() {
        let buf = [0x00, 0x00, 0xDC, 0x05]; // plain speed-only frame
        let d = parse_treadmill_data_ext(&buf, true).unwrap();
        assert_eq!(d.kingsmith_steps, None);
    }

    /// The family gate: KingSmith FTMS names (and the Zeal OEM variant) opt
    /// in; everything else — including nameless devices — stays out.
    #[test]
    fn kingsmith_family_gate_matches_only_kingsmith_names() {
        for name in [
            "KS-MC21-D06BFD",
            "ks-hd-z1d",
            "KS-AP-X10",
            " KS-NG-1 ",
            "ZP-ZEALR1-AB",
        ] {
            assert!(is_kingsmith_name(name), "{name}");
        }
        for name in ["", "URTM024", "MRK-TW50", "SPERAX_RM-01", "LifeSpan-TM"] {
            assert!(!is_kingsmith_name(name), "{name}");
        }
    }

    // ---- Fitness Machine Status (0x2ADA) -------------------------------------

    /// The event shapes seen on real KS-MC21 captures, plus the spec map.
    #[test]
    fn machine_status_events_decode() {
        assert_eq!(parse_machine_status(&[0x01]), Ok(MachineStatus::Reset));
        assert_eq!(
            parse_machine_status(&[0x02, 0x01]),
            Ok(MachineStatus::StoppedByUser)
        );
        assert_eq!(
            parse_machine_status(&[0x02, 0x02]),
            Ok(MachineStatus::PausedByUser)
        );
        assert_eq!(
            parse_machine_status(&[0x03]),
            Ok(MachineStatus::StoppedBySafetyKey)
        );
        assert_eq!(
            parse_machine_status(&[0x04]),
            Ok(MachineStatus::StartedOrResumedByUser)
        );
        // 0x05 + 0x0190 LE = target speed 4.00 km/h (the value in the real
        // MC-21 capture's speed-change command).
        assert_eq!(
            parse_machine_status(&[0x05, 0x90, 0x01]),
            Ok(MachineStatus::TargetSpeedChanged(4.0))
        );
        assert_eq!(
            parse_machine_status(&[0xFF]),
            Ok(MachineStatus::ControlPermissionLost)
        );
        // Unknown opcodes pass through raw; an unknown stop/pause parameter is
        // still definitely not-running.
        assert_eq!(
            parse_machine_status(&[0x47]),
            Ok(MachineStatus::Other(0x47))
        );
        assert_eq!(
            parse_machine_status(&[0x02, 0x7F]),
            Ok(MachineStatus::StoppedByUser)
        );
    }

    /// Malformed status notifications error instead of panicking: empty, and
    /// events cut off before their required parameter.
    #[test]
    fn malformed_machine_status_returns_err() {
        assert!(matches!(
            parse_machine_status(&[]),
            Err(FtmsError::TooShort { .. })
        ));
        assert!(matches!(
            parse_machine_status(&[0x02]),
            Err(FtmsError::TooShort { .. })
        ));
        assert!(matches!(
            parse_machine_status(&[0x05, 0x90]),
            Err(FtmsError::TooShort { .. })
        ));
    }

    /// Only pause needs an overlay: a paused belt and a stopped belt both read
    /// 0 km/h. Start/stop/safety/reset clear it; target changes claim nothing.
    #[test]
    fn pause_effect_maps_the_events() {
        assert_eq!(pause_effect(&MachineStatus::PausedByUser), Some(true));
        for ev in [
            MachineStatus::Reset,
            MachineStatus::StoppedByUser,
            MachineStatus::StoppedBySafetyKey,
            MachineStatus::StartedOrResumedByUser,
        ] {
            assert_eq!(pause_effect(&ev), Some(false), "{ev:?}");
        }
        for ev in [
            MachineStatus::TargetSpeedChanged(4.0),
            MachineStatus::ControlPermissionLost,
            MachineStatus::Other(0x47),
        ] {
            assert_eq!(pause_effect(&ev), None, "{ev:?}");
        }
    }

    /// The overlay only upgrades a stationary belt to Paused — a moving belt
    /// (or an absent state) is never overridden.
    #[test]
    fn effective_state_only_upgrades_standby_to_paused() {
        use BeltState::*;
        assert_eq!(effective_state(Some(Standby), true), Some(Paused));
        assert_eq!(effective_state(Some(Standby), false), Some(Standby));
        assert_eq!(effective_state(Some(Running), true), Some(Running));
        assert_eq!(effective_state(Some(Running), false), Some(Running));
        assert_eq!(effective_state(None, true), None);
        assert_eq!(effective_state(None, false), None);
    }

    /// The paused presentation reaches the contract's PAUSED code — the state
    /// a 2ADA pause event produces for a stationary belt.
    #[test]
    fn paused_overlay_presents_as_the_contract_paused_code() {
        let d = FtmsTreadmillData {
            instantaneous_speed: Some(0.0),
            ..Default::default()
        };
        let sample = Sample {
            state: effective_state(to_sample(&d).state, true),
            ..to_sample(&d)
        };
        let t = Telemetry::from_sample(&sample, "km/h");
        assert_eq!(t.status, Some(0x05));
        assert_eq!(t.status_name.as_deref(), Some("PAUSED"));
        assert!(!t.is_running);
    }

    /// KingSmith extension steps flow into the neutral sample; everything
    /// non-KingSmith keeps steps absent (never zero).
    #[test]
    fn kingsmith_steps_flow_into_the_sample() {
        let d = FtmsTreadmillData {
            instantaneous_speed: Some(4.0),
            kingsmith_steps: Some(1234),
            ..Default::default()
        };
        assert_eq!(to_sample(&d).steps, Some(1234));
        assert_eq!(to_sample(&FtmsTreadmillData::default()).steps, None);
    }

    // ---- Advertised-name matching --------------------------------------------

    fn adv(name: &str) -> Advertisement {
        Advertisement {
            name: name.into(),
            services: vec![],
        }
    }

    /// Every verified walking-pad prefix must make the device discoverable by
    /// name alone (many pads omit 0x1826 from the advertisement), and matching
    /// is case-insensitive.
    #[test]
    fn known_ftms_walking_pads_match_by_name() {
        for name in [
            "URTM024",
            "MRK-TW50",
            "SF-T7515",
            "CITYSPORTS-LINKER",
            "WELLFIT TM-101",
            "MOBVOI TM",
            "MOBVOI WMTP01",
            "SWALK LITE-1234",
            "ANPLUS-T1",
            "ANPIUS-T1",
            "YPOO-MINI PRO-8",
            "THERUN  T15", // double space — what the device really advertises
            "FOCUS M3",
            "KS-MC21-D06BFD",
            "KS-HD-Z1D",
            "KS-AP-X10",
            "KS-NG-AB12",
            "SPERAX_RM-01",
            "urtm024", // case-insensitive
            "ks-mc21-d06bfd",
        ] {
            assert!(Ftms.matches(&adv(name)), "{name}");
        }
    }

    /// Near-misses must NOT match: the hyphen-less SPERAX_RM01 and the
    /// SPERAX_RM-02 speak a different, proprietary protocol (qdomyos routes
    /// them to a dedicated driver), Merach rowers share the MRK- namespace,
    /// and unrelated devices stay out entirely.
    #[test]
    fn non_ftms_names_do_not_match() {
        for name in [
            "SPERAX_RM01",   // proprietary Sperax — no hyphen
            "SPERAX_RM-02",  // proprietary Sperax
            "MRK-S26S-1",    // Merach rower
            "MRK-R06-2",     // Merach bike
            "WalkingPad A1", // WiLink generation, not FTMS
            "LifeSpan-TM",
            "Some Headphones",
            "",
        ] {
            assert!(!Ftms.matches(&adv(name)), "{name}");
        }
    }

    /// Matching on the advertised 0x1826 service still works — that path is
    /// what covers every FTMS treadmill whose name we don't know.
    #[test]
    fn service_uuid_still_matches_without_a_name() {
        assert!(Ftms.matches(&Advertisement {
            name: String::new(),
            services: vec![crate::drivers::sig_uuid(0x1826)],
        }));
    }

    /// `supports()` verifies the notify ROLE on Treadmill Data, not merely
    /// the UUID: this driver subscribes to 2ACD, so a read-only 2ACD is a
    /// table we cannot read and must fall through to the next driver.
    /// Both subscription flavours count (NOTIFY and INDICATE — see
    /// util::has_notify), so the pre-83afe5b indicate-only regression cannot
    /// recur through this check.
    #[test]
    fn supports_requires_the_notify_role_on_treadmill_data() {
        use btleplug::api::CharPropFlags;
        use std::collections::BTreeSet;
        let table = |props: CharPropFlags| -> BTreeSet<btleplug::api::Characteristic> {
            [btleplug::api::Characteristic {
                uuid: crate::drivers::sig_uuid(0x2acd),
                service_uuid: crate::drivers::sig_uuid(0x1826),
                properties: props,
                descriptors: BTreeSet::new(),
            }]
            .into()
        };
        let anon = adv("");
        assert!(Ftms.supports(&anon, &table(CharPropFlags::NOTIFY)));
        assert!(
            Ftms.supports(&anon, &table(CharPropFlags::INDICATE)),
            "indicate-only Treadmill Data must stay claimable — \
             util::has_notify accepts both subscription flavours"
        );
        assert!(
            !Ftms.supports(&anon, &table(CharPropFlags::READ)),
            "a read-only 2ACD must NOT claim the device: FTMS was the only \
             supports() in the tree matching on UUID presence alone, and a \
             table without the notify role cannot be subscribed to \
             (Ftms::supports in ftms.rs; the dispatch consequence is pinned \
             in tests/driver_matrix.rs)"
        );
        assert!(!Ftms.supports(&anon, &table(CharPropFlags::empty())));
    }

    // ---- Golden pins: decoded record → Sample → Telemetry ------------------
    //
    // These values were captured against the PRE-refactor `ftms_to_telemetry`
    // (the old direct FTMS→Telemetry mapping in ble.rs), so they pin that the
    // Sample detour puts byte-identical raw fields on the wire. The raw ints
    // are asserted exactly; the floats are derived from them by the shared
    // unit helpers and follow automatically.

    use crate::telemetry::Telemetry;

    fn telem(d: &FtmsTreadmillData, unit: &str) -> Telemetry {
        Telemetry::from_sample(&to_sample(d), unit)
    }

    fn approx12(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn golden_full_frame_kmh() {
        let d = FtmsTreadmillData {
            instantaneous_speed: Some(6.0),
            total_distance_m: Some(12345),
            elapsed_time_s: Some(3661),
            total_energy_kcal: Some(250),
            ..Default::default()
        };
        let t = telem(&d, "km/h");
        assert_eq!(t.speed_raw, Some(600));
        assert_eq!(t.status, Some(3));
        assert_eq!(t.status_name.as_deref(), Some("RUNNING"));
        assert!(t.is_running);
        assert_eq!(t.distance_raw, Some(1235));
        assert_eq!(t.distance_m, Some(12350));
        assert_eq!(t.duration_s, Some(3661));
        assert_eq!(t.calories, Some(250));
        assert_eq!(t.steps, None);
        assert!(approx12(t.speed_kmh.unwrap(), 6.0));
        assert!(approx12(t.speed_mph.unwrap(), 6.0 / 1.609344));
        assert!(approx12(t.distance_km.unwrap(), 12.35));
        assert!(approx12(t.distance_mi.unwrap(), 12.35 / 1.609344));
    }

    #[test]
    fn golden_speed_mph_console() {
        let d = FtmsTreadmillData {
            instantaneous_speed: Some(3.0),
            ..Default::default()
        };
        let t = telem(&d, "mph");
        assert_eq!(t.speed_raw, Some(186));
        assert_eq!(t.status, Some(3));
        assert!(approx12(t.speed_kmh.unwrap(), 2.99337984));
        assert!(approx12(t.speed_mph.unwrap(), 1.86));
    }

    #[test]
    fn golden_stopped_belt() {
        let d = FtmsTreadmillData {
            instantaneous_speed: Some(0.0),
            ..Default::default()
        };
        let t = telem(&d, "km/h");
        assert_eq!(t.speed_raw, Some(0));
        assert_eq!(t.status, Some(1));
        assert_eq!(t.status_name.as_deref(), Some("STANDBY"));
        assert!(!t.is_running);
    }

    #[test]
    fn golden_distance_only_frame() {
        let d = FtmsTreadmillData {
            total_distance_m: Some(100),
            ..Default::default()
        };
        let t = telem(&d, "km/h");
        assert_eq!(t.speed_raw, None);
        assert_eq!(t.status, None);
        assert_eq!(t.status_name, None);
        assert!(!t.is_running);
        assert_eq!(t.distance_raw, Some(10));
        assert_eq!(t.distance_m, Some(100));
    }

    /// The driver UUIDs and the documented string set must agree.
    #[test]
    fn driver_uuids_match_the_documented_set() {
        assert_eq!(
            crate::drivers::sig_uuid(0x1826).to_string(),
            FITNESS_MACHINE_SERVICE_UUID
        );
        assert_eq!(
            crate::drivers::sig_uuid(0x2acd).to_string(),
            TREADMILL_DATA_UUID
        );
        assert_eq!(
            crate::drivers::sig_uuid(0x2ada).to_string(),
            FITNESS_MACHINE_STATUS_UUID
        );
    }
}
