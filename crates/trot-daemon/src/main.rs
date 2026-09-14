//! `trot` — the honest little treadmill tracker.
//!
//! `trot daemon` runs the tracking engine (talks to the treadmill, serves the
//! local API). The other subcommands are thin terminal clients that read the
//! daemon's handshake file and query that API — so the CLI and any UI see the
//! exact same data.

mod diagnose;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

/// The mark — the runner on a treadmill — as a pixel map.
///
/// Lifted straight from the real logo (`docs/brand/app-icon.svg`, the same art
/// the README and the website use) rather than redrawn, so the CLI can't drift
/// from the brand. One character per pixel: `.` is transparent, `1`–`4` index
/// `PALETTE`. 40×31, which becomes 40×16 on screen because half-block characters
/// stack two pixel rows in one text row — and 40 columns fits an 80-column
/// terminal.
const PIXELS: &str = "\
.................1111111................\n\
................111111111...............\n\
................112111111...............\n\
..............44211111..1...............\n\
..............44211111..1...............\n\
................111111111...............\n\
................111111111...............\n\
.................1111111................\n\
...........1111111222................111\n\
..........111111111111...11........11111\n\
..........111.21111111..111...1111111111\n\
.........111.22111111111111...111111111.\n\
.........111.2111111111111....11111121..\n\
.........11141111111.1112.........321...\n\
............2111111...............221...\n\
............3211112...............113...\n\
............2221124111............113...\n\
...........111211211111..........2113...\n\
.......11.11112...21111..........111....\n\
......11111111......111..........121....\n\
......1211111.......211..........121....\n\
......11............222..........12.....\n\
.....121...........411111.......121.....\n\
......11...........411111.......122.....\n\
....................1111........122.....\n\
..111111111111111111111111111111122111..\n\
..111111111111111111111111111112221111..\n\
1144433333334443333333333344444224444411\n\
2244433333334443333333333344443244444422\n\
..222222222222222222222222222222222222..\n\
..222222222222222222222222222222222222..";

/// The logo's four greens, in the order `PIXELS` indexes them: highlight,
/// phosphor, mid, shadow. These are the exact values from the SVG, which is what
/// gives the ASCII art the same gradient as the mark everywhere else.
const PALETTE: [(u8, u8, u8); 4] = [
    (0xB7, 0xFF, 0x5A),
    (0x9B, 0xF4, 0x3E),
    (0x6D, 0xDB, 0x2D),
    (0x31, 0x94, 0x23),
];

/// How much colour the terminal will take.
enum Colour {
    /// 24-bit: the logo's real gradient.
    True,
    /// One green. Better than nothing on a 16-colour terminal.
    Basic,
    /// Block characters only — `NO_COLOR`, or output that isn't a terminal.
    None,
}

fn colour_support() -> Colour {
    // https://no-color.org — any value means "don't".
    if std::env::var_os("NO_COLOR").is_some() {
        return Colour::None;
    }
    match std::env::var("COLORTERM").as_deref() {
        Ok("truecolor") | Ok("24bit") => Colour::True,
        _ => Colour::Basic,
    }
}

