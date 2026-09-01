//! The driver system: one self-contained module per treadmill protocol, a
//! neutral [`Sample`] they all emit, and the registry the engine consults.
//!
//! The split of responsibilities is deliberate:
//!
//! * A **driver** knows how to recognise its device and turn its Bluetooth
//!   traffic into [`Sample`]s — SI units, nothing vendor-shaped. That is all.
//! * The **engine** (`ble.rs`) owns everything else: scanning, connecting,
//!   reconnect/backoff, give-up-after-N-failures, cancellation (pause,
//!   device switch, shutdown), session detection, persistence throttling and
//!   the WebSocket broadcast. A driver never touches any of it, which is what
//!   keeps everything protocol-shaped inside one driver file.
//!
//! Adding a driver: write `drivers/yourdevice.rs` and register it in
//! [`DRIVERS`] — the scan path and the connect path both consult the
//! registry, so that makes the device discoverable *and* connectable. Then
//! wire it into the layer's shared guarantees: the [`DRIVERS`] rationale
//! bullet, the exact-ids registry test vector below, and — easiest to forget
//! because nothing fails without it — a row in `cross_driver.rs`'s
//! absent-vs-zero invariant table. The full list, with the why of each, is
//! in `docs/drivers/README.md` ("Register it").

#[cfg(test)]
mod cross_driver;
pub mod ftms;
pub mod kingsmith_wilink;
pub mod lifespan;
pub mod pitpat;
pub mod sperax;
pub mod urevo;
pub mod util;

use anyhow::Result;
use async_trait::async_trait;
use btleplug::api::Characteristic;
use btleplug::platform::Peripheral;
use std::collections::BTreeSet;
use uuid::Uuid;

/// Every driver Trot ships, in priority order — when a device satisfies more
/// than one driver, the first match wins. **Order is load-bearing: strict
/// drivers come before permissive ones.**
///
/// * `LifeSpan` matches strictly (advertised-name prefix AND the exact
///   notify/write characteristic roles) and outranks FTMS because LifeSpan
///   consoles expose their native service alongside whatever else they
///   advertise, and the native protocol reports steps where FTMS cannot.
/// * `KingSmithWiLink` matches strictly too (recognised or absent name AND
///   the FE01-notify/FE02-write roles, with the known FTMS and app-cipher
///   KingSmith models carved out by name — the app-cipher generation is
///   deliberately unsupported, so its carve-outs fall to no driver) and
///   outranks FTMS for the same reason: the native protocol reports steps.
/// * `Urevo` and `Sperax` both live on the contested `0xFFF0` block with
///   LifeSpan-shaped roles, so they require a recognised advertised name
///   (`URTM041…` / `SPERAX_RM01…`, `SPERAX_RM-02…`) on top of the roles.
///   They outrank FTMS because the E1L exposes both protocols and only the
///   native one reports steps (Sperax's hyphen-less models don't speak FTMS
///   at all) — and they must come before the LifeSpan fallback or it would
///   claim their FFF1/FFF2 shape first.
/// * `PitPat` (the PitPat/Deerrun/SupeRun OEM family) requires a recognised
///   `PITPAT-T*` name (or, nameless, its distinctive FBA0 layout) plus one
///   of its four verified transport layouts — including the Deerrun variant
///   on 0xFFF0 with the notify/write roles SWAPPED relative to LifeSpan,
///   which the role checks keep out of every LifeSpan entry.
/// * `Ftms` requires the standard Treadmill Data characteristic.
/// * `LifeSpanFallback` is the deliberate last resort: a device nobody else
///   claimed, whose `FFF1`/`FFF2` roles are exactly LifeSpan-shaped, is
///   driven as LifeSpan even under an unrecognised name — that is what keeps
///   an already-paired console working when its advertised name isn't in our
///   prefix list. Every future driver goes **before** it.
///
/// **This is the registration point.** The line here is what makes a driver
/// live — and it comes with a rationale bullet above, the exact-ids test
/// vector below, and a `cross_driver.rs` row (docs/drivers/README.md,
/// "Register it", lists all five obligations).
pub static DRIVERS: &[&dyn Driver] = &[
    &lifespan::LifeSpan,
    &kingsmith_wilink::KingSmithWiLink,
    &urevo::Urevo,
    &sperax::Sperax,
    &pitpat::PitPat,
    &ftms::Ftms,
    &lifespan::LifeSpanFallback,
];

