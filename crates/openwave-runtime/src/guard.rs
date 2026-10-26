//! Keeps playback off a reappearing Wave XLR output until its capture runs.
//!
//! The Wave XLR (0fd9:007d) runs capture and playback from one firmware
//! clock. When playback starts before capture, capture delivers digital
//! silence until the unit loses power or is reset. When the unit reappears
//! (a wake through a monitor hub, a replug, a USB reset), PipeWire reconnects
//! already-open playback streams such as a browser tab to the new sink at
//! once, seconds before the keepalive opens capture. Suspending the sink
//! doesn't stop a linked stream from starting it, and moving streams after
//! the sink appears still loses the race when a stream connects between two
//! checks; both were tried on hardware.
//!
//! So the guard acts on the disappearance: it records which streams play to
//! the Wave XLR sink, and the moment that sink is removed it moves them to a
//! temporary `openwave-hold` null sink. They are already elsewhere when the
//! unit comes back, and stay there until the unit itself delivers capture
//! data: its ALSA capture `hw_ptr` advances, which takes about a second after
//! capture starts. Starting playback in that second wedges it as well, and the
//! keepalive can't tell, since PipeWire feeds it silence meanwhile. Then the
//! streams go back. While a hold lasts, `openwave-hold` is also the default
//! sink, so streams that follow the default can't jump onto the returning unit. A sink that appears without a
//! preceding removal is held the same way, and streams landing on a held sink
//! are moved off every 50 ms as a backstop. Streams that followed the default
//! sink follow it again afterwards.
//!
//! A disconnect can last minutes or hours. After 30 s the held streams are
//! parked on the default
//! sink so they keep playing, pinned there so they don't follow the Wave XLR
//! back on their own, and the hold stays armed until the unit returns, however
//! late. The hold sink only exists while streams are actually on it.
//!
//! Only the Wave XLR is guarded: it is the only unit this was observed and
//! tested on. The XLR Dock and Wave:3 share the clock design and can be added
//! once confirmed.

use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const HOLD_SINK: &str = "openwave-hold";
const GUARDED_PREFIX: &str = "Elgato_Systems_Elgato_Wave_XLR_";
const TICK: Duration = Duration::from_millis(50);
const TRACK_EVERY: Duration = Duration::from_secs(1);
const MAX_HOLD: Duration = Duration::from_secs(10);
const MAX_WAIT_RETURN: Duration = Duration::from_secs(30);

/// `usb-<vendor_product_serial>-00` for `alsa_{input,output}.<key>.<profile>`,
/// shared by a unit's source and sink.
pub fn device_key(node_name: &str) -> Option<&str> {
    let rest = node_name
        .strip_prefix("alsa_output.")
        .or_else(|| node_name.strip_prefix("alsa_input."))?;
    rest.rsplit_once('.').map(|(key, _)| key)
}

pub fn is_guarded_sink(node_name: &str) -> bool {
    node_name
        .strip_prefix("alsa_output.usb-")
        .is_some_and(|rest| rest.starts_with(GUARDED_PREFIX))
}

/// The USB serial of a guarded unit from a node name, e.g.
/// `alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00.mono-fallback`
/// gives `A01DA401224UGC`.
pub fn node_serial(node_name: &str) -> Option<&str> {
    let rest = device_key(node_name)?
        .strip_prefix("usb-")?
        .strip_prefix(GUARDED_PREFIX)?;
    Some(rest.rsplit_once('-').map_or(rest, |(serial, _)| serial))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkEvent {
    pub name: String,
    pub removed: bool,
    pub initial: bool,
    /// ALSA card of an added sink (`alsa.card`).
    pub card: Option<u64>,
}

/// One `pw-dump --monitor` batch to sink events; `seen` maps ids to sink names.
pub fn sink_events(
    batch: &Value,
    seen: &mut HashMap<u64, String>,
    initial: bool,
) -> Vec<SinkEvent> {
    let mut events = Vec::new();
    for object in batch.as_array().into_iter().flatten() {
        let Some(id) = object["id"].as_u64() else {
            continue;
        };
        if object["info"].is_null() {
            if let Some(name) = seen.remove(&id) {
                events.push(SinkEvent {
                    name,
                    removed: true,
                    initial: false,
                    card: None,
                });
            }
            continue;
        }
        if seen.contains_key(&id) || object["type"] != "PipeWire:Interface:Node" {
            continue;
        }
        let props = &object["info"]["props"];
        if props["media.class"] != "Audio/Sink" {
            continue;
        }
        let Some(name) = props["node.name"].as_str() else {
            continue;
        };
        seen.insert(id, name.to_owned());
        let card = props["alsa.card"]
            .as_u64()
            .or_else(|| props["alsa.card"].as_str().and_then(|c| c.parse().ok()));
        events.push(SinkEvent {
            name: name.to_owned(),
            removed: false,
            initial,
            card,
        });
    }
    events
}

/// Sink name to pactl index, from `pactl list short sinks`.
pub fn parse_short_sinks(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let index = f.next()?.parse().ok()?;
            Some((f.next()?.to_owned(), index))
        })
        .collect()
}