/// Draw the mark with half blocks: `▀` fills the top half of a cell, so setting
/// the foreground to the upper pixel and the background to the lower one paints
/// two pixels per character. Transparent pixels are left as the terminal's own
/// background rather than painted black, so the art sits on whatever theme the
/// user runs.
fn render_logo(colour: &Colour) -> String {
    let rows: Vec<&[u8]> = PIXELS.lines().map(|l| l.as_bytes()).collect();
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let px = |y: usize, x: usize| -> Option<usize> {
        rows.get(y)
            .and_then(|r| r.get(x))
            .filter(|c| **c != b'.')
            .map(|c| (c - b'1') as usize)
    };

    let mut out = String::new();
    for y in (0..rows.len()).step_by(2) {
        // Pixel art runs in long blocks of one colour, so only emit an escape
        // when the pair actually changes — otherwise the banner is mostly ANSI
        // by weight. Reset at end of line so nothing leaks into clap's output.
        let mut current: Option<(Option<usize>, Option<usize>)> = None;
        for x in 0..width {
            let cell = (px(y, x), px(y + 1, x));
            let glyph = match cell {
                (None, None) => ' ',
                (Some(_), Some(_)) => '\u{2580}', // upper half block
                (Some(_), None) => '\u{2580}',
                (None, Some(_)) => '\u{2584}', // lower half block
            };
            if cell == (None, None) {
                if current.is_some() {
                    if !matches!(colour, Colour::None) {
                        out.push_str("\x1b[0m");
                    }
                    current = None;
                }
                out.push(glyph);
                continue;
            }
            if current != Some(cell) {
                match colour {
                    Colour::True => {
                        out.push_str("\x1b[0m");
                        let (top, bottom) = cell;
                        if let Some(t) = top {
                            let (r, g, b) = PALETTE[t];
                            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                        }
                        if let Some(b_) = bottom {
                            let (r, g, b) = PALETTE[b_];
                            // Only paint a background when the glyph's other half
                            // is actually filled; `▄` carries its own colour.
                            if top.is_some() {
                                out.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
                            } else {
                                out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                            }
                        }
                    }
                    Colour::Basic => out.push_str("\x1b[0m\x1b[92m"),
                    Colour::None => {}
                }
                current = Some(cell);
            }
            // With both halves lit and no colour to distinguish them, a full
            // block is the honest rendering.
            out.push(match (colour, cell) {
                (Colour::True, _) => glyph,
                (_, (Some(_), Some(_))) => '\u{2588}',
                _ => glyph,
            });
        }
        if current.is_some() && !matches!(colour, Colour::None) {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

/// The `--help` banner.
///
/// Drawn only when stdout is a terminal, so `trot --help | grep` and anything
/// scripted still get clean, greppable text. The house rule is that output is
/// data; the personality lives in `--help` and nowhere else.
fn help_banner() -> String {
    if !std::io::stdout().is_terminal() {
        return String::new();
    }
    render_logo(&colour_support())
}

#[derive(Parser)]
#[command(
    name = "trot",
    version,
    about = "TROT — it's really only treadmilling."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the tracking daemon (talks to the treadmill, serves the API).
    Daemon,
    /// Record a bounded BLE support bundle (no upload or workout database).
    Diagnose(diagnose::Options),
    /// Today's totals.
    Today,
    /// Whether the daemon is up and a treadmill is connected.
    Status,
    /// Recent sessions.
    Log {
        /// Only sessions from the last 7 days.
        #[arg(long)]
        week: bool,
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Scan for nearby treadmills and pick one to pair (interactive).
    Scan {
        /// How long to scan for, in seconds (1–15).
        #[arg(long, default_value_t = 6.0)]
        seconds: f64,
        /// Show every Bluetooth device, not just treadmills.
        #[arg(long)]
        all: bool,
        /// Just print the list; don't show the interactive picker.
        #[arg(long)]
        list: bool,
    },
    /// List paired treadmills (the active one is marked with *).
    Devices,
    /// Pair a treadmill from `trot scan` and make it the active one.
    Pair {
        /// The device id printed by `trot scan`.
        device_id: String,
        /// A friendly name to remember it by.
        #[arg(long)]
        name: Option<String>,
    },
    /// Forget the active treadmill.
    Unpair,
    /// Print (or install) a shell completion script.
    ///
    /// Generated from this binary's own command tree, so it can never describe a
    /// flag that doesn't exist.
    Completions {
        /// bash · zsh · fish · powershell · elvish. Guessed from $SHELL if omitted.
        shell: Option<clap_complete::Shell>,
        /// Write it where the shell will find it, instead of printing to stdout.
        #[arg(long)]
        install: bool,
    },
}

/// Where TROT keeps its database + handshake file. Override with `TROT_DATA_DIR`.
fn data_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("TROT_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let pd = directories::ProjectDirs::from("", "", "trot")
        .context("could not determine a platform data directory")?;
    Ok(pd.data_dir().to_path_buf())
}

fn main() -> Result<()> {
    // Built from the derived command rather than `Cli::parse()` so the banner can
    // be decided at runtime (is this a terminal? is NO_COLOR set?), which a
    // static `before_help` attribute cannot do. Setting it to an empty string
    // still costs a blank line, so when there's no banner we don't set it.
    let mut cmd = Cli::command();
    let banner = help_banner();
    if !banner.is_empty() {
        cmd = cmd.before_help(banner);
    }
    match Cli::from_arg_matches(&cmd.get_matches())?.cmd {
        Cmd::Daemon => run_daemon(),
        Cmd::Diagnose(options) => diagnose::run(options),
        Cmd::Today => cmd_today(),
        Cmd::Status => cmd_status(),
        Cmd::Log { week, limit } => cmd_log(week, limit),
        Cmd::Scan { seconds, all, list } => cmd_scan(seconds, all, list),
        Cmd::Devices => cmd_devices(),
        Cmd::Pair { device_id, name } => cmd_pair(device_id, name),
        Cmd::Unpair => cmd_unpair(),
        Cmd::Completions { shell, install } => cmd_completions(shell, install),
    }
}

// ── shell completions ───────────────────────────────────────────────────────

/// Where a completion script has to live for each shell to find it, relative to
/// $HOME. Returns `None` for shells with no conventional drop-in directory —
/// PowerShell and Elvish want the script sourced from a profile instead, so
/// `--install` tells the user what to do rather than guessing at their profile.
fn completion_path(shell: clap_complete::Shell) -> Option<(PathBuf, &'static str)> {
    use clap_complete::Shell;
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(match shell {
        // XDG-style drop-in that bash-completion has loaded on demand since 2.9.
        Shell::Bash => (
            home.join(".local/share/bash-completion/completions/trot"),
            "Restart your shell (bash-completion picks this up automatically).",
        ),
        // zsh loads any `_name` file that sits on $fpath.
        Shell::Zsh => (
            home.join(".zfunc/_trot"),
            "Add this to ~/.zshrc if it isn't there yet:\n  \
             fpath=(~/.zfunc $fpath)\n  autoload -Uz compinit && compinit",
        ),
        Shell::Fish => (
            home.join(".config/fish/completions/trot.fish"),
            "Fish loads this on its own — start a new shell.",
        ),
        _ => return None,
    })
}

/// Guess the shell from `$SHELL` so `trot completions --install` usually needs
/// no argument. Only ever a convenience: if the guess fails we ask rather than
/// writing a bash script into someone's zsh.
fn detect_shell() -> Option<clap_complete::Shell> {
    let sh = std::env::var("SHELL").ok()?;
    let name = sh.rsplit('/').next()?;
    name.parse::<clap_complete::Shell>().ok()
}

fn cmd_completions(shell: Option<clap_complete::Shell>, install: bool) -> Result<()> {
    let shell = match shell.or_else(detect_shell) {
        Some(s) => s,
        None => anyhow::bail!(
            "could not tell which shell you're using — name it, e.g. `trot completions zsh`"
        ),
    };
    let mut cmd = Cli::command();

    if !install {
        clap_complete::generate(shell, &mut cmd, "trot", &mut std::io::stdout());
        return Ok(());
    }

    let (path, hint) = match completion_path(shell) {
        Some(p) => p,
        None => anyhow::bail!(
            "{shell} has no standard completions directory — write the script into your \
             profile instead:\n  trot completions {shell} >> <your profile>"
        ),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    // Generate into memory first so a write failure can't leave a half-written
    // script that the shell would then try to source.
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut cmd, "trot", &mut script);
    trot_core::config::atomic_write(&path, &script)
        .with_context(|| format!("could not write {}", path.display()))?;

    println!("Installed {shell} completions to {}", path.display());
    println!("{hint}");
    Ok(())
}

// ── daemon ──────────────────────────────────────────────────────────────────

fn run_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Refuse to start a second daemon on the same data directory. Two daemons
    // would fight over the Bluetooth adapter, both overwrite runtime.json (so the
    // CLI would reach only one of them), and interleave writes into the same
    // SQLite file. We probe for a *live* daemon rather than just a lock file, so a
    // stale handshake left by a crash never blocks a legitimate restart.
    if let Some((port, _)) = live_daemon() {
        anyhow::bail!(
            "a trot daemon is already running on this data directory \
             (API on 127.0.0.1:{port}).\n\
             Stop it first, or point this one somewhere else with TROT_DATA_DIR."
        );
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let dir = data_dir()?;
        let engine = trot_core::start_engine(dir.clone()).await?;

        // Publish {port, token} so the CLI and Nowhere can reach the daemon. This
        // file holds the API token; `atomic_write` creates it 0600 before writing
        // a byte, so it is never briefly world-readable on a shared host.
        let runtime = serde_json::json!({ "port": engine.port, "token": engine.state.token });
        let runtime_path = dir.join("runtime.json");
        if let Err(e) =
            trot_core::config::atomic_write(&runtime_path, &serde_json::to_vec_pretty(&runtime)?)
        {
            tracing::warn!("could not write handshake file: {e}");
        }
        tracing::info!(
            "trot daemon ready — API on 127.0.0.1:{} (handshake: {})",
            engine.port,
            runtime_path.display()
        );

        wait_for_shutdown_signal().await;
        // Disconnect the treadmill cleanly before we drop the runtime — otherwise
        // the SC110 keeps the link open until it's power-cycled.
        engine.state.shutdown(Duration::from_secs(5)).await;
        let _ = std::fs::remove_file(&runtime_path);
        tracing::info!("trot daemon shutting down");
        Ok::<(), anyhow::Error>(())
    })
}

/// Resolve when the daemon should stop — Ctrl-C, SIGTERM (what `kill` and most
/// supervisors send), or our PARENT DYING. The parent arm matters for Nowhere:
/// on macOS a Cmd-Q can tear the app down without it sending us a signal or
/// killing us, orphaning the daemon with the treadmill still connected. Watching
/// for the orphan lets us disconnect cleanly instead of leaking the BLE link
/// until the treadmill is power-cycled. (Also covers a Nowhere crash.)
///
/// **Windows has no parent arm.** There is no `getppid()` equivalent, and the
/// supported way to bind a child's lifetime to its parent is for the PARENT to
/// create a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assign this
/// process to it — the OS then terminates us when the parent's handle closes.
/// That is the embedding app's job, not something the child can arrange for
/// itself. Until Nowhere does that on Windows, a parent that dies without
/// signalling leaves the daemon running and the Bluetooth link held; `trot`
/// still exits cleanly on Ctrl-C, and `POST /api/shutdown` always works.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let parent = wait_for_parent_death();
        tokio::pin!(parent);
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                    _ = &mut parent => {}
                }
            }
            Err(_) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = &mut parent => {}
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Resolves when our parent process goes away. Reparenting flips `getppid()` (an
/// orphan is adopted by init/launchd, pid 1), so we record the launching pid and
/// watch for it to change. If we were started directly by init there's no parent
/// to lose, so we never resolve.
#[cfg(unix)]
async fn wait_for_parent_death() {
    let initial = unsafe { libc::getppid() };
    if initial <= 1 {
        std::future::pending::<()>().await;
    }
    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if unsafe { libc::getppid() } != initial {
            tracing::info!("trot: parent process gone — shutting down to release the treadmill");
            return;
        }
    }
}

