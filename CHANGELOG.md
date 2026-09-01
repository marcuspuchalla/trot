# Changelog

All notable changes to `trot` are documented here.

Write new entries under `## Unreleased` as you make the change, while you still
remember why. The "Cut release" workflow promotes that section to a version
number — it never writes the prose for you, and nothing here is generated from
commit messages.

## Unreleased

- `trot scan` no longer aborts with a D-Bus `GetAll ... doesn't exist` error about
  half the way through: it now reads device properties while discovery is still
  active (BlueZ deletes temporary device objects on stop, which used to kill the
  scan), and a device that vanishes mid-scan is skipped instead of failing the
  whole scan.
- **Urevo URTM030 now reports steps.** The Urevo driver only knew `URTM041`, so
  a URTM030 (which exposes the same proprietary service `0xFFF0` with notify
  `FFF1`/write `FFF2` alongside FTMS) fell through to FTMS, which has no step
  counter. URTM030 speaks the same native status protocol, but its firmware
  computes the trailer over bytes `1..len-2` (excluding the STX) where the E1L
  counts it — so the Urevo driver now picks the checksum variant from the
  advertised name and decodes steps, speed, distance and duration from the
  native stream. Calories, which the native protocol doesn't carry, are taken
  from the pad's FTMS service when one is present and ridden on the native
  samples — so a Urevo pad that also exposes FTMS keeps its energy readings.
- **No more per-second `urevo decode error: expected at least 6 bytes, got 5`
  log spam** while a URTM030 pad is idle (belt stopped, just booted). That
  frame is the firmware's wake ack/keepalive, not a status frame; the driver
  now recognises and skips it instead of warning on every arrival.

## 0.4.0

Every device that shares an account now reads the same number for a day, by
construction rather than by agreement.

### Fixed
- **A device that followed part of someone else's walk kept a number that could
  never rise again — and spread it to the whole account.** Banked session
  totals were preferred only when this device had no data of its own for a
  session. But a follower has plenty: sync brings the walker's sessions, its
  rollups, and while live-following its raw tail. So the follower recomputed
  the walk from a partial copy, against its *own* rollup floor, which stops
  advancing as soon as the device records nothing itself — leaving every bucket
  above that floor invisible.

  It then wrote that number over the recorder's verdict and published it, so
  each device bootstrapping from the account inherited it. Measured on a
  half-followed walk: 650 steps against the walker's 1200, permanently.

  Authority now follows the recording device (`source`), never the amount of a
  session a device happens to hold. A device will not bank a verdict for a
  session it did not record, and no longer rolls up another device's samples —
  with no older local sample to seed the de-glitch, an imported tail's first
  odometer reading was being banked as a fresh day baseline and the upsert
  replaced the walker's correct bucket with it.

### Changed
- **A session now carries its own total, and that is what every device
  displays.** `sessions` gains `steps_total`, `duration_s_total`,
  `distance_raw_total` and `calories_total`, written by the device that
  recorded the walk and carried verbatim over sync and export.

  De-glitching a treadmill's odometer needs the raw sample stream, and only the
  recording device ever has it — a synced peer receives session rows and
  nothing else. So each engine had its own way of answering "how many steps
  today": this one summed de-glitched per-minute deltas, while a client holding
  only session rows subtracted odometer endpoints. Two algorithms over two
  datasets, agreeing only by coincidence — one real day read 4,909 steps over
  6 sessions on the desktop and 4,496 over 5 on the web, and no amount of
  syncing brought them together.

  Now the recorder computes the answer once and banks it on the row; everyone
  else sums those columns. The day-wide de-glitch walk is unchanged — each
  accepted increment is simply attributed to the session that owns its sample,
  so per-session totals still add up to exactly the day total. Sessions
  recorded before this are backfilled from their samples the first time that
  day is read; sessions whose samples have already been pruned keep the old
  endpoint arithmetic.

  Additive on `/api`: `/api/sessions` and `/api/sessions/:id` gain the four
  fields, and `/api/today` reports corrected values under unchanged keys.
  A day's session count now includes a walk in progress rather than only
  finished ones.

## 0.3.9

