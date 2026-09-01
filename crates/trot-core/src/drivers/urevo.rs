//! Urevo (E1L family) driver — the proprietary step-reporting protocol on
//! service 0xFFF0.
//!
//! Protocol knowledge was ported from and re-verified against
//! **blak3r/treadspan** (MIT, © 2025 Blake Robertson) — see
//! THIRD-PARTY-NOTICES.md: `arduino/src/TreadmillDeviceUrevoProtocol.h` (the
//! wake write and the field map) and `protocol-analysis/urevo-E1L/` (a
//! 568-unique-frame annotated capture of a real E1L, model `URTM041`, which
//! is this module's fixture source). qdomyos-zwift has **no** Urevo
//! proprietary path — it routes all `URTM*` devices to FTMS — so treadspan is
//! the only upstream, and every field below was re-checked against the raw
//! capture rather than taken on faith (twice that mattered; see the checksum
//! and distance notes).
//!
//! Why this driver exists at all: the E1L exposes standard FTMS *and* this
//! proprietary service, and the proprietary one is strictly better — FTMS on
//! this hardware reports no steps, while the native stream counts steps
//! accurately (per the treadspan author, counting stops when you step off
//! the belt). So this driver outranks FTMS in the registry, exactly like the
//! other native protocols.
//!
//! **The 0xFFF0 collision, again:** service `0xFFF0` with notify `FFF1` /
//! write `FFF2` is byte-for-byte the LifeSpan layout — with a completely
//! different protocol behind it. `supports()` therefore requires a recognised
//! `URTM041` advertised name (verified from the E1L's real advertisements in
//! the treadspan capture) *and* the characteristic roles; a nameless
//! FFF1/FFF2 device is left for the deliberate LifeSpan fallback at the end
//! of the registry, and `URTM024` (Urevo Spacewalk 3S) is plain FTMS and must
//! stay with the FTMS driver.
//!
//! Interaction model: **init handshake, then push.** The pad is silent until
//! it receives the status-stream request `02 51 0B 03` on FFF2 (the only
//! frame this driver ever writes — the vendor app's captures show it verbatim,
//! written to a pad in standby that stayed in standby), after which it streams
//! status frames on FFF1 about three times a second, in every state including
//! standby.
//!
//! Status frame (all multi-byte integers **little-endian**; 19 bytes when the
//! console is active, 6 bytes in deep standby):
//!   byte  0:      0x02 (STX)
//!   byte  1:      0x51 (message family: status)
//!   byte  2:      status (see [`belt_state`])
//!   byte  3:      belt speed, 0.1 mph
//!   byte  4:      (unknown; 0x00 in every captured frame)
//!   bytes 5..7:   elapsed time, seconds (u16 LE — verified against the
//!                 capture's wall-clock timestamps)
//!   bytes 7..9:   distance, 0.01 mile units (u16 LE — see below)
//!   bytes 9..11:  distance again, 0.001 mile units (u16 LE; unparsed)
//!   bytes 11..13: steps (u16 LE)
//!   bytes 13..17: (unknown; 0x00 in every captured frame)
//!   byte  17:     checksum: `sum(bytes 0..17) mod 256, XOR 0x5A` — the
//!   E1L fixture; the URTM030 firmware computes the same trailer over
//!   bytes 1..17, excluding the STX (see [`ChecksumKind`])
//!   byte  18:     0x03 (ETX)
//!
//! Provenance notes, because two published claims about this protocol are
//! wrong and this module corrects both:
//!
//! * **Distance is 0.01 mi per unit, not 0.1 mi.** treadspan's comment says
//!   "0.1 miles", but its own conversion constant (16.0934 m per unit) is the
//!   0.01-mile value, and the capture proves it: the field advances ~96.5
//!   units per integrated mile of belt speed (0.1 mi/unit would put a walking
//!   pad at 12 mph). Bytes 9..11 are the same odometer at 0.001 mi/unit
//!   (~1064 units/mile in the capture, advancing in a strict 10:1 lock with
//!   bytes 7..9 over constant-speed stretches); left unparsed because no
//!   working implementation reads it and the coarse field is already verified.
//! * **The speed byte is plain 0.1 mph.** A published third-party analysis
//!   claims Urevo needs a "0.006225680934 conversion factor"; it does not —
//!   the console simply displays mph while FTMS reports 0.01 km/h, and
//!   0.01 km/h→mph is 0.00621371. Standard scaling throughout.
//!
//! The checksum rule (`sum XOR 0x5A`) is this module's own derivation —
//! treadspan does not validate inbound frames — verified over all 568 unique
//! captured frames including the 6-byte standby frame. The E1L (URTM041)
//! sums bytes 0..len-2; the URTM030 firmware was verified (live capture, a
//! walking session) to sum bytes 1..len-2 instead — the STX is framing, not
//! data. The two conventions never agree on the same frame, so the parser
//! takes the device's expected variant explicitly rather than "accept both":
//! accepting either would smuggle in a corrupted frame whose flipped byte
//! happens to satisfy the other rule. A corrupt counter that parses cleanly
//! poisons step totals, so unlike upstream we reject any frame whose trailer
//! doesn't check out under the device's own rule.
//!
//! No energy field is known in the native protocol, so `calories` comes from
//! the pad's FTMS service when it has one (see [`FTMS_DATA_UUID`] and
//! [`cache_ftms_energy`]); on a pad without it, calories stay `None` —
//! absent, not zero.

use super::util::{run_init_sequence, GattIo, InitStep};
use super::{Advertisement, BeltState, Driver, DriverHost, Emit, Sample};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;

