# SupeRun BA09-B test build — 0.5.3-pr6.1

This GPL prerelease is for the PR #6 contributor's SupeRun BA09-B, firmware 37.
It is not a stable release or a Nowhere update. It contains the original author's
three commits plus a narrower decoder and regression tests. The extra signature
is empirical, not a proven unique model identifier. Hardware validation of this
revision remains outstanding. Do not use it to establish support for other pads.

## Linux testing

Download the Linux archive matching `uname -m` (x86_64 or aarch64), verify the
supplied SHA-256 checksum, and extract it. Run the extracted executable directly;
do not use the stable installer. Quit Nowhere and other treadmill apps and stop
any existing Trot daemon first. These examples assume the extracted binary is
`./trot` in the current directory.

Keep test history separate from your usual database. In the first terminal:

```sh
export TROT_DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/trot-pr6.XXXXXX")"
printf 'Test data directory: %s\n' "$TROT_DATA_DIR"
./trot --version
./trot scan --all
./trot daemon
```

The version must be `0.5.3-pr6.1`. Select your pad during the scan. `--all` is
intentional: the cached `Mindtree-HID` name alone is not an advertised-name match.
GATT discovery after selection must choose `pitpat`. In a second terminal, set
`TROT_DATA_DIR` to the exact printed directory before running the same binary:

```sh
export TROT_DATA_DIR="/paste/the/exact/printed/directory"
./trot diagnose --duration 180 --output ba09-pr6-first.zip
```

Walk at your normal comfortable speed using the treadmill's own controls. Note
console steps (or a manual count), speed, elapsed time, and distance at the start
and end. Include a normal belt stop and restart to check counter resets. After
the capture, use `./trot today` and `./trot log` in that same terminal to inspect
recorded totals. A diagnostic ZIP alone is not evidence that totals are correct.

Stop the daemon with Ctrl+C and start `./trot daemon` again in the first terminal,
without deleting the test directory or re-pairing. Check reconnection after BlueZ
has cached `Mindtree-HID`, and capture another report with a new output filename.
Keep the directory for comparison; your normal history remains separate.

Report the OS/architecture, firmware, successful or failed reconnection,
expected versus recorded counts, and console readings. Review the ZIP before
sharing: raw BLE frames and names may identify the device. Share `summary.txt`
first if unsure. Do not share your database, credentials, or encryption keys.

## Review scope

CONFIRMED by source review: the patch adds no BLE writes, dependencies, install
hooks, subprocesses, network destinations, permissions, database/schema changes,
or workflow changes. PitPat writes remain the fixed status query, optionally
wrapped in the existing transport envelope; incoming telemetry does not supply
the opcode or payload. The parser checks minimum length and checksum before
reading the tail and only accepts the captured 60-byte signature for that field.
Registry tests cover the initial and cached names plus rejected GATT shapes.

PLAUSIBLE limitation: another model could share this signature but assign a
different meaning to the tail. This test build does not establish otherwise.
The byte-24 discriminator can also reject a real BA09-B variant; captures will
help determine that. No physical treadmill was used by the maintainer for this
build. Contributor permission for proprietary distribution is a separate matter;
this build is distributed under Trot's GPL licence only.