Two devices sharing an account now agree about how far you walked. They did
not, and the disagreement was permanent rather than a delay.

### Fixed
- **A device syncing a walk from another device kept a step count that was
  short, and stayed short.** Every engine rolls raw samples into per-minute
  buckets. A device receiving a walk over sync rolls up a minute whose samples
  have only partly arrived, banks a short bucket, and moves its rollup floor
  past it — after which those clipped samples can never be re-rolled. The
  recording device's correct bucket for that minute then arrived and was
  discarded as a duplicate, because a merge could only insert, never correct.
  A dump now names the device that produced it, and a device is authoritative
  for the sessions it recorded: its rows replace what a receiving device
  derived for itself. Everyone else stays insert-only, which is what keeps that
  safe — a device echoing an old copy back cannot overwrite the original.
- **A session imported while it was still in progress never finished.** The
  same insert-only rule meant its final step count and end time could never
  arrive, so another device showed a walk that ran for ever.
- **A restart could end a walk happening on someone else's device.** Closing
  sessions left open by a crash matched every open session regardless of which
  device recorded it, then published the invented end time back — truncating a
  live walk for every device on the account. It now only closes its own.

### Added
- `/api/state` gains `remote_active`: whether another device of this account is
  mid-walk. The desktop menu bar is driven from native code, because a
  backgrounded webview is throttled, and it could otherwise only ever know
  about a treadmill attached to that machine.

## 0.3.8

Everything here exists so that a *second* device can watch a walk happening on
the first one. Nothing changes for a machine reading its own treadmill.

### Added
- **`/api/export?include=raw&since=<ts>` bounds the raw samples by time.**
  A day's total is the per-minute rollups *plus* the raw tail above the rollup
  floor — which is why the device doing the walking is always right, and why a
  device syncing from it was not: the export left the raw tail out, so a
  follower received only rollups and read low by everything walked since the
  rollup loop last ran. It can now carry that tail without shipping the whole
  history, which at one sample per second across a week is thousands of rows
  and several megabytes — far too much to push every twenty seconds.

### Changed
- **The rollup loop runs every 60 seconds instead of every 300.** Invisible
  locally, for the same reason as above. Very visible to a follower, which
  could sit five minutes behind however often it synced. Sixty seconds matches
  the one-minute bucket resolution, so a bucket is banked about as soon as it
  is complete rather than five at a time.

## 0.3.7

**No engine changes**, again — this corrects the release documentation.

### Fixed
- **[docs/releasing.md](docs/releasing.md) described a gap that has since been
  closed.** It said the Nowhere app's CI published unsigned, unnotarized macOS
  bundles and that only local builds were signed. That was true when written and
  is no longer: those builds are now signed with the Developer ID certificate,
  notarized, and stapled — both the app and the disk image. The document now
  also records how far one release tag reaches (desktop bundles *and* a
  TestFlight upload), and describes the certificate pre-flight that fails in
  seconds, by name, rather than eight minutes into a build.

## 0.3.6

**No engine changes.** Nothing about reading a treadmill, storing a session or
serving the API is different from 0.3.5 — if you are running that, there is
nothing here you need. This release exists to make the *next* one harder to get
wrong.

### Added
- **`Cut release`, a one-button release.** Releasing meant bumping two
  `Cargo.toml`s, writing the changelog, committing and tagging, in that order;
  doing it out of order is how 0.3.2 shipped a tag with no release behind it.
  The workflow now promotes the `## Unreleased` section to a version heading,
  bumps the crates, tags and pushes. The build pipeline it hands off to — the
  test gate, five platforms, codesigning, notarization — is unchanged.
- **A `## Unreleased` section in this file**, which is where changes are now
  written as they are made. Release notes are not generated from commit
  messages here, deliberately: the 0.3.5 entry explains that `0xFFF0` is
  squatted by at least five vendors and that one of them swaps the notify and
  write roles, and no tool that reads `git log` writes that sentence.
- **[docs/releasing.md](docs/releasing.md)**, documenting both this repo's
  release path and the hand-off to the Nowhere app, which bundles this engine.

## 0.3.5

Four more treadmill protocols, a lot of correctness work behind them, and two
protocols deliberately left out.

