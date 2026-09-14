//! CLI owns local files. Daemon capture endpoints never accept filesystem paths.
use anyhow::{Context, Result};
use clap::Args;
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use trot_core::diagnostics::Capture;

#[derive(Args)]
pub struct Options {
    /// Capture duration, including discovery (1–600 seconds).
    #[arg(long,default_value_t=90,value_parser=clap::value_parser!(u64).range(1..=600))]
    duration: u64,
    /// Destination ZIP; never overwritten. A .partial.json file is retained on failure.
    #[arg(long, default_value = "trot-diagnostic.zip")]
    output: PathBuf,
    /// Exact advertised name or platform ID (standalone only; duplicate names require picker).
    #[arg(long)]
    device: Option<String>,
    /// Only scan/discover GATT, with no telemetry queries or subscriptions (standalone).
    #[arg(long)]
    inventory: bool,
    /// Do not show a picker. Requires --device when no daemon is running.
    #[arg(long)]
    non_interactive: bool,
    /// Console display unit for a standalone capture.
    #[arg(long,default_value="km/h",value_parser=["km/h","mph"])]
    display_unit: String,
    /// Optional console readings/model/firmware to accompany the trace; do not include secrets.
    #[arg(long)]
    note: Option<String>,
}

pub fn run(options: Options) -> Result<()> {
    if options.note.as_ref().is_some_and(|s| s.len() > 4096) {
        anyhow::bail!("note must be at most 4096 bytes");
    }
    // Reserve both destinations before touching BLE. Never truncate an existing report.
    let archive = private_file(&options.output)?;
    let partial = options.output.with_extension("partial.json");
    let mut journal = match private_file(&partial) {
        Ok(f) => f,
        Err(e) => {
            drop(archive);
            let _ = std::fs::remove_file(&options.output);
            return Err(e);
        }
    };
    let seed = json!({"schema":"trot.diagnostic.v1","cli_version":env!("CARGO_PKG_VERSION"),"status":"started; capture may be incomplete","events":[]});
    serde_json::to_writer_pretty(&mut journal, &seed)?;
    journal.flush()?;
    eprintln!("Capturing BLE diagnostics for up to {} seconds. No upload or history export.\nSelected device names, raw BLE bytes and backend errors can contain identifying data. Review before sharing.",options.duration);
    let rt = tokio::runtime::Runtime::new()?;
    let mut report = if let Some((port, token)) = super::live_daemon() {
        if options.device.is_some() || options.inventory {
            json!({"schema":"trot.diagnostic.v1","events":[],"error":"A daemon is running. Stop it normally before selecting a standalone target or using --inventory; it was not stopped automatically."})
        } else {
            rt.block_on(attached(port, &token, options.duration, &mut journal))
        }
    } else {
        rt.block_on(standalone(&options, &mut journal))
    };
    report["cli_version"] = json!(env!("CARGO_PKG_VERSION"));
    report["console_observations"] =
        json!({"source":"user supplied, not verified","note":options.note});
    checkpoint(&mut journal, &report)?;
    let summary = summary(&report);
    let mut zip = zip::ZipWriter::new(archive);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("summary.txt", opts)?;
    zip.write_all(summary.as_bytes())?;
    let events = report["events"].take();
    zip.start_file("trace.jsonl", opts)?;
    if let Some(events) = events.as_array() {
        for event in events {
            serde_json::to_writer(&mut zip, event)?;
            zip.write_all(b"\n")?;
        }
    }
    zip.start_file("manifest.json", opts)?;
    serde_json::to_writer_pretty(&mut zip, &report)?;
    zip.finish()?.sync_all()?;
    drop(journal);
    let _ = std::fs::remove_file(partial);
    println!("{summary}\nSaved: {}", options.output.display());
    Ok(())
}
fn private_file(path: &std::path::Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).with_context(|| {
        format!(
            "cannot create {}; choose a new --output filename",
            path.display()
        )
    })
}
fn checkpoint(file: &mut std::fs::File, report: &Value) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer_pretty(&mut *file, report)?;
    let len = file.stream_position()?;
    file.set_len(len)?;
    file.flush()?;
    Ok(())
}
fn request(port: u16, token: &str, method: &str, path: &str) -> ureq::Request {
    ureq::request(method, &format!("http://127.0.0.1:{port}{path}"))
        .set("x-sc110-token", token)
        .timeout(Duration::from_secs(10))
}
async fn attached(port: u16, token: &str, seconds: u64, journal: &mut std::fs::File) -> Value {
    let start = request(port, token, "POST", "/api/diagnose").send_json(json!({"seconds":seconds}));
    let start: Value = match start.and_then(|r| r.into_json().map_err(Into::into)) {
        Ok(v) => v,
        Err(_) => {
            return json!({"schema":"trot.diagnostic.v1","events":[],"mode":"attached","error":"Running engine does not accept diagnostic capture (older version, authentication failure or another capture active). Update/restart it normally, or quit Nowhere and the old daemon and rerun this command. No second BLE connection was opened."})
        }
    };
    let Some(id) = start["capture_id"].as_str() else {
        return json!({"events":[],"error":"engine returned no capture ID"});
    };
    eprintln!(
        "Attached to engine {}. Existing recording continues normally.",
        start["engine_version"]
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut last = json!({"schema":"trot.diagnostic.v1","events":[],"engine_version":start["engine_version"],"mode":"attached"});
    loop {
        match request(port, token, "GET", &format!("/api/diagnose/{id}"))
            .call()
            .and_then(|r| r.into_json().map_err(Into::into))
        {
            Ok(v) => {
                last = v;
                if let Err(e) = checkpoint(journal, &last) {
                    last["export_error"] = json!(e.to_string());
                    break;
                }
            }
            Err(_) => {
                last["error"] =
                    json!("engine capture could not be retrieved; earlier checkpoint retained");
                break;
            }
        }
        if last["end_reason"].is_string() {
            break;
        }
        tokio::select! {
            _=&mut interrupt=>{last["client_end_reason"]=json!("interrupted");break;},
            _=tokio::time::sleep_until(deadline)=>break,
            _=tokio::time::sleep(Duration::from_secs(2))=>{},
        }
    }
    let _ = request(port, token, "DELETE", &format!("/api/diagnose/{id}")).call();
    if let Ok(v) = request(port, token, "GET", &format!("/api/diagnose/{id}"))
        .call()
        .and_then(|r| r.into_json::<Value>().map_err(Into::into))
    {
        let reason = last["client_end_reason"].clone();
        last = v;
        last["client_end_reason"] = reason;
    }
    last
}
async fn standalone(options: &Options, journal: &mut std::fs::File) -> Value {
    let c = Arc::new(Capture::default());
    let id = c
        .start(options.duration, "standalone_no_database")
        .expect("CLI checked duration");
    c.event("settings",json!({"display_unit":options.display_unit,"inventory_only":options.inventory,"initial_state":"fresh decoder; no persisted session or accounting"}));
    let capture = c.clone();
    let device = options.device.clone();
    let inventory = options.inventory;
    let unit = options.display_unit.clone();
    let non_interactive = options.non_interactive;
    let collector = tokio::spawn(async move {
        trot_core::diagnostics::standalone(
            capture,
            device.as_deref(),
            inventory,
            &unit,
            move |names| {
                if names.is_empty() || non_interactive || !std::io::stdin().is_terminal() {
                    return None;
                }
                dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt("Select the treadmill to diagnose (no persistent pairing)")
                    .items(names)
                    .interact_opt()
                    .ok()
                    .flatten()
            },
        )
        .await
    });
    tokio::pin!(collector);
    let result = loop {
        tokio::select! {
            result=&mut collector=>break result.unwrap_or_else(|e|Err(e.into())),
            _=tokio::time::sleep(Duration::from_secs(2))=>{if let Ok(v)=c.snapshot(&id) {if let Err(e)=checkpoint(journal,&v) { eprintln!("Partial-file checkpoint failed: {e}; capture remains bounded in memory"); }}},
        }
    };
    if let Err(ref e) = result {
        c.event("collection_error", json!({"error":format!("{e:#}")}));
    }
    let _ = c.finish(
        &id,
        if result.is_ok() {
            "completed"
        } else {
            "collection_failed"
        },
    );
    let mut report = c.snapshot(&id).unwrap_or_else(|_| json!({"events":[]}));
    if let Err(e) = result {
        report["error"] = json!(format!("{e:#}"));
    }
    report
}
fn summary(report: &Value) -> String {
    let events = report["events"].as_array();
    let count = |kind: &str| {
        events
            .map(|e| e.iter().filter(|v| v["kind"] == kind).count())
            .unwrap_or(0)
    };
    format!("Trot diagnostic — engine {}, CLI {}\nMode: {}\nNotifications: {}; decoded samples: {}; LifeSpan decode errors: {}; write starts: {}\nDropped events: {}\nCollection error: {}\nRaw bytes describe what this application observed; request associations may be inferred.\nNo hardware compatibility fix is implied by successful capture. Review raw data before sharing.",report["engine_version"],env!("CARGO_PKG_VERSION"),report["mode"],count("rx"),count("decoded_sample"),count("decode_error"),count("write_start"),report["dropped_events"],report["error"])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_keeps_failure_separate_from_connection() {
        let v = json!({"events":[{"kind":"link_connected"},{"kind":"rx"},{"kind":"decode_error"}]});
        let s = summary(&v);
        assert!(s.contains("Notifications: 1; decoded samples: 0; LifeSpan decode errors: 1"));
    }
}
