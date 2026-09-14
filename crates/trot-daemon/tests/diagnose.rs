//! Real CLI/archive checks against a loopback fixture, never a Bluetooth device.
use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

fn exercise(old_engine: bool) {
    let dir = std::env::temp_dir().join(format!(
        "trot-diag-test-{}-{}",
        std::process::id(),
        if old_engine { "old" } else { "new" }
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        dir.join("runtime.json"),
        format!("{{\"port\":{port},\"token\":\"PRIVATE_TEST_TOKEN\"}}"),
    )
    .unwrap();
    let before = std::fs::read(dir.join("runtime.json")).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let server = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; content_length]).unwrap();
            let (status, body) = if first.starts_with("GET /api/health ") {
                ("200 OK", "{}")
            } else if old_engine {
                ("404 Not Found", "unsupported")
            } else if first.starts_with("POST /api/diagnose ") {
                (
                    "200 OK",
                    r#"{"capture_id":"test","engine_version":"fixture-engine"}"#,
                )
            } else if first.starts_with("DELETE") {
                ("200 OK", "{}")
            } else {
                (
                    "200 OK",
                    r#"{"schema":"trot.diagnostic.v1","engine_version":"fixture-engine","mode":"attached","end_reason":"duration_elapsed","dropped_events":0,"events":[{"kind":"rx","detail":{"payload":{"hex":"42 aa 00 00 00 00"}}},{"kind":"decode_error"}]}"#,
                )
            };
            write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let output = dir.join("report.zip");
    let result = Command::new(env!("CARGO_BIN_EXE_trot"))
        .args([
            "diagnose",
            "--non-interactive",
            "--duration",
            "1",
            "--output",
        ])
        .arg(&output)
        .env("TROT_DATA_DIR", &dir)
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
    assert_eq!(zip.len(), 3);
    let mut manifest = String::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    assert!(!manifest.contains("PRIVATE_TEST_TOKEN"));
    let v: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    if old_engine {
        assert!(v["error"].as_str().unwrap().contains("older version"));
    } else {
        assert_eq!(v["engine_version"], "fixture-engine");
    }
    let mut trace = String::new();
    zip.by_name("trace.jsonl")
        .unwrap()
        .read_to_string(&mut trace)
        .unwrap();
    if !old_engine {
        assert!(trace.contains("42 aa 00 00 00 00"));
    }
    assert_eq!(std::fs::read(dir.join("runtime.json")).unwrap(), before);
    assert!(!dir.join("report.partial.json").exists());
    let original = std::fs::read(&output).unwrap();
    let repeat = Command::new(env!("CARGO_BIN_EXE_trot"))
        .args(["diagnose", "--output"])
        .arg(&output)
        .env("TROT_DATA_DIR", &dir)
        .output()
        .unwrap();
    assert!(!repeat.status.success());
    assert_eq!(std::fs::read(&output).unwrap(), original);
    drop(zip);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn attached_capture_exports_zip_without_persisting_or_leaking_token() {
    exercise(false);
}
#[test]
fn old_daemon_produces_useful_report_without_starting_bluetooth() {
    exercise(true);
}
