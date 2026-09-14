//! PitPat / Deerrun / SupeRun driver — the shared OEM protocol behind the
//! `PITPAT-T*` walking pads (frames `6A/68 <len> … <xor> 43`), sold under many
//! retail names (PitPat T01, SupeRun BA06-B1/BA04, Deerrun pads, and a long
//! tail of Amazon/AliExpress house brands on the same module).
//!
//! ## Sources (see THIRD-PARTY-NOTICES.md)
//!
//! * **peteh/pacekeeper** (GPL-3.0, same license as Trot) —
//!   `src/TreadmillHandler.cpp` + `src/platform.h`: the primary source. The
//!   only implementation verified on real hardware whose full telemetry
//!   decode includes steps (PitPat-T01 / SupeRun BA06-B1 — one device, two
//!   retail names). Establishes the FBA0 service layout and, crucially, the
//!   interaction model on that hardware: **subscribe-and-push** — pacekeeper
//!   never writes a single frame to read the stream.
//! * **azmke/pitpat-treadmill-control** (MIT) — `src/treadmill_data.py` +
//!   `src/bluetooth_manager.py`: a decompiled-vendor-app-grade decoder and
//!   the FFFF/FF01/FF02 transport. Source of the inbound XOR checksum rule,
//!   the 4-byte `4D 00 <seq> <len>` transport envelope, the
//!   firmware-conditional duration unit, and the only published real capture
//!   (the 52-byte idle frame pinned in the tests below).
//! * **KeiranY/PitPat-WebBT** (public domain) — `treadmill.js`: independent
//!   confirmation of every field offset on FBA0, and the characterization of
//!   `6A 05 FD F8 43` as the heartbeat/status query.
//! * **sirfergy/HomeAssistantWalkingPad** (GPL-3.0) — `protocol.py`: a
//!   pacekeeper-derived rewrite whose tested finding settles the unit
//!   question (below).
//! * **cagnulein/qdomyos-zwift** (GPL-3.0) — `src/devices/deerruntreadmill/`:
//!   the `PITPAT-T` advertised-name matcher (`src/devices/bluetooth.cpp`),
//!   the Deerrun FFF0 transport with its **swapped** notify/write roles, and
//!   the provenance of the poll frame. Its treadmill driver is mostly belt
//!   control (its init sequence even starts the tape); none of that is
//!   ported — see "What we send" below.
//!
//! ## The transport is not one thing
//!
//! At least four service/characteristic layouts carry this one protocol, so
//! `supports()` probes rather than assumes ([`select_transport`]), in
//! declining order of how well each is verified:
//!
//! | Variant | Service | Write | Notify | Outbound framing |
//! |---|---|---|---|---|
//! | PitPat (pacekeeper hardware) | `FBA0` | `FBA1` | `FBA2` | bare |
//! | SupeRun (azmke hardware) | `FFFF` | `FF01` | `FF02` | `4D`-enveloped |
//! | Deerrun native (qdomyos) | `FFF0` | **`FFF1`** | **`FFF2`** | `4D`-enveloped |
//! | seen in GATT dumps | `1910` | `2B11` | `2B10` | bare (unverified) |
//!
//! The Deerrun variant is the reason the codebase's `0xFFF0` guards exist:
//! same service block as LifeSpan/Urevo/Sperax, with the notify and write
//! roles **swapped**. Because every driver here verifies characteristic
//! *roles* (`util::has_notify`/`has_write`), a Deerrun can never be claimed
//! by a LifeSpan entry (it fails their role check) and a LifeSpan can never
//! be claimed by this driver (it fails ours) — `drivers/mod.rs` pins the
//! whole adjudication. The `1910` layout comes from the GATT dump in
//! pacekeeper's `src/platform.h`, where `2B10` is annotated `[read,notify]`
//! and `2B11` `[read,write…]` — that annotation is where our role
//! assignment comes from. No implementation drives the protocol over it, so
//! it is unverified and probed last; a device exposing both it and `FBA0`
//! lands on the verified `FBA0`. (qdomyos knows `1910` only as a fallback
//! *unlock* service and never mentions `2B10` at all.)
//!
//! ## Interaction model
//!
//! **Subscribe-and-push, with a query fallback.** The pacekeeper hardware
//! streams ~50-byte status frames on its own once subscribed — zero writes.
//! The other implementations poll: qdomyos writes `6A 05 FD F8 43` every
//! update tick, KeiranY/azmke echo it after every notification as a
//! "heartbeat". So this driver subscribes first, sends one status query to
//! wake request/response firmware, and thereafter only repeats the query
//! when the stream goes quiet — push firmware gets pacekeeper's zero-write
//! behaviour, poll firmware gets its prompt.
//!
//! ## What we send, and why only that
//!
//! The frame **length byte is the verb family** in this protocol: length
//! `0x05` frames carry a bare command byte and no payload (nothing to set);
//! length `0x17` (23-byte) frames are the **actuation family** — start,
//! stop, pause and set-speed all share that one shape, with the command at
//! byte 12 and a speed payload at bytes 6..8 (every upstream builds them; a
//! test pins that we never do). Of the three reported init frames:
//!
//! * `6A 05 FD F8 43` (command `0xFD`, empty payload) — **sent.** The
//!   status query: qdomyos' "poll", KeiranY/azmke's "heartbeat", written
//!   continuously by both with no effect on the belt, and the frame the
//!   ≥31-byte status packets answer. An empty-payload frame carries no
//!   value to set. This is the only frame this driver ever writes.
//! * `6B 05 9D 98 43` — **never sent.** A different frame family entirely
//!   (prefix `0x6B`), which qdomyos labels "unlock" and writes to a separate
//!   vendor channel before its belt-control sequence. A write that exists to
//!   enable actuation is exactly what Trot's policy bans, and pacekeeper
//!   proves the telemetry stream flows without it.
//! * `6A 05 D7 D2 43` (command `0xD7`) — **never sent.** Uncharacterised:
//!   it appears only in qdomyos' init, uncommented, nothing reads a reply to
//!   it, and pacekeeper again proves it unnecessary for observing. Same
//!   precedent as WiLink's dropped `B1`/`B3` frames and Sperax's dropped
//!   `0x13`. Its bytes survive below only as a checksum vector.
//!
//! On the enveloped transports the query is wrapped as
//! `4D 00 <seq> 05 6A 05 FD F8 43` — a transport header (constant `4D 00`,
//! a sequence counter, the inner length), byte-identical to azmke's
//! heartbeat wrapping and qdomyos' non-PitPat poll.
//!
//! ## Status frame (≥31 bytes; all multi-byte integers **big-endian**)
//!
//! Per-field provenance: pacekeeper = `TreadmillHandler::notifyCallback`,
//! azmke = `TreadmillData.__init__`, KeiranY = `handleNotification`; the
//! three agree on every offset, and the azmke real capture verifies them.
//!
//! ```text
//! byte  0:      0x68 on the one real published frame; NOT validated — no
//!               upstream checks it, and the inner prefix on the enveloped
//!               transports is unconfirmed
//! byte  1:      frame length (== the full frame length on the real capture)
//! bytes 3..5:   current speed, thousandths of km/h
//! bytes 5..7:   target speed (decoded upstream; no Sample field — unparsed)
//! bytes 7..11:  distance, thousandths of km — i.e. metres
//! bytes 14..18: steps
//! bytes 18..20: energy, kcal
//! bytes 20..24: elapsed time — ms on firmware ≥20, SECONDS before (azmke's
//!               firmware-conditional rule; the fw byte is in the frame, so
//!               the rule is self-contained. pacekeeper/KeiranY/sirfergy
//!               divide by 1000 unconditionally, which is identical on all
//!               tested hardware — the capture reports firmware 27)
//! byte  25:     firmware version
//! byte  26:     flags — bit 7 imperial *display*, bits 3..4 the belt state
//!               (see [`belt_state`]); bits 0..2 and 5..6 carry
//!               wifi/bracelet status (decoded by azmke, irrelevant here)
//! bytes 27..29: maximum speed (unparsed — no Sample field)
//! byte  len-2:  checksum: XOR of bytes 1..=len-3 (azmke's inbound rule,
//!               verified on the real capture; identical to the outbound
//!               rule every builder uses)
//! byte  len-1:  0x43 terminator
//! ```
//!
//! Unparsed on purpose: target speed, max speed (no neutral field wants
//! them), byte 24 (azmke's "cycle id"), the extended tail (serial number,
//! battery, motor diagnostics — azmke decodes them; nothing here needs
//! them). Incline at byte 11 is decoded by azmke/qdomyos but these pads
//! have no powered incline and `Sample` has no field for it.
//!
//! ## Two upstream disagreements, resolved here
//!
//! * **The wire is metric even when flags bit 7 is set.** pacekeeper and
//!   KeiranY *label* the values "mph"/"mi" when the bit is set but never
//!   rescale them — pacekeeper's field is literally named `distanceKm` and
//!   is consumed as km unconditionally. azmke records the bit as
//!   `unit_mode` and never reads it again. sirfergy asserts the invariant
//!   in a test. So all four decoders agree in behaviour, whatever their
//!   labels say: nothing converts. We read the bit as a property of the
//!   console's display, not of the wire, and a test pins that a frame
//!   differing only in this bit decodes to identical SI values.
//! * **qdomyos' non-PitPat inbound decode is not portable.** It reads speed
//!   at bytes 9..11 in units of 0.01 km/h — inconsistent with the envelope
//!   plus the field map above, and its own expression
//!   (`(value[9] << 8) & 0xff`, which is always zero) contradicts the u16 it
//!   claims to read. azmke's strip-4-bytes-then-shared-field-map, verified
//!   against its real capture, is what we follow on every transport. For
//!   the same reason the inbound checksum follows azmke (XOR bytes
//!   1..=len-3) and not qdomyos' long-frame variant (XOR bytes 5..=len-3,
//!   then XOR byte 1) — the two agree on every published frame (bytes 2..5
//!   are zero in all of them) but diverge on real running frames, where
//!   bytes 3..5 carry the speed, and only azmke's rule has been validated
//!   against a real inbound frame.
//!
//! qdomyos fabricates distance by integrating speed over wall-clock time;
//! this protocol reports distance directly, so nothing is integrated here.

