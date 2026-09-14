//! Sperax driver — the proprietary `F5 … FA` protocol on service 0xFFF0
//! (`SPERAX_RM01`, the hyphen-less model, and `SPERAX_RM-02`).
//!
//! Protocol knowledge was ported from **cagnulein/qdomyos-zwift** (GPL-3.0,
//! Roberto Viola) — `src/devices/speraxtreadmill/speraxtreadmill.cpp`, the
//! only known implementation, whose byte-verbatim captured frames ("Frame
//! from pkt6905" etc.) are this module's checksum and framing vectors — and
//! cross-checked against **blak3r/treadspan** (MIT) —
//! `protocol-analysis/sperax-rm-01/`, whose service dump and app capture
//! establish which Sperax hardware does NOT speak this protocol. See
//! THIRD-PARTY-NOTICES.md.
//!
//! ⚠️ **Two hardware revisions, distinguished only by a hyphen.** Verified in
//! qdomyos-zwift's device matcher (`src/devices/bluetooth.cpp`):
//! `SPERAX_RM01` (no hyphen) and `SPERAX_RM-02` are routed to this
//! proprietary protocol; `SPERAX_RM-01` (with hyphen) is routed to FTMS —
//! our FTMS driver pins it by name, and treadspan's RM-01 capture confirms
//! it (the vendor app on an RM-01 talks FTMS + a vendor extension, never the
//! `F5 … FA` frames). Real advertised names carry a suffix
//! (`SPERAX_RM-01_74FE70` in the capture), so matching is by prefix. Tests
//! below pin both directions of the split.
//!
//! And 0xFFF0/FFF1/FFF2 is the same contested vendor block LifeSpan and
//! Urevo sit on, so `supports()` requires the recognised name AND the
//! characteristic roles; nameless devices are left to the LifeSpan fallback.
//!
//! Interaction model: **init handshake, then poll.** One handshake frame
//! (sent twice, as upstream does), then the status query on a timer; the
//! treadmill answers with ≥24-byte packets on FFF1.
//!
//! ## Wire format (derived here from qdomyos' captured frames — upstream
//! sends them verbatim and never documents the structure)
//!
//! A logical frame is
//! `F5 <len> 00 <cmd> <payload…> <crc-lo> <crc-hi> FA`, where `len` is the
//! logical frame length and the CRC-16 (poly 0x8408-family, reflected,
//! LSB-first with polynomial 0xA327, init 0xFFFF, no final XOR) covers
//! everything before the CRC itself. On the wire, interior bytes in
//! `0xF0..=0xFF` are escaped as `F0 <low nibble>` so the `F5`/`FA` framing
//! bytes stay unambiguous, and the length byte is patched to the escaped
//! (wire) length. Both rules were recovered from and verify against all 64
//! captured frames in the upstream source (`build_frame` reproduces every
//! one we use byte-identically, plus a third captured frame we deliberately
//! do not send — see the tests).
//!
//! ## What we send, and why only that
//!
//! qdomyos' command byte 0x15 is the actuation family (speed / start /
//! stop); nothing this driver builds may carry it — a test pins the exact
//! write set. Of qdomyos' three-frame init:
//!
//! * `F5 07 00 01 26 D8 FA` (cmd 0x01, **no payload**) — sent, twice, as
//!   upstream does. An empty-payload frame carries no value to set; it is a
//!   hello/wake for the telemetry stream, the legitimate init-handshake kind.
//! * `F5 09 00 13 01 00 89 B8 FA` (cmd 0x13, **payload `01 00`**) —
//!   DELIBERATELY NOT SENT. That payload byte can be a setting, nobody has
//!   documented what it does, and Trot only writes frames it can prove are
//!   reads. If real hardware turns out not to stream without it, reopen this
//!   with a capture showing what the frame actually does.
//! * `F5 08 00 19 F0 0A 59 FA` (cmd 0x19) — the recurring status query
//!   (qdomyos' "noop", sent on every poll tick), the frame the data packets
//!   answer. This is the poll.
//!
//! ## What the packets carry
//!
//! qdomyos parses exactly two fields from the ≥24-byte packets, and so do
//! we: **steps** as u16 big-endian at bytes 15..17 (fixed from the start),
//! and **speed** in 0.1 km/h at byte `len-7` (fixed from the END — escapes
//! near the tail shift absolute positions, which is presumably why upstream
//! anchors it there). **Distance is not in the packet.** qdomyos fabricates
//! it by integrating speed over wall-clock time; Trot does not — a derived
//! value presented as a measurement is exactly what `Sample`'s `Option`
//! fields exist to avoid — so `distance_m` stays `None`, as do duration and
//! calories. No status field is known either; the belt state is derived from
//! speed, as the FTMS driver does.
//!
//! No inbound capture of a data packet is public (treadspan's Sperax capture
//! is the FTMS-speaking RM-01), so inbound parsing follows upstream's
//! hardware-verified reader exactly: envelope checks (F5 prefix, FA
//! terminator, minimum length, and the length byte agreeing with the wire
//! length — the module's own framing rule, which also rejects the
//! settings/info replies the device sends on the same characteristic) but
//! **no inbound CRC enforcement** — we cannot verify where the trailer sits
//! on frames nobody has published, and rejecting every real frame on a
//! wrong guess would be worse than upstream's parity.
//!
//! One known inconsistency, kept deliberately: the header above documents
//! that interior bytes `0xF0..=0xFF` are escaped on the wire, yet inbound
//! parsing reads RAW WIRE offsets — an escape occurring before byte 15
//! would shift the step field and misread it. That is upstream's
//! hardware-verified reader taken verbatim, and it stays that way as
//! upstream parity until a real inbound capture shows whether data packets
//! are escaped at all; decoding through `unescape_frame` instead would
//! diverge from the only tested implementation on hardware nobody owns.

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

