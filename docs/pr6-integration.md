# PR #6 integration — 20 November 2026

Reviewed original head: d76962df960162fd9cc6146b35c47ca18d4ee898.
Reviewed corrected implementation: efa068b3d24874b50a1a527ab81f6662d86232e3.
The maintainer elected to include this corrected implementation without waiting
for another contributor response. Original authorship is retained in history.

CONFIRMED: the full diff adds no GATT writes, dependencies, workflow changes,
process execution, remote-code loading, permission changes, or database changes.
Incoming frame fields do not supply the fixed telemetry-query opcode/payload.
Length and checksum validation precede access to the gated extended step field.
The registry regression covers the cached Mindtree-HID name and wrong GATT shapes.
No suspicious executable behaviour was found in this scoped review; this does
not prove the absence of vulnerabilities throughout the software.

CONFIRMED: the contributor's original PR body explicitly accepts the then-current
CLA in CONTRIBUTING.md at commit 3767756efa38577011b4065d1c721a7578788a4a.
That agreement includes GPL distribution and a grant to Marcus to use, modify
and distribute under other licences. It is not acceptance of CLA version 2.
The new consent request remains unanswered; no maintainer has checked a box or
accepted on the contributor's behalf. This integration does not assert new
sublicensing/transfer permissions or change the pinned engine in Nowhere.

PLAUSIBLE: another model could share the observed header, firmware and byte-24
signature while giving the tail a different meaning. The narrowed decoder has
capture/regression coverage but no new physical-hardware validation. Keep the
compatibility caveat; do not represent the maintainer's build as hardware-tested.

Original PR: https://github.com/marcuspuchalla/trot/pull/6
Corrected GPL test release: https://github.com/marcuspuchalla/trot/releases/tag/v0.5.3-pr6.1