use super::util::{run_init_sequence, GattIo, InitStep};
use super::{Advertisement, BeltState, Driver, DriverHost, Emit, Sample};
use crate::diagnostics::Link as Peripheral;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use btleplug::api::{Characteristic, Peripheral as _};
use futures::StreamExt;
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;

// ---- UUIDs: the four transport variants -------------------------------------

/// PitPat native service (the pacekeeper hardware). Distinctive to this
/// protocol family in every known source — the PitPat *bike* (`PITPAT-S*`)
/// lives on 0xFBB0, one block over.
pub const PITPAT_SERVICE_UUID: Uuid = super::sig_uuid(0xfba0);
pub const PITPAT_WRITE_UUID: Uuid = super::sig_uuid(0xfba1);
pub const PITPAT_NOTIFY_UUID: Uuid = super::sig_uuid(0xfba2);

/// SupeRun variant (azmke hardware), service 0xFFFF.
pub const SUPERUN_WRITE_UUID: Uuid = super::sig_uuid(0xff01);
pub const SUPERUN_NOTIFY_UUID: Uuid = super::sig_uuid(0xff02);

/// Deerrun native variant: the contested 0xFFF0 block with the notify/write
/// roles SWAPPED relative to LifeSpan/Urevo/Sperax — write on FFF1, notify
/// on FFF2.
pub const DEERRUN_WRITE_UUID: Uuid = super::sig_uuid(0xfff1);
pub const DEERRUN_NOTIFY_UUID: Uuid = super::sig_uuid(0xfff2);

/// The 0x1910 vendor service seen alongside FBA0 in the pacekeeper GATT
/// dump. No implementation drives the protocol over it; probed last.
pub const V1910_WRITE_UUID: Uuid = super::sig_uuid(0x2b11);
pub const V1910_NOTIFY_UUID: Uuid = super::sig_uuid(0x2b10);

// ---- Advertised names -------------------------------------------------------
//
// qdomyos' matcher (`PITPAT-T*`, upper-cased prefix) is the only verified
// list; the pacekeeper hardware — retail name "SupeRun BA06-B1" — advertises
// `PitPat-T01` (pacekeeper's README documents it), and no source shows a
// DEERRUN/SUPERUN/BA06 advertised name. The `-T` in the prefix is
// load-bearing: `PITPAT-S*` is the PitPat *bike* (qdomyos routes it to a
// bike driver) and must never match a treadmill-only tool.

/// Name prefixes of pads verified to speak this protocol.
pub const ADV_NAME_PREFIXES: &[&str] = &["PITPAT-T"];

// ---- Wire constants ---------------------------------------------------------

/// Outbound frame prefix (requests). Inbound frames carried 0x68 on the one
/// real capture; no upstream validates the inbound prefix and neither do we.
pub const REQ_PREFIX: u8 = 0x6A;
/// Every frame, in and out, ends with this byte.
pub const TERMINATOR: u8 = 0x43;
/// Length byte of the query family: prefix, length, command, checksum,
/// terminator — no payload.
pub const SHORT_FRAME_LEN: u8 = 0x05;
/// The status-query command byte (qdomyos' poll, KeiranY/azmke's heartbeat).
pub const CMD_STATUS_QUERY: u8 = 0xFD;

/// The one frame this driver ever writes: the status query
/// `6A 05 FD F8 43`. Checksum = XOR of bytes 1..=2 = `05 ^ FD` = `F8`.
pub const STATUS_QUERY: [u8; 5] = [
    REQ_PREFIX,
    SHORT_FRAME_LEN,
    CMD_STATUS_QUERY,
    0xF8,
    TERMINATOR,
];