// ── CLI client ──────────────────────────────────────────────────────────────

/// The daemon's handshake: (port, token) from runtime.json, or None if no file.
fn runtime() -> Option<(u16, String)> {
    let path = data_dir().ok()?.join("runtime.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let port = v["port"].as_u64()? as u16;
    let token = v["token"].as_str().unwrap_or("").to_string();
    Some((port, token))
}

fn daemon_port() -> Result<u16> {
    runtime()
        .map(|(p, _)| p)
        .context("no running trot daemon — start it with `trot daemon`")
}

/// (port, token) if a daemon is actually reachable — not just a stale handshake
/// file left behind by a crash. Used to decide between the API and doing the work
/// locally: while the daemon is up it owns the Bluetooth adapter, so scanning and
/// pairing must go through it.
fn live_daemon() -> Option<(u16, String)> {
    let (port, token) = runtime()?;
    // Connection-refused on loopback returns immediately when nothing's listening.
    ureq::get(&format!("http://127.0.0.1:{port}/api/health"))
        .call()
        .ok()
        .map(|_| (port, token))
}

fn get(path: &str) -> Result<serde_json::Value> {
    let port = daemon_port()?;
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| anyhow::anyhow!("cannot reach the trot daemon: {e}"))?;
    Ok(resp.into_json()?)
}

