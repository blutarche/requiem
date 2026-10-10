use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use requiem_core::{Command, SeekTarget};
use requiem_ipc::Server;
use requiem_macos::{UiAction, ViewTracker};

/// Runs the platform UI on the main thread until `stop` fires. On macOS that is the status
/// item and media keys; elsewhere it only waits. `stop_tx` is how the UI asks to quit.
pub fn run_platform(stop_tx: &Sender<()>, stop: Receiver<()>, server: &Server) {
    thread::scope(|scope| {
        let (views_tx, views) = unbounded();
        let (actions_tx, actions) = unbounded();
        // Closing `done` ends the view thread, which would otherwise wait on the server.
        let (done_tx, done) = bounded::<()>(0);
        let events = server.subscribe();
        let _ = thread::Builder::new()
            .name("requiem-daemon-view".into())
            .spawn_scoped(scope, move || {
                let mut tracker = ViewTracker::new();
                loop {
                    select! {
                        recv(events) -> ev => match ev {
                            Ok(ev) => {
                                if let Some(view) = tracker.apply(&ev)
                                    && views_tx.send(view).is_err()
                                {
                                    return;
                                }
                            }
                            Err(_) => return,
                        },
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
                        UiAction::OpenGui => {
                            if !server.raise_gui() {
                                launch_gui();
                            }
                        }
                        UiAction::Quit => {
                            let _ = stop_tx.try_send(());
                        }
                    }
                }
            });
        requiem_macos::run(views, actions_tx, stop);
        drop(done_tx);
    });
}

/// The app that holds `exe` as a helper (`X.app/Contents/Helpers/requiemd.app/...`), which is
/// the GUI app to `open`. `None` for dev builds, which have no outer app.
fn gui_app(exe: &Path) -> Option<&Path> {
    exe.ancestors().find(|dir| {
        dir.extension().is_some_and(|e| e == "app") && exe.starts_with(dir.join("Contents/Helpers"))
    })
}

fn launch_gui() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("requiemd: cannot locate the GUI: {e}");
            return;
        }
    };
    let child = match gui_app(&exe) {
        Some(app) => Process::new("/usr/bin/open").arg(app).spawn(),
        None => {
            let gui: PathBuf = exe.with_file_name("requiem-gui");
            if !gui.exists() {
                eprintln!("requiemd: {} not found", gui.display());
                return;
            }
            spawn_detached(&gui)
        }
    };
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
}
