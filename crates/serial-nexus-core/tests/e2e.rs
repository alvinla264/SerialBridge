//! End-to-end tests: a real core on a Unix socket, the real client library and
//! a simulated serial device (PTY), so shared access, control, history,
//! writes, failures and lifecycle are exercised without hardware.

mod support;

use std::time::Duration;

use serial_nexus_client::{ClientInfo, ControlReleaseReason, Event, Role, Source, WaitSpec};
use serial_nexus_protocol::ErrorCode;
use support::{connect, read_until_contains, start_core, wait_event, wait_until, SimulatedDevice};

fn fast() -> (tempfile::TempDir, std::path::PathBuf) {
    start_core(Duration::from_millis(300), Duration::from_secs(300), 2_000)
}

#[test]
fn open_read_and_write_round_trip() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("test-ai"));

    let opened = ai.open(&device.path, Some(115_200), true).expect("open");
    assert_eq!(opened.baud, 115_200);
    assert_eq!(opened.role, Role::Controller);
    assert!(opened.control.holder.is_some());

    device.write(b"boot complete\r\n");
    let history = read_until_contains(
        &ai,
        opened.device_id,
        "boot complete",
        Duration::from_secs(2),
    );
    let line = history.lines.last().expect("a line");
    assert_eq!(line.source, Source::Device);
    assert_eq!(line.text, "boot complete");
    assert!(!line.partial);

    // The device answers the AI's write, and the wait returns that answer.
    let writer = ai.clone();
    let pending_write = std::thread::spawn(move || {
        writer.write_bytes(
            opened.device_id,
            b"version\r",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(2_000),
            }),
        )
    });
    device.read_until(b"version\r", Duration::from_secs(2));
    device.write(b"firmware 1.2.3\r\n");
    let written = pending_write.join().expect("write thread").expect("write");
    assert_eq!(written.bytes_written, b"version\r".len());
    assert!(!written.timed_out);
    assert_eq!(written.lines.len(), 1);
    assert_eq!(written.lines[0].text, "firmware 1.2.3");

    // History keeps source labels: the AI write is visible too.
    let history = ai.read(opened.device_id, Some(20)).expect("read");
    assert!(history
        .lines
        .iter()
        .any(|line| line.source == Source::Ai && line.text == "version"));
}

#[test]
fn writes_are_sent_exactly_without_added_line_endings() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let human = connect(&socket, ClientInfo::human("test-tui"));
    let opened = human.open(&device.path, Some(9_600), false).expect("open");
    assert_eq!(opened.role, Role::Human);

    human
        .write_bytes(opened.device_id, b"a\r\nb", None)
        .expect("write");
    let received = device.read_until(b"a\r\nb", Duration::from_secs(2));
    assert!(received.ends_with(b"a\r\nb"), "got {received:?}");
}

#[test]
fn conflicting_baud_is_rejected_and_join_uses_active_rate() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let first = connect(&socket, ClientInfo::ai("first"));
    let second = connect(&socket, ClientInfo::ai("second"));

    let opened = first
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let error = second
        .open(&device.path, Some(9_600), false)
        .expect_err("conflicting baud");
    assert_eq!(error.code(), Some(ErrorCode::BaudConflict));

    let joined = second.open(&device.path, None, false).expect("join");
    assert_eq!(joined.device_id, opened.device_id);
    assert_eq!(joined.baud, 115_200);
    assert_eq!(joined.role, Role::Reader);

    let same_rate = second
        .open(&device.path, Some(115_200), false)
        .expect("join at active rate");
    assert_eq!(same_rate.baud, 115_200);
}

#[test]
fn one_ai_writer_and_read_only_join() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let owner = connect(&socket, ClientInfo::ai("owner"));
    let other = connect(&socket, ClientInfo::ai("other"));
    let events = other.subscribe();

    let owned = owner
        .open(&device.path, Some(115_200), true)
        .expect("open owner");
    assert_eq!(owned.role, Role::Controller);

    let joined = other
        .open(&device.path, Some(115_200), true)
        .expect("join other");
    assert_eq!(joined.role, Role::Reader);
    let holder = joined.control.holder.expect("holder reported");
    assert_eq!(holder.client_id, owner.hello().client_id);
    let denied = wait_event(&events, Duration::from_secs(1), |event| {
        matches!(event, Event::ControlDenied { .. })
    });
    assert!(matches!(denied, Event::ControlDenied { .. }));

    let error = other
        .write_bytes(joined.device_id, b"nope", None)
        .expect_err("reader cannot write");
    assert_eq!(error.code(), Some(ErrorCode::ControlHeld));

    // A human writes without taking control.
    let human = connect(&socket, ClientInfo::human("tui"));
    let human_open = human.open(&device.path, None, false).expect("human join");
    assert_eq!(human_open.role, Role::Human);
    human
        .write_bytes(human_open.device_id, b"human", None)
        .expect("human writes");
    device.read_until(b"human", Duration::from_secs(2));
}

