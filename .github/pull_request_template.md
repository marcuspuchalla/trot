<!-- Thanks for contributing. Keep this short — a sentence or two is fine. -->

## Contributor Licence Agreement

<!-- This is the one box that cannot be skipped. Everything else here is a
     checklist; this is the term the project depends on. -->

- [ ] I have read the [Contributor Licence Agreement](../blob/main/CONTRIBUTING.md#contributor-licence-agreement)
      and I agree to it.

<!-- In short: you keep your copyright; your contribution ships GPLv3 to every
     user of Trot and that cannot be revoked; and you grant Marcus Puchalla a
     licence to also distribute it under other licences. That last point is what
     lets the same engine run inside Nowhere on iOS and Android, where the
     platform forbids the separate-process split used on desktop. Without it
     from every contributor, that stops being possible for anyone. The full
     reasoning is in CONTRIBUTING.md — please read it rather than just ticking. -->

## What this changes

<!-- And why. If it fixes an issue: "Fixes #123". -->

## How it was checked

<!-- Tests you added, or how you verified it by hand. If you tested against a
     real treadmill, say which one — that's the part we can't reproduce. -->

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets` is clean (CI treats warnings as errors)
- [ ] `cargo fmt --all --check` passes

## Anything worth flagging

<!-- Delete what doesn't apply. -->

- [ ] Changes the `/api` or `/ws` surface — that's a public contract; adding is
      fine, changing or removing is breaking
- [ ] Changes CLI commands or flags — regenerate `completions/` (CI diffs them)
- [ ] Touches the de-glitching in `db.rs` — please add the data shape that broke
- [ ] Adds a network dependency — Trot is local-first, so this needs discussing
- [ ] Adds a BLE write to a driver — writes that *query* the device are fine;
      writes that actuate the belt will be declined (Trot observes, never
      controls — see docs/drivers/README.md)