/// Transport envelope prefix on the FFFF and FFF0 variants:
/// `4D 00 <seq> <inner len>` wraps every outbound frame (azmke's heartbeat
/// header, qdomyos' non-PitPat wrapping).
pub const ENVELOPE_PREFIX: u8 = 0x4D;
/// The envelope is 4 bytes; azmke strips exactly this many from inbound
/// notifications on the enveloped transport.
pub const ENVELOPE_LEN: usize = 4;

/// 31 bytes — the bound every published decoder applies (pacekeeper
/// `length < 31`, azmke `len(payload) < 31`, KeiranY `byteLength < 31`,
/// sirfergy `MIN_STATE_PACKET_LENGTH`). Every field this driver parses sits
/// below offset 29, so the bound is comfortable rather than tight.
pub const MIN_STATUS_FRAME_LEN: usize = 31;

/// Flags byte, bits 3..4: the belt state (values below). The other bits
/// carry the imperial-display bit (7) and wifi/bracelet status — masking
/// with this keeps them out of state decoding.
pub const STATE_MASK: u8 = 0x18;
pub const STATE_STOPPED: u8 = 0x00;
pub const STATE_RUNNING: u8 = 0x08;
pub const STATE_PAUSED: u8 = 0x10;
pub const STATE_COUNTDOWN: u8 = 0x18;
/// Flags bit 7: the console *displays* imperial units. The wire stays
/// metric regardless (see the module docs); this bit is deliberately not
/// used for scaling.
pub const IMPERIAL_DISPLAY_FLAG: u8 = 0x80;

/// Firmware versions ≥ this report the duration field in milliseconds;
/// older firmware reports seconds (azmke's decompiled-app rule,
/// `firmware_version > 19`).
pub const FIRMWARE_DURATION_MS_MIN: u8 = 20;

/// Push firmware streams several frames a second; request/response firmware
/// answers the query promptly. A quiet second means either an idle
/// request/response pad (re-poll) or a dying link (counted).
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive quiet intervals before the link is declared dead — same
/// rationale as the LifeSpan/WiLink/Sperax drivers: macOS can hold a stale
/// handle open with no disconnect event.
const MAX_DEAD_POLLS: u32 = 15;

// ---- Transport probing ------------------------------------------------------

/// One of the four service layouts this protocol ships behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Service FBA0 — write FBA1, notify FBA2. Hardware-verified
    /// (pacekeeper); bare frames.
    PitPat,
    /// Service FFFF — write FF01, notify FF02. Hardware-verified (azmke);
    /// `4D`-enveloped frames.
    SupeRun,
    /// Service FFF0 — write FFF1, notify FFF2 (roles swapped versus
    /// LifeSpan). From qdomyos; `4D`-enveloped frames.
    Deerrun,
    /// Service 1910 — write 2B11, notify 2B10. Seen in GATT dumps only;
    /// bare frames assumed (it coexists with the bare FBA0 on the same
    /// hardware). Probed last.
    V1910,
}

impl Transport {
    /// Probe order: hardware-verified layouts first, the dump-only 1910
    /// last, so a device exposing several lands on the best-understood one.
    pub const ALL: [Transport; 4] = [
        Transport::PitPat,
        Transport::SupeRun,
        Transport::Deerrun,
        Transport::V1910,
    ];

    pub fn write_uuid(self) -> Uuid {
        match self {
            Transport::PitPat => PITPAT_WRITE_UUID,
            Transport::SupeRun => SUPERUN_WRITE_UUID,
            Transport::Deerrun => DEERRUN_WRITE_UUID,
            Transport::V1910 => V1910_WRITE_UUID,
        }
    }

    pub fn notify_uuid(self) -> Uuid {
        match self {
            Transport::PitPat => PITPAT_NOTIFY_UUID,
            Transport::SupeRun => SUPERUN_NOTIFY_UUID,
            Transport::Deerrun => DEERRUN_NOTIFY_UUID,
            Transport::V1910 => V1910_NOTIFY_UUID,
        }
    }

    /// Does this transport wrap frames in the `4D 00 <seq> <len>` envelope?
    pub fn enveloped(self) -> bool {
        matches!(self, Transport::SupeRun | Transport::Deerrun)
    }
}

/// The transport this GATT table carries, if any — notify and write roles
/// both verified, not just UUIDs. This is what keeps a LifeSpan-shaped FFF0
/// table (notify FFF1 / write FFF2) out of this driver: only the *swapped*
/// arrangement selects the Deerrun transport.
pub fn select_transport(gatt: &BTreeSet<Characteristic>) -> Option<Transport> {
    Transport::ALL.into_iter().find(|t| {
        super::util::has_notify(gatt, t.notify_uuid())
            && super::util::has_write(gatt, t.write_uuid())
    })
}

// ---- Frame building ---------------------------------------------------------

/// The frame checksum every builder in every upstream uses, and azmke
/// validates inbound: XOR of everything between the prefix byte and the
/// checksum itself (`bytes[1..=len-3]`).
pub fn frame_checksum(frame: &[u8]) -> u8 {
    super::util::checksum_xor(&frame[1..frame.len() - 2])
}

/// Wrap a frame for the enveloped transports: `4D 00 <seq> <len> <inner…>`.
pub fn wrap_envelope(seq: u8, inner: &[u8]) -> Vec<u8> {
    let mut wire = Vec::with_capacity(ENVELOPE_LEN + inner.len());
    wire.extend_from_slice(&[ENVELOPE_PREFIX, 0x00, seq, inner.len() as u8]);
    wire.extend_from_slice(inner);
    wire
}

/// The status query as this transport puts it on the wire.
pub fn status_query_frame(transport: Transport, seq: u8) -> Vec<u8> {
    if transport.enveloped() {
        wrap_envelope(seq, &STATUS_QUERY)
    } else {
        STATUS_QUERY.to_vec()
    }
}

/// The init handshake: exactly one write — the status query (sequence 0 on
/// the enveloped transports). Declared as `InitStep`s so the write set is
/// pinnable by tests the same way as the other drivers'.
pub fn init_steps(transport: Transport) -> Vec<InitStep> {
    vec![InitStep::write(
        transport.write_uuid(),
        status_query_frame(transport, 0),
    )]
}

// ---- Status-frame parsing ---------------------------------------------------

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("expected at least {MIN_STATUS_FRAME_LEN} bytes, got {0}")]
    BadLength(usize),
    #[error("missing 0x43 terminator")]
    BadTerminator,
    #[error("checksum mismatch: computed 0x{computed:02x}, frame carries 0x{found:02x}")]
    BadChecksum { computed: u8, found: u8 },
}

/// One decoded status frame, fields as the wire reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Belt speed in thousandths of km/h.
    pub speed_raw: u32,
    /// Distance in thousandths of a kilometre — i.e. metres.
    pub distance_raw: u32,
    /// Cumulative steps.
    pub steps: u32,
    /// Cumulative energy, kcal.
    pub calories: u32,
    /// Elapsed time as the wire carries it: ms on firmware ≥20, seconds
    /// before (see [`FIRMWARE_DURATION_MS_MIN`]).
    pub duration_raw: u32,
    /// Firmware version byte — decides the duration unit.
    pub fw_version: u8,
    /// Raw flags byte (state bits, imperial-display bit, wifi/bracelet).
    pub flags: u8,
}

