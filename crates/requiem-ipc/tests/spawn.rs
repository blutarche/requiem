//! `Client::connect_or_spawn` against a fake daemon: this test binary itself, re-run with
//! `FAKE_DAEMON` set, serves a `Session` over fakes and writes what it saw to daemon.log.

use std::env;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::panic;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use requiem_core::testing::{FakeEngineHandle, FakeProvider};
use requiem_core::{Command, Event, Session, SessionConfig};
use requiem_ipc::{
    Client, Incoming, Role, ServeError, Server, ServerHooks, UpdateState, UpdateStatus, socket_dir,
};

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
    let dir = socket_dir().unwrap();
    let log = dir.join("daemon.log");
    // Over the rotation threshold, so the first spawn moves it aside.
    let old = "left over from an earlier run\n".repeat(40_000);
    fs::write(&log, &old).unwrap();

    // Plays a daemon that is stopping: it holds the lock, but its socket is gone. The first
    // spawn exits 0 on seeing it; the lock is let go after that, so the one respawn wins.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("daemon.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let release = {
        let log = log.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + WAIT;
            while !fs::read_to_string(&log).is_ok_and(|t| t.contains("already running")) {
                assert!(Instant::now() < deadline, "the first daemon never gave up");
                thread::sleep(Duration::from_millis(10));
            }
            drop(lock);
        })
    };
    let (client, rx) = Client::connect_or_spawn(&exe, Role::Cli, WAIT).unwrap();
    release.join().unwrap();
    expect_snapshot(&rx);

    assert_eq!(fs::read_to_string(dir.join("daemon.log.old")).unwrap(), old);
    let text = fs::read_to_string(&log).unwrap();
    assert_eq!(text.matches("already running").count(), 1, "{text:?}");
    let daemons: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("fake daemon"))
        .collect();
    assert_eq!(daemons.len(), 2, "{text:?}");
    // SAFETY: getsid has no preconditions.
    let own_sid = unsafe { libc::getsid(0) }.to_string();
    for line in daemons {
        let field = |name: &str| {
            line.split(' ')
                .find_map(|kv| kv.strip_prefix(name))
                .unwrap_or_else(|| panic!("no {name} in {line:?}"))
        };
        assert_eq!(field("pid="), field("sid="), "not a session leader: {line}");
        assert_ne!(field("sid="), own_sid);
        assert_eq!(field("stdin_null="), "true");
    }

    // A running daemon is reused, not respawned.
    let (again, again_rx) = Client::connect_or_spawn(&exe, Role::Gui, WAIT).unwrap();
    expect_snapshot(&again_rx);
    assert_eq!(fs::read_to_string(&log).unwrap(), text);

    client.send(Command::Shutdown).unwrap();
    for rx in [&rx, &again_rx] {
        loop {
            match rx.recv_timeout(WAIT).expect("still connected") {
                Incoming::Goodbye => break,
                Incoming::Disconnected(why) => panic!("disconnected without a Goodbye: {why}"),
                _ => {}
            }
        }
    }
    drop(again);

    // A daemon that fails is reported, pointing at the log.
    let fails = ["/usr/bin/false", "/bin/false"]
        .map(Path::new)
        .into_iter()
        .find(|p| p.exists())
        .expect("no false(1)");
    let Err(err) = Client::connect_or_spawn(fails, Role::Cli, WAIT) else {
        panic!("connected through {}", fails.display());
    };
    let msg = err.to_string();
    assert!(msg.contains("exited with"), "{msg}");
    assert!(msg.contains(&*log.to_string_lossy()), "{msg}");
}

fn idle_update() -> UpdateStatus {
    UpdateStatus {
        current: "test".into(),
        auto: true,
        state: UpdateState::Idle,
    }
}

fn expect_snapshot(rx: &Receiver<Incoming>) {
    assert_eq!(
        rx.recv_timeout(WAIT).unwrap(),
        Incoming::Update(idle_update())
    );
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
        on_update: Box::new(|_| {}),
    };
    let server = match Server::start(session, hooks, idle_update()) {
        Ok(s) => s,
        Err(ServeError::AlreadyRunning) => {
            println!("already running");
            return;
        }
        Err(e) => {
            eprintln!("fake daemon: {e}");
            process::exit(1);
        }
    };
    // Bounded, so a failed run leaves no daemon behind.
    let _ = quit_rx.recv_timeout(Duration::from_secs(30));
    server.shutdown();
}