/// A treadmill protocol driver. In-tree, compiled in, reviewed — there is no
/// dynamic loading, deliberately.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Short stable identifier ("lifespan", "ftms"). Shows up in logs.
    fn id(&self) -> &'static str;

    /// Does this advertisement look like a device you can drive? Called during
    /// `trot scan` — before any connection exists — so all you have is the
    /// advertised name and service UUIDs. Be permissive here; [`Self::supports`]
    /// gets the real service table later.
    fn matches(&self, adv: &Advertisement) -> bool;

    /// Does this connected device look like yours up close? Called after
    /// connect + service discovery to pick the driver, with the full GATT
    /// characteristic table (UUIDs *and* properties) plus the advertisement.
    ///
    /// Match on what you will actually subscribe to or write — and be aware
    /// that a service UUID alone proves nothing: 0xFFF0 alone hosts at least
    /// six mutually incompatible vendor protocols, some with the notify/write
    /// roles swapped. When your protocol shares a service with others, check
    /// characteristic properties and the advertised name, not just UUIDs. A
    /// device whose advertisement looked like yours but whose table doesn't
    /// check out falls through to the next driver in [`DRIVERS`].
    fn supports(&self, adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> bool;

    /// Drive the device: subscribe/poll/handshake as the protocol requires and
    /// call `emit` with a cumulative [`Sample`] on every update.
    ///
    /// Sustained [`BeltState::Running`] opens a session; the engine debounces
    /// by *time* held, not by frame count, so emit as often as your protocol
    /// updates — a fast stream cannot flap sessions and a slow one is not
    /// penalised.
    ///
    /// Run forever. Do not watch for shutdown or pause and do not disconnect —
    /// the engine cancels this future and tears the link down itself. Return
    /// `Err` only when the link is dead or unusable; that triggers the
    /// engine's reconnect-with-backoff path.
    async fn run(&self, link: &Peripheral, host: &DriverHost<'_>, emit: Emit<'_>) -> Result<()>;
}

/// The sink a driver feeds. Call it with the full latest state (not a delta)
/// each time anything changes; the engine handles throttling and sessions.
pub type Emit<'a> = &'a mut (dyn FnMut(Sample) + Send);

/// What a driver may see of a device while it is only advertising (scan and
/// pairing, before any connection).
#[derive(Debug, Clone)]
pub struct Advertisement {
    /// Advertised local name; empty when the device doesn't broadcast one.
    pub name: String,
    /// Advertised service UUIDs. Often a subset of the real GATT table.
    pub services: Vec<Uuid>,
}

/// One reading from the belt, in SI units. This is the only currency a driver
/// deals in — no vendor encodings, no display units.
///
/// Every field is `Option` because "this device cannot report that" is
/// meaningful: FTMS treadmills have no step counter, so their driver leaves
/// `steps` as `None` and the rest of the engine treats it as absent rather
/// than zero. Leave out what your device doesn't know; never invent values.
///
/// Counters (`distance_m`, `steps`, `duration_s`, `calories`) are cumulative
/// since the session started on the console, exactly as the device reports
/// them — the engine's storage layer de-glitches resets and stale frames, so
/// don't try to smooth them in the driver.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// Belt speed, km/h.
    pub speed_kmh: Option<f64>,
    /// Distance, meters.
    pub distance_m: Option<f64>,
    /// Step count, as the console reports it.
    pub steps: Option<u32>,
    /// Elapsed workout time, seconds.
    pub duration_s: Option<u32>,
    /// Energy, kcal.
    pub calories: Option<u32>,
    /// What the belt is doing, if the device reports it.
    pub state: Option<BeltState>,
}