### Added
- **Native adapters for four more protocol families**, joining LifeSpan and
  generic FTMS:
  - **WalkingPad\* / KingSmith (WiLink)** — A1, A1 Pro, C2, R1 Pro, P1.
  - **Urevo\* E1L** — the pad broadcasts FTMS too, but only its own protocol
    reports steps.
  - **Sperax\* RM01 / RM-02** — speed and steps; the protocol carries no
    distance and Trot does not invent one.
  - **PitPat\* / Deerrun\* / SupeRun\*** — the OEM protocol behind a long tail
    of budget pads, across four different service layouts.
  Only LifeSpan is tested on real hardware. The rest are written from published
  reverse engineering and pinned against captured frames, so a report from a
  real machine is the most useful thing you can send.
- **Adding a treadmill is now one file plus a registration.** Device support is
  a registry of drivers behind a trait; the engine, storage and API are
  untouched by a new one. [docs/drivers/README.md](docs/drivers/README.md) is a
  full guide for contributors.
- `/api/state` gains `driver` (which adapter claimed your treadmill) and
  `steps_supported` (`null` until step data is seen, then `true` — never
  asserted false, because some devices legitimately report no steps for their
  first poll cycles). `/api/diag` gains `rejected_samples`.
- `trot --help` and the CLI are unchanged.

### Fixed
- **A device whose reported state alternated could open and close a session
  every few frames**, each one forcing a full recompute of the day under the
  database lock, and leaving session rows that survived restart and retention.
  Session detection now debounces by *time* rather than by frame count, which is
  correct across the ~20× spread in how often different protocols report.
- **`0xFFF0` is not one protocol.** At least five vendors squat that service,
  and one swaps the notify and write roles. Driver selection now verifies
  characteristic roles and the advertised name, so a Urevo or Deerrun pad can no
  longer be driven with LifeSpan opcodes. A role-checked fallback keeps working
  for consoles whose name we don't recognise.
- **Devices that report indications rather than notifications** are no longer
  refused. This could have stopped a working treadmill from connecting after an
  upgrade, with no way to tell it apart from a switched-off machine.
- A malformed frame can no longer overflow the distance conversion, and counter
  values are rate-gated before they reach storage.
- The FTMS adapter was written from the specification and never tested against
  hardware; it now handles the things real devices actually do — staggered
  subscription, status notifications, and the KingSmith step extension behind a
  device check.

### Removed
- **The FitShow and KingSmith app-cipher adapters**, deliberately, before their
  first release. Both worked. Both rested on material this project would rather
  not build on: FitShow's protocol partly traced to an unlicensed vendor
  document, and the app-cipher adapter reproduced KingSmith's own obfuscation
  tables. Every remaining adapter is built on openly licensed reverse
  engineering, credited in
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) and itemised in
  [docs/provenance.md](docs/provenance.md).

## 0.3.4

Documentation-only release: the engine is byte-for-byte the behaviour of 0.3.3.

### Fixed
- **The docs bundled inside the release archives were stale.** Each archive
  carries `README.md` and `CHANGELOG.md`, and 0.3.3's copies were built before
  notarization started working — so they still told macOS users to run
  `xattr -dr com.apple.quarantine`, which has been unnecessary since
  2026-08-07. This release ships the corrected text.

  The 0.3.3 *binaries* are unaffected and remain fully notarized; only the
  paperwork travelling with them was out of date.

## 0.3.3

### Fixed
- **macOS binaries are notarized.** Apple's Notary Service had accepted every
  submission since 2026-08-01 and left all of them `In Progress` — the hold new
  Developer ID accounts get while the service learns to recognise them. It
  cleared on 2026-08-07, retroactively, for all fourteen submissions at once.
  Nothing needed rebuilding or re-releasing: a notarization ticket binds to the
  binary's code hash and is served by Apple, so the bytes already published
  became notarized where they stood. `xattr -dr com.apple.quarantine` is no
  longer needed for any release.

