# Contributing to Trot

Trot is a small project with a narrow purpose: read an under-desk treadmill over
Bluetooth, keep the record locally, and expose it as a plain API. Contributions
that make it do that better are very welcome.

## The most useful thing you can contribute

**An adapter for a treadmill we can't read yet.** Trot currently ships six
drivers: native adapters for LifeSpan/Omni, KingSmith WalkingPad (the WiLink
generation), Urevo, Sperax and PitPat/Deerrun/SupeRun, plus generic FTMS for
everything else. Two are tested on real hardware: LifeSpan by us, and the Urevo URTM030 by the
contributor who added it. The rest are ports of open-source reverse engineering
pinned against published captures, so hardware reports (even "it works") are
valuable on their own — the Urevo support exists because somebody sent one.
(Two further families — the app-cipher KingSmith generation and the FitShow
OEM platform — had drivers that were deliberately removed for licensing
prudence; see `docs/provenance.md` before proposing to re-add either.)

If your treadmill doesn't work, a bug report with a `trot scan --all` listing and
a `GET /api/diag` dump is genuinely valuable even if you never write a line of
code. It's the part nobody else can do for us.

Want to write the adapter yourself? All the protocol work is one
self-contained file, plus a handful of small registration edits that wire it
into the driver layer's shared guarantees — you don't need to touch the
engine, storage, or the API to add one.
**[docs/drivers/README.md](docs/drivers/README.md)** walks through the whole
thing — identifying your device, capturing raw frames, the driver trait, the
full registration list ("Register it"), testing without hardware, and a
complete worked example.

## Getting set up

```sh
git clone https://github.com/marcuspuchalla/trot
cd trot
cargo build
cargo test --workspace
```

You need **Rust 1.85 or newer**. On Linux you also need `libdbus-1-dev` and
`pkg-config` — btleplug talks to BlueZ over D-Bus.

You do **not** need a treadmill to work on most of the codebase: the protocol
decoders, storage, de-glitching, and the HTTP API are all covered by tests that
run without hardware.

## Before you open a pull request

CI runs these on Linux, macOS and Windows, and a release cannot be built unless
they pass — so it's worth running them locally first:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets   # CI treats warnings as errors
cargo test --workspace
```

If you changed the CLI's commands or flags, regenerate the completion scripts —
CI diffs them and fails if they've drifted:

```sh
cargo run --bin trot -- completions bash       > completions/trot.bash
cargo run --bin trot -- completions zsh        > completions/_trot
cargo run --bin trot -- completions fish       > completions/trot.fish
cargo run --bin trot -- completions powershell > completions/_trot.ps1
cargo run --bin trot -- completions elvish     > completions/trot.elv
```

If your change is user-visible, add a note under `## Unreleased` in
[`CHANGELOG.md`](CHANGELOG.md) in the same pull request — while you still
remember why. That section becomes the release notes verbatim; nothing here is
generated from commit messages. See [docs/releasing.md](docs/releasing.md).

## Things worth knowing about the codebase

- **Trot observes treadmills; it never controls them.** No code in this tree
  starts, stops, or changes the speed of a belt, and none ever will — that's
  a design commitment, not a missing feature. It's what lets someone run a
  daemon with Bluetooth access to the machine under their feet and know it
  cannot move that machine, whatever else goes wrong. A PR that adds belt
  control (speed, start/stop, incline, mode) will be declined, however well
  built. Writes that merely *ask* the device for data — poll frames, init
  handshakes — are fine and often required; the driver guide
  ([docs/drivers/README.md](docs/drivers/README.md)) spells out the
  distinction, which matters because most reference implementations you'd
  port from do both.
- **`/api` + `/ws` is a public contract.** Other things are built on it. Adding a
  route is easy; changing or removing one is a breaking change and needs to be
  treated as such.
- **Output is data.** CLI subcommands print their result and nothing else — no
  banners, no taglines. The one flourish lives in `--help`, deliberately.
- **The device lies.** Treadmill odometers emit stale frames, reset mid-session,
  and wrap. `db.rs` has a de-glitching accumulator with tests pinning real-world
  failure shapes. If you touch it, add the case that broke.
- **Local-first is not decoration.** No accounts, no cloud, no telemetry, and
  nothing on the landing page fetched from a third party. Changes that quietly
  add a network dependency will be declined.

## Style

Match the code around you. Comments should explain *why* something is the way it
is — especially where the reason is a hardware quirk or a platform constraint
that isn't obvious from the code.

## Licensing

Trot is **GPLv3**. Everything published here stays GPLv3, and any fork of it
stays GPLv3 — that is the point of the licence and it will not change.

### Contributor Licence Agreement

By opening a pull request you agree to the following. It is short on purpose,
and the reasoning is below it — please read that too rather than just accepting.

> 1. You certify that you wrote the contribution yourself, or otherwise have
>    the right to submit it under these terms, and that you are not knowingly
>    including anyone else's copyrighted code.
> 2. You retain copyright in your contribution. You are not signing it away.
> 3. You grant Marcus Puchalla a perpetual, worldwide, irrevocable,
>    royalty-free licence to use, modify and distribute your contribution,
>    **including under licences other than the GPL**.
> 4. You grant every recipient of Trot the same rights the GPLv3 gives them.
>    Your contribution ships under GPLv3 like the rest of the project.

**Why point 3 exists, plainly.** Trot is the engine inside
[Nowhere](https://nowhere.fitness), a desktop app that may one day be sold.
Trot and Nowhere are separate programs that talk over a local HTTP API, and
that boundary is deliberate — but keeping the option to license the engine
differently requires that one person can license all of it. Today that is true,
because every line has one author. The first contribution accepted without
point 3 would end it permanently, for everyone, forever.

**What point 3 does not do.** It does not let anyone take your work proprietary
and close the door behind them: point 4 is unconditional, so your contribution
is GPLv3 to every user of Trot, on the same terms as the rest of the code, and
that cannot be revoked. Trot itself will not stop being open source.

If you are not comfortable with this — that is a completely reasonable position
and plenty of people hold it. Open an issue instead. A protocol capture, a
`trot scan --all` listing or a `/api/diag` dump from an unsupported treadmill is
genuinely the most valuable thing anyone can contribute, and none of it needs a
CLA.

### Third-party code

**Independent implementation is the rule.** Every driver here is written
against protocol *facts* — byte offsets, checksums, framing rules —
established by reading other people's reverse engineering. Functional
protocol literals — UUIDs, opcodes, magic command frames, cipher tables,
captured frames used as test fixtures — are acceptable *when they are needed
for interoperability or testing and are credited*. Upstream implementation
logic, comments and prose are not acceptable, whatever the licence — do not
paste them into Trot. See
[`docs/licensing-analysis.md`](docs/licensing-analysis.md) for the project's
reasoning, and `docs/drivers/README.md` for how to do it properly.

If you learned something from another project, credit it in your module header
and add it to [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md) — with its
licence, and a note saying exactly what you learned. If you add a literal
third-party-derived item (a frame, a table, a capture, a name list), record it
as a row in [`docs/provenance.md`](docs/provenance.md), which explains the
format. Over-crediting is fine. Silently copying is not.

### Trademarks

The name "Trot" and the runner mark are reserved and are *not* covered by the
GPL (see the Trademarks section of the README). Fork the code freely — just give
your fork its own name.