#[test]
fn control_expires_when_idle_and_connect_reclaims_it() {
    let (_dir, socket) = start_core(
        Duration::from_millis(300),
        Duration::from_millis(300),
        2_000,
    );
    let device = SimulatedDevice::new();
    let owner = connect(&socket, ClientInfo::ai("idle-owner"));
    let events = owner.subscribe();

    let opened = owner.open(&device.path, Some(115_200), true).expect("open");
    assert_eq!(opened.role, Role::Controller);
    let released = wait_event(&events, Duration::from_secs(3), |event| {
        matches!(
            event,
            Event::ControlReleased {
                reason: ControlReleaseReason::Idle,
                ..
            }
        )
    });
    assert!(matches!(released, Event::ControlReleased { .. }));

    let status = owner.status(opened.device_id).expect("status");
    assert!(status.control.holder.is_none());

    // Any request without control does not write.
    let error = owner
        .write_bytes(opened.device_id, b"x", None)
        .expect_err("control expired");
    assert_eq!(error.code(), Some(ErrorCode::ControlHeld));

    let reclaimed = owner
        .open(&device.path, Some(115_200), true)
        .expect("reclaim");
    assert_eq!(reclaimed.role, Role::Controller);
    owner
        .write_bytes(opened.device_id, b"x", None)
        .expect("write after reclaim");
}

#[test]
fn control_released_immediately_when_holder_disconnects() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let owner = connect(&socket, ClientInfo::ai("owner"));
    let watcher = connect(&socket, ClientInfo::ai("watcher"));
    let events = watcher.subscribe();

    let opened = owner.open(&device.path, Some(115_200), true).expect("open");
    let joined = watcher.open(&device.path, None, false).expect("join");
    assert_eq!(joined.role, Role::Reader);
    drop(owner);

    let released = wait_event(&events, Duration::from_secs(2), |event| {
        matches!(
            event,
            Event::ControlReleased {
                reason: ControlReleaseReason::Disconnected,
                ..
            }
        )
    });
    assert!(matches!(released, Event::ControlReleased { .. }));

    let reclaimed = watcher.open(&device.path, None, true).expect("reclaim");
    assert_eq!(reclaimed.role, Role::Controller);
    let _ = opened;
}

#[test]
fn last_client_close_closes_device() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let first = connect(&socket, ClientInfo::ai("first"));
    let second = connect(&socket, ClientInfo::ai("second"));

    let opened = first
        .open(&device.path, Some(115_200), false)
        .expect("open");
    second.open(&device.path, None, false).expect("join");
    first.close(opened.device_id).expect("first close");

    let open_device = second
        .list_devices()
        .expect("list")
        .devices
        .into_iter()
        .find(|entry| entry.path == device.path)
        .expect("device listed");
    assert!(open_device.open);
    assert_eq!(open_device.client_count, 1);

    second.close(opened.device_id).expect("second close");
    wait_until(Duration::from_secs(2), || {
        second
            .list_devices()
            .map(|devices| {
                devices
                    .devices
                    .iter()
                    .find(|entry| entry.path == device.path)
                    .map(|entry| !entry.open)
                    .unwrap_or(true)
            })
            .unwrap_or(false)
    });
}

#[test]
fn partial_line_is_included_and_marked() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("partial"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    device.write(b"prompt> ");
    let history = read_until_contains(&ai, opened.device_id, "prompt> ", Duration::from_secs(2));
    let partial = history.lines.last().expect("partial line");
    assert!(partial.partial);
    assert_eq!(partial.text, "prompt> ");

    // A timed-out send returns the partial tail too, marked partial.
    let written = ai
        .write_bytes(
            opened.device_id,
            b"no-response",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(150),
            }),
        )
        .expect("write");
    assert!(written.timed_out);
    assert!(written.lines.last().is_some_and(|line| line.partial));
}