### Changed
- **The macOS bundle identifier is now `dev.puchalla.trot`** (was
  `com.marcuspuchalla.trot`). Trot's home also moved to
  <https://trot.puchalla.dev>; the old address redirects permanently.

  The identifier is embedded in the binary alongside Trot's Bluetooth usage
  descriptions, so it is part of how macOS identifies Trot for privacy
  purposes. **macOS will therefore ask for Bluetooth permission once more
  after you upgrade**, and a stale "trot" entry may remain in System Settings
  › Privacy & Security › Bluetooth, which is safe to remove. This was done now,
  while Trot is young, precisely so it never has to happen again.

## 0.3.2

Step-accuracy release. Everything here was found by recomputing a real captured
day rather than by reading code, and each fix has a test that fails without it.

### Fixed
- **Steps walked before the app connected were dropped.** The rollup writer
  never banked a day's first reading, so an opening counter value — steps
  already on the belt — vanished the moment the rollup loop ran, and became
  unrecoverable once raw was pruned. The rollups now carry the baseline.
- **A sample gap longer than 180 s lost the steps accrued during it.** The
  de-glitch walk restarted with no predecessor after an outage, so the counter
  increment across the gap was never banked. The walk is now seeded from the
  last recorded value at any age.
- **Sessions could report 0 steps for a real walk.** A session records the
  telemetry that opened it, but the treadmill zeroes its counter shortly AFTER
  the belt starts — so the baseline was often the previous session's total,
  making `steps_end - start_steps` negative. On one captured day that hid 286
  steps across three sessions in `trot log`. The baseline now self-heals from
  the first post-reset reading (within 5 s of session start, so a genuinely
  adopted walk in progress is untouched), and the read paths treat an end value
  below the start as a completed reset rather than clamping to zero.

### Changed
- A one-time startup migration (`user_version` 2) recomputes every rollup bucket
  from retained raw, repairing days damaged by the pre-0.3.1 mid-bucket
  truncation, and repairs stale session baselines.
  **This only works while a damaged day's raw samples are inside the 7-day
  retention window.** Days already pruned keep their under-counted totals; a
  stale baseline that a session later outgrew is indistinguishable from genuine
  adoption without raw and is left alone.

## 0.3.1

### Fixed
- **Step counts were silently under-reported, and the gap grew all day.** The
  rollup cutoff was `now - 60s`, which lands in the middle of a minute. That
  minute was written from only the samples seen so far, then `last_rolled`
  advanced past its end so the remainder was never rolled — and because the
  upsert replaces `steps_delta` rather than adding to it, the truncated value
  was permanent. One minute was gutted per rollup run, forever. Measured on real
  data: 2688 steps of raw samples stored as 2251 (-16%), with affected buckets
  retaining ~28% of their samples. The cutoff is now aligned to a bucket
  boundary, so only complete minutes are rolled.
- A sample landing exactly on a rollup boundary was counted by neither the run
  that ended there (`ts < start`) nor the one that began there (`ts > start`).
  With one sample per second that is a guaranteed loss of a sample per boundary,
  which also under-reported running time. The lower bound is now inclusive.

## 0.3.0

Shell completions and a signed macOS build.

### Changed
- **Minimum supported Rust is now 1.85** (was 1.77), so `clap_complete` can track
  its 4.6 line rather than being pinned back to 4.5 to keep the old floor true.

### Added
- `trot completions <shell>` — shell completion for the subcommands and flags,
  so `trot da<Tab>` becomes `trot daemon`. `--install` writes the script where
  the shell will find it and guesses the shell from `$SHELL`; bash, zsh, fish,
  PowerShell and Elvish are supported. Pre-generated scripts also ship in every
  release archive under `completions/` for packagers. CI regenerates them and
  fails if they've drifted from the command tree.
- `trot --help` draws the Trot mark as ASCII art, in the logo's own colours.
  Terminal only — piped output stays clean and greppable — and it honours
  `NO_COLOR` and falls back on 16-colour terminals.
- **macOS binaries are now signed** with a Developer ID certificate and the
  hardened runtime. Notarization is wired up but **not yet working**: Apple's
  Notary Service has accepted every submission and then left it `In Progress`
  indefinitely, so a browser-downloaded archive still needs
  `xattr -dr com.apple.quarantine` for now.
  *(Resolved on 2026-08-07 — see 0.3.3. The workaround is no longer needed for
  any release, including this one.)*