/// POST to the daemon. Mutating routes require the per-launch token (loopback
/// Host + `x-sc110-token`), so we read both from the handshake file.
fn post(path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
    let (port, token) =
        runtime().context("no running trot daemon — start it with `trot daemon`")?;
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = ureq::post(&url)
        .set("x-sc110-token", &token)
        .send_json(body)
        .map_err(|e| anyhow::anyhow!("cannot reach the trot daemon: {e}"))?;
    Ok(resp.into_json()?)
}

/// Steps walked in a session, given its recorded start and end counter values.
///
/// A session opens on the telemetry that STARTED it, but the treadmill zeroes
/// its counter shortly after the belt begins — so the recorded start is often
/// the previous session's total. When the end is below the start, that reset
/// happened and the end value IS the session's total. Clamping the subtraction
/// to zero instead (as this used to) reports 0 steps for a real walk: on one
/// observed day that hid 286 steps across three sessions.
fn session_steps(steps_end: i64, start_steps: i64) -> i64 {
    if steps_end < start_steps {
        steps_end.max(0)
    } else {
        steps_end - start_steps
    }
}

fn fmt_dur(s: i64) -> String {
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m {sec:02}s")
    }
}

fn fmt_dist(distance_raw: i64, unit: &str) -> String {
    let meters = (distance_raw * 10) as f64; // storage keeps decameters
    if unit == "mph" {
        format!("{:.2} mi", meters / 1609.344)
    } else {
        format!("{:.2} km", meters / 1000.0)
    }
}