fn u16_be(frame: &[u8], at: usize) -> u32 {
    u16::from_be_bytes([frame[at], frame[at + 1]]) as u32
}

fn u32_be(frame: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]])
}

/// Parse a bare (envelope-free) status frame. Pure function of the bytes;
/// never panics on malformed input.
///
/// Stricter than three of the four decoders, deliberately: only azmke
/// verifies the inbound checksum, but a corrupt counter that parses cleanly
/// silently poisons someone's step totals, so any frame whose trailer
/// doesn't XOR out is rejected (the rule verifies on the real capture — see
/// the tests).
pub fn parse_status(frame: &[u8]) -> Result<Status, ProtocolError> {
    if frame.len() < MIN_STATUS_FRAME_LEN {
        return Err(ProtocolError::BadLength(frame.len()));
    }
    if frame[frame.len() - 1] != TERMINATOR {
        return Err(ProtocolError::BadTerminator);
    }
    let computed = frame_checksum(frame);
    let found = frame[frame.len() - 2];
    if computed != found {
        return Err(ProtocolError::BadChecksum { computed, found });
    }
    Ok(Status {
        speed_raw: u16_be(frame, 3),
        distance_raw: u32_be(frame, 7),
        steps: u32_be(frame, 14),
        calories: u16_be(frame, 18),
        duration_raw: u32_be(frame, 20),
        fw_version: frame[25],
        flags: frame[26],
    })
}

/// Decode one notification, handling the transport envelope.
///
/// On the enveloped transports the inner frame (first 4 bytes stripped, as
/// azmke does) is tried first and the bare interpretation second; on the
/// bare transports the other way round. Trying both directions costs
/// nothing — the checksum arbitrates, and a frame that decodes under the
/// wrong interpretation would need a one-in-256 trailer coincidence *after*
/// passing the terminator check — and it keeps the driver working if a
/// variant turns out to envelope (or not) contrary to its sources. The
/// error reported is the preferred interpretation's.
pub fn decode_notification(frame: &[u8], enveloped: bool) -> Result<Status, ProtocolError> {
    let stripped: Option<&[u8]> =
        (frame.len() >= ENVELOPE_LEN + MIN_STATUS_FRAME_LEN).then(|| &frame[ENVELOPE_LEN..]);
    if enveloped {
        match stripped {
            Some(inner) => parse_status(inner).or_else(|e| parse_status(frame).map_err(|_| e)),
            None => parse_status(frame),
        }
    } else {
        parse_status(frame).or_else(|e| match stripped {
            Some(inner) => parse_status(inner).map_err(|_| e),
            None => Err(e),
        })
    }
}

/// The flags byte's state bits (already masked with [`STATE_MASK`]) as a
/// neutral [`BeltState`].
///
/// Per-value provenance (pacekeeper's enum, azmke's `running_state`,
/// KeiranY's `statusArr` — all three agree):
///
/// * `0x00` stopped → `Standby`.
/// * `0x08` running → `Running`.
/// * `0x10` paused → `Paused`.
/// * `0x18` countdown — the 3-2-1 before the belt moves. The workout is
///   starting, so `Running`, the same call the Urevo driver makes for its
///   "starting" state.
///
/// A 2-bit field can hold nothing else, but the unknown-byte passthrough is
/// kept (and tested) so an unmasked or future caller degrades to
/// [`BeltState::Other`] rather than lying.
pub(crate) fn belt_state(state_bits: u8) -> BeltState {
    match state_bits {
        STATE_STOPPED => BeltState::Standby,
        STATE_RUNNING => BeltState::Running,
        STATE_PAUSED => BeltState::Paused,
        STATE_COUNTDOWN => BeltState::Running,
        other => BeltState::Other(other),
    }
}

/// The duration field in seconds: firmware ≥20 reports ms, older firmware
/// seconds (azmke's rule; the three unconditional-÷1000 decoders agree on
/// all tested hardware, which reports firmware 27).
fn duration_seconds(raw: u32, fw_version: u8) -> u32 {
    if fw_version >= FIRMWARE_DURATION_MS_MIN {
        raw / 1000
    } else {
        raw
    }
}

/// A [`Status`] as a neutral SI sample. The wire is metric regardless of
/// the imperial-display flag (see the module docs), so this is pure
/// scaling; `host.display_unit` is irrelevant to this driver.
pub(crate) fn to_sample(s: &Status) -> Sample {
    Sample {
        speed_kmh: Some(s.speed_raw as f64 / 1000.0),
        distance_m: Some(s.distance_raw as f64),
        steps: Some(s.steps),
        duration_s: Some(duration_seconds(s.duration_raw, s.fw_version)),
        calories: Some(s.calories),
        state: Some(belt_state(s.flags & STATE_MASK)),
    }
}

// ---- The driver -------------------------------------------------------------

fn normalized(name: &str) -> String {
    name.trim().to_ascii_uppercase()
}

/// Does the advertised name identify a PitPat-family treadmill? The `-T`
/// keeps the PitPat bike (`PITPAT-S*`) out.
fn matches_name(name: &str) -> bool {
    let n = normalized(name);
    ADV_NAME_PREFIXES.iter().any(|pfx| n.starts_with(pfx))
}

pub struct PitPat;