// ---- UUIDs (service 0xFFF0 — the contested block) ---------------------------
pub const SERVICE_UUID: Uuid = super::sig_uuid(0xfff0);
pub const NOTIFY_CHAR_UUID: Uuid = super::sig_uuid(0xfff1);
pub const WRITE_CHAR_UUID: Uuid = super::sig_uuid(0xfff2);

// ---- Advertised names -------------------------------------------------------

/// Name prefixes of the proprietary-protocol Sperax pads, from qdomyos'
/// matcher. `SPERAX_RM-01` (hyphen) is deliberately NOT here — it speaks
/// FTMS and the FTMS driver's name list pins it.
pub const ADV_NAME_PREFIXES: &[&str] = &["SPERAX_RM01", "SPERAX_RM-02"];

// ---- Wire constants ---------------------------------------------------------

pub const FRAME_START: u8 = 0xF5;
pub const FRAME_END: u8 = 0xFA;
/// Escape marker: `F0 <n>` on the wire encodes the logical byte `0xF0 | n`.
pub const ESCAPE: u8 = 0xF0;

/// The empty-payload handshake command.
pub const CMD_HELLO: u8 = 0x01;
/// The recurring status query (qdomyos' "noop").
pub const CMD_STATUS_QUERY: u8 = 0x19;
// qdomyos' cmd 0x13 init frame (payload `01 00`) is DELIBERATELY ABSENT: an
// undocumented write whose payload can be a setting has no place in an
// observe-only driver. Its captured bytes survive only as a CRC vector in
// the tests. Cmd 0x15 is the actuation family and is likewise never built.

/// CRC-16: reflected, LSB-first polynomial 0xA327, init 0xFFFF, no final
/// XOR, stored little-endian. Recovered by brute force from the 64 captured
/// frames in the upstream source; every one verifies.
pub const CRC_POLY_REFLECTED: u16 = 0xA327;
pub const CRC_INIT: u16 = 0xFFFF;

/// Data packets are at least 24 bytes (qdomyos discards shorter ones).
pub const STATUS_FRAME_MIN_LEN: usize = 24;
/// Steps: u16 big-endian at this fixed offset from the frame start.
pub const STEPS_OFFSET: usize = 15;
/// Speed: one byte, 0.1 km/h, at this fixed offset from the frame END
/// (qdomyos' `17 + (len - 24)`).
pub const SPEED_OFFSET_FROM_END: usize = 7;