/// A playback stream from `pactl -f json list sink-inputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInput {
    pub index: u64,
    pub sink: u64,
    /// PipeWire node id, for clearing the explicit target a move leaves.
    pub object_id: Option<u64>,
    /// Application or node name, for the log.
    pub name: Option<String>,
}

pub fn parse_sink_inputs(json: &str) -> Vec<SinkInput> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(SinkInput {
                index: item["index"].as_u64()?,
                sink: item["sink"].as_u64()?,
                object_id: item["properties"]["object.id"]
                    .as_str()
                    .and_then(|id| id.parse().ok()),
                name: ["application.name", "node.name", "media.name"]
                    .iter()
                    .find_map(|key| item["properties"][*key].as_str())
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn run_tool(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pactl(args: &[&str]) -> Option<String> {
    run_tool("pactl", args)
}

fn sink_index(sink: &str) -> Option<u64> {
    parse_short_sinks(&pactl(&["list", "short", "sinks"])?)
        .get(sink)
        .copied()
}

fn sink_inputs() -> Vec<SinkInput> {
    pactl(&["-f", "json", "list", "sink-inputs"])
        .map_or_else(Vec::new, |json| parse_sink_inputs(&json))
}

fn default_sink() -> Option<String> {
    pactl(&["get-default-sink"]).map(|name| name.trim().to_owned())
}

/// The unit's capture is delivering samples: its ALSA capture PCM is running
/// and `hw_ptr` has moved past zero.
pub fn capture_status_delivering(status: &str) -> bool {
    let running = status.lines().any(|l| l.trim() == "state: RUNNING");
    let hw_ptr = status.lines().find_map(|l| {
        l.trim()
            .strip_prefix("hw_ptr")?
            .trim()
            .strip_prefix(':')?
            .trim()
            .parse::<u64>()
            .ok()
    });
    running && hw_ptr.is_some_and(|p| p > 0)
}

fn capture_delivering(card: u64) -> bool {
    std::fs::read_to_string(format!("/proc/asound/card{card}/pcm0c/sub0/status"))
        .is_ok_and(|status| capture_status_delivering(&status))
}

fn set_default_sink(sink: &str) -> bool {
    pactl(&["set-default-sink", sink]).is_some()
}

fn move_stream(index: u64, sink: &str) -> bool {
    pactl(&["move-sink-input", &index.to_string(), sink]).is_some()
}

/// Undo the explicit target a move leaves, so the stream follows the default
/// sink again.
fn follow_default(object_id: u64) {
    let id = object_id.to_string();
    for key in ["target.object", "target.node"] {
        let _ = run_tool("pw-metadata", &["-d", &id, key]);
    }
}

struct Hold {
    since: Instant,
    returned: Option<Instant>,
    moved: usize,
    /// The held sink was the default sink when the hold began.
    was_default: bool,
    /// Streams parked on another sink while the unit stayed away.
    parked: Vec<u64>,
    parked_at: Option<Instant>,
}

fn names(streams: &[SinkInput]) -> String {
    let names: Vec<&str> = streams
        .iter()
        .map(|s| s.name.as_deref().unwrap_or("?"))
        .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

enum Message {
    Sink(SinkEvent),
    CaptureRunning(String),
}

pub struct PlaybackGuard {
    tx: mpsc::Sender<Message>,
    cancel: Arc<AtomicBool>,
    monitor: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}

impl PlaybackGuard {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let monitor = Arc::new(Mutex::new(None));
        let worker = {
            let (tx, cancel, monitor) = (tx.clone(), cancel.clone(), monitor.clone());
            thread::Builder::new()
                .name("openwave-guard".into())
                .spawn(move || Guard::default().run(rx, tx, cancel, monitor))
                .ok()
        };
        Self {
            tx,
            cancel,
            monitor,
            worker,
        }
    }

    /// The keepalive for `capture_node` has data: its unit's output may start.
    pub fn capture_running(&self, capture_node: &str) {
        let _ = self
            .tx
            .send(Message::CaptureRunning(capture_node.to_owned()));
    }
}

impl Drop for PlaybackGuard {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(child) = self
            .monitor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            let _ = child.kill();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_monitor(
    tx: mpsc::Sender<Message>,
    cancel: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
) {
    thread::spawn(move || {
        while !cancel.load(Ordering::Acquire) {
            let child = Command::new("pw-dump")
                .args(["--monitor", "--no-colors"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                log::warn!("Playback guard cannot watch PipeWire; retrying");
                thread::sleep(Duration::from_secs(2));
                continue;
            };
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                continue;
            };
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
            let mut seen = HashMap::new();
            let stream =
                serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<Value>();
            for (index, batch) in stream.enumerate() {
                let Ok(batch) = batch else { break };
                for event in sink_events(&batch, &mut seen, index == 0) {
                    if tx.send(Message::Sink(event)).is_err() {
                        return;
                    }
                }
            }
            if let Some(mut child) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

#[derive(Default)]
struct Guard {
    /// Present guarded sinks and their pactl index.
    present: HashMap<String, u64>,
    /// Last known streams on each present, unheld guarded sink.
    on_sink: HashMap<String, Vec<SinkInput>>,
    /// The guarded sinks that were the default sink at the last track.
    default: HashSet<String>,
    held: HashMap<String, Hold>,
    module: Option<String>,
    /// ALSA card of each guarded sink seen.
    cards: HashMap<String, u64>,
    /// The guarded sink to make the default again when its hold ends.
    restore_default: Option<String>,
}

impl Guard {
    fn run(
        mut self,
        rx: mpsc::Receiver<Message>,
        tx: mpsc::Sender<Message>,
        cancel: Arc<AtomicBool>,
        monitor: Arc<Mutex<Option<Child>>>,
    ) {
        spawn_monitor(tx, cancel.clone(), monitor);
        let mut last_track = Instant::now();
        while !cancel.load(Ordering::Acquire) {
            match rx.recv_timeout(TICK) {
                Ok(Message::Sink(event)) if is_guarded_sink(&event.name) => self.sink_event(event),
                Ok(Message::CaptureRunning(capture)) => self.capture_running(&capture),
                Ok(Message::Sink(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.evict_backstop();
            self.release_delivering();
            self.expire();
            if last_track.elapsed() >= TRACK_EVERY {
                last_track = Instant::now();
                self.track();
            }
        }
        let held: Vec<String> = self.held.keys().cloned().collect();
        for sink in held {
            self.release(&sink, "shutdown");
        }
    }

    fn sink_event(&mut self, event: SinkEvent) {
        let name = event.name;
        if event.removed {
            self.present.remove(&name);
            let streams = self.on_sink.remove(&name).unwrap_or_default();
            if self.held.contains_key(&name) || !self.ensure_hold_sink() {
                return;
            }
            let moved = streams
                .iter()
                .filter(|s| move_stream(s.index, HOLD_SINK))
                .count();
            log::info!(
                "Output {name} disappeared; holding {moved} of {} streams until capture runs",
                streams.len()
            );
            let was_default = self.default.contains(&name);
            if was_default {
                self.hold_default(&name);
            }
            self.held.insert(
                name,
                Hold {
                    since: Instant::now(),
                    returned: None,
                    moved,
                    was_default,
                    parked: Vec::new(),
                    parked_at: None,
                },
            );
            return;
        }
        let Some(index) = sink_index(&name) else {
            return;
        };
        self.present.insert(name.clone(), index);
        if let Some(card) = event.card {
            self.cards.insert(name.clone(), card);
        }
        let already: Vec<SinkInput> = if event.initial {
            Vec::new()
        } else {
            sink_inputs()
                .into_iter()
                .filter(|s| s.sink == index)
                .collect()
        };
        if self.held.contains_key(&name) {
            self.ensure_hold_sink();
            if let Some(hold) = self.held.get_mut(&name) {
                hold.returned = Some(Instant::now());
                log::info!(
                    "Output {name} is back after {} ms; its streams are still held ({} parked); already on it: {}",
                    hold.since.elapsed().as_millis(),
                    hold.parked.len(),
                    names(&already)
                );
            }
            self.evict_backstop();
        } else if !event.initial && self.ensure_hold_sink() {
            let was_default = default_sink().as_deref() == Some(name.as_str());
            if was_default {
                self.hold_default(&name);
            }
            let now = Instant::now();
            self.held.insert(
                name.clone(),
                Hold {
                    since: now,
                    returned: Some(now),
                    moved: 0,
                    was_default,
                    parked: Vec::new(),
                    parked_at: None,
                },
            );
            log::info!(
                "Output {name} appeared; holding playback until capture runs; already on it: {}",
                names(&already)
            );
            self.evict_backstop();
        }
    }

    fn capture_running(&mut self, capture: &str) {
        let Some(key) = device_key(capture) else {
            return;
        };
        // Normally the release waits for the unit's own capture data (see
        // release_delivering); this only covers sinks without a known card.
        let ready: Vec<String> = self
            .held
            .keys()
            .filter(|sink| {
                device_key(sink) == Some(key)
                    && self.present.contains_key(*sink)
                    && !self.cards.contains_key(*sink)
            })
            .cloned()
            .collect();
        for sink in ready {
            self.release(&sink, "capture running, card unknown");
        }
    }

    /// Release held, present sinks whose unit delivers capture data.
    fn release_delivering(&mut self) {
        let ready: Vec<String> = self
            .held
            .keys()
            .filter(|sink| {
                self.present.contains_key(*sink)
                    && self
                        .cards
                        .get(*sink)
                        .is_some_and(|card| capture_delivering(*card))
            })
            .cloned()
            .collect();
        for sink in ready {
            self.release(&sink, "capture delivering");
        }
    }

    /// Make the hold sink the default while `sink` is held, so streams that
    /// follow the default don't land on the unit when it returns.
    fn hold_default(&mut self, sink: &str) {
        if self.restore_default.is_none() && self.ensure_hold_sink() && set_default_sink(HOLD_SINK)
        {
            self.restore_default = Some(sink.to_owned());
            log::info!("Default sink is {HOLD_SINK} while {sink} is held");
        }
    }

    /// Anything that lands on a held, present sink goes to the hold sink.
    fn evict_backstop(&mut self) {
        let targets: HashMap<u64, String> = self
            .held
            .keys()
            .filter_map(|sink| Some((*self.present.get(sink)?, sink.clone())))
            .collect();
        if targets.is_empty() {
            return;
        }
        for stream in sink_inputs() {
            if let Some(sink) = targets.get(&stream.sink)
                && move_stream(stream.index, HOLD_SINK)
                && let Some(hold) = self.held.get_mut(sink)
            {
                hold.moved += 1;
                log::info!(
                    "Moved stream {} ({}) off held output {sink}",
                    stream.index,
                    stream.name.as_deref().unwrap_or("?")
                );
            }
        }
    }

    fn expire(&mut self) {
        let away: Vec<String> = self
            .held
            .iter()
            .filter(|(_, h)| {
                h.returned.is_none() && h.parked_at.is_none() && h.since.elapsed() > MAX_WAIT_RETURN
            })
            .map(|(sink, _)| sink.clone())
            .collect();
        for sink in away {
            self.park(&sink);
        }
        let expired: Vec<String> = self
            .held
            .iter()
            .filter(|(_, h)| h.returned.is_some_and(|at| at.elapsed() > MAX_HOLD))
            .map(|(sink, _)| sink.clone())
            .collect();
        for sink in expired {
            self.release(&sink, "timeout");
        }
    }

    /// The unit is still away: let the held streams play on the default sink,
    /// pinned there, and keep the hold armed for whenever the unit returns.
    fn park(&mut self, sink: &str) {
        let target = default_sink().filter(|d| d != HOLD_SINK && d != sink);
        let on_hold: Vec<SinkInput> = sink_index(HOLD_SINK)
            .map(|index| {
                sink_inputs()
                    .into_iter()
                    .filter(|s| s.sink == index)
                    .collect()
            })
            .unwrap_or_default();
        let parked: Vec<u64> = match &target {
            Some(target) => on_hold
                .iter()
                .filter(|s| move_stream(s.index, target))
                .map(|s| s.index)
                .collect(),
            None => Vec::new(),
        };
        log::info!(
            "Output {sink} is still away after {} s; parked {} of {} streams ({}) on {} until it returns",
            MAX_WAIT_RETURN.as_secs(),
            parked.len(),
            on_hold.len(),
            names(&on_hold),
            target
                .as_deref()
                .unwrap_or("nothing (no other default sink)")
        );
        if let Some(hold) = self.held.get_mut(sink) {
            hold.parked.extend(parked);
            hold.parked_at = Some(Instant::now());
        }
        self.unload_idle_hold_sink();
    }

    /// The hold sink is only needed while a held unit is present or streams
    /// are still on it.
    fn unload_idle_hold_sink(&mut self) {
        let needed = self
            .held
            .values()
            .any(|h| h.returned.is_some() || h.parked_at.is_none());
        if !needed && let Some(module) = self.module.take() {
            let _ = pactl(&["unload-module", &module]);
        }
    }

    /// Record the streams on each present, unheld guarded sink: one pactl
    /// call a second, and none while no Wave XLR is present.
    fn track(&mut self) {
        let unheld: Vec<(String, u64)> = self
            .present
            .iter()
            .filter(|(sink, _)| !self.held.contains_key(*sink))
            .map(|(sink, index)| (sink.clone(), *index))
            .collect();
        if unheld.is_empty() {
            return;
        }
        let streams = sink_inputs();
        let default = default_sink();
        self.default.clear();
        for (sink, index) in unheld {
            if default.as_deref() == Some(sink.as_str()) {
                self.default.insert(sink.clone());
            }
            self.on_sink.insert(
                sink,
                streams
                    .iter()
                    .filter(|s| s.sink == index)
                    .cloned()
                    .collect(),
            );
        }
    }

    fn ensure_hold_sink(&mut self) -> bool {
        if sink_index(HOLD_SINK).is_some() {
            return true;
        }
        match pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={HOLD_SINK}"),
            "sink_properties=device.description=OpenWave-holding-playback",
        ]) {
            Some(id) => {
                self.module = Some(id.trim().to_owned());
                true
            }
            None => {
                log::warn!("Playback guard cannot create {HOLD_SINK}; the output is not held");
                false
            }
        }
    }

    /// Move everything on the hold sink back to `sink`, or to the default
    /// sink if `sink` doesn't exist (the unit never came back).
    fn release(&mut self, sink: &str, reason: &str) {
        let Some(hold) = self.held.remove(sink) else {
            return;
        };
        if self.restore_default.as_deref() == Some(sink) {
            self.restore_default = None;
            // The hold sink may already be gone (unloaded while the unit was
            // away), so restore whenever the unit is present.
            if self.present.contains_key(sink) && set_default_sink(sink) {
                log::info!("Default sink is {sink} again");
            }
        }
        let (mut back, mut failed) = (0, 0);
        let hold_index = sink_index(HOLD_SINK);
        if hold_index.is_some() || !hold.parked.is_empty() {
            let follow = hold.was_default && default_sink().as_deref() == Some(sink);
            let parked: HashSet<u64> = hold.parked.iter().copied().collect();
            let returning = sink_inputs()
                .into_iter()
                .filter(|s| Some(s.sink) == hold_index || parked.contains(&s.index));
            for stream in returning {
                if move_stream(stream.index, sink) {
                    back += 1;
                    if follow && let Some(id) = stream.object_id {
                        follow_default(id);
                    }
                } else if move_stream(stream.index, "@DEFAULT_SINK@") {
                    back += 1;
                } else {
                    failed += 1;
                }
            }
        }
        log::info!(
            "Released output {sink} ({reason}) after {} ms; moved {} streams away, {back} back, {failed} failed",
            hold.since.elapsed().as_millis(),
            hold.moved
        );
        self.unload_idle_hold_sink();
    }
}

/// USB-reset the Wave XLR behind a capture node. Capture delivering digital
/// silence is the shared-clock wedge; recycling the keepalive can't clear it,
/// a reset can while the guard keeps playback off the reappearing sink.
pub fn reset_unit(capture_node: &str) -> std::result::Result<(), String> {
    const VID: u16 = 0x0fd9;
    const PID: u16 = 0x007d;
    let serial = node_serial(capture_node).ok_or("not a Wave XLR capture node")?;
    let devices = rusb::devices().map_err(|e| e.to_string())?;
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if (desc.vendor_id(), desc.product_id()) != (VID, PID) {
            continue;
        }
        let Ok(handle) = device.open() else { continue };
        if handle
            .read_serial_number_string_ascii(&desc)
            .ok()
            .as_deref()
            != Some(serial)
        {
            continue;
        }
        return handle.reset().map_err(|e| e.to_string());
    }
    Err(format!("no Wave XLR with serial {serial}"))
}