fn cmd_today() -> Result<()> {
    let v = get("/api/today")?;
    let t = &v["totals"];
    let unit = v["display_unit"].as_str().unwrap_or("km/h");
    println!("Today · {}", v["date"].as_str().unwrap_or(""));
    println!("  steps      {}", t["steps"].as_i64().unwrap_or(0));
    println!(
        "  distance   {}",
        fmt_dist(t["distance_raw"].as_i64().unwrap_or(0), unit)
    );
    println!(
        "  time       {}",
        fmt_dur(t["duration_s"].as_i64().unwrap_or(0))
    );
    println!("  calories   {}", t["calories"].as_i64().unwrap_or(0));
    println!("  sessions   {}", t["sessions"].as_i64().unwrap_or(0));
    Ok(())
}

fn cmd_status() -> Result<()> {
    let h = get("/api/health")?;
    println!("daemon      up");
    println!(
        "treadmill   {}",
        if h["connected"].as_bool().unwrap_or(false) {
            "connected"
        } else {
            "not connected"
        }
    );
    Ok(())
}

fn cmd_log(week: bool, limit: i64) -> Result<()> {
    let v = get(&format!("/api/sessions?limit={limit}"))?;
    let empty = vec![];
    let rows = v["sessions"].as_array().unwrap_or(&empty);
    let cutoff = if week {
        // seconds since epoch, 7 days ago
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        now - 7.0 * 86400.0
    } else {
        0.0
    };

    let mut shown = 0;
    for s in rows {
        let started = s["started_ts"].as_f64().unwrap_or(0.0);
        if started < cutoff {
            continue;
        }
        let date = s["local_date"].as_str().unwrap_or("");
        let start_steps = s["start_steps"].as_i64().unwrap_or(0);
        let steps = session_steps(s["steps_end"].as_i64().unwrap_or(0), start_steps);
        let dur = s["duration_s_end"].as_i64().unwrap_or(0);
        println!("{date}   {steps:>6} steps   {:>8}", fmt_dur(dur));
        shown += 1;
    }
    if shown == 0 {
        println!("No sessions yet. Hop on the belt.");
    }
    Ok(())
}