/// Poll cadence. qdomyos polls at its 200 ms default; there is no evidence
/// of a WiLink-style minimum-spacing requirement, but nothing needs speed
/// updates faster than twice a second either, so we poll at half the rate
/// and pay half the radio chatter.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive unanswered polls before the link is declared dead — same
/// rationale as the LifeSpan and WiLink drivers: macOS can hold a stale
/// handle open with no disconnect event.
const MAX_DEAD_POLLS: u32 = 15;

/// A belt reporting 0.0 km/h is stopped; anything else is running. The wire
/// has 0.1 km/h resolution, so the threshold is exact.
const RUNNING_THRESHOLD_RAW: u8 = 0;

// ---- CRC, escaping, frame building ------------------------------------------

/// CRC-16 over `data` (see [`CRC_POLY_REFLECTED`]).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = CRC_INIT;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC_POLY_REFLECTED
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Escape the interior of a logical frame for the wire: every byte in
/// `0xF0..=0xFF` between the framing bytes becomes `F0 <low nibble>`, and
/// the length byte is patched to the wire length. The first and last bytes
/// (`F5` / `FA`) pass through untouched.
pub fn escape_frame(logical: &[u8]) -> Vec<u8> {
    let mut wire = Vec::with_capacity(logical.len() + 2);
    wire.push(logical[0]);
    for &b in &logical[1..logical.len() - 1] {
        if b >= 0xF0 {
            wire.push(ESCAPE);
            wire.push(b & 0x0F);
        } else {
            wire.push(b);
        }
    }
    wire.push(logical[logical.len() - 1]);
    wire[1] = wire.len() as u8;
    wire
}

/// Undo [`escape_frame`]: `F0 <n>` in the interior becomes `0xF0 | n`.
/// Diagnostic/test helper; inbound parsing reads wire offsets as upstream
/// does.
pub fn unescape_frame(wire: &[u8]) -> Vec<u8> {
    let mut logical = Vec::with_capacity(wire.len());
    let mut i = 0;
    while i < wire.len() {
        if wire[i] == ESCAPE && i + 1 < wire.len() && i > 0 && i < wire.len() - 1 {
            logical.push(0xF0 | (wire[i + 1] & 0x0F));
            i += 2;
        } else {
            logical.push(wire[i]);
            i += 1;
        }
    }
    if logical.len() > 1 {
        logical[1] = logical.len() as u8;
    }
    logical
}

/// Build the wire form of `F5 <len> 00 <cmd> <payload…> <crc LE> FA`.
///
/// The CRC is computed over the logical frame (with the logical length
/// byte); escaping and the wire-length patch happen after — the order the
/// captured frames prove (frames whose CRC bytes needed escaping carry the
/// escaped length on the wire but verify against the logical one).
pub fn build_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let logical_len = 4 + payload.len() + 3;
    let mut logical = Vec::with_capacity(logical_len);
    logical.push(FRAME_START);
    logical.push(logical_len as u8);
    logical.push(0x00);
    logical.push(cmd);
    logical.extend_from_slice(payload);
    let crc = crc16(&logical);
    logical.extend_from_slice(&crc.to_le_bytes());
    logical.push(FRAME_END);
    escape_frame(&logical)
}

/// The init handshake: the hello frame twice, exactly as upstream sends it,
/// with a settle delay between writes (upstream waits on each write's
/// completion signal, up to 300 ms; we space conservatively). Writes are
/// unacknowledged because that is the write type upstream uses on this
/// hardware.
pub fn init_steps() -> Vec<InitStep> {
    let hello = build_frame(CMD_HELLO, &[]);
    vec![
        InitStep::write(WRITE_CHAR_UUID, hello.clone())
            .without_response()
            .then_wait_ms(150),
        InitStep::write(WRITE_CHAR_UUID, hello)
            .without_response()
            .then_wait_ms(150),
    ]
}