#[test]
fn invalid_utf8_does_not_break_the_session() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("bytes"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    device.write(&[0xff, 0xfe, b'o', b'k', b'\n']);
    let history = read_until_contains(&ai, opened.device_id, "ok", Duration::from_secs(2));
    let line = history.lines.last().expect("line");
    assert_eq!(line.source, Source::Device);
    assert!(line.text.ends_with("ok"));
    assert!(line.text.contains('\u{fffd}'));

    // The connection still works afterwards.
    assert_eq!(ai.status(opened.device_id).expect("status").baud, 115_200);
}

#[test]
fn history_is_bounded_and_reports_dropped_lines() {
    let (_dir, socket) = start_core(Duration::from_millis(300), Duration::from_secs(300), 3);
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("history"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    for index in 0..6 {
        device.write(format!("line-{index}\n").as_bytes());
    }
    let history = read_until_contains(&ai, opened.device_id, "line-5", Duration::from_secs(2));
    assert!(
        history.lines.len() <= 3,
        "bounded history: {:?}",
        history.lines
    );
    assert!(
        history.dropped_lines >= 3,
        "dropped {}",
        history.dropped_lines
    );
}

#[test]
fn unplugged_device_notifies_clients_and_closes() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("unplug"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");
    let events = ai.subscribe();

    // Dropping the master is a hangup: the core's reads fail.
    drop(device);

    wait_event(
        &events,
        Duration::from_secs(3),
        |event| matches!(event, Event::DeviceClosed { device_id, .. } if *device_id == opened.device_id),
    );
    let error = ai.read(opened.device_id, None).expect_err("device gone");
    assert!(error.is_device_gone());
}

#[test]
fn core_exits_after_its_grace_period() {
    let (_dir, socket) = start_core(Duration::from_millis(250), Duration::from_secs(300), 2_000);
    let ai = connect(&socket, ClientInfo::ai("last-one"));
    ai.list_devices().expect("works");
    drop(ai);

    wait_until(Duration::from_secs(5), || !socket.exists());
}

#[test]
fn new_device_without_baud_uses_the_documented_default() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("default-baud"));
    let opened = ai.open(&device.path, None, true).expect("open");
    assert_eq!(opened.baud, serial_nexus_protocol::DEFAULT_BAUD);
}

#[test]
fn one_client_can_use_multiple_devices_independently() {
    let (_dir, socket) = fast();
    let mut first = SimulatedDevice::new();
    let mut second = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("multi"));

    let a = ai.open(&first.path, Some(115_200), true).expect("open a");
    let b = ai.open(&second.path, Some(9_600), true).expect("open b");
    assert_ne!(a.device_id, b.device_id);
    assert_eq!(a.role, Role::Controller);
    assert_eq!(b.role, Role::Controller);

    ai.write_bytes(a.device_id, b"to-a", None).expect("write a");
    ai.write_bytes(b.device_id, b"to-b", None).expect("write b");
    first.read_until(b"to-a", Duration::from_secs(2));
    second.read_until(b"to-b", Duration::from_secs(2));

    first.write(b"from-a\n");
    second.write(b"from-b\n");
    read_until_contains(&ai, a.device_id, "from-a", Duration::from_secs(2));
    read_until_contains(&ai, b.device_id, "from-b", Duration::from_secs(2));

    ai.close(a.device_id).expect("close a");
    let error = ai.read(a.device_id, None).expect_err("a closed");
    assert!(error.is_device_gone());
    assert_eq!(ai.status(b.device_id).expect("b still open").baud, 9_600);
}

#[test]
fn idle_timers_are_per_device() {
    let (_dir, socket) = start_core(
        Duration::from_millis(300),
        Duration::from_millis(400),
        2_000,
    );
    let first = SimulatedDevice::new();
    let second = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("timers"));
    let events = ai.subscribe();

    let a = ai.open(&first.path, Some(115_200), true).expect("open a");
    let b = ai.open(&second.path, Some(115_200), true).expect("open b");

    // Keep A active; B must expire on its own.
    let deadline = std::time::Instant::now() + Duration::from_millis(1_000);
    while std::time::Instant::now() < deadline {
        ai.status(a.device_id).expect("refresh a");
        std::thread::sleep(Duration::from_millis(80));
    }

    let released = wait_event(&events, Duration::from_secs(1), |event| {
        matches!(
            event,
            Event::ControlReleased { device_id, reason: ControlReleaseReason::Idle, .. }
                if *device_id == b.device_id
        )
    });
    assert!(matches!(released, Event::ControlReleased { .. }));

    assert!(ai
        .status(a.device_id)
        .expect("status a")
        .control
        .holder
        .is_some());
    assert!(ai
        .status(b.device_id)
        .expect("status b")
        .control
        .holder
        .is_none());
}