// ── pairing ───────────────────────────────────────────────────────────────────

fn cmd_scan(seconds: f64, all: bool, list: bool) -> Result<()> {
    // If the daemon is up it owns the adapter, so let it scan; otherwise scan
    // ourselves so you can pair before ever starting the daemon.
    let v = if live_daemon().is_some() {
        get(&format!("/api/scan?seconds={seconds}&all_devices={all}"))?
    } else {
        trot_core::config::init_paths(&data_dir()?);
        tokio::runtime::Runtime::new()?.block_on(trot_core::ble::scan(seconds, all))?
    };

    let empty = vec![];
    let devices = v["devices"].as_array().unwrap_or(&empty);
    if devices.is_empty() {
        println!("No treadmills found. Make sure it's powered on and nearby, then try again.");
        println!("(Tip: `trot scan --all` lists every Bluetooth device.)");
        return Ok(());
    }

    let name_of = |d: &serde_json::Value| match d["name"].as_str().unwrap_or("") {
        "" => "(unnamed)".to_string(),
        n => n.to_string(),
    };

    // Interactive picker by default on a real terminal; plain list when piped or
    // when --list is passed, so scripts keep working.
    if list || !std::io::stdout().is_terminal() {
        for d in devices {
            let id = d["device_id"].as_str().unwrap_or("");
            let sig = d["rssi"]
                .as_i64()
                .map(|r| format!("   {r} dBm"))
                .unwrap_or_default();
            println!("  {}{sig}", name_of(d));
            println!("      {id}");
        }
        return Ok(());
    }

    let labels: Vec<String> = devices
        .iter()
        .map(|d| {
            let sig = d["rssi"]
                .as_i64()
                .map(|r| format!("  ·  {r} dBm"))
                .unwrap_or_default();
            format!("{}{sig}", name_of(d))
        })
        .collect();

    let choice = dialoguer::Select::new()
        .with_prompt("Select a treadmill  (↑/↓ to move, Enter to pair, Esc to cancel)")
        .items(&labels)
        .default(0)
        .interact_opt()?;

    match choice {
        Some(i) => {
            let id = devices[i]["device_id"].as_str().unwrap_or("");
            let name = match devices[i]["name"].as_str().unwrap_or("") {
                "" => None,
                n => Some(n.to_string()),
            };
            do_pair(id, name)?;
        }
        None => println!("Cancelled — nothing paired."),
    }
    Ok(())
}

fn cmd_devices() -> Result<()> {
    let v = if live_daemon().is_some() {
        get("/api/devices")?
    } else {
        trot_core::config::init_paths(&data_dir()?);
        let cfg = trot_core::config::load_devices();
        serde_json::json!({
            "active": cfg.active,
            "devices": cfg.devices.iter()
                .map(|d| serde_json::json!({"id": d.id, "name": d.name}))
                .collect::<Vec<_>>(),
        })
    };

    let active = v["active"].as_str();
    let empty = vec![];
    let devices = v["devices"].as_array().unwrap_or(&empty);
    if devices.is_empty() {
        println!("No paired treadmills yet. Run `trot scan`, then `trot pair <id>`.");
        return Ok(());
    }
    for d in devices {
        let id = d["id"].as_str().unwrap_or("");
        let name = match d["name"].as_str().unwrap_or("") {
            "" => "(unnamed)",
            n => n,
        };
        let mark = if Some(id) == active { "* " } else { "  " };
        println!("{mark}{name}");
        println!("      {id}");
    }
    println!("\n* = active (the treadmill the daemon connects to)");
    Ok(())
}

fn cmd_pair(device_id: String, name: Option<String>) -> Result<()> {
    let id = device_id.trim();
    if id.is_empty() {
        anyhow::bail!("device id must not be empty — copy it from `trot scan`");
    }
    do_pair(id, name)
}