// ---- Status-frame parsing ---------------------------------------------------

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("expected at least {STATUS_FRAME_MIN_LEN} bytes, got {0}")]
    BadLength(usize),
    #[error("bad prefix 0x{0:02x}")]
    BadPrefix(u8),
    /// The frame's own length byte disagrees with the wire length. Every
    /// frame in this protocol carries its wire length at byte 1 (the
    /// framing rule `escape_frame` implements), so a mismatch means this is
    /// not a well-formed single frame — a settings/info reply of another
    /// length, or several frames coalesced into one notification. Without
    /// this check such replies decode into "steps" and "speed" and get
    /// emitted as telemetry.
    #[error("length byte says {declared}, wire carries {actual} bytes")]
    BadDeclaredLength { declared: u8, actual: usize },
    #[error("missing 0xFA terminator")]
    BadTerminator,
}

/// The two fields a data packet verifiably carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Cumulative steps.
    pub steps: u32,
    /// Belt speed in 0.1 km/h.
    pub speed_raw: u8,
}

/// Parse a notification into a [`Status`]. Pure function of the bytes; never
/// panics on malformed input. Field offsets are upstream's exactly (wire
/// bytes, steps start-anchored, speed end-anchored) — see the module docs
/// for why no more than the envelope is validated.
pub fn parse_status(frame: &[u8]) -> Result<Status, ProtocolError> {
    if frame.len() < STATUS_FRAME_MIN_LEN {
        return Err(ProtocolError::BadLength(frame.len()));
    }
    if frame[0] != FRAME_START {
        return Err(ProtocolError::BadPrefix(frame[0]));
    }
    // The length byte must agree with the wire length (see the error's doc
    // — this is what keeps the device's settings/info replies, and coalesced
    // notifications, from decoding into telemetry).
    if frame[1] as usize != frame.len() {
        return Err(ProtocolError::BadDeclaredLength {
            declared: frame[1],
            actual: frame.len(),
        });
    }
    if frame[frame.len() - 1] != FRAME_END {
        return Err(ProtocolError::BadTerminator);
    }
    let steps = ((frame[STEPS_OFFSET] as u32) << 8) | frame[STEPS_OFFSET + 1] as u32;
    let speed_raw = frame[frame.len() - SPEED_OFFSET_FROM_END];
    Ok(Status { steps, speed_raw })
}

/// A [`Status`] as a neutral SI sample. The packet carries no distance, no
/// duration, no energy and no status byte: those stay `None` — absent, not
/// zero, and never derived (upstream integrates distance from speed over
/// time; we refuse to present a derivation as a measurement). State comes
/// from the speed, as in the FTMS driver.
pub(crate) fn to_sample(s: &Status) -> Sample {
    Sample {
        speed_kmh: Some(s.speed_raw as f64 / 10.0),
        distance_m: None,
        steps: Some(s.steps),
        duration_s: None,
        calories: None,
        state: Some(if s.speed_raw > RUNNING_THRESHOLD_RAW {
            BeltState::Running
        } else {
            BeltState::Standby
        }),
    }
}

// ---- The driver -------------------------------------------------------------

fn normalized(name: &str) -> String {
    name.trim().to_ascii_uppercase()
}

/// Does the advertised name identify a proprietary-protocol Sperax pad?
/// Prefix comparison keeps the per-unit suffix (`SPERAX_RM01_xxxxxx`)
/// matching, and cannot cross the hyphen split: `SPERAX_RM-01…` starts with
/// neither prefix.
fn matches_name(name: &str) -> bool {
    let n = normalized(name);
    ADV_NAME_PREFIXES.iter().any(|pfx| n.starts_with(pfx))
}

/// Notify on FFF1, write on FFF2 — the LifeSpan-shaped roles, which is why
/// the name gate is mandatory.
fn gatt_shape_matches(gatt: &BTreeSet<Characteristic>) -> bool {
    super::util::has_notify(gatt, NOTIFY_CHAR_UUID) && super::util::has_write(gatt, WRITE_CHAR_UUID)
}

pub struct Sperax;