// ---- UUIDs (service 0xFFF0 — the contested block) ---------------------------
pub const SERVICE_UUID: Uuid = super::sig_uuid(0xfff0);
pub const NOTIFY_CHAR_UUID: Uuid = super::sig_uuid(0xfff1);
pub const WRITE_CHAR_UUID: Uuid = super::sig_uuid(0xfff2);

/// FTMS Treadmill Data characteristic — the *other* service these pads
/// expose. It carries no steps, but it does report energy, which the native
/// stream lacks; where it exists we ride its calories on the native samples.
pub const FTMS_DATA_UUID: Uuid = super::sig_uuid(0x2acd);

// ---- Advertised names -------------------------------------------------------
//
// The E1L advertises `URTM041` (verified in treadspan's app capture, 97
// advertising reports). Only names verified to speak this protocol belong
// here: `URTM024` (Spacewalk 3S) and the rest of the URTM range are plain
// FTMS and are served by the FTMS driver — an over-broad "URTM" prefix here
// would steal them. Comparison is case-insensitive.

/// Name prefixes of Urevo pads verified to speak the proprietary protocol.
pub const ADV_NAME_PREFIXES: &[&str] = &["URTM030", "URTM041"];

// ---- Wire constants ---------------------------------------------------------

pub const FRAME_PREFIX: u8 = 0x02; // STX
pub const TERMINATOR: u8 = 0x03; // ETX
/// Message family of the status stream — request and response both carry it.
pub const MSG_STATUS: u8 = 0x51;
/// The trailer is `sum(bytes) mod 256` XOR this mask (derived from and
/// verified over all 568 unique captured frames).
pub const CHECKSUM_XOR_MASK: u8 = 0x5A;

/// The status-stream request, byte-identical to what the vendor app writes
/// (three occurrences in treadspan's app capture, handle FFF2). Family 0x51
/// is the telemetry family — the same first two bytes every status frame
/// echoes back. The app's *settings/control* writes use different family
/// bytes; none of them is ported (see `the_driver_only_ever_writes_the_
/// status_stream_request` below).
pub const WAKE_FRAME: [u8; 4] = [FRAME_PREFIX, MSG_STATUS, 0x0B, TERMINATOR];

/// Deep-standby frames are 6 bytes: STX, family, status, one unknown byte,
/// checksum, ETX.
pub const MIN_FRAME_LEN: usize = 6;
/// Shortest frame whose counter fields can be trusted.
///
/// The counters end at byte 12 (steps are bytes 11..13), but the TRAILER is
/// positional from the END — checksum at `len-2`, ETX at `len-1` — so on a
/// 13-byte frame bytes 11 and 12 ARE the trailer: `steps` would decode as
/// `(ETX << 8) | checksum` ≈ 800–1000, a plausible, correctly-checksummed
/// fabrication that flows straight into stored history. On a 14-byte frame
/// the low steps byte is still the checksum. 15 is the first length at which
/// every counter byte is clear of the trailer, so 15 is the floor.
/// Start-anchored offsets then stay valid on any longer variant (the only
/// observed full frame is 19 bytes). Do NOT lower this back to 13 to "accept
/// shorter variants": a shorter variant does not carry these counters.
pub const COUNTER_FRAME_MIN_LEN: usize = 15;

/// km/h per 0.1 mph wire unit.
pub const KMH_PER_RAW_SPEED: f64 = 0.160_934_4;
/// Meters per 0.01 mile wire unit (treadspan's 16.0934, at full precision).
pub const METERS_PER_RAW_DISTANCE: f64 = 16.093_44;

/// The pad streams ~3 Hz in every observed state, so a silent link is
/// suspect; after this we either re-arm the stream or declare the link dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

// ---- Frame building ---------------------------------------------------------

/// The init handshake: exactly one write, the status-stream request. Declared
/// as `InitStep`s so the write set is pinnable by tests the same way as the
/// other drivers'.
pub fn init_steps() -> Vec<InitStep> {
    vec![InitStep::write(WRITE_CHAR_UUID, WAKE_FRAME)]
}

/// The inbound trailer: `sum(bytes) mod 256, XOR 0x5A`.
pub fn checksum(bytes: &[u8]) -> u8 {
    super::util::checksum_sum(bytes) ^ CHECKSUM_XOR_MASK
}

// ---- Status-frame parsing ---------------------------------------------------

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("expected at least {MIN_FRAME_LEN} bytes, got {0}")]
    BadLength(usize),
    #[error("bad prefix 0x{0:02x}")]
    BadPrefix(u8),
    /// A well-prefixed frame of another message family. The pad answers the
    /// vendor app's settings queries on the same characteristic; skip them.
    #[error("not a status frame (message family 0x{0:02x})")]
    NotStatus(u8),
    #[error("missing 0x03 terminator")]
    BadTerminator,
    #[error("checksum mismatch: computed 0x{computed:02x}, frame carries 0x{found:02x}")]
    BadChecksum { computed: u8, found: u8 },
}

/// The counters carried by a full-length status frame, as the wire reports
/// them. Deep-standby frames omit them entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    /// Belt speed in 0.1 mph.
    pub speed_raw: u8,
    /// Elapsed time, seconds.
    pub duration_s: u32,
    /// Distance in 0.01 mile units.
    pub distance_raw: u32,
    /// Cumulative steps.
    pub steps: u32,
}

/// One decoded status frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Status byte (see [`belt_state`]).
    pub status: u8,
    /// Present on full frames; `None` on the 6-byte deep-standby frame.
    pub counters: Option<Counters>,
}

fn u16_le(frame: &[u8], at: usize) -> u32 {
    (frame[at] as u32) | ((frame[at + 1] as u32) << 8)
}