/// Pair a device and make it active. Goes through a live daemon (which connects
/// immediately) or writes the config directly when the daemon is down.
fn do_pair(id: &str, name: Option<String>) -> Result<()> {
    let label = name.clone().unwrap_or_else(|| id.to_string());
    if live_daemon().is_some() {
        post(
            "/api/pair",
            serde_json::json!({ "device_id": id, "name": name }),
        )?;
        println!("Paired \"{label}\". The running daemon is connecting to it now.");
    } else {
        trot_core::config::init_paths(&data_dir()?);
        trot_core::config::add_and_activate(id, name.as_deref());
        println!("Paired \"{label}\" and set as active.");
        println!("Start tracking with:  trot daemon");
    }
    Ok(())
}

fn cmd_unpair() -> Result<()> {
    if live_daemon().is_some() {
        post("/api/unpair", serde_json::json!({}))?;
    } else {
        trot_core::config::init_paths(&data_dir()?);
        if let Some(active) = trot_core::config::active_device_id() {
            trot_core::config::forget(&active);
        }
    }
    println!("Unpaired the active treadmill.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR escapes so two renderings can be compared on shape alone.
    fn glyphs(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        // A full block and an upper-half block occupy the same pixels; only the
        // colour path can tell them apart, so normalise before comparing.
        out.replace('\u{2588}', "\u{2580}")
    }

    #[test]
    fn logo_is_a_sane_shape() {
        let art = render_logo(&Colour::None);
        let lines: Vec<&str> = art.lines().collect();
        // 31 pixel rows, two per text row.
        assert_eq!(lines.len(), 16, "half blocks should halve 31 rows to 16");
        assert!(
            lines.iter().all(|l| l.chars().count() <= 40),
            "must stay inside an 80-column terminal"
        );
        assert!(
            art.contains('\u{2588}') || art.contains('\u{2580}'),
            "should actually draw something"
        );
    }

    /// Colour must not change the picture — only how it's painted. This is what
    /// catches a renderer that drops or shifts a cell when emitting escapes.
    #[test]
    fn every_colour_mode_draws_the_same_picture() {
        let plain = glyphs(&render_logo(&Colour::None));
        assert_eq!(glyphs(&render_logo(&Colour::Basic)), plain, "basic colour");
        assert_eq!(glyphs(&render_logo(&Colour::True)), plain, "truecolor");
    }

    /// Nothing may leak past the banner into clap's own output.
    #[test]
    fn colour_is_always_closed() {
        for mode in [Colour::True, Colour::Basic] {
            let art = render_logo(&mode);
            for line in art.lines() {
                if line.contains('\u{1b}') {
                    assert!(
                        line.trim_end().ends_with("\u{1b}[0m"),
                        "each coloured line must reset: {line:?}"
                    );
                }
            }
        }
    }

    /// The palette is the logo's, not an approximation.
    #[test]
    fn palette_matches_the_brand_svg() {
        assert_eq!(PALETTE[0], (0xB7, 0xFF, 0x5A));
        assert_eq!(PALETTE[1], (0x9B, 0xF4, 0x3E)); // phosphor, the brand accent
        assert_eq!(PALETTE[3], (0x31, 0x94, 0x23));
    }
}

#[cfg(test)]
mod session_steps_tests {
    use super::session_steps;

    #[test]
    fn a_reset_during_the_session_reports_the_end_value() {
        // Real rows from a captured day: the baseline is the PREVIOUS session's
        // total because the counter had not zeroed yet when the session opened.
        assert_eq!(session_steps(99, 432), 99);
        assert_eq!(session_steps(87, 765), 87);
    }

    #[test]
    fn a_normal_session_still_subtracts_its_baseline() {
        assert_eq!(session_steps(764, 100), 664);
        assert_eq!(session_steps(1306, 0), 1306);
    }

    #[test]
    fn nonsense_never_yields_a_negative() {
        assert_eq!(session_steps(-5, 100), 0);
    }
}
