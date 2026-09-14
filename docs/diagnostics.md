# Collecting a treadmill diagnostic

Available in Trot 0.5.1 and later. This collects evidence, not a compatibility fix.

```sh
trot --version
trot diagnose --duration 90 --output trot-diagnostic.zip
```

On Windows, `trot.exe` works identically; if the executable is in the current
PowerShell directory, use `./trot.exe`. Keep the terminal open until the command
prints the saved path. You can use the treadmill normally during capture; Trot
never starts/stops the belt or changes its speed or incline.

When a compatible daemon is running in the CLI's data directory, capture attaches
without reconnecting or interrupting normal recording. It reports the actual
engine version, which can differ from the CLI/Nowhere app version. An old daemon
produces an explanatory report: update/restart it normally or quit Nowhere and
stop the old `trot daemon` before rerunning. A daemon outside that data directory
may not be discoverable; close other treadmill clients before standalone capture.

Without a daemon, the command scans for five seconds and lets you pick a device,
including devices no driver recognises. Selection is temporary: it does not pair
persistently, open/migrate your workout DB or start sync. One adapter (the first
reported by the backend, as in normal Trot) is used. Unrecognised fallback devices
receive inventory only; diagnostic mode does not force protocols or guess writes.

For a device with a unique advertised name, including the DT3-BT issue:

```sh
trot diagnose --device "LifeSpan" --duration 90 --display-unit mph --output dt3-diagnostic.zip --note "DT-3-BT; console shows mph; observed speed/time/steps/distance: ..."
```

Use `--display-unit "km/h"` if that is the console's actual setting. `--device`
requires standalone mode; an attached capture always observes the engine's
current device and settings. Duplicate names require the picker or an exact
platform ID. `--non-interactive` skips the picker and requires an explicit target
in standalone mode. `--inventory` scans/discovers GATT but does not subscribe or
send telemetry queries. There is no arbitrary hex-write/forced-driver option.

## What to share

Review the ZIP, then attach it to your issue. It contains:

- `summary.txt`: mode, versions, observed notification/sample/error counts and
  collection failures. Zero decoded samples despite notifications is a useful result.
- `manifest.json`: engine build identity, coverage/limitations, selected-device
  information in capture events, limits and optional user-supplied observations.
- `trace.jsonl`: ordered JSON events, exact bytes and lengths, characteristic UUIDs,
  write outcomes, driver decisions, decoded samples and attached published state.

Include the make/model, firmware if known, Nowhere version, actual console units,
and displayed speed, elapsed time, steps and distance during capture. Observations
at two points can help determine scale. Mark approximate times; these are human
observations, not measurements made by Trot.

No database, activity-history export, account credentials, encryption keys,
launch tokens or environment dump is included. However, selected-device names,
raw manufacturer/service payloads, BLE replies and backend errors may contain
serial numbers or other identifying information. `--note` is included verbatim
as JSON data. Do not put secrets there. This is not a guaranteed anonymous archive.
Share `summary.txt` first if unsure about posting raw data publicly. No automatic
upload occurs.

## Limits and failure handling

Duration is 1–600 seconds; allow additional time for interactive selection and
bounded cleanup/export. Capture is limited to 16 MiB of event JSON or 40,000 events;
individual raw values are capped at 4,096 bytes and marked if truncated. A full or
busy collector drops events rather than blocking BLE; drop counts are exported.
The first retained events are not overwritten. Reports with loss are incomplete.

A `.partial.json` checkpoint is written during capture and retained if archive
creation fails. Ctrl+C normally finalizes the archive and disconnects a standalone
collector. A hard kill can leave only the latest checkpoint (or an incomplete
checkpoint if killed during writing). Existing output files are never overwritten;
choose a new filename when repeating a capture. Disk-full errors cannot guarantee
successful finalization.

Failure to capture telemetry still produces a report when export is possible.
Check the summary and manifest rather than treating creation of a ZIP as evidence
that the treadmill works. The initial release uses plain ZIP storage for wide
compatibility; it does not compress or silently redact unknown protocol bytes.

Timestamps reflect application observation/dequeue, not radio transmission.
A timed-out/cancelled write may still have reached the device. LifeSpan response
association is inferred from the last poll, because the reply does not echo its
opcode. RX events are recorded before the driver drains/filters them, but this is
not a Bluetooth radio/HCI sniffer and does not capture packets hidden by the OS.

Attached captures can begin mid-session: startup/subscriptions may be unobserved,
and the initial decoder/gate state is not a complete replay seed. Match inventory
at attachment is explicitly labelled as cached from the last discovery. Detailed
decode-error events currently cover LifeSpan; raw transport captures cover all
six built-in drivers. Full offline replay, app UI tracing and sync diagnostics are
future extensions, not capabilities claimed by this release.
