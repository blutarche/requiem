use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use requiem_core::{Command, Event, SeekTarget};
use requiem_ipc::{Server, UpdateRequest, UpdateState, UpdateStatus};
use requiem_macos::{UiAction, UiUpdate, ViewTracker};

/// Runs the platform UI on the main thread until `stop` fires. On macOS that is the status
/// item and media keys; on Linux the tray icon and MPRIS, which run on their own threads
/// while this only waits. `stop_tx` is how the UI asks to quit. Each of `statuses` goes to
/// clients and the menu; the menu's update items call `on_update`. `exe` is
/// [`own_exe`]'s, which finds the GUI.
pub fn run_platform(
    stop_tx: &Sender<()>,
    stop: Receiver<()>,
    server: &Server,
    statuses: Receiver<UpdateStatus>,
    on_update: &(dyn Fn(UpdateRequest) + Sync),
    exe: Option<&Path>,
) {
    thread::scope(|scope| {
        let (updates_tx, updates) = unbounded();
        let (actions_tx, actions) = unbounded();
        // Closing `done` ends the view thread, which would otherwise wait on the server.
        let (done_tx, done) = bounded::<()>(0);
        let events = server.subscribe();
        #[cfg(target_os = "linux")]
        let (desktop, desktop_actions) = {
            let (tx, rx) = unbounded();
            (requiem_linux::start(tx), rx)
        };
        let _ = thread::Builder::new()
            .name("requiem-daemon-view".into())
            .spawn_scoped(scope, move || {
                let mut tracker = ViewTracker::new();
                loop {
                    select! {
                        recv(events) -> ev => match ev {
                            Ok(ev) => {
                                #[cfg(target_os = "linux")]
                                if let Some(desktop) = &desktop {
                                    desktop.apply(&ev);
                                }
                                let view = tracker.apply(&ev).map(UiUpdate::View);
                                let extra = match ev {
                                    Event::Position(at) if requiem_macos::wants_positions() => {
                                        Some(UiUpdate::Position(at))
                                    }
                                    Event::Volume(v) => Some(UiUpdate::Volume(v)),
                                    _ => None,
                                };
                                let mut sent = false;
                                for update in view.into_iter().chain(extra) {
                                    if updates_tx.send(update).is_err() {
                                        return;
                                    }
                                    sent = true;
                                }
                                if sent {
                                    requiem_macos::wake();
                                }
                            }
                            Err(_) => return,
                        },
                        recv(statuses) -> status => {
                            let Ok(status) = status else { return };
                            let update = UiUpdate::Updates {
                                check: !matches!(
                                    status.state,
                                    UpdateState::NotUpdatable { .. } | UpdateState::Unsupported
                                ),
                                ready: match &status.state {
                                    UpdateState::Ready { version } => Some(version.clone()),
                                    _ => None,
                                },
                            };
                            server.set_update(status);
                            if updates_tx.send(update).is_err() {
                                return;
                            }
                            requiem_macos::wake();
                        }
                        recv(done) -> _ => return,
                    }
                }
            });
        let _ = thread::Builder::new()
            .name("requiem-daemon-actions".into())
            .spawn_scoped(scope, move || {
                // Ends when `run` returns and drops its sender.
                for action in actions {
                    match action {
                        UiAction::Toggle => server.send(Command::TogglePause),
                        UiAction::Next => server.send(Command::Next),
                        UiAction::Prev => server.send(Command::Prev),
                        UiAction::Seek(to) => server.send(Command::Seek(SeekTarget::Absolute(to))),
                        UiAction::SetVolume(v) => server.send(Command::SetVolume(v)),
                        UiAction::OpenGui => {
                            if !server.raise_gui() {
                                launch_gui(exe);
                            }
                        }
                        UiAction::CheckForUpdates => on_update(UpdateRequest::Check),
                        UiAction::RestartToUpdate => on_update(UpdateRequest::Install),
                        UiAction::Quit => crate::request_stop(stop_tx),
                    }
                }
            });
        #[cfg(target_os = "linux")]
        let _ = thread::Builder::new()
            .name("requiem-daemon-desktop".into())
            .spawn_scoped(scope, move || {
                // Ends once the view thread drops `desktop`, the last holder of a sender.
                for action in desktop_actions {
                    match action.command() {
                        Some(cmd) => server.send(cmd),
                        None => crate::request_stop(stop_tx),
                    }
                }
            });
        requiem_macos::run(updates, actions_tx, stop);
        drop(done_tx);
    });
}