/// Which slice the checksum trailer covers. The two Urevo firmware
/// conventions disagree on whether the STX counts as data, and they never
/// both succeed on the same frame — the parser therefore takes the device's
/// allocated variant explicitly (never "accept either", which would let a
/// corrupted frame satisfying the other rule through).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksumKind {
    /// The E1L (URTM041): `sum(bytes 0..len-2) mod 256, XOR 0x5A`.
    IncludeStx,
    /// The URTM030 firmware: `sum(bytes 1..len-2) mod 256, XOR 0x5A`.
    ExcludeStx,
}

/// Parse a notification into a [`Status`]. Pure function of the bytes; never
/// panics on malformed input. Counts the STX byte in the checksum (the
/// E1L/URTM041 convention); use [`parse_status_with`] for a specific variant.
pub fn parse_status(frame: &[u8]) -> Result<Status, ProtocolError> {
    parse_status_with(frame, ChecksumKind::IncludeStx)
}

/// Like [`parse_status`], but verifies the trailer under the device's own
/// [`ChecksumKind`].
pub fn parse_status_with(frame: &[u8], kind: ChecksumKind) -> Result<Status, ProtocolError> {
    if frame.len() < MIN_FRAME_LEN {
        return Err(ProtocolError::BadLength(frame.len()));
    }
    if frame[0] != FRAME_PREFIX {
        return Err(ProtocolError::BadPrefix(frame[0]));
    }
    if frame[1] != MSG_STATUS {
        return Err(ProtocolError::NotStatus(frame[1]));
    }
    if frame[frame.len() - 1] != TERMINATOR {
        return Err(ProtocolError::BadTerminator);
    }
    let computed = match kind {
        ChecksumKind::IncludeStx => checksum(&frame[..frame.len() - 2]),
        ChecksumKind::ExcludeStx => checksum(&frame[1..frame.len() - 2]),
    };
    let found = frame[frame.len() - 2];
    if computed != found {
        return Err(ProtocolError::BadChecksum { computed, found });
    }
    let counters = if frame.len() >= COUNTER_FRAME_MIN_LEN {
        Some(Counters {
            speed_raw: frame[3],
            duration_s: u16_le(frame, 5),
            distance_raw: u16_le(frame, 7),
            steps: u16_le(frame, 11),
        })
    } else {
        None
    };
    Ok(Status {
        status: frame[2],
        counters,
    })
}

/// The wire's status byte as a neutral [`BeltState`].
///
/// Per-value provenance (capture = treadspan's annotated E1L session log,
/// comments = `TreadmillDeviceUrevoProtocol.h`):
///
/// * `0x00` standby — capture (the deep-standby frames) and comments.
/// * `0x01` — observed in the capture immediately after a full stop, with
///   speed 0 and frozen counters; not named by treadspan. A stopped belt, so
///   `Standby`.
/// * `0x02` starting — comments only (never captured); treadspan opens its
///   session on it, i.e. the belt is beginning to move: `Running`.
/// * `0x03` running — capture (553 frames) and comments.
/// * `0x04` pausing — capture: the belt decelerating after the pause button,
///   speed still non-zero. The user has paused: `Paused`.
/// * `0x06` off (display asleep) — comments only. Not running: `Standby`.
/// * `0x0A` paused — capture: speed 0, counters held.
///
/// Everything else is unverified and passes through as [`BeltState::Other`].
pub(crate) fn belt_state(v: u8) -> BeltState {
    match v {
        0x00 | 0x01 | 0x06 => BeltState::Standby,
        0x02 | 0x03 => BeltState::Running,
        0x04 | 0x0A => BeltState::Paused,
        other => BeltState::Other(other),
    }
}

/// A [`Status`] as a neutral SI sample. The wire speaks imperial (0.1 mph,
/// 0.01 mi) regardless of anything the user configured, so the conversion is
/// fixed; `host.display_unit` is irrelevant to this driver.
pub(crate) fn to_sample(s: &Status) -> Sample {
    match &s.counters {
        Some(c) => Sample {
            speed_kmh: Some(c.speed_raw as f64 * KMH_PER_RAW_SPEED),
            distance_m: Some(c.distance_raw as f64 * METERS_PER_RAW_DISTANCE),
            steps: Some(c.steps),
            duration_s: Some(c.duration_s),
            calories: None, // no verified energy field — absent, not zero
            state: Some(belt_state(s.status)),
        },
        None => Sample {
            // Deep standby: the pad reports only its state. Everything else
            // is absent, not zero — a zero here would poison day totals.
            state: Some(belt_state(s.status)),
            ..Sample::default()
        },
    }
}

// ---- The driver -------------------------------------------------------------

fn normalized(name: &str) -> String {
    name.trim().to_ascii_uppercase()
}

/// Does the advertised name identify a proprietary-protocol Urevo pad?
fn matches_name(name: &str) -> bool {
    let n = normalized(name);
    ADV_NAME_PREFIXES.iter().any(|pfx| n.starts_with(pfx))
}

/// Notify on FFF1, write on FFF2 — the same shape as LifeSpan, which is
/// exactly why the name gate above is mandatory.
fn gatt_shape_matches(gatt: &BTreeSet<Characteristic>) -> bool {
    super::util::has_notify(gatt, NOTIFY_CHAR_UUID) && super::util::has_write(gatt, WRITE_CHAR_UUID)
}

pub struct Urevo;