- Prebuilt binaries for **Linux arm64** (Raspberry Pi, ARM servers), alongside
  macOS (Intel + Apple Silicon), Linux x86_64 and Windows x64.

## 0.2.1

### Added
- `/api/health` now reports the engine's own `version`. The desktop app ships
  the engine as a separate sidecar binary, so it can be older than the app
  bundling it — this lets a client show both without asking the user to run a
  diagnostic dump.

## 0.2.0

New device controls, plus a second audit pass that turned up a performance
problem and a couple of real bugs.

### Added
- `POST /api/connect` / `POST /api/disconnect` — reversibly drop the Bluetooth
  link while leaving the treadmill paired and the engine (and sync) running.
- `GET /api/steps/by-device` — daily step totals split by the device that
  recorded them, with `device_name` added to `/api/settings`.
- The daemon gives up auto-connecting after repeated failures and waits for a
  manual reconnect, instead of scanning forever.
- The README now documents the whole `/api` + `/ws` surface and the security
  model.

### Fixed
- **Today's totals are no longer recomputed on every Bluetooth poll.** They were
  recalculated 10–15 times a second, each time re-walking every raw sample of the
  day (~410 ms once a day had 50k samples) while holding the database lock, which
  made the engine progressively slower during a walk and stalled API reads behind
  it. Now cached for a second and invalidated on session boundaries.
- **`duration_running_s` was wrong by roughly 30×** — it converted a sample count
  to seconds with a hardcoded 2.5 s spacing that never matched the real rate.
- **A reconnect could be silently ignored.** A wake arriving in a narrow window
  was dropped, leaving the worker parked forever while `/api/connect` reported
  success.
- Raw samples are stored once a second rather than on every poll (~1M rows a day
  before), with status transitions always written through.
- A second `trot daemon` on the same data directory is now refused instead of
  both fighting over the adapter and the database.
- Failed database writes are logged instead of silently discarded, and
  `busy_timeout` lets a contended write wait rather than fail.
- The BLE worker and rollup loop are restarted if they panic; a panic can no
  longer poison a lock and take the API down with it.
- Config, snapshot and handshake files are created private (0600) *before* being
  written, and flushed to disk, closing a window where the API token was briefly
  world-readable.
- Responses carry `X-Content-Type-Options: nosniff`.

## 0.1.1

Hardening + correctness pass from a pre-launch security & code audit. No CLI or
`/ws` route/shape changes.

- **Security:** the request guard now rejects disallowed browser `Origin`s on
  every request, closing the `/ws` upgrade (which CORS does not cover) to
  cross-site pages. The daemon writes its `runtime.json` handshake (which holds
  the API token) `0600` and locks the data directory `0700` on Unix.
- **Correctness:** the analytics timeseries now de-glitches its raw tail the same
  way the daily totals do, so a single stale device frame can no longer spike a
  chart bucket. The step de-glitcher also drops a garbage stale-high opening
  frame instead of counting it as baseline steps.
- **Robustness:** config, settings, snapshot, and handshake files are written
  atomically (temp file + rename), so a crash mid-write can't leave a truncated
  file that resets your pairing/settings. `/api/data/reset` refuses to run on an
  already-empty database (no more clobbering a prior snapshot). `/api/analytics`
  rejects absurd range/resolution combinations that would ask SQLite for millions
  of buckets.
- Misleading security comments corrected to match the implementation.

## 0.1.0

First release.

- `trot daemon` — the engine: connects to your treadmill over Bluetooth Low
  Energy and serves a local **HTTP + WebSocket API**.
- `trot scan` / `pair` / `devices` / `unpair` — interactive pairing and device
  management; the daemon connects on start and disconnects cleanly on stop.
- `trot today` / `status` / `log` — read your activity straight from the terminal.
- **LifeSpan** under-desk treadmills via a native adapter; **generic FTMS**
  treadmills (walking pads and full-size) that broadcast the standard profile.
- Local-first: SQLite storage, no account, no cloud, no telemetry.
- Prebuilt binaries for macOS (Intel + Apple Silicon), Windows, and Linux, with
  shell / PowerShell installers.