/// The app that holds `exe` as a helper (`X.app/Contents/Helpers/requiemd.app/...`), which is
/// the GUI app to `open`. `None` for dev builds, which have no outer app.
pub(crate) fn gui_app(exe: &Path) -> Option<&Path> {
    exe.ancestors().find(|dir| {
        dir.extension().is_some_and(|e| e == "app") && exe.starts_with(dir.join("Contents/Helpers"))
    })
}

/// This player's executable without `..` or symbolic links, so `gui_app` finds the app
/// whatever path the player was started by. Called once at start-up: an update moves the
/// files only later.
pub(crate) fn own_exe() -> Option<PathBuf> {
    match std::env::current_exe() {
        Ok(exe) => Some(resolved(exe)),
        Err(e) => {
            eprintln!("requiemd: cannot locate this executable: {e}");
            None
        }
    }
}

fn resolved(exe: PathBuf) -> PathBuf {
    fs::canonicalize(&exe).unwrap_or(exe)
}

fn launch_gui(exe: Option<&Path>) {
    let Some(exe) = exe else {
        eprintln!("requiemd: cannot locate the GUI");
        return;
    };
    match gui_app(exe) {
        Some(app) => open_app(app),
        None => {
            let gui: PathBuf = exe.with_file_name("requiem-gui");
            if !gui.exists() {
                eprintln!("requiemd: {} not found", gui.display());
                return;
            }
            launched(spawn_detached(&gui));
        }
    }
}

/// Opens `app` by its exact path.
pub(crate) fn open_app(app: &Path) {
    launched(Process::new("/usr/bin/open").arg(app).spawn());
}

fn launched(child: std::io::Result<Child>) {
    match child {
        Ok(child) => reap(child),
        Err(e) => eprintln!("requiemd: cannot launch the GUI: {e}"),
    }
}

fn spawn_detached(exe: &Path) -> std::io::Result<Child> {
    let mut cmd = Process::new(exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, so it may run between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Waits from a thread so a launcher that exits is not left a zombie.
fn reap(mut child: Child) {
    let _ = thread::Builder::new()
        .name("requiem-daemon-reaper".into())
        .spawn(move || {
            let status = child.wait();
            debug!("gui launcher exited: {status:?}");
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_layout_opens_the_outer_app() {
        let exe = Path::new(
            "/Applications/requiem.app/Contents/Helpers/requiemd.app/Contents/MacOS/requiemd",
        );
        assert_eq!(gui_app(exe), Some(Path::new("/Applications/requiem.app")));
    }

    #[test]
    fn dev_layouts_have_no_outer_app() {
        assert_eq!(gui_app(Path::new("/usr/local/bin/requiemd")), None);
        assert_eq!(gui_app(Path::new("/x/target/release/requiemd")), None);
        assert_eq!(
            gui_app(Path::new("/x/requiem.app/Contents/MacOS/requiemd")),
            None
        );
        assert_eq!(
            gui_app(Path::new("/x/requiem.app/Contents/Resources/requiemd")),
            None
        );
    }

    #[test]
    fn a_player_started_through_dot_dot_finds_its_app_once_resolved() {
        let dir = std::env::temp_dir().join(format!("rqui{}", std::process::id()));
        let app = dir.join("Requiem.app");
        let helper = app.join("Contents/Helpers/requiemd.app/Contents/MacOS");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(&helper).unwrap();
        fs::write(helper.join("requiemd"), b"").unwrap();
        let started = app.join("Contents/MacOS/../Helpers/requiemd.app/Contents/MacOS/requiemd");
        let raw = gui_app(&started).map(Path::to_owned);
        let exe = resolved(started);
        let found = gui_app(&exe).map(Path::to_owned);
        let want = fs::canonicalize(&app).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(raw, None);
        assert_eq!(found, Some(want));
        let missing = PathBuf::from("/nonexistent/requiemd");
        assert_eq!(resolved(missing.clone()), missing);
    }
}
