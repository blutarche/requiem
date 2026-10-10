//! `Client::connect_or_spawn` against a fake daemon: this test binary itself, re-run with
//! `FAKE_DAEMON` set, serves a `Session` over fakes.

use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::panic;
use std::process;
use std::time::Duration;

use crossbeam_channel::{Receiver, bounded};
use requiem_core::testing::{FakeEngineHandle, FakeProvider};
use requiem_core::{Command, Event, Session, SessionConfig};
use requiem_ipc::{Client, Incoming, Role, ServeError, Server, ServerHooks, socket_dir};

const FAKE_DAEMON: &str = "REQUIEM_IPC_TEST_FAKE_DAEMON";
const WAIT: Duration = Duration::from_secs(10);

fn main() {
    if env::var_os(FAKE_DAEMON).is_some() {
        fake_daemon();
        return;
    }
    let tmp = env::temp_dir().join(format!("rqs{}", process::id()));
    fs::create_dir(&tmp).unwrap();
    // SAFETY: still single-threaded; the spawned daemon inherits both variables.
    unsafe {
        env::set_var("REQUIEM_SOCKET_DIR", tmp.join("s"));
        env::set_var(FAKE_DAEMON, "1");
    }
    let result = panic::catch_unwind(run);
    let _ = fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        panic::resume_unwind(e);
    }
    println!("test connect_or_spawn ... ok");
}

fn run() {
    let exe = env::current_exe().unwrap();
    let log = socket_dir().unwrap().join("daemon.log");
    fs::write(&log, "left over from an earlier run\n").unwrap();

    let (client, rx) = Client::connect_or_spawn(&exe, Role::Cli, WAIT).unwrap();
    expect_snapshot(&rx);

    let text = fs::read_to_string(&log).unwrap();
    assert!(!text.contains("left over"), "log not truncated: {text:?}");
    let line = text
        .lines()
        .find(|l| l.starts_with("fake daemon"))
        .unwrap_or_else(|| panic!("no daemon output in the log: {text:?}"));
    let field = |name: &str| {
        line.split(' ')
            .find_map(|kv| kv.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in {line:?}"))
    };
    assert_eq!(
        field("pid="),
        field("sid="),
        "daemon is not a session leader"
    );
    // SAFETY: getsid has no preconditions.
    let own_sid = unsafe { libc::getsid(0) };
    assert_ne!(field("sid="), own_sid.to_string());
    assert_eq!(field("stdin_null="), "true");

    // A running daemon is reused; a respawn would have truncated the log.
    let (again, again_rx) = Client::connect_or_spawn(&exe, Role::Gui, WAIT).unwrap();
    expect_snapshot(&again_rx);
    assert_eq!(fs::read_to_string(&log).unwrap(), text);

    client.send(Command::Shutdown).unwrap();
    for rx in [&rx, &again_rx] {
        loop {
            if let Incoming::Disconnected(_) = rx.recv_timeout(WAIT).expect("still connected") {
                break;
            }
        }
    }
    drop(again);
}

fn expect_snapshot(rx: &Receiver<Incoming>) {
    let first = rx.recv_timeout(WAIT).unwrap();
    assert!(
        matches!(first, Incoming::Event(Event::QueueChanged { .. })),
        "{first:?}"
    );
}

fn fake_daemon() {
    let same = |a: &str, b: &str| match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.rdev() == b.rdev(),
        _ => false,
    };
    // SAFETY: getsid has no preconditions.
    let sid = unsafe { libc::getsid(0) };
    println!(
        "fake daemon pid={} sid={sid} stdin_null={}",
        process::id(),
        same("/dev/fd/0", "/dev/null")
    );

    let session = Session::spawn(
        vec![FakeProvider::new("fake")],
        FakeEngineHandle::new().factory(),
        SessionConfig::default(),
    );
    let (quit_tx, quit_rx) = bounded(1);
    let hooks = ServerHooks {
        on_shutdown: Box::new(move || {
            let _ = quit_tx.try_send(());
        }),
    };
    let server = match Server::start(session, hooks) {
        Ok(s) => s,
        Err(ServeError::AlreadyRunning) => return,
        Err(e) => {
            eprintln!("fake daemon: {e}");
            process::exit(1);
        }
    };
    // Bounded, so a failed run leaves no daemon behind.
    let _ = quit_rx.recv_timeout(Duration::from_secs(30));
    server.shutdown();
}
