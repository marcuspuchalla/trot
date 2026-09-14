//! Bounded, opt-in support capture. No storage, credentials or cloud data.
use crate::drivers::{self, Advertisement, DriverHost, Sample};
use anyhow::{anyhow, Result};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENTS: usize = 40_000;
const MAX_FRAME: usize = 4096;

#[derive(Default)]
pub struct Capture {
    inner: Mutex<Option<Session>>,
    busy_drops: std::sync::atomic::AtomicU64,
}
struct Session {
    id: String,
    start: Instant,
    duration: Duration,
    finished: Option<String>,
    events: Vec<Value>,
    bytes: usize,
    dropped: u64,
    sequence: u64,
    mode: String,
}
impl Capture {
    pub fn start(&self, seconds: u64, mode: &str) -> Result<String> {
        if !(1..=600).contains(&seconds) {
            return Err(anyhow!("duration must be 1–600 seconds"));
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .as_ref()
            .is_some_and(|s| s.finished.is_none() && s.start.elapsed() < s.duration)
        {
            return Err(anyhow!("another diagnostic capture is active"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.busy_drops
            .store(0, std::sync::atomic::Ordering::Relaxed);
        *guard = Some(Session {
            id: id.clone(),
            start: Instant::now(),
            duration: Duration::from_secs(seconds),
            finished: None,
            events: Vec::new(),
            bytes: 0,
            dropped: 0,
            sequence: 0,
            mode: mode.into(),
        });
        Ok(id)
    }
    pub fn remaining(&self) -> Duration {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.duration.saturating_sub(s.start.elapsed()))
            .unwrap_or_default()
    }
    pub fn event(&self, kind: &str, detail: Value) {
        // Export can clone a large trace. Never block the BLE task behind it.
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.busy_drops
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let Some(s) = guard.as_mut() else {
            return;
        };
        if s.finished.is_some() || s.start.elapsed() >= s.duration {
            return;
        }
        s.sequence += 1;
        let event = json!({"seq":s.sequence,"elapsed_us":s.start.elapsed().as_micros() as u64,"kind":kind,"detail":detail});
        let size = event.to_string().len();
        if s.bytes + size > MAX_BYTES || s.events.len() >= MAX_EVENTS {
            s.dropped += 1;
            return;
        }
        s.bytes += size;
        s.events.push(event);
    }
    pub fn finish(&self, id: &str, reason: &str) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = guard
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or_else(|| anyhow!("capture not found"))?;
        if s.finished.is_none() {
            s.finished = Some(reason.into());
        }
        Ok(())
    }
    pub fn snapshot(&self, id: &str) -> Result<Value> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = guard
            .as_ref()
            .filter(|s| s.id == id)
            .ok_or_else(|| anyhow!("capture not found"))?;
        let ended = s
            .finished
            .clone()
            .or_else(|| (s.start.elapsed() >= s.duration).then(|| "duration_elapsed".into()));
        Ok(
            json!({"schema":"trot.diagnostic.v1","capture_id":s.id,"engine_version":env!("CARGO_PKG_VERSION"),"build_commit":env!("TROT_BUILD_COMMIT"),"build_dirty":env!("TROT_BUILD_DIRTY"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"mode":s.mode,"requested_seconds":s.duration.as_secs(),"elapsed_seconds":s.start.elapsed().as_secs_f64(),"end_reason":ended,"dropped_events":s.dropped+self.busy_drops.load(std::sync::atomic::Ordering::Relaxed),"drop_counts":{"capacity":s.dropped,"export_contention":self.busy_drops.load(std::sync::atomic::Ordering::Relaxed)},"retained_bytes":s.bytes,"timestamp_semantics":"application stream dequeue / operation observation, not radio timestamps","privacy":"No database, account, token or environment dump. Selected device names, raw BLE data and backend errors may contain identifiers; review before sharing.","coverage":{"transport":"all built-in drivers; notifications observed when polled, before driver drains/filters","replay":"partial: initial decoder/gate state not exported","radio_packets":"unavailable","history":"excluded"},"events":s.events}),
        )
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(MAX_FRAME)
        .map(|v| format!("{v:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn frame(bytes: &[u8]) -> Value {
    json!({"hex":hex(bytes),"length":bytes.len(),"truncated":bytes.len()>MAX_FRAME})
}
pub fn sample(s: &Sample) -> Value {
    json!({"steps":s.steps,"distance_m":s.distance_m,"duration_s":s.duration_s,"calories":s.calories,"speed_kmh":s.speed_kmh,"belt_state":format!("{:?}",s.state)})
}

/// Instruments the same Peripheral methods drivers already use. No extra writes,
/// subscriptions, notification reader, pacing or retry is introduced.
pub struct Link {
    peripheral: Peripheral,
    pub capture: Arc<Capture>,
    generation: String,
}
impl std::ops::Deref for Link {
    type Target = Peripheral;
    fn deref(&self) -> &Peripheral {
        &self.peripheral
    }
}
impl Link {
    pub fn new(peripheral: Peripheral, capture: Arc<Capture>) -> Self {
        Self {
            peripheral,
            capture,
            generation: uuid::Uuid::new_v4().to_string(),
        }
    }
    pub fn record(&self, kind: &str, detail: Value) {
        self.capture
            .event(kind, json!({"connection":self.generation,"data":detail}));
    }
    pub async fn write(
        &self,
        c: &Characteristic,
        bytes: &[u8],
        ty: WriteType,
    ) -> btleplug::Result<()> {
        observed_write(
            self.capture.clone(),
            &self.generation,
            c,
            bytes,
            ty,
            self.peripheral.write(c, bytes, ty),
        )
        .await
    }
    pub async fn subscribe(&self, c: &Characteristic) -> btleplug::Result<()> {
        self.record(
            "subscribe_start",
            json!({"service":c.service_uuid,"characteristic":c.uuid}),
        );
        let result = self.peripheral.subscribe(c).await;
        self.record("subscribe_result",json!({"characteristic":c.uuid,"ok":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:?}"))}));
        result
    }
    pub async fn notifications(
        &self,
    ) -> btleplug::Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        let stream = self.peripheral.notifications().await?;
        Ok(observe_notifications(
            stream,
            self.capture.clone(),
            self.generation.clone(),
        ))
    }
}
fn observe_notifications<S>(
    stream: S,
    capture: Arc<Capture>,
    generation: String,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>>
where
    S: Stream<Item = ValueNotification> + Send + 'static,
{
    Box::pin(stream.map(move |n| {
        capture.event(
            "rx",
            json!({"connection":generation,"characteristic":n.uuid,"payload":frame(&n.value)}),
        );
        n
    }))
}
async fn observed_write<F>(
    capture: Arc<Capture>,
    generation: &str,
    c: &Characteristic,
    bytes: &[u8],
    ty: WriteType,
    write: F,
) -> btleplug::Result<()>
where
    F: std::future::Future<Output = btleplug::Result<()>>,
{
    let op = uuid::Uuid::new_v4().to_string();
    capture.event("write_start",json!({"connection":generation,"data":{"operation":op,"service":c.service_uuid,"characteristic":c.uuid,"mode":format!("{ty:?}"),"payload":frame(bytes)}}));
    let mut pending = Pending {
        capture: capture.clone(),
        generation: generation.into(),
        op: op.clone(),
        complete: false,
    };
    let result = write.await;
    pending.complete = true;
    capture.event("write_result",json!({"connection":generation,"data":{"operation":op,"ok":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:?}"))}}));
    result
}
struct Pending {
    capture: Arc<Capture>,
    generation: String,
    op: String,
    complete: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.complete {
            self.capture.event("write_cancelled",json!({"connection":self.generation,"data":{"operation":self.op,"transmission":"unknown: cancellation or timeout does not prove non-transmission"}}));
        }
    }
}

pub fn inventory(adv: &Advertisement, gatt: &BTreeSet<Characteristic>) -> Value {
    json!({"name":adv.name,"name_utf8_hex":hex(adv.name.as_bytes()),"advertised_services":adv.services,"gatt":gatt.iter().map(|c|json!({"service":c.service_uuid,"characteristic":c.uuid,"properties":format!("{:?}",c.properties)})).collect::<Vec<_>>(),"drivers":drivers::DRIVERS.iter().map(|d|json!({"driver":d.id(),"scan_match":d.matches(adv),"gatt_and_name_support":d.supports(adv,gatt)})).collect::<Vec<_>>(),"selection":"first supporting driver in registry order; booleans are the production predicates"})
}

/// Selected-device capture without AppState, config paths, SQLite or sync.
/// The chooser sees names locally; other devices' names/IDs are not exported.
pub async fn standalone<F>(
    capture: Arc<Capture>,
    target: Option<&str>,
    inventory_only: bool,
    unit: &str,
    choose: F,
) -> Result<()>
where
    F: FnOnce(&[String]) -> Option<usize> + Send,
{
    let connection = Arc::new(Mutex::new(None::<Peripheral>));
    let scanning = Arc::new(Mutex::new(None::<Adapter>));
    let result = tokio::select! {
        result=standalone_inner(capture.clone(),target,inventory_only,unit,choose,connection.clone(),scanning.clone())=>result,
        _=tokio::time::sleep(capture.remaining())=>Ok(()),
        _=tokio::signal::ctrl_c()=>{ capture.event("cancelled",json!({})); Ok(()) },
    };
    let adapter = scanning.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(a) = adapter {
        let _ = tokio::time::timeout(Duration::from_secs(3), a.stop_scan()).await;
    }
    let peripheral = connection.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(p) = peripheral {
        let disconnected = tokio::time::timeout(Duration::from_secs(3), p.disconnect()).await;
        capture.event(
            "disconnected",
            json!({"ok":matches!(disconnected,Ok(Ok(())))}),
        );
    }
    result
}
async fn standalone_inner<F>(
    capture: Arc<Capture>,
    target: Option<&str>,
    inventory_only: bool,
    unit: &str,
    choose: F,
    connection: Arc<Mutex<Option<Peripheral>>>,
    scanning: Arc<Mutex<Option<Adapter>>>,
) -> Result<()>
where
    F: FnOnce(&[String]) -> Option<usize> + Send,
{
    capture.event("adapter_enumeration", json!({}));
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    capture.event(
        "adapters",
        json!({"count":adapters.len(),"selected_index":0}),
    );
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter available"))?;
    *scanning.lock().unwrap_or_else(|e| e.into_inner()) = Some(adapter.clone());
    capture.event("scan_start", json!({"seconds":5}));
    adapter.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut devices = Vec::new();
    for p in adapter.peripherals().await? {
        match p.properties().await {
            Ok(Some(props)) => devices.push((p, props)),
            Ok(None) => capture.event("properties_unavailable", json!({})),
            Err(_) => capture.event(
                "properties_error",
                json!({"detail":"backend could not read candidate properties"}),
            ),
        }
    }
    let _ = adapter.stop_scan().await;
    capture.event("scan_result", json!({"count":devices.len()}));
    let selected = if let Some(target) = target {
        let matches: Vec<_> = devices
            .iter()
            .enumerate()
            .filter(|(_, (p, props))| {
                p.id().to_string() == target || props.local_name.as_deref() == Some(target)
            })
            .map(|(i, _)| i)
            .collect();
        if matches.len() != 1 {
            return Err(anyhow!(
                "target absent or ambiguous; select one device interactively"
            ));
        }
        Some(matches[0])
    } else {
        let names = devices
            .iter()
            .enumerate()
            .map(|(i, (_, p))| {
                format!(
                    "device {}: {} (RSSI {:?})",
                    i + 1,
                    serde_json::to_string(&p.local_name).unwrap_or_default(),
                    p.rssi
                )
            })
            .collect::<Vec<_>>();
        choose(&names)
    };
    let (peripheral, props) = selected
        .and_then(|i| devices.into_iter().nth(i))
        .ok_or_else(|| anyhow!("no target selected"))?;
    let adv = Advertisement {
        name: props.local_name.unwrap_or_default(),
        services: props.services,
    };
    capture.event("selected_advertisement",json!({"name":adv.name,"services":adv.services,"rssi":props.rssi,"manufacturer_data":props.manufacturer_data.iter().map(|(k,v)|json!({"company":k,"data":frame(v)})).collect::<Vec<_>>(),"service_data":props.service_data.iter().map(|(k,v)|json!({"service":k,"data":frame(v)})).collect::<Vec<_>>() }));
    *connection.lock().unwrap_or_else(|e| e.into_inner()) = Some(peripheral.clone());
    capture.event("connect_start", json!({}));
    tokio::time::timeout(Duration::from_secs(15), peripheral.connect()).await??;
    capture.event("link_connected", json!({}));
    connected_capture(&peripheral, capture.clone(), adv, inventory_only, unit).await
}
async fn connected_capture(
    peripheral: &Peripheral,
    capture: Arc<Capture>,
    adv: Advertisement,
    inventory_only: bool,
    unit: &str,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), peripheral.discover_services()).await??;
    capture.event("inventory", inventory(&adv, &peripheral.characteristics()));
    if inventory_only {
        return Ok(());
    }
    let driver = drivers::for_device(&adv, &peripheral.characteristics())
        .ok_or_else(|| anyhow!("no driver supports this device; inventory captured"))?;
    if driver.id() == "lifespan-fallback" {
        return Err(anyhow!("unrecognised device: inventory captured; diagnostic mode does not probe fallback protocols"));
    }
    capture.event(
        "driver_selected",
        json!({"driver":driver.id(),"display_unit":unit}),
    );
    let link = Link::new(peripheral.clone(), capture.clone());
    let recorder = |tag: u8, bytes: &[u8]| {
        capture.event("driver_frame",json!({"tag":tag,"tag_semantics":"driver assigned; LifeSpan tag is inferred request opcode","payload":frame(bytes)}))
    };
    let host = DriverHost::new(unit.into(), &recorder);
    let mut emit = |s: Sample| capture.event("decoded_sample", sample(&s));
    tokio::select! {
        result=driver.run(&link,&host,&mut emit)=>result,
        _=tokio::signal::ctrl_c()=>{ capture.event("cancelled",json!({}));Ok(()) },
        _=tokio::time::sleep(capture.remaining())=>Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_limits_and_ownership() {
        let c = Capture::default();
        assert!(c.start(0, "test").is_err());
        let id = c.start(60, "test").unwrap();
        assert!(c.start(60, "other").is_err());
        c.event("rx", json!({"payload":frame(&[0x12,0xaa,0,0,0,0])}));
        assert!(c.snapshot("wrong").is_err());
        c.finish(&id, "cancelled").unwrap();
        c.event("ignored", json!({}));
        let v = c.snapshot(&id).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
        assert_eq!(v["end_reason"], "cancelled");
    }
    #[test]
    fn bounded_frames_and_events() {
        assert_eq!(frame(&vec![0; MAX_FRAME + 1])["truncated"], true);
        let c = Capture::default();
        let id = c.start(60, "test").unwrap();
        for _ in 0..MAX_EVENTS + 2 {
            c.event("small", json!({}));
        }
        let v = c.snapshot(&id).unwrap();
        assert_eq!(v["dropped_events"], 2);
        assert_eq!(v["events"].as_array().unwrap().len(), MAX_EVENTS);
    }
    #[tokio::test]
    async fn transport_keeps_discarded_bytes_and_write_cancellation() {
        use btleplug::api::CharPropFlags;
        use futures::FutureExt;
        let capture = Arc::new(Capture::default());
        let id = capture.start(60, "test").unwrap();
        let c = Characteristic {
            uuid: uuid::Uuid::from_u128(1),
            service_uuid: uuid::Uuid::from_u128(2),
            properties: CharPropFlags::WRITE,
            descriptors: BTreeSet::new(),
        };
        let bytes = [0xa1, 0x82, 0, 0, 0, 0];
        let calls = std::sync::atomic::AtomicUsize::new(0);
        observed_write(
            capture.clone(),
            "connection",
            &c,
            &bytes,
            WriteType::WithResponse,
            async {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Notification arrives before host write completion.
                let n = ValueNotification {
                    uuid: c.uuid,
                    value: vec![0x42, 0xaa, 0, 0, 0, 0],
                };
                let mut stream = observe_notifications(
                    futures::stream::iter([n]),
                    capture.clone(),
                    "connection".into(),
                );
                assert_eq!(stream.next().await.unwrap().value[0], 0x42);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let cancelled = observed_write(
            capture.clone(),
            "connection",
            &c,
            &bytes,
            WriteType::WithResponse,
            futures::future::pending(),
        );
        assert!(cancelled.now_or_never().is_none());
        let v = capture.snapshot(&id).unwrap();
        let events = v["events"].as_array().unwrap();
        let kinds = events
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "write_start",
                "rx",
                "write_result",
                "write_start",
                "write_cancelled"
            ]
        );
        assert_eq!(
            events[0]["detail"]["data"]["payload"]["hex"],
            "a1 82 00 00 00 00"
        );
        assert_eq!(events[1]["detail"]["payload"]["hex"], "42 aa 00 00 00 00");
    }
    #[test]
    fn expiration_stops_recording_without_a_client() {
        let c = Capture::default();
        let id = c.start(1, "test").unwrap();
        c.inner.lock().unwrap().as_mut().unwrap().start = Instant::now() - Duration::from_secs(2);
        c.event("late", json!({}));
        let v = c.snapshot(&id).unwrap();
        assert_eq!(v["end_reason"], "duration_elapsed");
        assert_eq!(v["events"].as_array().unwrap().len(), 0);
    }
}