/// What the belt is doing. `Running` is what opens and (its absence) closes
/// sessions; the other states exist because some consoles distinguish them and
/// clients display them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeltState {
    /// Powered on, belt stopped.
    Standby,
    /// Belt moving.
    Running,
    /// Console showing the post-workout summary screen.
    Summary,
    /// Workout paused.
    Paused,
    /// A state Trot doesn't know; the raw device value is passed through.
    ///
    /// Three consequences of that passthrough, all deliberate — this rustdoc
    /// is the one home for them (the SQL comments in `db.rs` point here):
    ///
    /// 1. **The raw byte lands in the contract's presentation namespace.**
    ///    `telemetry::status_code` serializes `Other(v)` as `v` itself, and
    ///    the contract's `status` field already assigns meanings to
    ///    `0x01/0x03/0x04/0x05` — so `Other(5)` reads as PAUSED and
    ///    `Other(3)` as RUNNING to any API client. Frozen behaviour: the
    ///    LifeSpan driver pins `status_code(belt_state(v)) == v` for all 256
    ///    bytes, because for that console the bytes ARE the contract's codes
    ///    and changing the mapping would move bytes on the wire for real
    ///    devices. Prefer a named state where your protocol's evidence
    ///    allows one (WiLink maps known wire values away from the
    ///    colliding codes for exactly this reason).
    /// 2. **`Other` never opens or closes a session.** `Telemetry::
    ///    is_running` derives from `state == Running`, not from the status
    ///    byte, so an unknown byte that happens to be `0x03` presents as
    ///    RUNNING (consequence 1) yet accrues no session — sessions follow
    ///    the driver's judgement, never a raw byte.
    /// 3. **`Other(3)` interacts with stored aggregates via session
    ///    attribution.** Samples store the presentation byte, and "running
    ///    time" aggregates count status-3 samples *within sessions* —
    ///    consequence 2 means `Other(3)` samples are never in one, so they
    ///    never count. Both SQL sites (`timeseries`' raw path and the
    ///    rollup writer's `running_samples`, in `db.rs`) share that
    ///    in-session definition; comments there point back here.
    Other(u8),
}

/// What the engine provides to a running driver.
pub struct DriverHost<'a> {
    /// The unit the user's console displays ("km/h" or "mph"). Only relevant
    /// to drivers whose wire format depends on the console's display setting
    /// (LifeSpan encodes speed in hundredths of the *displayed* unit). Drivers
    /// that report SI natively should ignore it.
    pub display_unit: String,
    recorder: &'a (dyn Fn(u8, &[u8]) + Send + Sync),
}

impl<'a> DriverHost<'a> {
    pub fn new(display_unit: String, recorder: &'a (dyn Fn(u8, &[u8]) + Send + Sync)) -> Self {
        DriverHost {
            display_unit,
            recorder,
        }
    }

    /// Record a raw frame into the diagnostics ring buffer (dumped by
    /// `/api/diag` as `recent_frames`). Call this for every frame you receive,
    /// with a tag of your choosing (LifeSpan uses the request opcode) — it is
    /// the tool a contributor uses to reverse-engineer and debug a protocol
    /// without attaching a debugger to a moving treadmill.
    pub fn record_frame(&self, tag: u8, frame: &[u8]) {
        (self.recorder)(tag, frame)
    }
}

/// The driver claiming a connected device, if any. First match in [`DRIVERS`]
/// order wins.
pub fn for_device(
    adv: &Advertisement,
    gatt: &BTreeSet<Characteristic>,
) -> Option<&'static dyn Driver> {
    DRIVERS.iter().copied().find(|d| d.supports(adv, gatt))
}

/// Every driver whose `supports()` accepts the device, in registry order —
/// [`for_device`]'s winner is the first entry. Registry order is
/// load-bearing and cannot be tested against real hardware, so the engine
/// logs this whole set on connect (`ble.rs`'s dispatch line): with it, any
/// future "wrong driver claimed my treadmill" dispute is a one-line bug
/// report instead of a reconstruction. `tests/driver_matrix.rs` asserts
/// exact supporter sets through this same function.
pub fn supporters(adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> Vec<&'static str> {
    DRIVERS
        .iter()
        .filter(|d| d.supports(adv, gatt))
        .map(|d| d.id())
        .collect()
}

/// Would any driver want this advertisement? The scan path uses this, so a
/// newly registered driver is discoverable with no further wiring.
pub fn any_match(adv: &Advertisement) -> bool {
    DRIVERS.iter().any(|d| d.matches(adv))
}

/// Registered driver ids, for error messages.
pub fn ids() -> Vec<&'static str> {
    DRIVERS.iter().map(|d| d.id()).collect()
}