#[test]
fn concurrent_writes_are_serialized_not_interleaved() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("writer-ai"));
    let human = connect(&socket, ClientInfo::human("writer-human"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");
    human.open(&device.path, None, false).expect("join");

    let ai_bytes = vec![b'A'; 1_000];
    let human_bytes = vec![b'B'; 1_000];
    let first = ai.clone();
    let second = human.clone();
    let (a_bytes, b_bytes) = (ai_bytes.clone(), human_bytes.clone());
    let a = std::thread::spawn(move || first.write_bytes(opened.device_id, &a_bytes, None));
    let b = std::thread::spawn(move || second.write_bytes(opened.device_id, &b_bytes, None));

    let received = device.read_n(2_000, Duration::from_secs(3));
    a.join().expect("a thread").expect("a write");
    b.join().expect("b thread").expect("b write");

    let expected_ab: Vec<u8> = ai_bytes.iter().chain(human_bytes.iter()).copied().collect();
    let expected_ba: Vec<u8> = human_bytes.iter().chain(ai_bytes.iter()).copied().collect();
    assert!(
        received == expected_ab || received == expected_ba,
        "a write was interleaved: {:?}",
        String::from_utf8_lossy(&received[..received.len().min(40)])
    );
}

#[test]
fn fragmented_device_output_assembles_one_line() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("fragments"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    device.write(b"hel");
    let partial = read_until_contains(&ai, opened.device_id, "hel", Duration::from_secs(2));
    assert!(partial.lines.last().unwrap().partial);

    device.write(b"lo\n");
    let history = read_until_contains(&ai, opened.device_id, "hello", Duration::from_secs(2));
    let complete = history.lines.last().expect("line");
    assert_eq!(complete.text, "hello");
    assert!(!complete.partial);
    assert!(
        !history
            .lines
            .iter()
            .any(|line| line.partial && line.text == "hel"),
        "stale partial left behind: {:?}",
        history.lines
    );
}

#[test]
fn timed_out_write_does_not_consume_the_device_tail() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("tail-owner"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    device.write(b"reply>");
    read_until_contains(&ai, opened.device_id, "reply>", Duration::from_secs(2));

    let written = ai
        .write_bytes(
            opened.device_id,
            b"cmd",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(150),
            }),
        )
        .expect("write");
    assert!(written.timed_out);
    assert!(written
        .lines
        .iter()
        .any(|line| line.partial && line.text == "reply>"));

    let history = ai
        .read(opened.device_id, Some(50))
        .expect("read after timeout");
    assert!(
        history
            .lines
            .iter()
            .any(|line| line.partial && line.text == "reply>"),
        "history lost the device tail: {:?}",
        history.lines
    );
}

#[test]
fn alias_paths_join_the_same_open_device() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let directory = tempfile::TempDir::new().expect("alias dir");
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&device.path, &alias).expect("symlink");

    let first = connect(&socket, ClientInfo::ai("real-path"));
    let second = connect(&socket, ClientInfo::ai("alias-path"));
    let opened = first.open(&device.path, Some(115_200), true).expect("open");

    let joined = second
        .open(alias.to_string_lossy().as_ref(), None, false)
        .expect("alias joins");
    assert_eq!(joined.device_id, opened.device_id);
    assert_eq!(joined.baud, 115_200);

    let error = second
        .open(alias.to_string_lossy().as_ref(), Some(9_600), false)
        .expect_err("conflicting baud via alias");
    assert_eq!(error.code(), Some(ErrorCode::BaudConflict));
}

