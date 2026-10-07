<!-- Thanks for contributing. Keep this short — a sentence or two is fine. -->

## Contributor Licence Agreement

- [ ] I have read and agree to the [Trot CLA version 2](https://github.com/marcuspuchalla/trot/blob/main/CLA.md) (7 October 2026), including proprietary Nowhere use and sublicensing/transfer to its operating company. I have authority to grant these rights.

Third-party material or additional authors (identify sources/licences and who
must separately consent; write “none” if applicable):

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
