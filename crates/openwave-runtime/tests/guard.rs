use openwave_runtime::guard::{
    SinkEvent, SinkInput, capture_status_delivering, device_key, is_guarded_sink, node_serial,
    parse_short_sinks, parse_sink_inputs, sink_events,
};
use serde_json::json;
use std::collections::HashMap;

const SINK: &str = "alsa_output.usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00.analog-stereo";
const SOURCE: &str =
    "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00.mono-fallback";

#[test]
fn source_and_sink_of_one_unit_share_a_key_and_serial() {
    assert_eq!(device_key(SINK), device_key(SOURCE));
    assert_eq!(
        device_key(SINK),
        Some("usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00")
    );
    assert_eq!(node_serial(SOURCE), Some("A01DA401224UGC"));
    assert_eq!(device_key("openwave-hold"), None);
}

#[test]
fn only_the_wave_xlr_sink_is_guarded() {
    assert!(is_guarded_sink(SINK));
    assert!(!is_guarded_sink(SOURCE));
    assert!(!is_guarded_sink(
        "alsa_output.usb-Elgato_Systems_Elgato_XLR_Dock_X-00.analog-stereo"
    ));
    assert!(!is_guarded_sink(
        "alsa_output.usb-Elgato_Systems_Elgato_Wave_3_X-00.analog-stereo"
    ));
    assert!(!is_guarded_sink(
        "alsa_output.usb-Elgato_Elgato_Wave_XLR_Dock_MK.2_X-00.analog-stereo"
    ));
    assert!(!is_guarded_sink("alsa_output.pci-0000_01_00.1.hdmi-stereo"));
    assert!(!is_guarded_sink("openwave-hold"));
}

#[test]
fn monitor_batches_report_initial_removed_and_returning_sinks() {
    let mut seen = HashMap::new();
    let sink = json!({"id": 57, "type": "PipeWire:Interface:Node",
        "info": {"props": {"node.name": SINK, "media.class": "Audio/Sink", "alsa.card": 0}}});
    let source = json!({"id": 58, "type": "PipeWire:Interface:Node",
        "info": {"props": {"node.name": SOURCE, "media.class": "Audio/Source"}}});

    let first = sink_events(&json!([sink, source]), &mut seen, true);
    assert_eq!(
        first,
        vec![SinkEvent {
            name: SINK.into(),
            removed: false,
            initial: true,
            card: Some(0)
        }]
    );
    assert!(
        sink_events(&json!([sink]), &mut seen, false).is_empty(),
        "updates are not additions"
    );

    let gone = sink_events(
        &json!([{"id": 57, "info": null}, {"id": 99, "info": null}]),
        &mut seen,
        false,
    );
    assert_eq!(
        gone,
        vec![SinkEvent {
            name: SINK.into(),
            removed: true,
            initial: false,
            card: None
        }]
    );

    let back = sink_events(&json!([sink]), &mut seen, false);
    assert_eq!(
        back,
        vec![SinkEvent {
            name: SINK.into(),
            removed: false,
            initial: false,
            card: Some(0)
        }]
    );
}

#[test]
fn pactl_output_maps_streams_to_sinks() {
    let sinks = parse_short_sinks(&format!(
        "56\t{SINK}\tPipeWire\ts24le 2ch 48000Hz\tRUNNING\n70\topenwave-hold\tPipeWire\tfloat32le 2ch 48000Hz\tIDLE\n"
    ));
    assert_eq!(sinks.get(SINK), Some(&56));
    assert_eq!(sinks.get("openwave-hold"), Some(&70));

    let inputs = parse_sink_inputs(
        r#"[{"index":101,"sink":56,"properties":{"object.id":"212","application.name":"Firefox"}},
            {"index":102,"sink":70,"properties":{"node.name":"openwave_loop_output_personal"}},
            {"index":103,"sink":56}]"#,
    );
    assert_eq!(
        inputs,
        vec![
            SinkInput {
                index: 101,
                sink: 56,
                object_id: Some(212),
                name: Some("Firefox".into()),
            },
            SinkInput {
                index: 102,
                sink: 70,
                object_id: None,
                name: Some("openwave_loop_output_personal".into()),
            },
            SinkInput {
                index: 103,
                sink: 56,
                object_id: None,
                name: None,
            },
        ]
    );
    assert!(parse_sink_inputs("not json").is_empty());
}

#[test]
fn only_a_running_capture_with_samples_counts_as_delivering() {
    let running = |hw_ptr: u32| {
        format!(
            "state: RUNNING\nowner_pid   : 1\ntrigger_time: 64010.982\n-----\nhw_ptr      : {hw_ptr}\nappl_ptr    : 0\n"
        )
    };
    assert!(
        !capture_status_delivering(&running(0)),
        "started but no samples yet"
    );
    assert!(capture_status_delivering(&running(1728)));
    assert!(!capture_status_delivering("closed\n"));
    assert!(!capture_status_delivering(
        "state: SETUP\nhw_ptr      : 480\n"
    ));
}