#[async_trait]
impl Driver for PitPat {
    fn id(&self) -> &'static str {
        "pitpat"
    }

    fn matches(&self, adv: &Advertisement) -> bool {
        // A recognised name, or the distinctive FBA0 service. The other
        // three services prove nothing at scan time (0xFFF0 is the
        // contested block LifeSpan already lists, 0xFFFF and 0x1910 are
        // generic-looking vendor values).
        matches_name(&adv.name) || adv.services.contains(&PITPAT_SERVICE_UUID)
    }

    fn supports(&self, adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        // A recognised name plus any verified transport claims the device.
        // Nameless devices are accepted only on the FBA0 layout: that block
        // is distinctive to this protocol in every known source (the PitPat
        // bike sits on FBB0), and platforms sometimes drop the name at
        // connect time — the WiLink precedent. A nameless FFF0-swapped
        // (Deerrun-shaped) device stays unclaimed on purpose: 0xFFF0 is the
        // contested block, and mis-claiming there is the failure this
        // codebase is built to prevent.
        let Some(transport) = select_transport(gatt) else {
            return false;
        };
        matches_name(&adv.name)
            || (normalized(&adv.name).is_empty() && transport == Transport::PitPat)
    }

    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()> {
        let chars: BTreeSet<Characteristic> = link.characteristics();
        let transport = select_transport(&chars)
            .ok_or_else(|| anyhow!("no PitPat transport variant in the GATT table"))?;
        let notify_char = chars
            .iter()
            .find(|c| c.uuid == transport.notify_uuid())
            .cloned()
            .ok_or_else(|| anyhow!("notify characteristic missing"))?;

        // Subscribe first, then wake — push firmware streams unsolicited
        // and the first frame must not be missed.
        link.subscribe(&notify_char).await?;
        let mut notifications = link.notifications().await?;
        run_init_sequence(link, &init_steps(transport)).await?;
        let mut seq: u8 = 1; // the init query used sequence 0

        let mut dead_polls: u32 = 0;
        loop {
            match tokio::time::timeout(POLL_INTERVAL, notifications.next()).await {
                Ok(Some(n)) => {
                    dead_polls = 0;
                    host.record_frame(CMD_STATUS_QUERY, &n.value); // raw capture for /api/diag
                    match decode_notification(&n.value, transport.enveloped()) {
                        Ok(status) => emit(to_sample(&status)),
                        Err(e) => tracing::debug!("pitpat frame skipped: {e}"),
                    }
                }
                Ok(None) => return Err(anyhow!("notification stream ended")),
                Err(_) => {
                    // A quiet interval: push firmware never goes silent
                    // while alive, request/response firmware wants the
                    // query repeated. Nudge with the status query — still
                    // the only frame this driver ever writes.
                    if !link.is_connected().await.unwrap_or(false) {
                        return Err(anyhow!("link dropped; reconnecting"));
                    }
                    dead_polls += 1;
                    if dead_polls >= MAX_DEAD_POLLS {
                        return Err(anyhow!("link unresponsive; forcing reconnect"));
                    }
                    let frame = status_query_frame(transport, seq);
                    seq = seq.wrapping_add(1);
                    link.write_uuid(transport.write_uuid(), &frame, true)
                        .await?;
                }
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
    /// one logical frame — the status query — in its bare or enveloped
    /// transport encoding; the count is part of the assertion. The length
    /// byte is the verb family in this protocol: the 0x17-length actuation
    /// frames (start/stop/pause/set-speed), the 0x6B-prefixed unlock frame
    /// and the uncharacterised `6A 05 D7 D2 43` init must never appear.
    #[test]
    fn the_driver_only_ever_writes_the_status_query() {
        for transport in [Transport::PitPat, Transport::V1910] {
            let frames: Vec<Vec<u8>> = init_steps(transport)
                .iter()
                .map(|s| s.payload.clone())
                .collect();
            assert_eq!(frames, vec![hx("6a 05 fd f8 43")], "{transport:?}");
        }
        for transport in [Transport::SupeRun, Transport::Deerrun] {
            let frames: Vec<Vec<u8>> = init_steps(transport)
                .iter()
                .map(|s| s.payload.clone())
                .collect();
            assert_eq!(
                frames,
                vec![hx("4d 00 00 05 6a 05 fd f8 43")],
                "{transport:?}"
            );
        }

        // The poll loop's only frame is the identical query at a later
        // sequence number.
        assert_eq!(
            status_query_frame(Transport::PitPat, 7),
            hx("6a 05 fd f8 43")
        );
        assert_eq!(
            status_query_frame(Transport::Deerrun, 7),
            hx("4d 00 07 05 6a 05 fd f8 43")
        );

        // Pin the verb properties of everything we send: query family
        // (length 0x05), the status-query command, and never the actuation
        // length, on both the bare and the enveloped encoding.
        for transport in Transport::ALL {
            let wire = status_query_frame(transport, 0);
            let inner = if transport.enveloped() {
                &wire[ENVELOPE_LEN..]
            } else {
                &wire[..]
            };
            assert_eq!(inner[0], REQ_PREFIX, "{transport:?}");
            assert_eq!(inner[1], SHORT_FRAME_LEN, "query family in {transport:?}");
            assert_ne!(inner[1], 0x17, "actuation family in {transport:?}");
            assert_eq!(inner[2], CMD_STATUS_QUERY, "{transport:?}");
            assert_ne!(inner[0], 0x6B, "unlock family in {transport:?}");
        }
    }

    /// The single init write goes to the transport's write characteristic,
    /// immediately (no protocol delay is documented anywhere).
    #[tokio::test(start_paused = true)]
    async fn init_sequence_is_one_write_to_the_transport_write_char() {
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
        for (transport, want_uuid, want_frame) in [
            (Transport::PitPat, PITPAT_WRITE_UUID, hx("6a 05 fd f8 43")),
            (
                Transport::SupeRun,
                SUPERUN_WRITE_UUID,
                hx("4d 00 00 05 6a 05 fd f8 43"),
            ),
            (
                Transport::Deerrun,
                DEERRUN_WRITE_UUID,
                hx("4d 00 00 05 6a 05 fd f8 43"),
            ),
            (Transport::V1910, V1910_WRITE_UUID, hx("6a 05 fd f8 43")),
        ] {
            let link = MockLink::default();
            let start = Instant::now();
            run_init_sequence(&link, &init_steps(transport))
                .await
                .unwrap();
            let writes = link.writes.lock().unwrap().clone();
            assert_eq!(writes.len(), 1, "exactly one init write ({transport:?})");
            assert_eq!(writes[0].0, want_uuid, "{transport:?}");
            assert_eq!(writes[0].1, want_frame, "{transport:?}");
            assert_eq!(writes[0].2 - start, Duration::ZERO, "{transport:?}");
        }
    }

    // ---- Checksum vectors ----------------------------------------------------

    /// The XOR trailer rule (bytes 1..=len-3) must reproduce the trailer of
    /// every published frame of this protocol — the query we send, the two
    /// init frames we deliberately do NOT send, and (decode-direction
    /// vectors only, exactly like the Sperax driver's dropped 0x13) the
    /// actuation frames qdomyos builds, which pin the rule across the
    /// 23-byte family without any of those bytes ever leaving this file.
    #[test]
    fn checksum_reproduces_every_published_trailer() {
        for (raw, why) in [
            ("6a 05 fd f8 43", "the status query — the frame we send"),
            ("6b 05 9d 98 43", "qdomyos' unlock preamble — NEVER SENT"),
            (
                "6a 05 d7 d2 43",
                "qdomyos' uncharacterised init — NEVER SENT",
            ),
            (
                // qdomyos deerruntreadmill.cpp, the PitPat stop frame.
                "6a 17 00 00 00 00 00 00 05 00 8a 00 02 00 00 00 00 00 12 2e 0c aa 43",
                "an actuation-family frame — NEVER BUILT, vector only",
            ),
            (
                // qdomyos deerruntreadmill.cpp, the PitPat "start" init frame.
                "6a 17 00 00 00 00 00 00 00 05 00 81 00 00 00 00 00 00 00 00 00 00 00 93 43",
                "an actuation-family frame — NEVER BUILT, vector only",
            ),
        ] {
            let frame = hx(raw);
            assert_eq!(frame_checksum(&frame), frame[frame.len() - 2], "{why}");
        }
        assert_eq!(STATUS_QUERY.to_vec(), hx("6a 05 fd f8 43"));
    }

    // ---- The real captured frame ---------------------------------------------
    //
    // The only public real capture: azmke/pitpat-treadmill-control's example
    // payload (treadmill_data.py) — a 52-byte idle frame from real hardware.
    // Firmware 27, max speed 6000 (the pads' 6.0 km/h ceiling), a serial
    // number in the extended tail, all counters zero, state = stopped.

    const IDLE_CAPTURE: &str = "68 34 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
         00 00 2a 1b 00 17 70 00 05 00 74 6c 4b 61 31 39 31 66 55 70 54 73 73 36 30 33 0e 00 40 43";

    #[test]
    fn decodes_the_real_captured_idle_frame() {
        let frame = hx(IDLE_CAPTURE);
        assert_eq!(frame.len(), 52);
        assert_eq!(
            frame[1] as usize,
            frame.len(),
            "length byte == frame length"
        );
        // The inbound checksum rule verifies on the real frame — this is
        // the vector the whole inbound-validation decision rests on.
        assert_eq!(frame_checksum(&frame), 0x40);

        let s = parse_status(&frame).unwrap();
        assert_eq!(s.speed_raw, 0);
        assert_eq!(s.distance_raw, 0);
        assert_eq!(s.steps, 0);
        assert_eq!(s.calories, 0);
        assert_eq!(s.duration_raw, 0);
        assert_eq!(s.fw_version, 27);
        assert_eq!(s.flags, 0x00);

        let sample = to_sample(&s);
        assert_eq!(sample.state, Some(BeltState::Standby));
        // An idle pad genuinely reports zero counters — they are reported
        // values, not absences.
        assert_eq!(sample.steps, Some(0));
        assert_eq!(sample.speed_kmh, Some(0.0));
    }

    // ---- Synthetic fixtures --------------------------------------------------
    //
    // No public capture shows a running belt, so these frames are built to
    // the field map all four upstreams agree on (offsets, big-endian, XOR
    // trailer) to pin OUR reader against THAT map. They are labelled
    // synthetic accordingly.

    /// Encode a [`Status`] back into a wire frame (the exact inverse of
    /// `parse_status`'s field map), padded to `len` bytes.
    fn build_frame(s: &Status, len: usize) -> Vec<u8> {
        let mut f = vec![0u8; len];
        f[0] = 0x68;
        f[1] = len as u8;
        f[3..5].copy_from_slice(&(s.speed_raw as u16).to_be_bytes());
        f[7..11].copy_from_slice(&s.distance_raw.to_be_bytes());
        f[14..18].copy_from_slice(&s.steps.to_be_bytes());
        f[18..20].copy_from_slice(&(s.calories as u16).to_be_bytes());
        f[20..24].copy_from_slice(&s.duration_raw.to_be_bytes());
        f[25] = s.fw_version;
        f[26] = s.flags;
        f[len - 1] = TERMINATOR;
        f[len - 2] = frame_checksum(&f);
        f
    }

    /// 2.5 km/h, 1234 m, 2211 steps, 87 kcal, 30:05 elapsed, running.
    fn running_frame() -> Vec<u8> {
        build_frame(
            &Status {
                speed_raw: 2500,
                distance_raw: 1234,
                steps: 2211,
                calories: 87,
                duration_raw: 1_805_000,
                fw_version: 27,
                flags: STATE_RUNNING | 0x01,
            },
            52,
        )
    }

    #[test]
    fn decodes_a_running_frame() {
        let s = parse_status(&running_frame()).unwrap();
        assert_eq!(s.speed_raw, 2500);
        assert_eq!(s.distance_raw, 1234);
        assert_eq!(s.steps, 2211);
        assert_eq!(s.calories, 87);
        assert_eq!(s.duration_raw, 1_805_000);
        assert_eq!(s.flags & STATE_MASK, STATE_RUNNING);
    }

    /// Counters exercise the full big-endian width — a wrong endianness or
    /// width would silently corrupt step data.
    #[test]
    fn wide_counters_are_big_endian_across_the_full_width() {
        let wide = Status {
            speed_raw: 0x1234,
            distance_raw: 0x0102_0304,
            steps: 0x0A0B_0C0D,
            calories: 0x2345,
            duration_raw: 0x0011_2233,
            fw_version: 27,
            flags: 0,
        };
        // Encode → decode round-trips the full width.
        assert_eq!(parse_status(&build_frame(&wide, 31)).unwrap(), wide);
    }

    /// A 31-byte frame — the minimum every upstream accepts — parses; one
    /// byte less does not.
    #[test]
    fn the_31_byte_minimum_is_honoured() {
        let f = build_frame(
            &Status {
                speed_raw: 1500,
                distance_raw: 100,
                steps: 150,
                calories: 5,
                duration_raw: 60_000,
                fw_version: 27,
                flags: STATE_RUNNING,
            },
            31,
        );
        assert!(parse_status(&f).is_ok());
        assert_eq!(
            parse_status(&f[..30]),
            Err(ProtocolError::BadLength(30)),
            "30 bytes — one short of a status frame"
        );
    }

    // ---- The transport envelope ----------------------------------------------

    /// On the enveloped transports a notification carries 4 header bytes
    /// before the frame (azmke strips exactly 4); the decoder must recover
    /// the same fields either way — and must still decode a *bare* frame
    /// arriving on a transport believed to envelope (and vice versa),
    /// because the envelope claim for Deerrun rests on qdomyos alone.
    #[test]
    fn enveloped_notifications_decode_to_the_same_status() {
        let bare = running_frame();
        let enveloped = wrap_envelope(0x07, &bare);
        assert_eq!(enveloped[..4], [0x4D, 0x00, 0x07, 52]);

        let want = parse_status(&bare).unwrap();
        assert_eq!(decode_notification(&enveloped, true).unwrap(), want);
        assert_eq!(
            decode_notification(&bare, true).unwrap(),
            want,
            "bare fallback"
        );
        assert_eq!(decode_notification(&bare, false).unwrap(), want);
        assert_eq!(
            decode_notification(&enveloped, false).unwrap(),
            want,
            "envelope fallback"
        );
        // Garbage fails under both interpretations, with the preferred
        // interpretation's error reported.
        assert!(decode_notification(&[0u8; 40], true).is_err());
        assert!(decode_notification(&[0u8; 10], false).is_err());
    }

    // ---- The imperial-display flag -------------------------------------------

    /// Flags bit 7 means the *panel* displays miles; the wire stays metric
    /// (sirfergy's tested finding — no upstream rescales, and neither may
    /// we). The same frame with and without the bit must decode to
    /// identical SI values, and the bit must not bleed into the state bits.
    #[test]
    fn the_imperial_flag_changes_nothing_but_the_flag() {
        let fields = Status {
            speed_raw: 2500,
            distance_raw: 1234,
            steps: 2211,
            calories: 87,
            duration_raw: 1_805_000,
            fw_version: 27,
            flags: STATE_RUNNING,
        };
        let metric = build_frame(&fields, 52);
        let imperial = build_frame(
            &Status {
                flags: STATE_RUNNING | IMPERIAL_DISPLAY_FLAG,
                ..fields.clone()
            },
            52,
        );
        let m = parse_status(&metric).unwrap();
        let i = parse_status(&imperial).unwrap();
        assert_eq!(i.speed_raw, m.speed_raw);
        assert_eq!(i.distance_raw, m.distance_raw);
        assert_eq!(i.steps, m.steps);
        assert_eq!(i.flags & STATE_MASK, STATE_RUNNING);

        let sm = to_sample(&m);
        let si = to_sample(&i);
        assert_eq!(si.speed_kmh, sm.speed_kmh, "no mph rescaling");
        assert_eq!(si.distance_m, sm.distance_m, "no mile rescaling");
        assert_eq!(si.state, Some(BeltState::Running));

        // And through Telemetry on an mph console the *presentation*
        // converts — proof the driver reported SI and nothing else.
        let t = Telemetry::from_sample(&si, "mph");
        assert_eq!(t.speed_raw, Some(155), "2.5 km/h ≈ 1.55 mph in centi-units");
    }

    // ---- Belt state ----------------------------------------------------------

    #[test]
    fn belt_states_map_and_unknowns_pass_through() {
        assert_eq!(belt_state(STATE_STOPPED), BeltState::Standby);
        assert_eq!(belt_state(STATE_RUNNING), BeltState::Running);
        assert_eq!(belt_state(STATE_PAUSED), BeltState::Paused);
        assert_eq!(
            belt_state(STATE_COUNTDOWN),
            BeltState::Running,
            "3-2-1 countdown: the workout is starting (the Urevo call)"
        );
        for v in [0x01u8, 0x09, 0x28, 0x7f, 0xff] {
            assert_eq!(belt_state(v), BeltState::Other(v), "byte 0x{v:02x}");
        }
    }

    /// The full state × noise-bit matrix: wifi/bracelet/imperial bits must
    /// never change which belt state comes out.
    #[test]
    fn state_bits_are_isolated_from_the_other_flag_bits() {
        for (bits, want) in [
            (STATE_STOPPED, BeltState::Standby),
            (STATE_RUNNING, BeltState::Running),
            (STATE_PAUSED, BeltState::Paused),
            (STATE_COUNTDOWN, BeltState::Running),
        ] {
            for noise in [0x00u8, 0x01, 0x07, 0x80, 0xE7] {
                let f = build_frame(
                    &Status {
                        fw_version: 27,
                        flags: bits | noise,
                        ..Status::default()
                    },
                    31,
                );
                let s = parse_status(&f).unwrap();
                assert_eq!(
                    to_sample(&s).state,
                    Some(want),
                    "state 0x{bits:02x} noise 0x{noise:02x}"
                );
            }
        }
    }

    // ---- Duration units ------------------------------------------------------

    /// Firmware ≥20 reports milliseconds, older firmware seconds — azmke's
    /// vendor-app rule, self-contained because the firmware byte rides in
    /// the same frame. Both sides of the boundary are pinned.
    #[test]
    fn duration_unit_follows_the_firmware_version() {
        // Modern firmware (the capture reports 27): milliseconds.
        let s = parse_status(&running_frame()).unwrap();
        assert_eq!(duration_seconds(s.duration_raw, s.fw_version), 1805);
        assert_eq!(to_sample(&s).duration_s, Some(1805));

        // Old firmware: the same raw number IS seconds.
        let old = parse_status(&build_frame(
            &Status {
                speed_raw: 2000,
                duration_raw: 601,
                fw_version: 12,
                flags: STATE_RUNNING,
                ..Status::default()
            },
            31,
        ))
        .unwrap();
        assert_eq!(to_sample(&old).duration_s, Some(601));

        // The boundary, both sides.
        assert_eq!(duration_seconds(5000, 19), 5000, "fw 19 → seconds");
        assert_eq!(duration_seconds(5000, 20), 5, "fw 20 → ms");
    }

    // ---- Malformed input -----------------------------------------------------

    #[test]
    fn malformed_frames_error_without_panicking() {
        assert_eq!(parse_status(&[]), Err(ProtocolError::BadLength(0)));
        assert_eq!(
            parse_status(&hx("6a 05 fd f8 43")),
            Err(ProtocolError::BadLength(5)),
            "our own query frame is not a status frame"
        );
        // Right length, missing terminator.
        let mut no_term = running_frame();
        let n = no_term.len();
        no_term[n - 1] = 0x00;
        assert_eq!(parse_status(&no_term), Err(ProtocolError::BadTerminator));
        // Truncated mid-frame: the terminator lands elsewhere.
        assert!(parse_status(&running_frame()[..40]).is_err());
    }

    /// A single corrupted counter byte must be rejected, not parsed into
    /// someone's step history — and fixing the trailer must make the same
    /// bytes parse again (proof the rejection was the checksum's).
    #[test]
    fn corruption_is_rejected_and_a_fixed_trailer_parses_again() {
        let mut corrupt = running_frame();
        corrupt[17] ^= 0x01; // flip one steps byte: 2211 → 2210
        assert!(matches!(
            parse_status(&corrupt),
            Err(ProtocolError::BadChecksum { .. })
        ));
        let n = corrupt.len();
        corrupt[n - 2] = frame_checksum(&corrupt);
        assert_eq!(parse_status(&corrupt).unwrap().steps, 2210);
    }

    // ---- Sample / Telemetry golden pins --------------------------------------

    /// Fixture frame → Sample → Telemetry: metric scaling and the
    /// presentation re-encoding, pinned end to end.
    #[test]
    fn golden_fixture_to_telemetry() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let s = parse_status(&running_frame()).unwrap();
        let sample = to_sample(&s);
        assert!(approx(sample.speed_kmh.unwrap(), 2.5));
        assert!(approx(sample.distance_m.unwrap(), 1234.0));
        assert_eq!(sample.steps, Some(2211));
        assert_eq!(sample.duration_s, Some(1805));
        assert_eq!(sample.calories, Some(87));
        assert_eq!(sample.state, Some(BeltState::Running));

        let t = Telemetry::from_sample(&sample, "km/h");
        assert_eq!(t.speed_raw, Some(250), "2.50 km/h in centi-units");
        assert_eq!(t.distance_raw, Some(123), "1234 m → 123 decameters");
        assert_eq!(t.steps, Some(2211));
        assert_eq!(t.duration_s, Some(1805));
        assert_eq!(t.calories, Some(87));
        assert_eq!(t.status_name.as_deref(), Some("RUNNING"));
        assert!(t.is_running);
    }

    /// A paused pad presents as the contract's PAUSED code; a stopped one
    /// as STANDBY.
    #[test]
    fn paused_and_stopped_present_as_the_contract_codes() {
        let paused = parse_status(&build_frame(
            &Status {
                distance_raw: 1234,
                steps: 2211,
                calories: 87,
                duration_raw: 1_805_000,
                fw_version: 27,
                flags: STATE_PAUSED,
                ..Status::default()
            },
            52,
        ))
        .unwrap();
        let t = Telemetry::from_sample(&to_sample(&paused), "km/h");
        assert_eq!(t.status, Some(0x05));
        assert_eq!(t.status_name.as_deref(), Some("PAUSED"));
        assert!(!t.is_running);

        let stopped = parse_status(&hx(IDLE_CAPTURE)).unwrap();
        let t = Telemetry::from_sample(&to_sample(&stopped), "km/h");
        assert_eq!(t.status, Some(0x01));
        assert_eq!(t.status_name.as_deref(), Some("STANDBY"));
        assert!(!t.is_running);
    }

    // ---- Transport selection -------------------------------------------------

    use btleplug::api::CharPropFlags;

    const N: CharPropFlags = CharPropFlags::NOTIFY;
    const W: CharPropFlags = CharPropFlags::WRITE;

    fn gatt(chars: &[(Uuid, CharPropFlags)]) -> BTreeSet<Characteristic> {
        chars
            .iter()
            .map(|(uuid, properties)| Characteristic {
                uuid: *uuid,
                service_uuid: PITPAT_SERVICE_UUID,
                properties: *properties,
                descriptors: BTreeSet::new(),
            })
            .collect()
    }

    fn pitpat_shape() -> Vec<(Uuid, CharPropFlags)> {
        vec![(PITPAT_WRITE_UUID, W), (PITPAT_NOTIFY_UUID, N)]
    }
    fn superun_shape() -> Vec<(Uuid, CharPropFlags)> {
        vec![(SUPERUN_WRITE_UUID, W), (SUPERUN_NOTIFY_UUID, N)]
    }
    /// The Deerrun shape: LifeSpan's UUIDs with the roles SWAPPED — write
    /// on FFF1, notify on FFF2.
    fn deerrun_shape() -> Vec<(Uuid, CharPropFlags)> {
        vec![
            (DEERRUN_WRITE_UUID, CharPropFlags::WRITE_WITHOUT_RESPONSE),
            (DEERRUN_NOTIFY_UUID, N),
        ]
    }
    fn v1910_shape() -> Vec<(Uuid, CharPropFlags)> {
        vec![(V1910_WRITE_UUID, W), (V1910_NOTIFY_UUID, N)]
    }

    #[test]
    fn each_variant_layout_selects_its_transport() {
        assert_eq!(
            select_transport(&gatt(&pitpat_shape())),
            Some(Transport::PitPat)
        );
        assert_eq!(
            select_transport(&gatt(&superun_shape())),
            Some(Transport::SupeRun)
        );
        assert_eq!(
            select_transport(&gatt(&deerrun_shape())),
            Some(Transport::Deerrun)
        );
        assert_eq!(
            select_transport(&gatt(&v1910_shape())),
            Some(Transport::V1910)
        );
        assert_eq!(select_transport(&gatt(&[])), None);

        // Only the enveloped transports envelope.
        assert!(!Transport::PitPat.enveloped());
        assert!(Transport::SupeRun.enveloped());
        assert!(Transport::Deerrun.enveloped());
        assert!(!Transport::V1910.enveloped());
    }

    /// The pacekeeper hardware exposes 1910 AND FBA0 — the verified FBA0
    /// must win. And roles are verified, not just UUIDs: a LifeSpan-shaped
    /// FFF0 table (notify FFF1, write FFF2 — the opposite arrangement) must
    /// select NOTHING here, or this driver would claim LifeSpan consoles.
    #[test]
    fn probe_priority_and_role_verification() {
        let both: Vec<_> = v1910_shape().into_iter().chain(pitpat_shape()).collect();
        assert_eq!(select_transport(&gatt(&both)), Some(Transport::PitPat));

        // LifeSpan roles on the FFF0 UUIDs: refused.
        let lifespan_shaped = gatt(&[(DEERRUN_WRITE_UUID, N), (DEERRUN_NOTIFY_UUID, W)]);
        assert_eq!(select_transport(&lifespan_shaped), None);

        // Half a table: refused.
        assert_eq!(select_transport(&gatt(&[(PITPAT_NOTIFY_UUID, N)])), None);
        // UUIDs present but no properties: refused.
        assert_eq!(
            select_transport(&gatt(&[
                (PITPAT_WRITE_UUID, CharPropFlags::default()),
                (PITPAT_NOTIFY_UUID, CharPropFlags::default()),
            ])),
            None
        );
    }

    // ---- Name matching -------------------------------------------------------

    fn adv(name: &str) -> Advertisement {
        Advertisement {
            name: name.into(),
            services: vec![],
        }
    }

    #[test]
    fn only_pitpat_treadmill_names_match() {
        for name in ["PitPat-T01", "PITPAT-T01", "pitpat-t01", " PitPat-T02X "] {
            assert!(PitPat.matches(&adv(name)), "{name}");
        }
        for name in [
            "PITPAT-S1", // the PitPat BIKE (qdomyos routes it to a bike driver)
            "PITPAT",    // underspecified — could be the bike
            "SUPERUN",   // retail brand; no evidence it is ever advertised
            "BA06-B1",   // retail model number; the device advertises PitPat-T01
            "DEERRUN",   // retail brand; no evidence it is ever advertised
            "LifeSpan-TM",
            "URTM041",
            "WalkingPad A1",
            "",
        ] {
            assert!(!PitPat.matches(&adv(name)), "{name}");
        }
        // The distinctive FBA0 service also surfaces the device in scans.
        assert!(PitPat.matches(&Advertisement {
            name: String::new(),
            services: vec![PITPAT_SERVICE_UUID],
        }));
    }

    // ---- supports(): names × transports --------------------------------------

    #[test]
    fn supports_needs_a_name_or_the_distinctive_fba0_layout() {
        // A recognised name claims every transport variant.
        for shape in [
            pitpat_shape(),
            superun_shape(),
            deerrun_shape(),
            v1910_shape(),
        ] {
            assert!(PitPat.supports(&adv("PitPat-T01"), &gatt(&shape)));
        }
        // Nameless: only FBA0 — distinctive to this protocol — is claimed
        // (the WiLink don't-strand-a-paired-pad precedent). The contested
        // or generic layouts are refused without a name.
        assert!(PitPat.supports(&adv(""), &gatt(&pitpat_shape())));
        assert!(!PitPat.supports(&adv(""), &gatt(&superun_shape())));
        assert!(!PitPat.supports(&adv(""), &gatt(&deerrun_shape())));
        assert!(!PitPat.supports(&adv(""), &gatt(&v1910_shape())));
        // A foreign or carved-out name: refused on every shape.
        for name in ["PITPAT-S1", "LifeSpan-TM", "Mystery Pad 3000"] {
            assert!(
                !PitPat.supports(&adv(name), &gatt(&pitpat_shape())),
                "{name}"
            );
            assert!(
                !PitPat.supports(&adv(name), &gatt(&deerrun_shape())),
                "{name}"
            );
        }
        // The right name with no recognisable transport: refused.
        assert!(!PitPat.supports(&adv("PitPat-T01"), &gatt(&[])));
        // The right name with LifeSpan-shaped FFF0 roles: refused — that
        // table is not this protocol, whatever the name says.
        assert!(!PitPat.supports(
            &adv("PitPat-T01"),
            &gatt(&[(DEERRUN_WRITE_UUID, N), (DEERRUN_NOTIFY_UUID, W)])
        ));
    }
}
