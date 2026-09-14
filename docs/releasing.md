# Releasing

Trot and Nowhere are separate programs with separate release pipelines, joined
by one optional hand-off. This is how both work.

## The short version

1. Write what changed under `## Unreleased` in `CHANGELOG.md`, as you make the
   change.
2. Actions → **Cut release** → pick `patch` / `minor` / `major` → Run.
3. Done. Everything else — tests, five platform builds, signing, notarization,
   the GitHub Release — already happens on its own.

If the engine change matters to Nowhere users, set **Also update Nowhere** in
step 2 and fill in the three prose fields. Otherwise leave it at `no`.

## What "Cut release" actually does

It promotes `## Unreleased` to a version heading, bumps both crates, refreshes
`Cargo.lock`, commits, tags, and pushes. The tag push is the trigger the
existing `release.yml` (dist) has always used, so nothing about the build
pipeline changed — the same test gate, the same five targets, the same
codesigning, the same post-announce notarization.

**It will not write your changelog.** If `## Unreleased` is empty, the workflow
fails before doing anything else. This is on purpose. The 0.3.5 entry explains
that `0xFFF0` is squatted by at least five vendors and that one of them swaps
the notify and write roles — no tool that reads commit messages produces that
sentence, and a release note that says `fix: correct FFF0 matching` is worse
than the one you would have written. The version number automates. The prose is
yours.

## The hand-off to Nowhere

Nowhere bundles `trot` as a sidecar binary, so a new engine means a new app
build. Which engine an app contains is recorded in `.trot-version` in the
nowhere repo, and `desktop.yml` checks the engine out at exactly that tag —
so rebuilding a Nowhere tag a year from now produces the same app. Before this
was pinned, CI took whatever `main` happened to be at build time and the
release recorded nothing about it.

"Cut release" can dispatch to that repo, and there are two outcomes:

| You supply | What happens |
|---|---|
| title + summary + why | Nowhere bumps, tags and **publishes** — no further input |
| any of them blank | Nowhere opens a **pull request** with the bump done and the notes stubbed |

The split exists because Nowhere's `changelog.json` structurally requires a
`summary` and a `why` on every entry — the app shows them to users in its News
section. Neither workflow is permitted to invent them, so if you have not
written them, you get a PR to write them in rather than a release with
`TODO` in it. Choosing `pull request` explicitly always gives you the PR.

## Secrets this needs

| Secret | Where | Scope |
|---|---|---|
| `RELEASE_PAT` | trot | fine-grained PAT, `contents: write` on **trot and nowhere** |
| `RELEASE_PAT` | nowhere | fine-grained PAT, `contents: write` + `pull-requests: write` on nowhere |
| `TROT_REPO_TOKEN` | nowhere | existing — read access to trot, for the engine checkout |

These cannot be the built-in `GITHUB_TOKEN`. GitHub deliberately does not fire
workflows for pushes made with it, so a tag pushed that way would be created
and then silently build nothing. It is the same rule that forces
`notarize-macos.yml` to run as a dist post-announce job rather than
`on: release` — see the comment in `dist-workspace.toml`.

## Where it reaches

One release tag in the nowhere repo builds everything: the desktop bundles for
macOS, Linux and Windows via `desktop.yml`, **and** a TestFlight upload via
`ios-testflight.yml`. Both read the engine version from `.trot-version`, as does
the Xcode Cloud post-clone script, so all three build paths of a given commit
contain the same engine.

macOS bundles from CI are signed with the Developer ID certificate, notarized,
and stapled — both the `.app` and the disk image, since Tauri notarizes the app
and then builds the image around it, leaving the image without a ticket of its
own. The repo variable `REQUIRE_SIGNED_MACOS=true` makes an unsigned macOS build
a hard failure rather than a warning.

That guard is worth its keep. A build that signs but skips notarization still
exits 0, emitting one warning line in a long log while producing a `.dmg`
Gatekeeper rejects on every install. Nowhere 0.1.13 was built that way locally:
the App Store Connect key was present but named `NOWHERE_API_*` while Tauri only
reads `APPLE_API_*`, so it signed, warned once, and carried on. The names are
mapped now, and `desktop.yml` verifies the finished bundle rather than trusting
the exit code.

`desktop.yml` also pre-flights the certificate before building anything: it
imports it into a throwaway keychain and fails in seconds, by name, if the
password is wrong or if an **Apple Distribution** certificate was exported where
a **Developer ID Application** one was needed. Those two look nearly identical in
Keychain Access and only the latter can sign software distributed outside the
App Store.

This matters because the failure is quiet by nature. A build that signs but
skips notarization still exits 0 — it emits one warning line in a long log and
produces a `.dmg` Gatekeeper rejects on every install. That is exactly what
happened building 0.1.13 locally: the App Store Connect key was present but
named `NOWHERE_API_*` while Tauri only reads `APPLE_API_*`, so notarization was
skipped silently. `scripts/build-macos.sh` now maps the names and notarizes the
disk image itself, which Tauri does not do.

### Preserve the publication gate

`release.yml` has an explicit `host` dependency on `custom-test` and requires
both artifact build jobs to succeed. cargo-dist 0.32 generated a host condition
that allowed skipped builds after failed tests and published an empty 0.5.1
release. `allow-dirty = ["ci"]` preserves this correction. If regenerating CI,
reapply the dependency and success conditions before committing.