/// Full 128-bit form of a 16-bit Bluetooth SIG assigned UUID,
/// e.g. `0x1826` → `00001826-0000-1000-8000-00805f9b34fb`.
pub const fn sig_uuid(short: u16) -> Uuid {
    Uuid::from_u128(((short as u128) << 96) | 0x0000_1000_8000_0080_5f9b_34fb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use btleplug::api::CharPropFlags;

    fn adv(name: &str, services: &[u16]) -> Advertisement {
        Advertisement {
            name: name.into(),
            services: services.iter().map(|s| sig_uuid(*s)).collect(),
        }
    }

    fn chr(short: u16, properties: CharPropFlags) -> Characteristic {
        Characteristic {
            uuid: sig_uuid(short),
            service_uuid: sig_uuid(0x0000),
            properties,
            descriptors: BTreeSet::new(),
        }
    }

    fn gatt(chars: &[(u16, CharPropFlags)]) -> BTreeSet<Characteristic> {
        chars.iter().map(|(u, p)| chr(*u, *p)).collect()
    }

    const N: CharPropFlags = CharPropFlags::NOTIFY;
    const W: CharPropFlags = CharPropFlags::WRITE;

    #[test]
    fn sig_uuid_builds_the_base_form() {
        assert_eq!(
            sig_uuid(0x1826).to_string(),
            "00001826-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            sig_uuid(0xfff0).to_string(),
            "0000fff0-0000-1000-8000-00805f9b34fb"
        );
    }

    /// The union of driver `matches()` must cover every supported device
    /// family — LifeSpan/ESP32 names and service 0xFFF0, the KingSmith WiLink
    /// names and service 0xFE00, FTMS 0x1826 plus the verified FTMS
    /// walking-pad name prefixes — and nothing else.
    #[test]
    fn scan_matching_covers_the_known_devices() {
        assert!(any_match(&adv("LifeSpan-TM", &[])));
        assert!(any_match(&adv("ESP32-treadmill", &[])));
        assert!(any_match(&adv("", &[0xfff0])));
        assert!(any_match(&adv("", &[0x1826])));
        assert!(any_match(&adv("WalkingPad A1", &[])));
        assert!(any_match(&adv("", &[0xfe00])));
        // The app-cipher KingSmith generation (X21/X23/G1/K12 Pro) is
        // deliberately unsupported — its driver was removed — so neither
        // its names nor its 0x1234 service surface in a scan.
        assert!(!any_match(&adv("KS-X21C-1234", &[])));
        assert!(!any_match(&adv("KS-NGCH-G1C", &[])));
        assert!(!any_match(&adv("", &[0x1234])));
        // FTMS walking pads that advertise a known name without 0x1826.
        assert!(any_match(&adv("URTM024", &[])));
        assert!(any_match(&adv("KS-MC21-D06BFD", &[])));
        assert!(any_match(&adv("SPERAX_RM-01", &[])));
        // The proprietary-protocol devices on the 0xFFF0 block: the Urevo
        // E1L and the hyphen-less/RM-02 Sperax revisions now have native
        // drivers (they used to be deliberately unclaimed).
        assert!(any_match(&adv("URTM041", &[])));
        assert!(any_match(&adv("URTM030", &[])));
        assert!(any_match(&adv("SPERAX_RM01", &[])));
        assert!(any_match(&adv("SPERAX_RM-02", &[])));
        // The PitPat/Deerrun/SupeRun family: by name or its FBA0 service.
        assert!(any_match(&adv("PitPat-T01", &[])));
        assert!(any_match(&adv("", &[0xfba0])));
        assert!(
            !any_match(&adv("PITPAT-S1", &[])),
            "PITPAT-S is the PitPat bike — Trot reads treadmills only"
        );
        // The FitShow OEM family is deliberately unsupported — its driver
        // was removed — so its names no longer surface in a scan. FitShow
        // units that broadcast standard FTMS still surface via 0x1826.
        assert!(!any_match(&adv("FS-3D6CD7", &[])));
        assert!(!any_match(&adv("NOBLEPRO CONNECT 1", &[])));
        assert!(!any_match(&adv("TUNTURI T80-2", &[])));
        assert!(any_match(&adv("NOBLEPRO CONNECT 1", &[0x1826])));
        assert!(!any_match(&adv("Some Headphones", &[0x180f])));
        assert!(!any_match(&adv("", &[])));
    }

    /// Connect-time dispatch: a named LifeSpan console with the right
    /// characteristic roles wins outright — including over FTMS, because the
    /// native protocol reports steps where FTMS cannot.
    #[test]
    fn a_named_lifespan_console_takes_the_strict_driver() {
        let named = adv("LifeSpan-TM", &[]);
        assert_eq!(
            for_device(&named, &gatt(&[(0xfff1, N), (0xfff2, W), (0x2acd, N)])).map(|d| d.id()),
            Some("lifespan")
        );
        assert_eq!(
            for_device(&named, &gatt(&[(0xfff1, N), (0xfff2, W)])).map(|d| d.id()),
            Some("lifespan")
        );
    }

    /// A nameless device with LifeSpan-shaped roles and no other claim lands
    /// on the fallback — this is what keeps an already-paired console working
    /// when its advertised name isn't in the prefix list (or the platform
    /// doesn't surface a name at connect time).
    #[test]
    fn an_unnamed_lifespan_shaped_device_falls_back_to_lifespan() {
        let anon = adv("", &[]);
        assert_eq!(
            for_device(&anon, &gatt(&[(0xfff1, N), (0xfff2, W)])).map(|d| d.id()),
            Some("lifespan-fallback")
        );
    }

    /// A device that ALSO speaks real FTMS is claimed by FTMS before the
    /// LifeSpan fallback: an unrecognised name plus a generic FFFx vendor
    /// block plus standard FTMS is the KingSmith/ODM shape, where FTMS is the
    /// protocol that actually works and LifeSpan opcodes are garbage.
    #[test]
    fn ftms_outranks_the_lifespan_fallback() {
        let anon = adv("", &[]);
        assert_eq!(
            for_device(&anon, &gatt(&[(0xfff1, N), (0xfff2, W), (0x2acd, N)])).map(|d| d.id()),
            Some("ftms")
        );
        assert_eq!(
            for_device(&anon, &gatt(&[(0x2acd, N)])).map(|d| d.id()),
            Some("ftms")
        );
    }

    /// Role-swapped FFF1/FFF2 (the Deerrun shape: write on FFF1, notify on
    /// FFF2) must not be claimed by ANY LifeSpan entry — writing LifeSpan
    /// opcodes at it would mis-drive a different protocol. Likewise partial
    /// tables and non-treadmills get no driver.
    #[test]
    fn wrong_roles_or_partial_tables_get_no_driver() {
        let anon = adv("", &[]);
        assert!(for_device(&anon, &gatt(&[(0xfff1, W), (0xfff2, N)])).is_none());
        assert!(
            for_device(&adv("LifeSpan-TM", &[]), &gatt(&[(0xfff1, W), (0xfff2, N)])).is_none(),
            "even a LifeSpan name cannot override role verification"
        );
        assert!(for_device(&anon, &gatt(&[(0xfff1, N)])).is_none());
        assert_eq!(
            for_device(&anon, &gatt(&[(0xfff1, N), (0x2acd, N)])).map(|d| d.id()),
            Some("ftms"),
            "FFF1 without a writable FFF2 must not claim the device for LifeSpan"
        );
        assert!(for_device(&anon, &gatt(&[(0x2a37, N)])).is_none());
    }

    /// The 0xFFF0 collision, adjudicated by name: LifeSpan, Urevo and Sperax
    /// all expose the identical FFF1-notify/FFF2-write shape, so the
    /// advertised name is the only thing keeping each device off the others'
    /// protocols. A Urevo E1L must take the Urevo driver — even when it also
    /// exposes real FTMS (the native protocol reports steps; FTMS on this
    /// hardware does not) — and must never reach a LifeSpan entry.
    #[test]
    fn the_fff0_squatters_land_on_their_own_drivers() {
        let shape = gatt(&[(0xfff1, N), (0xfff2, W)]);
        let shape_with_ftms = gatt(&[(0xfff1, N), (0xfff2, W), (0x2acd, N)]);

        assert_eq!(
            for_device(&adv("URTM041", &[]), &shape).map(|d| d.id()),
            Some("urevo")
        );
        assert_eq!(
            for_device(&adv("URTM041", &[]), &shape_with_ftms).map(|d| d.id()),
            Some("urevo"),
            "the native protocol outranks the E1L's FTMS service"
        );
        // The Spacewalk 3S shares the URTM prefix but speaks plain FTMS.
        assert_eq!(
            for_device(&adv("URTM024", &[]), &shape_with_ftms).map(|d| d.id()),
            Some("ftms")
        );

        // The Sperax hyphen split, both directions.
        assert_eq!(
            for_device(&adv("SPERAX_RM01_74FE70", &[]), &shape).map(|d| d.id()),
            Some("sperax")
        );
        assert_eq!(
            for_device(&adv("SPERAX_RM-02", &[]), &shape).map(|d| d.id()),
            Some("sperax")
        );
        assert_eq!(
            for_device(&adv("SPERAX_RM-01_74FE70", &[]), &shape_with_ftms).map(|d| d.id()),
            Some("ftms"),
            "the hyphenated RM-01 is FTMS hardware"
        );

        // A named LifeSpan console still takes the LifeSpan driver, and a
        // nameless FFF0-shaped device still falls back to it — the two new
        // strict drivers must widen neither claim.
        assert_eq!(
            for_device(&adv("LifeSpan-TM", &[]), &shape).map(|d| d.id()),
            Some("lifespan")
        );
        assert_eq!(
            for_device(&adv("", &[]), &shape).map(|d| d.id()),
            Some("lifespan-fallback")
        );
    }

    /// The PitPat/Deerrun/SupeRun family, adjudicated across its four
    /// transport layouts. The Deerrun variant is the case the 0xFFF0 role
    /// checks were built for: LifeSpan's UUIDs with notify/write SWAPPED.
    /// A named PitPat on that shape must land on the PitPat driver — and on
    /// no LifeSpan entry — while the reverse arrangement (real LifeSpan
    /// roles) must never land on PitPat, whatever the name says.
    #[test]
    fn the_pitpat_family_lands_on_pitpat_across_all_transports() {
        const WWR: CharPropFlags = CharPropFlags::WRITE_WITHOUT_RESPONSE;
        let named = adv("PitPat-T01", &[]);

        // Every transport layout, by name.
        for (label, shape) in [
            ("FBA0", gatt(&[(0xfba1, W), (0xfba2, N)])),
            ("FFFF", gatt(&[(0xff01, W), (0xff02, N)])),
            ("FFF0 swapped", gatt(&[(0xfff1, WWR), (0xfff2, N)])),
            ("1910", gatt(&[(0x2b11, W), (0x2b10, N)])),
        ] {
            assert_eq!(
                for_device(&named, &shape).map(|d| d.id()),
                Some("pitpat"),
                "{label}"
            );
        }

        // Nameless: only the distinctive FBA0 layout is claimed. A nameless
        // Deerrun-shaped device stays unclaimed (pinned again in
        // `wrong_roles_or_partial_tables_get_no_driver`) — 0xFFF0 is the
        // contested block and nobody claims it without evidence.
        let anon = adv("", &[]);
        assert_eq!(
            for_device(&anon, &gatt(&[(0xfba1, W), (0xfba2, N)])).map(|d| d.id()),
            Some("pitpat")
        );
        assert!(for_device(&anon, &gatt(&[(0xfff1, WWR), (0xfff2, N)])).is_none());
        assert!(for_device(&anon, &gatt(&[(0xff01, W), (0xff02, N)])).is_none());
        assert!(for_device(&anon, &gatt(&[(0x2b11, W), (0x2b10, N)])).is_none());

        // Writing PitPat frames at a LifeSpan is the other half of the
        // failure: real LifeSpan roles (notify FFF1 / write FFF2) never
        // reach the PitPat driver — a LifeSpan console stays with LifeSpan,
        // and a PitPat-named device on that shape falls through to the
        // deliberate fallback (an unknown table, treated exactly like any
        // other unrecognised-name FFF1/FFF2 device — the benign
        // unanswered-polls failure, not a mis-decode).
        let lifespan_shape = gatt(&[(0xfff1, N), (0xfff2, W)]);
        assert_eq!(
            for_device(&adv("LifeSpan-TM", &[]), &lifespan_shape).map(|d| d.id()),
            Some("lifespan")
        );
        assert_eq!(
            for_device(&named, &lifespan_shape).map(|d| d.id()),
            Some("lifespan-fallback")
        );

        // The PitPat BIKE (PITPAT-S) must reach no driver even on a
        // treadmill-shaped table — Trot reads treadmills only.
        assert!(for_device(&adv("PITPAT-S1", &[]), &gatt(&[(0xfba1, W), (0xfba2, N)])).is_none());
    }

    /// The removed FitShow family (the driver was dropped deliberately —
    /// see docs/provenance.md): its names must now fall through the
    /// registry the same way any unrecognised name does, never to a
    /// mis-decode. On the contested 0xFFF0 block with LifeSpan-shaped
    /// roles that means the deliberate fallback (the benign
    /// unanswered-polls failure); with real FTMS present, FTMS wins; on
    /// the family's vendor-only AE00/FFE0 tables, no driver at all.
    #[test]
    fn removed_fitshow_names_fall_through_to_ftms_or_nothing() {
        let lifespan_shape = gatt(&[(0xfff1, N), (0xfff2, W)]);

        // FFF0 with LifeSpan roles: exactly the unrecognised-name path.
        assert_eq!(
            for_device(&adv("FS-3D6CD7", &[]), &lifespan_shape).map(|d| d.id()),
            Some("lifespan-fallback")
        );
        // Real FTMS alongside: FTMS claims it (steps no longer beat FTMS —
        // there is no native driver to report them).
        let fff0_with_ftms = gatt(&[(0xfff1, N), (0xfff2, W), (0x2acd, N)]);
        assert_eq!(
            for_device(&adv("TUNTURI T80-1", &[]), &fff0_with_ftms).map(|d| d.id()),
            Some("ftms")
        );
        assert_eq!(
            for_device(&adv("FS-3D6CD7", &[]), &fff0_with_ftms).map(|d| d.id()),
            Some("ftms")
        );
        assert_eq!(
            for_device(
                &adv("NOBLEPRO CONNECT 1", &[]),
                &gatt(&[(0xae02, N), (0xae01, W), (0x2acd, N)])
            )
            .map(|d| d.id()),
            Some("ftms"),
            "a NoblePro exposing FTMS still works, via the FTMS driver"
        );
        // The vendor-only tables nobody claims any more.
        for (label, shape) in [
            ("AE00", gatt(&[(0xae02, N), (0xae01, W)])),
            (
                "FFE0",
                gatt(&[(0xffe4, N), (0xffe1, CharPropFlags::WRITE_WITHOUT_RESPONSE)]),
            ),
        ] {
            assert!(
                for_device(&adv("FS-3D6CD7", &[]), &shape).is_none(),
                "{label}"
            );
        }
        // A modern FS-BT-C1 module: plain FTMS, vendor FFF1 notify-only
        // (no FFF2 write role) — the role check refuses the fallback and
        // the device lands on FTMS, exactly as before.
        assert_eq!(
            for_device(&adv("FS-AB12CD", &[]), &gatt(&[(0xfff1, N), (0x2acd, N)])).map(|d| d.id()),
            Some("ftms")
        );
        // Deerrun-swapped FFF0 roles: still no driver, whatever the name.
        assert!(for_device(
            &adv("FS-3D6CD7", &[]),
            &gatt(&[(0xfff1, CharPropFlags::WRITE_WITHOUT_RESPONSE), (0xfff2, N)])
        )
        .is_none());
    }

    /// The removed app-cipher KingSmith generation (the driver was dropped
    /// deliberately — see docs/provenance.md): WiLink's carve-outs still
    /// hold, because a WiLink driver must never poll an app-cipher pad —
    /// those names now fall to NO driver rather than to a sibling. The
    /// distinctive props transport (FED8/FED7 under service 0x1234) is
    /// likewise unclaimed, named or nameless.
    #[test]
    fn removed_app_cipher_kingsmith_devices_get_no_driver() {
        let props_shape: BTreeSet<Characteristic> = [
            Characteristic {
                uuid: sig_uuid(0xfed8),
                service_uuid: sig_uuid(0x1234),
                properties: N,
                descriptors: BTreeSet::new(),
            },
            Characteristic {
                uuid: sig_uuid(0xfed7),
                service_uuid: sig_uuid(0x1234),
                properties: CharPropFlags::WRITE_WITHOUT_RESPONSE,
                descriptors: BTreeSet::new(),
            },
        ]
        .into();
        let wilink_shape = gatt(&[(0xfe01, N), (0xfe02, W)]);

        // The app-cipher transport is unclaimed — polling any surviving
        // protocol at it would garble.
        assert!(for_device(&adv("KS-NGCH-G1C", &[]), &props_shape).is_none());
        assert!(for_device(&adv("", &[]), &props_shape).is_none());
        // An app-cipher name on WiLink's table: the carve-out still
        // refuses it (kingsmith_wilink::ADV_NAME_EXCLUDE_PREFIXES), so it
        // reaches no driver instead of being mis-driven as WiLink.
        assert!(for_device(&adv("KS-HDSY-X21C", &[]), &wilink_shape).is_none());
        assert!(for_device(&adv("KS-HC-R1AA", &[]), &wilink_shape).is_none());
        // A real WiLink pad is untouched by the removal.
        assert_eq!(
            for_device(&adv("WalkingPad A1", &[]), &wilink_shape).map(|d| d.id()),
            Some("kingsmith-wilink")
        );
        assert!(for_device(&adv("WalkingPad A1", &[]), &props_shape).is_none());
    }

    /// A named WalkingPad with the WiLink notify/write roles takes the native
    /// driver — including over FTMS (some newer pads expose both, and only
    /// the native protocol reports steps). The carved-out FTMS model with the
    /// KingSmith name falls through to FTMS.
    #[test]
    fn a_named_walkingpad_takes_the_wilink_driver() {
        let named = adv("WalkingPad A1", &[]);
        assert_eq!(
            for_device(&named, &gatt(&[(0xfe01, N), (0xfe02, W)])).map(|d| d.id()),
            Some("kingsmith-wilink")
        );
        assert_eq!(
            for_device(&named, &gatt(&[(0xfe01, N), (0xfe02, W), (0x2acd, N)])).map(|d| d.id()),
            Some("kingsmith-wilink")
        );
        assert_eq!(
            for_device(&adv("KS-HD-Z1D", &[]), &gatt(&[(0x2acd, N)])).map(|d| d.id()),
            Some("ftms"),
            "the FTMS WalkingPad Z1 must reach the FTMS driver, not WiLink"
        );
    }

    /// Trot observes treadmills; it never controls them (see
    /// docs/drivers/README.md). This is a tripwire for the most likely way
    /// actuation would re-enter the tree: porting FTMS Control Point code —
    /// or the vendor "unlock" writes that gate it on shared ODM modules —
    /// from an upstream reference. No driver source may mention the Control
    /// Point's 16-bit UUID or the known unlock characteristic UUIDs, in code
    /// OR in comments (documentation of those characteristics lives in git
    /// history and the upstream projects, not here).
    ///
    /// Honesty note: this is a tripwire, not a proof. Vendor actuation frames
    /// (a WiLink speed command, say) are plain bytes a source scan cannot
    /// tell from data, so the real guarantee is review — CONTRIBUTING.md and
    /// the driver guide state the policy a reviewer enforces.
    #[test]
    fn no_driver_references_control_point_or_unlock_uuids() {
        // Built at runtime so this test doesn't trip itself.
        let forbidden = [
            format!("2ad{}", 9),              // FTMS Fitness Machine Control Point
            format!("d18d2c1{}", 0),          // KingSmith ODM unlock characteristic
            format!("59554c5{}", 5),          // Merach unlock characteristic
            format!("CommandPreambl{}", 'e'), // the removed unlock-write helper
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/drivers");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("drivers dir must be listable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            checked += 1;
            let src = std::fs::read_to_string(&path)
                .expect("driver source must be readable")
                .to_ascii_lowercase();
            for needle in &forbidden {
                assert!(
                    !src.contains(&needle.to_ascii_lowercase()),
                    "{} references \"{}\" — Trot never writes belt commands; \
                     drop the control path (docs/drivers/README.md, \"Trot \
                     observes — it never controls\")",
                    path.display(),
                    needle
                );
            }
        }
        assert!(
            checked >= 5,
            "expected to scan the driver sources, found {checked}"
        );
    }

    /// Registry order is load-bearing: strict drivers first, the permissive
    /// LifeSpan fallback dead last. If this test fails because you added a
    /// driver, add it BEFORE "lifespan-fallback" — anything after the
    /// fallback can never claim an FFF1/FFF2-shaped device.
    #[test]
    fn registry_ids_are_unique_and_the_fallback_stays_last() {
        let ids = ids();
        assert_eq!(
            ids,
            vec![
                "lifespan",
                "kingsmith-wilink",
                "urevo",
                "sperax",
                "pitpat",
                "ftms",
                "lifespan-fallback"
            ]
        );
        assert_eq!(
            ids.last(),
            Some(&"lifespan-fallback"),
            "the permissive fallback must remain the last registry entry"
        );
    }
}