#[test]
fn close_then_immediate_reopen_succeeds() {
    let (_dir, socket) = fast();
    let device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("reopen"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");
    ai.close(opened.device_id).expect("close");
    let reopened = ai
        .open(&device.path, Some(9_600), true)
        .expect("immediate reopen");
    assert_eq!(reopened.baud, 9_600);
}

#[test]
fn a_waited_write_does_not_block_other_requests_on_its_connection() {
    let (_dir, socket) = fast();
    let first = SimulatedDevice::new();
    let second = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("concurrent"));
    let a = ai.open(&first.path, Some(115_200), true).expect("open a");
    let b = ai.open(&second.path, Some(115_200), true).expect("open b");

    // Device A never answers, so this write waits for its timeout.
    let writer = ai.clone();
    let waited = std::thread::spawn(move || {
        writer.write_bytes(
            a.device_id,
            b"no-answer",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(1_500),
            }),
        )
    });
    std::thread::sleep(Duration::from_millis(100));

    let started = std::time::Instant::now();
    ai.status(b.device_id).expect("status while a write waits");
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "status blocked behind a waited write: {:?}",
        started.elapsed()
    );
    let result = waited.join().expect("writer thread").expect("write");
    assert!(result.timed_out);
}

#[test]
fn a_large_read_is_truncated_instead_of_killing_the_connection() {
    let (_dir, socket) = start_core(Duration::from_millis(300), Duration::from_secs(300), 2_000);
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("big-read"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    // 2000 lines of 120 NULs: 240 KiB, inside the byte bound, but JSON escaping
    // would exceed the 1 MiB frame limit without response trimming.
    let line = format!("{}\n", "\0".repeat(120));
    for _ in 0..2_000 {
        device.write(line.as_bytes());
    }
    let history = read_until_contains(&ai, opened.device_id, "\0", Duration::from_secs(5));
    let read = ai
        .read(opened.device_id, Some(2_000))
        .expect("large read answered");
    assert!(!read.lines.is_empty());
    assert!(read.truncated);
    assert!(ai.is_connected());
    assert_eq!(
        ai.status(opened.device_id).expect("still connected").baud,
        115_200
    );
    let _ = history;
}

#[test]
fn close_drains_writes_that_were_already_accepted() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let human = connect(&socket, ClientInfo::human("drain"));
    let opened = human
        .open(&device.path, Some(115_200), false)
        .expect("open");

    let mut expected = Vec::new();
    for index in 0..32u8 {
        let payload = format!("{index:03};");
        human
            .write_bytes(opened.device_id, payload.as_bytes(), None)
            .expect("write accepted");
        expected.extend_from_slice(payload.as_bytes());
    }
    human.close(opened.device_id).expect("close");

    let received = device.read_n(expected.len(), Duration::from_secs(3));
    assert_eq!(
        received,
        expected,
        "close dropped accepted writes: {} of {} bytes",
        received.len(),
        expected.len()
    );
}

#[test]
fn pipelined_waited_writes_keep_arrival_order() {
    use std::io::{BufRead, BufReader, Write};

    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let mut raw = std::os::unix::net::UnixStream::connect(&socket).expect("raw connection");
    let mut reader = BufReader::new(raw.try_clone().expect("clone"));
    let mut next_id = 0u64;
    let send = |raw: &mut std::os::unix::net::UnixStream,
                next_id: &mut u64,
                method: &str,
                params: serde_json::Value|
     -> u64 {
        *next_id += 1;
        let message = serde_json::json!({ "id": *next_id, "method": method, "params": params });
        raw.write_all(message.to_string().as_bytes()).expect("send");
        raw.write_all(b"\n").expect("newline");
        *next_id
    };
    let read_until = |reader: &mut BufReader<std::os::unix::net::UnixStream>, id: u64| loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let message: serde_json::Value = serde_json::from_str(&line).expect("json");
        if message.get("id").and_then(|value| value.as_u64()) == Some(id) {
            return message;
        }
    };

    let hello = send(
        &mut raw,
        &mut next_id,
        "hello",
        serde_json::json!({ "protocol": 1, "client": { "kind": "ai", "name": "pipelined" } }),
    );
    read_until(&mut reader, hello);

    let open = send(
        &mut raw,
        &mut next_id,
        "open",
        serde_json::json!({ "port": device.path, "baud": 115_200, "request_control": true }),
    );
    let opened = read_until(&mut reader, open);
    let device_id = opened["result"]["data"]["device_id"]
        .as_u64()
        .expect("device id");

    // Send waited and plain writes alternately, then close, without reading
    // answers in between. Every payload must reach the device in this order.
    let mut expected = Vec::new();
    for index in 0..20u8 {
        let payload = format!("W{index:03};");
        expected.extend_from_slice(payload.as_bytes());
        let waited = send(
            &mut raw,
            &mut next_id,
            "write",
            serde_json::json!({
                "device_id": device_id,
                "data_b64": serial_nexus_protocol::encode_bytes(payload.as_bytes()),
                "wait": { "lines": 1, "timeout_ms": 5 }
            }),
        );
        let _ = waited;

        let payload = format!("N{index:03};");
        expected.extend_from_slice(payload.as_bytes());
        send(
            &mut raw,
            &mut next_id,
            "write",
            serde_json::json!({
                "device_id": device_id,
                "data_b64": serial_nexus_protocol::encode_bytes(payload.as_bytes())
            }),
        );
    }
    send(
        &mut raw,
        &mut next_id,
        "close",
        serde_json::json!({ "device_id": device_id }),
    );

    let received = device.read_n(expected.len(), Duration::from_secs(5));
    assert_eq!(
        received,
        expected,
        "writes were transmitted out of arrival order: {:?}",
        String::from_utf8_lossy(&received[..received.len().min(80)])
    );
}

