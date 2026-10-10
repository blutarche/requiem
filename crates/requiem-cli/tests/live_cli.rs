use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// Plays audio on the default output device and needs network access.
#[test]
#[ignore]
fn live_cli() {
    let dir = std::env::temp_dir().join(format!("requiem-live-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("live.script");
    std::fs::write(
        &script,
        "vol 5\n\
         search daft punk around the world\n\
         await results\n\
         play 1\n\
         await playing 20\n\
         sleep 4\n\
         seek +60\n\
         sleep 3\n\
         status\n\
         pause\n\
         sleep 1\n\
         resume\n\
         sleep 2\n\
         quit\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_requiem"))
        .arg("--script")
        .arg(&script)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "{:?}\n{stdout}\n{stderr}", out.status);
    assert!(stdout.contains("now playing:"), "{stdout}");
    assert!(!stdout.contains("track failed"), "{stdout}");
    assert!(!stdout.contains("provider broken"), "{stdout}");

    let status = stdout
        .lines()
        .find(|l| l.starts_with("status:"))
        .unwrap_or_else(|| panic!("no status line\n{stdout}"));
    let pos = status.split('|').nth(2).unwrap().split('/').next().unwrap();
    let (m, s) = pos.trim().split_once(':').unwrap();
    let secs = m.parse::<u64>().unwrap() * 60 + s.parse::<u64>().unwrap();
    assert!(secs >= 60, "position {pos} in {status}");
}

/// Plays audio on the default output device and needs network access.
#[test]
#[ignore]
fn live_cli_connect() {
    let dir = std::env::temp_dir().join(format!("rq-live-{}", std::process::id()));
    let socks = dir.join("sock");
    std::fs::create_dir_all(&socks).unwrap();
    std::fs::set_permissions(&socks, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let script = dir.join("live.script");
    std::fs::write(
        &script,
        "vol 5\n\
         search daft punk around the world\n\
         await results\n\
         play 1\n\
         await playing 20\n\
         sleep 5\n\
         status\n\
         shutdown-daemon\n",
    )
    .unwrap();

    // `requiemd` is a binary of another crate, so cargo does not build it for this test.
    let build = Command::new(env!("CARGO"))
        .args(["build", "-p", "requiem-daemon"])
        .status()
        .unwrap();
    assert!(build.success(), "{build:?}");
    let daemon_exe = Path::new(env!("CARGO_BIN_EXE_requiem")).with_file_name("requiemd");
    let mut daemon = Command::new(daemon_exe)
        .env("REQUIEM_SOCKET_DIR", &socks)
        .spawn()
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_requiem"))
        .env("REQUIEM_SOCKET_DIR", &socks)
        .arg("--connect")
        .arg("--script")
        .arg(&script)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let exit = wait_timeout(&mut daemon, Duration::from_secs(10));
    let _ = daemon.kill();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(out.status.success(), "{:?}\n{stdout}\n{stderr}", out.status);
    assert!(stdout.contains("now playing:"), "{stdout}");
    assert!(!stdout.contains("track failed"), "{stdout}");
    assert!(
        stdout.lines().any(|l| l.starts_with("status: playing")),
        "{stdout}"
    );
    assert_eq!(exit.and_then(|s| s.code()), Some(0), "daemon exit");
}

fn wait_timeout(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}