/// Merge an FTMS Treadmill Data notification's total energy (kcal) into the
/// running energy cache for the native stream.
///
/// The energy is cumulative per session and present only when the FTMS frame
/// carries it, so it stays behind the native samples as `Option`: `None`
/// until the first calibrated reading arrives, and never a fabricated zero.
fn cache_ftms_energy(kcal: &mut Option<u32>, frame: &[u8]) {
    match super::ftms::parse_treadmill_data(frame) {
        Ok(d) => {
            if let Some(k) = d.total_energy_kcal {
                *kcal = Some(k);
            }
        }
        Err(e) => tracing::debug!("ignoring unparseable FTMS frame: {e}"),
    }
}

/// Is this the 5-byte keepalive the URTM030 firmware streams (~1 Hz) while
/// the belt is stopped? It's the ack for the status-stream wake, not a
/// status frame, so the driver skips it instead of warning on every one.
/// Gated on the URTM030 checksum variant — the E1L's deep-standby frames are
/// 6 bytes and must keep flowing through the normal path.
fn is_idle_ack(value: &[u8], kind: ChecksumKind) -> bool {
    kind == ChecksumKind::ExcludeStx
        && value.len() == 5
        && value[0] == FRAME_PREFIX
        && value[1] == MSG_STATUS
}

/// Ride the cached FTMS energy on a native sample — but only on frames that
/// carry counters. A deep-standby frame reports only its state; tagging it
/// with calories would claim the pad sent an energy reading it didn't (and
/// would hand the pipeline a calories-only row whose every other field is
/// absent — a shape no other driver emits).
fn merge_energy(sample: &mut Sample, status: &Status, kcal: Option<u32>) {
    if status.counters.is_some() {
        sample.calories = kcal;
    }
}