#[test]
fn read_counts_the_partial_tail_in_the_requested_lines() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("line-count"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    device.write(b"old\nrecent\nprompt> ");
    read_until_contains(&ai, opened.device_id, "prompt> ", Duration::from_secs(2));

    let one = ai.read(opened.device_id, Some(1)).expect("read one");
    assert_eq!(one.lines.len(), 1, "asked for one line: {:?}", one.lines);
    assert!(one.lines[0].partial);
    assert_eq!(one.lines[0].text, "prompt> ");
}

#[test]
fn oversized_unfinished_frame_is_refused_and_core_stays_up() {
    use std::io::Write;

    let (_dir, socket) = fast();
    let mut raw = std::os::unix::net::UnixStream::connect(&socket).expect("raw connection");
    raw.set_write_timeout(Some(Duration::from_millis(500)))
        .expect("write timeout");
    raw.set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");

    // 2 MiB without a newline must not be buffered indefinitely.
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..32 {
        if raw.write_all(&chunk).is_err() {
            break;
        }
    }
    let mut byte = [0u8; 1];
    match std::io::Read::read(&mut raw, &mut byte) {
        Ok(0) => {}
        Err(_) => {}
        Ok(_) => panic!("core accepted an oversized unfinished frame"),
    }

    // The core is still serving other clients.
    let ai = connect(&socket, ClientInfo::ai("still-alive"));
    ai.list_devices().expect("core still answers");
}

#[test]
fn old_partial_tail_does_not_displace_newer_writes_in_history() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("ordering"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    device.write(b"prompt> ");
    read_until_contains(&ai, opened.device_id, "prompt> ", Duration::from_secs(2));

    ai.write_bytes(opened.device_id, b"LATEST", None)
        .expect("write");

    let one = ai.read(opened.device_id, Some(1)).expect("read one");
    assert_eq!(one.lines.len(), 1);
    assert_eq!(one.lines[0].text, "LATEST");

    let two = ai.read(opened.device_id, Some(2)).expect("read two");
    let texts: Vec<&str> = two.lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(texts, ["prompt> ", "LATEST"], "chronological order");
}

#[test]
fn successful_waited_write_returns_the_partial_tail_that_arrived() {
    let (_dir, socket) = fast();
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("tail-return"));
    let opened = ai.open(&device.path, Some(115_200), true).expect("open");

    let writer = ai.clone();
    let pending = std::thread::spawn(move || {
        writer.write_bytes(
            opened.device_id,
            b"cmd",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(2_000),
            }),
        )
    });
    device.read_until(b"cmd", Duration::from_secs(2));
    device.write(b"answer\nprompt> ");
    let written = pending.join().expect("write thread").expect("write");
    assert!(!written.timed_out);
    let last = written.lines.last().expect("partial tail");
    assert!(last.partial);
    assert_eq!(last.text, "prompt> ");
    assert!(written.lines.iter().any(|line| line.text == "answer"));

    // The tail is still in shared history for other clients.
    let history = ai.read(opened.device_id, Some(10)).expect("read");
    assert!(history
        .lines
        .iter()
        .any(|line| line.partial && line.text == "prompt> "));
}
