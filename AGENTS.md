# Trot agent instructions

Also read CLAUDE.md for project conventions and downstream documentation duties.

## Required security review for every pull request

Before approving, merging, or releasing a pull request, review its complete diff
and relevant surrounding code for both malicious behaviour and accidental
security regressions. Re-review changes after every new push and record the
reviewed head commit. A CLA checkbox, trusted author, or passing CI is not a
security review.

- Protect users' computers and phones: trace untrusted input into process
  execution, shell commands, filesystem paths, downloads, updates, native
  bridges, IPC, and network requests. Check for code injection, arbitrary file
  access, credential theft, unexpected telemetry/exfiltration, and weakened
  authentication, authorization, encryption, or platform permissions.
- Inspect dependencies and lockfiles, build/install hooks, CI workflows,
  release/signing scripts, native plugins, generated code, and bundled
  binaries/WASM when affected. Establish provenance for opaque artifacts;
  unexplained executable content blocks approval.
- Trot and Nowhere observe treadmills; they never control them. Trace every
  added or changed BLE/GATT write, including Rust and web Bluetooth paths.
  Document why each command only requests telemetry. Reject belt start/stop,
  speed, incline, mode changes, control-enabling unlocks, and unverified
  commands. Check that malformed device data or user input cannot transform
  a telemetry query into an actuating command. Treat incoming BLE frames as
  hostile: check bounds, lengths, checksums, resource limits, and failure paths.
- Read untrusted PR code before running it. Use an isolated scratch checkout
  and disposable data for tests; do not expose credentials, signing keys,
  production services, personal databases, or connected treadmills to
  unreviewed code. Never execute PR-controlled code in a privileged
  pull_request_target workflow.
- Check migrations, imports, and sync for data loss and cross-account access.
  Verify relevant changes against disposable copies of older databases and
  test malformed input and negative cases, not only happy paths.
- Record findings with file/line, concrete scenario, evidence, and remediation.
  Mark traced or reproduced findings CONFIRMED and inferences PLAUSIBLE.
  State what was checked and what remains unverified; do not claim that a
  review proves the absence of malicious code. Block approval on unresolved
  unsafe behaviour or unexplained executable changes.
- Review contributor consent and third-party provenance separately. Retain
  the author's actual acceptance, immutable agreement version, and covered
  contribution commits. Never accept on an author's behalf or assume consent
  to old terms covers newer terms.