#[async_trait]
impl Driver for Urevo {
    fn id(&self) -> &'static str {
        "urevo"
    }

    fn matches(&self, adv: &Advertisement) -> bool {
        // Name only. The 0xFFF0 service UUID proves nothing (LifeSpan's
        // scan-time matcher already lists devices advertising it), and the
        // URTM024/FTMS pads are matched by the FTMS driver's name list.
        matches_name(&adv.name)
    }

    fn supports(&self, adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        // Strict on purpose: recognised name AND the exact roles. A nameless
        // FFF1/FFF2 device must NOT land here — it is far more likely a
        // LifeSpan console that lost its name at connect time, and the
        // LifeSpanFallback at the end of the registry exists for it. (The
        // cost: a nameless E1L degrades to FTMS — steps absent — or to the
        // fallback's benign unanswered-polls failure. A wrong claim in either
        // direction would write one protocol at the other's hardware.)
        matches_name(&adv.name) && gatt_shape_matches(gatt)
    }

    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()> {
        let chars = link.characteristics();
        let notify_char = chars
            .iter()
            .find(|c| c.uuid == NOTIFY_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("notify characteristic (FFF1) missing"))?;
        if !chars.iter().any(|c| c.uuid == WRITE_CHAR_UUID) {
            return Err(anyhow!("write characteristic (FFF2) missing"));
        }

        // Subscribe first, then wake — the pad streams immediately after the
        // request and the first frame must not be missed.
        link.subscribe(&notify_char).await?;

        // These pads also expose a real FTMS service. It has no step counter
        // but it does report energy, which the native stream lacks — so where
        // a Treadmill Data (2ACD) notify is present we take calories from it
        // and ride them on the native samples. Optional on purpose: a pad
        // without it simply reports no calories (absent, never zero), and a
        // failed subscribe must not take the native stream down either.
        if let Some(c) = chars
            .iter()
            .find(|c| c.uuid == FTMS_DATA_UUID && c.properties.contains(CharPropFlags::NOTIFY))
        {
            if let Err(e) = link.subscribe(c).await {
                tracing::warn!("could not subscribe to FTMS energy (2ACD): {e}");
            }
        }

        let mut notifications = link.notifications().await?;
        run_init_sequence(link, &init_steps()).await?;

        // The checksum trailer covers the STX on the E1L (URTM041) but not on
        // the URTM030 firmware; pick the variant from the device's own name
        // (see ChecksumKind). A name that can't be read falls back to the E1L
        // rule — logged, because on a URTM030 that silently rejects every
        // frame, and a silent full-stream loss is exactly what the name gate
        // exists to prevent.
        let checksum_kind = match link.properties().await {
            Ok(Some(p)) => match p.local_name.as_deref().map(normalized) {
                Some(n) if n.starts_with("URTM030") => ChecksumKind::ExcludeStx,
                Some(_) => ChecksumKind::IncludeStx,
                None => {
                    tracing::warn!(
                        "no advertised name from the pad; assuming the E1L checksum — a URTM030 would be rejected"
                    );
                    ChecksumKind::IncludeStx
                }
            },
            Ok(None) => {
                tracing::warn!("pad reported no properties; assuming the E1L checksum");
                ChecksumKind::IncludeStx
            }
            Err(e) => {
                tracing::warn!("could not read pad properties ({e}); assuming the E1L checksum");
                ChecksumKind::IncludeStx
            }
        };

        let mut kcal: Option<u32> = None;
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, notifications.next()).await {
                Ok(Some(n)) => n,
                Ok(None) => return Err(anyhow!("notification stream ended")),
                Err(_) => {
                    if !link.is_connected().await.unwrap_or(false) {
                        return Err(anyhow!("link dropped; reconnecting"));
                    }
                    // The pad streams ~3 Hz even in standby, so quiet means
                    // the stream lapsed (console power-cycled, say). Re-arm
                    // it with the same status-stream request — still the only
                    // frame this driver ever writes.
                    link.write_uuid(WRITE_CHAR_UUID, &WAKE_FRAME, true).await?;
                    continue;
                }
            };

            if n.uuid == NOTIFY_CHAR_UUID {
                // The URTM030 firmware answers the status-stream wake with a 5-byte
                // keepalive (`02 51 00 0b 03`) while the belt is stopped, at
                // ~1 Hz. It's an ack, not a status frame, so skip it instead
                // of logging a decode warning every second.
                if is_idle_ack(&n.value, checksum_kind) {
                    continue;
                }
                host.record_frame(MSG_STATUS, &n.value); // raw capture for /api/diag
                match parse_status_with(&n.value, checksum_kind) {
                    Ok(status) => {
                        let mut sample = to_sample(&status);
                        merge_energy(&mut sample, &status, kcal);
                        emit(sample);
                    }
                    Err(ProtocolError::NotStatus(t)) => {
                        tracing::debug!("ignoring non-status frame family 0x{t:02x}");
                    }
                    Err(e) => tracing::warn!("urevo decode error: {e}"),
                }
            } else if n.uuid == FTMS_DATA_UUID {
                host.record_frame(0xCD, &n.value); // raw capture for /api/diag
                cache_ftms_energy(&mut kcal, &n.value);
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

    // ---- The write set -------------------------------------------------------

    /// Every byte this driver writes must be a read. The whole write set is
    /// one frame: the status-stream request, byte-identical to the vendor
    /// app's capture. The count is part of the assertion — the app also
    /// writes settings frames under other family bytes (`02 53 …` and
    /// friends), and none of those may ever appear here.
    #[test]
    fn the_driver_only_ever_writes_the_status_stream_request() {
        let frames: Vec<Vec<u8>> = init_steps().iter().map(|s| s.payload.clone()).collect();
        assert_eq!(
            frames,
            vec![hx("02 51 0b 03")],
            "init must be the status-stream request, nothing else"
        );
        // Family 0x51 is the status/telemetry family — the verb boundary in
        // this protocol. Everything we send must carry it.
        for frame in &frames {
            assert_eq!(frame[1], MSG_STATUS, "non-status family in {frame:02x?}");
        }
        // And the loop's only other write is the identical re-arm frame.
        assert_eq!(WAKE_FRAME.to_vec(), hx("02 51 0b 03"));
    }

    /// The single init write goes to FFF2, immediately (no protocol delay is
    /// documented, and the capture shows none).
    #[tokio::test(start_paused = true)]
    async fn init_sequence_is_one_write_to_fff2() {
        #[derive(Default)]
        struct MockLink {
            writes: Mutex<Vec<(Uuid, Vec<u8>, Instant)>>,
        }
        #[async_trait]
        impl GattIo for MockLink {
            async fn write_uuid(&self, c: Uuid, p: &[u8], _wr: bool) -> Result<()> {
                self.writes
                    .lock()
                    .unwrap()
                    .push((c, p.to_vec(), Instant::now()));
                Ok(())
            }
            async fn subscribe_uuid(&self, _c: Uuid) -> Result<()> {
                Ok(())
            }
        }
        let link = MockLink::default();
        let start = Instant::now();
        run_init_sequence(&link, &init_steps()).await.unwrap();
        let writes = link.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1, "exactly one init write");
        assert_eq!(writes[0].0, WRITE_CHAR_UUID);
        assert_eq!(writes[0].1, hx("02 51 0b 03"));
        assert_eq!(writes[0].2 - start, Duration::ZERO);
    }

    // ---- Real captured fixtures ----------------------------------------------
    //
    // All frames below are verbatim from treadspan's E1L capture log
    // (protocol-analysis/urevo-E1L/README.md, model URTM041). The expected
    // values follow the field map verified against that capture's wall-clock
    // timestamps and integrated belt speed.

    const RUNNING_FIXTURE: &str = "02 51 03 0e 00 45 01 0b 00 80 00 6b 01 00 00 00 00 fb 03";

    #[test]
    fn decodes_a_running_frame() {
        // 1.4 mph, 325 s, 0.11 mi, 363 steps.
        let s = parse_status(&hx(RUNNING_FIXTURE)).unwrap();
        assert_eq!(s.status, 0x03);
        let c = s.counters.unwrap();
        assert_eq!(c.speed_raw, 14);
        assert_eq!(c.duration_s, 325);
        assert_eq!(c.distance_raw, 11);
        assert_eq!(c.steps, 363);
    }

    #[test]
    fn decodes_the_annotated_treadspan_frame() {
        // The frame treadspan annotates in TreadmillDeviceUrevoProtocol.h:
        // 1.4 mph, 124 s, 126 steps — and distance raw 3, which is 0.03 mi
        // (not the header comment's "0.3": at 124 s that would need 8.7 mph).
        let s = parse_status(&hx(
            "02 51 03 0e 00 7c 00 03 00 2c 00 7e 00 00 00 00 00 d7 03",
        ))
        .unwrap();
        let c = s.counters.unwrap();
        assert_eq!(c.speed_raw, 14);
        assert_eq!(c.duration_s, 124);
        assert_eq!(c.distance_raw, 3);
        assert_eq!(c.steps, 126);
    }

    #[test]
    fn decodes_the_state_transition_frames() {
        // Pausing: belt decelerating (1.2 mph), counters still advancing.
        let s = parse_status(&hx(
            "02 51 04 0c 00 4a 01 0b 00 82 00 72 01 00 00 00 00 f4 03",
        ))
        .unwrap();
        assert_eq!(s.status, 0x04);
        assert_eq!(s.counters.as_ref().unwrap().speed_raw, 12);
        assert_eq!(s.counters.as_ref().unwrap().steps, 370);

        // Fully paused: speed 0, counters held.
        let s = parse_status(&hx(
            "02 51 0a 00 00 4a 01 0b 00 82 00 76 01 00 00 00 00 f6 03",
        ))
        .unwrap();
        assert_eq!(s.status, 0x0A);
        assert_eq!(s.counters.as_ref().unwrap().speed_raw, 0);
        assert_eq!(s.counters.as_ref().unwrap().steps, 374);

        // After a full stop: status 0x01, everything frozen.
        let s = parse_status(&hx(
            "02 51 01 00 00 4a 01 0b 00 82 00 76 01 00 00 00 00 f9 03",
        ))
        .unwrap();
        assert_eq!(s.status, 0x01);
        assert_eq!(s.counters.as_ref().unwrap().duration_s, 330);
    }

    /// The pad's deep-standby frame is 6 bytes and carries only the status —
    /// the counters must come out absent, not zero.
    #[test]
    fn decodes_the_short_standby_frame() {
        let s = parse_status(&hx("02 51 00 00 09 03")).unwrap();
        assert_eq!(s.status, 0x00);
        assert_eq!(s.counters, None);
        let sample = to_sample(&s);
        assert_eq!(sample.state, Some(BeltState::Standby));
        assert_eq!(sample.steps, None);
        assert_eq!(sample.speed_kmh, None);
        assert_eq!(sample.distance_m, None);
    }

    /// The trailer is END-anchored (checksum at `len-2`, ETX at `len-1`)
    /// while the counters are START-anchored (steps at 11..13). On a 13- or
    /// 14-byte frame those regions OVERLAP, so a valid trailer would decode
    /// as a plausible step count. Such frames must yield no counters at all.
    #[test]
    fn frames_too_short_for_the_counters_yield_none_not_trailer_bytes() {
        // A hypothetical mid-length variant: valid envelope, valid checksum,
        // counter region zeroed — at these lengths the trailer bytes sit
        // where steps would be read.
        for len in [13usize, 14] {
            let mut frame = vec![0u8; len];
            frame[0] = FRAME_PREFIX;
            frame[1] = MSG_STATUS;
            frame[2] = 0x03; // running
            frame[len - 1] = TERMINATOR;
            frame[len - 2] = checksum(&frame[..len - 2]);
            let s = parse_status(&frame).unwrap();
            assert_eq!(
                s.counters, None,
                "a {len}-byte frame reported counters — its trailer overlaps \
                 the steps field, so `steps` would be fabricated from the \
                 checksum/ETX bytes (see COUNTER_FRAME_MIN_LEN's comment in \
                 urevo.rs: the floor for the current field set is 15)"
            );
        }
        // The real 19-byte capture is unaffected: counters still decode.
        let s = parse_status(&hx(RUNNING_FIXTURE)).unwrap();
        assert_eq!(
            s.counters.unwrap().steps,
            363,
            "the 19-byte fixture must keep decoding"
        );
    }

    // ---- Checksum ------------------------------------------------------------

    /// The trailer rule (sum mod 256, XOR 0x5A) must verify on the real
    /// frames — it was derived from this capture, treadspan does not check
    /// it — and a single corrupted counter byte must be rejected rather than
    /// parsed into someone's step history.
    #[test]
    fn checksum_verifies_on_real_frames_and_rejects_corruption() {
        for raw in [
            RUNNING_FIXTURE,
            "02 51 03 06 00 16 00 00 00 06 00 0d 00 00 00 00 00 df 03",
            "02 51 00 00 09 03", // the 6-byte standby frame checks out too
        ] {
            let frame = hx(raw);
            assert_eq!(
                checksum(&frame[..frame.len() - 2]),
                frame[frame.len() - 2],
                "{raw}"
            );
        }
        // Flip one steps byte: reject.
        let mut corrupt = hx(RUNNING_FIXTURE);
        corrupt[11] ^= 0x01;
        assert!(matches!(
            parse_status(&corrupt),
            Err(ProtocolError::BadChecksum { .. })
        ));
        // Fixing the trailer makes the same bytes parse again.
        let n = corrupt.len();
        corrupt[n - 2] = checksum(&corrupt[..n - 2]);
        assert_eq!(parse_status(&corrupt).unwrap().counters.unwrap().steps, 362);
    }

    /// The URTM030 firmware's trailer is the same sums, over bytes 1..len-2
    /// (the STX is framing, not data). Verified on a live walking-session
    /// capture; the two conventions never agree, so each device only accepts
    /// its own variant.
    #[test]
    fn urtm030_checksum_covers_bytes_1_dot_dot_len_minus_2() {
        // A running capture (status 0x03, 19 bytes) and a deep-standby
        // frame, both from a live URTM030 session while walking.
        let running = hx("02 51 03 0a 00 50 01 09 00 61 00 cd 00 00 00 00 00 bc 03");
        let standby = hx("02 51 02 03 0c 03");

        for frame in [&running, &standby] {
            assert!(parse_status_with(frame, ChecksumKind::ExcludeStx).is_ok());
            assert!(
                parse_status_with(frame, ChecksumKind::IncludeStx).is_err(),
                "URTM030 frame {frame:02x?} would collide with the E1L variant"
            );
        }
        // The E1L fixture is the mirror image: only the E1L variant accepts it.
        let e1l = hx(RUNNING_FIXTURE);
        assert!(parse_status_with(&e1l, ChecksumKind::IncludeStx).is_ok());
        assert!(parse_status_with(&e1l, ChecksumKind::ExcludeStx).is_err());

        // The URTM030 counter offsets are the same as the E1L's.
        let s = parse_status_with(&running, ChecksumKind::ExcludeStx).unwrap();
        let c = s.counters.unwrap();
        assert_eq!(c.steps, 0x00cd, "step counter decodes at 11..13");
        assert_eq!(c.speed_raw, 0x0a, "speed decodes at byte 3");
        assert_eq!(s.status, 0x03, "status decodes at byte 2");

        // Corrupting a counter byte is rejected under the device's own rule.
        for i in 0..running.len() {
            let mut m = running.clone();
            m[i] ^= 0x01;
            assert!(
                parse_status_with(&m, ChecksumKind::ExcludeStx).is_err(),
                "URTM030 accepted a corrupted frame (flip at {i})"
            );
        }
    }

    /// The FTMS energy merge: a Treadmill Data frame that carries energy
    /// fills the cache, one that does not leaves it untouched, and garbage
    /// never mints a reading.
    #[test]
    fn ftms_energy_merges_only_present_calories() {
        // Real frames from a URTM030 walking session (FTMS ran in parallel
        // with the native stream in the first live capture).
        let with_kcal = hx("84 04 64 00 09 00 00 01 00 ff ff ff 23 00"); // 1 kcal
                                                                         // Flags 0x0004 (Total Distance): no speed and no energy fields at
                                                                         // all, so the cache is left alone.
        let without_energy = hx("04 00 64 00 09 00 00");
        // Real FTMS frame from the same session: energy present, value zero.
        // A reported zero is data, not absence — it must ride through.
        let zero_kcal = hx("84 04 00 00 05 00 00 00 00 ff ff ff 13 00");
        let garbage = vec![0xDE, 0xAD, 0xBE];

        let mut kcal = None;
        cache_ftms_energy(&mut kcal, &garbage);
        assert_eq!(kcal, None, "garbage must not mint a reading");
        cache_ftms_energy(&mut kcal, &without_energy);
        assert_eq!(
            kcal, None,
            "a frame without energy leaves the cache untouched"
        );
        cache_ftms_energy(&mut kcal, &zero_kcal);
        assert_eq!(kcal, Some(0), "a reported zero is data, not absence");
        cache_ftms_energy(&mut kcal, &with_kcal);
        assert_eq!(
            kcal,
            Some(1),
            "the energy field rides onto the native stream"
        );
    }

    /// The end-to-end merge invariant at the Sample boundary: a native frame
    /// and a calorie-bearing FTMS frame together produce a sample that has
    /// both native steps and the FTMS energy.
    #[test]
    fn native_sample_carries_the_merged_ftms_calories() {
        let native = parse_status_with(
            &hx("02 51 03 0a 00 50 01 09 00 61 00 cd 00 00 00 00 00 bc 03"),
            ChecksumKind::ExcludeStx,
        )
        .unwrap();
        let mut sample = to_sample(&native);
        assert_eq!(sample.steps, Some(0xcd), "native steps decode");
        assert_eq!(
            sample.calories, None,
            "calories are absent before an FTMS frame"
        );

        let mut kcal = None;
        cache_ftms_energy(&mut kcal, &hx("84 04 64 00 09 00 00 01 00 ff ff ff 23 00"));
        sample.calories = kcal;
        assert_eq!(sample.calories, Some(1));
        assert_eq!(
            sample.steps,
            Some(0xcd),
            "native counters survive the merge"
        );

        // A deep-standby frame reports only its state: it must NOT ride the
        // energy cache, or the pipeline would get a calories-only row.
        let standby =
            parse_status_with(&hx("02 51 02 03 0c 03"), ChecksumKind::ExcludeStx).unwrap();
        let mut idle = to_sample(&standby);
        let kcal = Some(9); // cranked during a previous walk
        merge_energy(&mut idle, &standby, kcal);
        assert_eq!(idle.calories, None, "standby frames never carry calories");
        assert_eq!(idle.steps, None, "…nor any other counter");
    }

    /// Only the URTM030 5-byte idle ack is skipped — never the E1L's 6-byte
    /// standby frames, never a running frame, and never a non-status family.
    #[test]
    fn idle_ack_is_the_short_status_family_frame_from_a_urtm030() {
        let ack = hx("02 51 00 0b 03"); // real URTM030 keepalive
        assert!(is_idle_ack(&ack, ChecksumKind::ExcludeStx), "the real ack");
        // The E1L variant is not gated: its frames must keep flowing.
        assert!(!is_idle_ack(&ack, ChecksumKind::IncludeStx));
        // Six-byte deep-standby and 19-byte running frames are not acks.
        assert!(!is_idle_ack(
            &hx("02 51 02 03 0c 03"),
            ChecksumKind::ExcludeStx
        ));
        assert!(!is_idle_ack(&hx(RUNNING_FIXTURE), ChecksumKind::ExcludeStx));
        // A short frame of another family is not our ack.
        assert!(!is_idle_ack(
            &hx("02 50 00 0b 03"),
            ChecksumKind::ExcludeStx
        ));
    }

    // ---- Malformed input -----------------------------------------------------

    #[test]
    fn malformed_frames_error_without_panicking() {
        assert_eq!(parse_status(&[]), Err(ProtocolError::BadLength(0)));
        assert_eq!(
            parse_status(&hx("02 51 03")),
            Err(ProtocolError::BadLength(3))
        );
        // A LifeSpan-style response, say — wrong prefix entirely.
        assert_eq!(
            parse_status(&hx("a1 aa 00 23 00 00")),
            Err(ProtocolError::BadPrefix(0xA1))
        );
        // Right envelope, missing terminator.
        let mut no_term = hx(RUNNING_FIXTURE);
        let n = no_term.len();
        no_term[n - 1] = 0x00;
        assert_eq!(parse_status(&no_term), Err(ProtocolError::BadTerminator));
        // Truncated mid-frame: the ETX lands elsewhere, so it fails cleanly.
        assert!(parse_status(&hx(RUNNING_FIXTURE)[..10]).is_err());
    }

    /// The vendor app also converses in other message families on the same
    /// characteristic (settings, 0x53 among them); those are expected traffic
    /// and must be identified, not mangled or warned about as corruption.
    #[test]
    fn non_status_families_are_identified_not_mangled() {
        // Synthetic: family 0x53 with a valid envelope and trailer.
        let mut frame = hx("02 53 00 00 00 03");
        frame[4] = checksum(&frame[..4]);
        assert_eq!(parse_status(&frame), Err(ProtocolError::NotStatus(0x53)));
    }

    // ---- Belt state ----------------------------------------------------------

    #[test]
    fn belt_states_map_and_unknowns_pass_through() {
        assert_eq!(belt_state(0x00), BeltState::Standby);
        assert_eq!(belt_state(0x01), BeltState::Standby, "post-stop");
        assert_eq!(belt_state(0x02), BeltState::Running, "starting");
        assert_eq!(belt_state(0x03), BeltState::Running);
        assert_eq!(belt_state(0x04), BeltState::Paused, "pausing");
        assert_eq!(belt_state(0x06), BeltState::Standby, "display off");
        assert_eq!(belt_state(0x0A), BeltState::Paused);
        for v in [0x05u8, 0x07, 0x09, 0x0B, 0x7f, 0xff] {
            assert_eq!(belt_state(v), BeltState::Other(v), "byte 0x{v:02x}");
        }
    }

    // ---- Sample / Telemetry golden pins --------------------------------------

    /// Fixture frame → Sample → Telemetry: the imperial→SI conversion and the
    /// presentation re-encoding, pinned end to end. The console displays mph,
    /// so the mph console is the natural presentation to check.
    #[test]
    fn golden_fixture_to_telemetry() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let s = parse_status(&hx(RUNNING_FIXTURE)).unwrap();
        let sample = to_sample(&s);
        assert!(approx(sample.speed_kmh.unwrap(), 14.0 * 0.1609344));
        assert!(approx(sample.distance_m.unwrap(), 11.0 * 16.09344));
        assert_eq!(sample.steps, Some(363));
        assert_eq!(sample.duration_s, Some(325));
        assert_eq!(sample.calories, None, "no verified energy field — absent");
        assert_eq!(sample.state, Some(BeltState::Running));

        let t = Telemetry::from_sample(&sample, "mph");
        assert_eq!(t.speed_raw, Some(140), "1.40 mph in centi-units");
        assert!(approx(t.speed_mph.unwrap(), 1.4));
        assert_eq!(t.distance_raw, Some(18), "177.03 m → 18 decameters");
        assert_eq!(t.steps, Some(363));
        assert_eq!(t.duration_s, Some(325));
        assert_eq!(t.calories, None);
        assert_eq!(t.status_name.as_deref(), Some("RUNNING"));
        assert!(t.is_running);
    }

    /// A paused pad presents as the contract's PAUSED code; a stopped one as
    /// STANDBY — never the other way around.
    #[test]
    fn paused_and_stopped_present_as_the_contract_codes() {
        let paused = parse_status(&hx(
            "02 51 0a 00 00 4a 01 0b 00 82 00 76 01 00 00 00 00 f6 03",
        ))
        .unwrap();
        let t = Telemetry::from_sample(&to_sample(&paused), "mph");
        assert_eq!(t.status, Some(0x05));
        assert_eq!(t.status_name.as_deref(), Some("PAUSED"));
        assert!(!t.is_running);

        let stopped = parse_status(&hx(
            "02 51 01 00 00 4a 01 0b 00 82 00 76 01 00 00 00 00 f9 03",
        ))
        .unwrap();
        let t = Telemetry::from_sample(&to_sample(&stopped), "mph");
        assert_eq!(t.status, Some(0x01));
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
    fn only_verified_urevo_names_match() {
        for name in ["URTM041", "urtm041", " URTM041-AB12 "] {
            assert!(Urevo.matches(&adv(name)), "{name}");
        }
        for name in [
            "URTM024", // Spacewalk 3S — plain FTMS, stays with the FTMS driver
            "URTM",    // unverified relatives stay with FTMS until proven
            "LifeSpan-TM",
            "SPERAX_RM01",
            "",
        ] {
            assert!(!Urevo.matches(&adv(name)), "{name}");
        }
    }

    // ---- supports(): the FFF0 disambiguation ---------------------------------

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

    fn urevo_shaped() -> BTreeSet<Characteristic> {
        gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
            (WRITE_CHAR_UUID, CharPropFlags::WRITE),
        ])
    }

    #[test]
    fn supports_needs_the_name_and_the_roles() {
        assert!(Urevo.supports(&adv("URTM041"), &urevo_shaped()));
        // Write-without-response also satisfies the write role.
        assert!(Urevo.supports(
            &adv("URTM041"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
                (WRITE_CHAR_UUID, CharPropFlags::WRITE_WITHOUT_RESPONSE),
            ])
        ));
        // Nameless: refused — a nameless FFF1/FFF2 device is far more likely
        // a LifeSpan console, and the fallback at the end of the registry
        // handles it.
        assert!(!Urevo.supports(&adv(""), &urevo_shaped()));
        // A LifeSpan name with the same shape: not ours.
        assert!(!Urevo.supports(&adv("LifeSpan-TM"), &urevo_shaped()));
        // Roles swapped (the Deerrun shape): refused, whatever the name says.
        assert!(!Urevo.supports(
            &adv("URTM041"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::WRITE),
                (WRITE_CHAR_UUID, CharPropFlags::NOTIFY),
            ])
        ));
        // Half a table: refused.
        assert!(!Urevo.supports(
            &adv("URTM041"),
            &gatt(&[(NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY)])
        ));
    }
}