#[async_trait]
impl Driver for Sperax {
    fn id(&self) -> &'static str {
        "sperax"
    }

    fn matches(&self, adv: &Advertisement) -> bool {
        // Name only: the 0xFFF0 service proves nothing (and LifeSpan's
        // scan-time matcher already lists devices advertising it), while the
        // FTMS RM-01 must keep surfacing via the FTMS driver.
        matches_name(&adv.name)
    }

    fn supports(&self, adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool {
        // Recognised name AND the exact roles. Nameless FFF1/FFF2 devices
        // belong to the LifeSpan fallback at the end of the registry, not to
        // a driver that would poll them with Sperax frames.
        matches_name(&adv.name) && gatt_shape_matches(gatt)
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

        // Subscribe first, then handshake — the device may answer the last
        // init frame immediately.
        link.subscribe(&notify_char).await?;
        let mut notifications = link.notifications().await?;
        run_init_sequence(link, &init_steps()).await?;

        let poll = build_frame(CMD_STATUS_QUERY, &[]);
        let mut spacer = CommandSpacer::new(POLL_INTERVAL);
        let mut dead_polls: u32 = 0;

        loop {
            spacer.pace().await;

            // Drain stale buffered notifications so the read below answers
            // THIS query.
            while notifications.next().now_or_never().flatten().is_some() {}

            // Bound the write: a stale link can block forever with no
            // disconnect event.
            match tokio::time::timeout(
                RESPONSE_TIMEOUT,
                link.write(&write_char, &poll, WriteType::WithoutResponse),
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
            host.record_frame(CMD_STATUS_QUERY, &frame); // raw capture for /api/diag

            match parse_status(&frame) {
                Ok(status) => emit(to_sample(&status)),
                Err(e) => tracing::debug!("sperax frame skipped: {e}"),
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

    // ---- CRC vectors (from qdomyos' captured frames) -------------------------

    /// The CRC parameters were brute-forced from the captured frames; these
    /// vectors pin them against three of them, including the cmd 0x13 frame
    /// we deliberately never send (a decode-direction vector only — the
    /// write-set test below proves it is never built into traffic).
    #[test]
    fn crc16_matches_the_captured_frames() {
        // Hello: F5 07 00 01 | 26 D8 | FA
        assert_eq!(crc16(&hx("f5 07 00 01")), 0xD826);
        // The dropped cmd 0x13 init: F5 09 00 13 01 00 | 89 B8 | FA
        assert_eq!(crc16(&hx("f5 09 00 13 01 00")), 0xB889);
        // Status query (logical form): F5 07 00 19 | FA 59 | FA
        assert_eq!(crc16(&hx("f5 07 00 19")), 0x59FA);
        assert_eq!(crc16(&[]), CRC_INIT, "empty input leaves the init value");
    }

    // ---- Escaping ------------------------------------------------------------

    /// The status query's CRC low byte is 0xFA — indistinguishable from the
    /// terminator without the escape — and the captured wire form proves the
    /// rule: `F0 0A` on the wire, wire length patched to 8.
    #[test]
    fn escaping_round_trips_and_matches_the_captured_wire_form() {
        let logical = hx("f5 07 00 19 fa 59 fa");
        let wire = escape_frame(&logical);
        assert_eq!(wire, hx("f5 08 00 19 f0 0a 59 fa"));
        assert_eq!(unescape_frame(&wire), logical);

        // A frame with nothing to escape passes through (only the length
        // byte is re-derived).
        let plain = hx("f5 07 00 01 26 d8 fa");
        assert_eq!(escape_frame(&plain), plain);
        assert_eq!(unescape_frame(&plain), plain);

        // Every byte in 0xF0..=0xFF escapes to F0 <low nibble> and returns.
        for b in 0xF0u8..=0xFF {
            let logical = vec![FRAME_START, 0x05, b, 0x00, FRAME_END];
            let wire = escape_frame(&logical);
            assert_eq!(&wire[2..4], &[ESCAPE, b & 0x0F], "byte 0x{b:02x}");
            assert_eq!(wire[1] as usize, wire.len());
            let mut back = unescape_frame(&wire);
            back[1] = 0x05; // undo the length re-derivation for comparison
            assert_eq!(back, logical, "byte 0x{b:02x}");
        }
    }

    // ---- Frame building: byte-identical to the captures ----------------------

    /// `build_frame` must reproduce qdomyos' captured wire bytes exactly —
    /// CRC, escaping and length patch all at once.
    #[test]
    fn build_frame_reproduces_the_captured_frames() {
        assert_eq!(build_frame(CMD_HELLO, &[]), hx("f5 07 00 01 26 d8 fa"));
        assert_eq!(
            build_frame(CMD_STATUS_QUERY, &[]),
            hx("f5 08 00 19 f0 0a 59 fa")
        );
        // The captured cmd 0x13 frame, reproduced as a framing/CRC vector.
        // NEVER sent: its payload byte could be a setting (see module docs
        // and the write-set test).
        assert_eq!(
            build_frame(0x13, &[0x01, 0x00]),
            hx("f5 09 00 13 01 00 89 b8 fa")
        );
    }

    // ---- The write set -------------------------------------------------------

    /// Every byte this driver writes must be a query. The init is the hello
    /// frame twice and nothing else — the count is part of the assertion:
    /// qdomyos sends a third frame here (cmd 0x13, payload `01 00`) whose
    /// payload can be a setting, and an observe-only driver has no business
    /// writing it. The poll loop's only frame is the status query. Command
    /// 0x15 is the actuation family (speed / start / stop); no frame we
    /// build may carry it.
    #[test]
    fn the_driver_only_ever_writes_the_hello_and_the_status_query() {
        let init: Vec<Vec<u8>> = init_steps().iter().map(|s| s.payload.clone()).collect();
        let hello = hx("f5 07 00 01 26 d8 fa");
        assert_eq!(
            init,
            vec![hello.clone(), hello],
            "init must be the hello frame twice, nothing else"
        );

        let poll = build_frame(CMD_STATUS_QUERY, &[]);
        assert_eq!(poll, hx("f5 08 00 19 f0 0a 59 fa"));

        // The command byte sits at wire offset 3 in every frame we send
        // (no escapes occur in the header); pin the allowed set.
        for frame in init.iter().chain(std::iter::once(&poll)) {
            assert!(
                frame[3] == CMD_HELLO || frame[3] == CMD_STATUS_QUERY,
                "unexpected command 0x{:02x} in {frame:02x?}",
                frame[3]
            );
            assert_ne!(frame[3], 0x15, "actuation family in {frame:02x?}");
        }
    }

    /// The handshake on the virtual clock: exactly two unacknowledged writes
    /// to FFF2, 150 ms apart, trailing delay included (it is the settle time
    /// before the first poll).
    #[tokio::test(start_paused = true)]
    async fn init_sequence_writes_the_hello_twice_with_spacing() {
        struct RecordedWrite {
            char_uuid: Uuid,
            payload: Vec<u8>,
            with_response: bool,
            at: Instant,
        }
        #[derive(Default)]
        struct MockLink {
            writes: Mutex<Vec<RecordedWrite>>,
        }
        #[async_trait]
        impl GattIo for MockLink {
            async fn write_uuid(&self, c: Uuid, p: &[u8], wr: bool) -> Result<()> {
                self.writes.lock().unwrap().push(RecordedWrite {
                    char_uuid: c,
                    payload: p.to_vec(),
                    with_response: wr,
                    at: Instant::now(),
                });
                Ok(())
            }
            async fn subscribe_uuid(&self, _c: Uuid) -> Result<()> {
                Ok(())
            }
        }
        let link = MockLink::default();
        let start = Instant::now();
        run_init_sequence(&link, &init_steps()).await.unwrap();
        assert_eq!(Instant::now() - start, Duration::from_millis(300));

        let writes = link.writes.lock().unwrap();
        assert_eq!(writes.len(), 2, "exactly the two hello writes");
        for (i, w) in writes.iter().enumerate() {
            assert_eq!(w.char_uuid, WRITE_CHAR_UUID, "frame {i}");
            assert_eq!(w.payload, hx("f5 07 00 01 26 d8 fa"), "frame {i}");
            assert!(!w.with_response, "upstream uses write-without-response");
            assert_eq!(w.at - start, Duration::from_millis(150 * i as u64));
        }
    }

    // ---- Status parsing ------------------------------------------------------
    //
    // No inbound data packet is public (see module docs), so these frames
    // are synthetic: built to upstream's field map — steps u16 BE at 15..17,
    // speed at len-7 — to pin OUR reader against THAT map. They are not
    // hardware captures and are labelled accordingly.

    /// A minimal 24-byte packet: steps 513, speed 2.7 km/h.
    fn synthetic_frame_24() -> Vec<u8> {
        let mut f = vec![0u8; 24];
        f[0] = FRAME_START;
        f[1] = 24;
        f[15] = 0x02; // steps high byte — big-endian, per upstream
        f[16] = 0x01; // steps low byte
        f[17] = 27; // speed at 17 + (24-24)
        f[23] = FRAME_END;
        f
    }

    #[test]
    fn decodes_steps_big_endian_and_speed() {
        let s = parse_status(&synthetic_frame_24()).unwrap();
        assert_eq!(s.steps, 513, "0x0201 read big-endian, not 258");
        assert_eq!(s.speed_raw, 27);
    }

    /// Speed is anchored to the frame END: in a 26-byte packet it sits at
    /// byte 19, and whatever occupies byte 17 must be ignored.
    #[test]
    fn speed_offset_tracks_the_frame_length() {
        let mut f = vec![0u8; 26];
        f[0] = FRAME_START;
        f[1] = 26;
        f[15] = 0x00;
        f[16] = 200; // 200 steps
        f[17] = 99; // decoy at the 24-byte position
        f[19] = 8; // speed at 17 + (26-24) = 0.8 km/h
        f[25] = FRAME_END;
        let s = parse_status(&f).unwrap();
        assert_eq!(s.steps, 200);
        assert_eq!(s.speed_raw, 8, "must read len-7, not absolute 17");
    }

    /// The length byte must agree with the wire length — the module's own
    /// framing rule (`escape_frame` patches byte 1 to the wire length). The
    /// device converses on FFF1 in more than status packets: a settings/info
    /// reply padded or coalesced to ≥24 bytes carries its own (different)
    /// length byte, and before this check it decoded into "steps" and
    /// "speed" and was emitted as telemetry.
    #[test]
    fn a_frame_whose_length_byte_disagrees_is_rejected() {
        // A ≥24-byte notification whose declared length is a settings
        // reply's (8), padded out by the stack — valid prefix, valid
        // terminator, plausible junk where the counters would be.
        let mut f = synthetic_frame_24();
        f[1] = 8;
        assert_eq!(
            parse_status(&f),
            Err(ProtocolError::BadDeclaredLength {
                declared: 8,
                actual: 24
            }),
            "a frame whose length byte disagrees with the wire length is not \
             a status packet and must not decode into steps/speed (framing \
             rule: byte 1 is the wire length — see escape_frame and the \
             module docs in sperax.rs)"
        );
        // Two coalesced frames in one notification fail the same way.
        let mut two = synthetic_frame_24();
        two.extend_from_slice(&synthetic_frame_24());
        assert!(matches!(
            parse_status(&two),
            Err(ProtocolError::BadDeclaredLength {
                declared: 24,
                actual: 48
            })
        ));
        // And the well-formed packet still parses.
        assert!(parse_status(&synthetic_frame_24()).is_ok());
    }

    #[test]
    fn malformed_frames_error_without_panicking() {
        assert_eq!(parse_status(&[]), Err(ProtocolError::BadLength(0)));
        assert_eq!(
            parse_status(&synthetic_frame_24()[..23]),
            Err(ProtocolError::BadLength(23)),
            "23 bytes — one short of a data packet"
        );
        let mut bad_prefix = synthetic_frame_24();
        bad_prefix[0] = 0xA1;
        assert_eq!(
            parse_status(&bad_prefix),
            Err(ProtocolError::BadPrefix(0xA1))
        );
        let mut bad_term = synthetic_frame_24();
        bad_term[23] = 0x00;
        assert_eq!(parse_status(&bad_term), Err(ProtocolError::BadTerminator));
    }

    // ---- Sample / Telemetry pins ---------------------------------------------

    /// The packet has no distance, duration or energy — those must stay
    /// absent all the way to the wire, and in particular distance must NOT
    /// be integrated from speed (upstream does; we refuse — a derived value
    /// presented as a measurement would quietly corrupt stored totals).
    #[test]
    fn unreported_fields_stay_absent_and_none_are_derived() {
        let s = parse_status(&synthetic_frame_24()).unwrap();
        let sample = to_sample(&s);
        assert_eq!(sample.distance_m, None, "distance is not in the packet");
        assert_eq!(sample.duration_s, None);
        assert_eq!(sample.calories, None);

        let t = Telemetry::from_sample(&sample, "km/h");
        assert_eq!(t.distance_raw, None);
        assert_eq!(t.distance_m, None);
        assert_eq!(t.duration_s, None);
        assert_eq!(t.calories, None);
    }

    /// SI conversion and speed-derived state, pinned through Telemetry.
    #[test]
    fn golden_synthetic_frame_to_telemetry() {
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let s = parse_status(&synthetic_frame_24()).unwrap();
        let sample = to_sample(&s);
        assert!(approx(sample.speed_kmh.unwrap(), 2.7));
        assert_eq!(sample.steps, Some(513));
        assert_eq!(sample.state, Some(BeltState::Running));

        let t = Telemetry::from_sample(&sample, "km/h");
        assert_eq!(t.speed_raw, Some(270), "2.70 km/h in centi-units");
        assert_eq!(t.steps, Some(513));
        assert_eq!(t.status_name.as_deref(), Some("RUNNING"));
        assert!(t.is_running);

        // A stationary belt presents as standby.
        let idle = Status {
            steps: 513,
            speed_raw: 0,
        };
        let t = Telemetry::from_sample(&to_sample(&idle), "km/h");
        assert_eq!(t.status_name.as_deref(), Some("STANDBY"));
        assert!(!t.is_running);
    }

    // ---- Name matching: the hyphen split -------------------------------------

    fn adv(name: &str) -> Advertisement {
        Advertisement {
            name: name.into(),
            services: vec![],
        }
    }

    /// The proprietary models match — including real-world suffixed names
    /// and case variants.
    #[test]
    fn proprietary_sperax_names_match() {
        for name in [
            "SPERAX_RM01",
            "SPERAX_RM01_74FE70",
            "sperax_rm01",
            "SPERAX_RM-02",
            "SPERAX_RM-02_ABC123",
        ] {
            assert!(Sperax.matches(&adv(name)), "{name}");
            assert!(matches_name(name), "{name}");
        }
    }

    /// The hyphenated RM-01 is FTMS hardware (verified in both upstream
    /// sources) and must never be claimed here — nor anything else.
    #[test]
    fn the_ftms_rm01_and_foreign_names_do_not_match() {
        for name in [
            "SPERAX_RM-01",        // the FTMS revision — the hyphen is the split
            "SPERAX_RM-01_74FE70", // as it really advertises
            "SPERAX",              // underspecified
            "LifeSpan-TM",
            "URTM041",
            "",
        ] {
            assert!(!Sperax.matches(&adv(name)), "{name}");
            assert!(!matches_name(name), "{name}");
        }
    }

    // ---- supports(): roles + the name gate -----------------------------------

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

    fn sperax_shaped() -> BTreeSet<Characteristic> {
        gatt(&[
            (NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY),
            (WRITE_CHAR_UUID, CharPropFlags::WRITE_WITHOUT_RESPONSE),
        ])
    }

    #[test]
    fn supports_needs_the_name_and_the_roles() {
        assert!(Sperax.supports(&adv("SPERAX_RM01_74FE70"), &sperax_shaped()));
        assert!(Sperax.supports(&adv("SPERAX_RM-02"), &sperax_shaped()));
        // The FTMS RM-01: refused even with the right-shaped table.
        assert!(!Sperax.supports(&adv("SPERAX_RM-01_74FE70"), &sperax_shaped()));
        // Nameless: refused — that is LifeSpan-fallback territory.
        assert!(!Sperax.supports(&adv(""), &sperax_shaped()));
        // Roles swapped: refused, whatever the name says.
        assert!(!Sperax.supports(
            &adv("SPERAX_RM01"),
            &gatt(&[
                (NOTIFY_CHAR_UUID, CharPropFlags::WRITE),
                (WRITE_CHAR_UUID, CharPropFlags::NOTIFY),
            ])
        ));
        // Half a table: refused.
        assert!(!Sperax.supports(
            &adv("SPERAX_RM01"),
            &gatt(&[(NOTIFY_CHAR_UUID, CharPropFlags::NOTIFY)])
        ));
    }
}
