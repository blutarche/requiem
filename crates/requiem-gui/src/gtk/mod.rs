//! The Linux window, on GTK 4.

mod art;
mod grid;
mod icons;
mod pages;
mod prefs;
mod shelves;
mod songs;
mod theme;
mod ui;
mod widgets;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib, graphene};

use crate::args::{Args, View};
use crate::backend::{Backend, Hooks, Live, Msg};
use crate::demo;
use crate::search_keys::{Dir, Spot};
use crate::view::{AppView, SnapshotState};
use crate::vm::{Intent, Vm};
use ui::Ui;

pub const APP_ID: &str = "io.github.blutarche.Requiem";

/// How long a snapshot of the connected window waits for its page and covers.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(20);
const SNAPSHOT_POLL: Duration = Duration::from_millis(100);

const HOOKS: Hooks = Hooks { post, quit };

fn post(batch: Vec<Msg>) {
    glib::MainContext::default().invoke(move || {
        ui::with(|ui| ui.receive(batch));
    });
}

/// The player quit on purpose: the window closes with it rather than respawn it.
fn quit() {
    glib::MainContext::default().invoke(|| {
        if let Some(app) = gio::Application::default() {
            app.quit();
        }
    });
}

pub fn run(args: Args) -> i32 {
    // Demo and snapshot runs stand alone; a normal launch raises the window already open.
    let alone = args.uses_demo_data() || args.snapshot.is_some();
    let flags = if alone {
        gio::ApplicationFlags::NON_UNIQUE
    } else {
        gio::ApplicationFlags::empty()
    };
    let app = gtk::Application::new(Some(APP_ID), flags);
    let args = Rc::new(RefCell::new(Some(args)));
    app.connect_activate(move |app| {
        if let Some(w) = app.active_window() {
            w.present();
            return;
        }
        if let Some(args) = args.borrow_mut().take() {
            start(app, &args);
        }
    });
    app.run_with_args::<&str>(&[]).into()
}

fn start(app: &gtk::Application, args: &Args) {
    theme::install();
    let (vm, backend) = if args.uses_demo_data() {
        art::seed_demo(demo::covers());
        let (vm, daemon) = demo::start_in(args.view);
        (vm, Backend::Demo(RefCell::new(daemon)))
    } else {
        let mut vm = Vm::new();
        vm.intent(Intent::SetFilter(prefs::service_filter()));
        vm.intent(Intent::SetLibrarySort(prefs::library_sort()));
        (vm, Backend::Live(Live::start(HOOKS)))
    };
    let ui = Ui::new(app, vm, backend);
    ui::install(ui.clone());
    if let Some((w, h)) = args.size {
        ui.window.set_default_size(w as i32, h as i32);
    }
    install_keys(&ui);
    ui.render_now();
    show_view(&ui, args.view);
    let snapshot = args.snapshot.clone();
    let demo = args.uses_demo_data();
    if snapshot.is_some() {
        // Drawn offscreen-equivalent: nothing asks for the user's attention.
        ui.window.set_decorated(false);
    }
    ui.window.present();
    if snapshot.is_none() && !ui.palette.is_open() {
        ui.show(ui.current_view());
    }
    match snapshot {
        Some(path) if demo => demo_snapshot(path),
        Some(path) => live_snapshot(path, Instant::now()),
        None => {}
    }
    if args.smoke {
        glib::timeout_add_local_once(Duration::from_secs(2), || std::process::exit(0));
    }
}

fn show_view(ui: &Ui, view: View) {
    match view {
        View::Home => ui.show(AppView::Home),
        View::HomeScrolled => {
            ui.show(AppView::Home);
            // Scrolled once laid out, which the shelves need to know their height.
            glib::idle_add_local_once(|| {
                ui::with(|ui| ui.shelves.scroll_to_end());
            });
        }
        View::HomeShelfPaged => {
            ui.show(AppView::Home);
            glib::idle_add_local_once(|| {
                ui::with(|ui| {
                    if let Some(row) = ui.shelves.first_card_row() {
                        ui.shelves.page(row, true);
                    }
                });
            });
        }
        View::HomeSpotify | View::HomeSignedOut => ui.show(AppView::Home),
        View::Library | View::LibrarySignedOut | View::LibraryUnavailable | View::SignIn => {
            ui.show(AppView::Library);
        }
        View::Collection | View::Artist | View::ArtistSpotify | View::Album => {
            ui.show(AppView::Collection);
        }
        View::Queue => ui.show(AppView::Queue),
        View::Settings | View::SettingsSession => ui.show(AppView::Settings),
        View::NowPlaying | View::NowPlayingEmpty | View::NowPlayingLast => ui.show_now_playing(),
        View::Palette => {
            // Over Home, as the macOS window shows it.
            ui.show(AppView::Home);
            ui.open_palette();
        }
        View::Search
        | View::Empty
        | View::Disconnected
        | View::Toast
        | View::OutOfDate
        | View::UpdateReady
        | View::UpdateFailed
        | View::NotUpdatable
        | View::UpToDate
        | View::SearchAlbums
        | View::SearchSongs
        | View::SearchArtists => ui.show(AppView::Search),
        View::SearchSongSelected => {
            let _ = ui.results_move_down();
            ui.move_all_selection(Dir::Down);
            ui.move_all_selection(Dir::Down);
        }
        View::SearchCardSelected => {
            let _ = ui.results_move_down();
            // Once laid out, so the selected card scrolls into view.
            glib::idle_add_local_once(|| {
                ui::with(|ui| {
                    // Down through the songs and the Artists row to the Albums row, then along it.
                    for _ in 0..64 {
                        if matches!(ui.all_spot(), Some(Spot::Card { row: 1, .. })) {
                            break;
                        }
                        ui.move_all_selection(Dir::Down);
                    }
                    ui.move_all_selection(Dir::Right);
                    ui.move_all_selection(Dir::Right);
                });
            });
        }
    }
}

/// The window's keys, heard before the focused widget.
fn install_keys(ui: &Rc<Ui>) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(|_, key, _, mods| {
        if ui::with(|ui| ui.key(key, mods)).unwrap_or(false) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    ui.window.add_controller(keys);
}

/// Writes the demo window once it is laid out and its covers are drawn.
fn demo_snapshot(path: PathBuf) {
    let started = Instant::now();
    poll(move || {
        let ready = ui::with(|ui| {
            ui.render_now();
            ui.root.width() > 0 && ui.root.is_mapped()
        })
        .unwrap_or(false);
        // Two polls at least, so a layout started by the first frame has settled.
        if ready && !art::busy() && started.elapsed() >= SNAPSHOT_POLL * 3
            || started.elapsed() >= SNAPSHOT_WAIT
        {
            exit_with_snapshot(&path);
            return true;
        }
        false
    });
}

/// Writes the connected window's snapshot once its page is in; exits 1 when the page failed
/// or never came.
fn live_snapshot(path: PathBuf, started: Instant) {
    poll(move || {
        let state = ui::with(|ui| {
            ui.render_now();
            ui.snapshot_state()
        });
        let waited = started.elapsed();
        match state {
            Some(SnapshotState::Loaded { covers: false }) => {
                exit_with_snapshot(&path);
                return true;
            }
            // Covers that never arrive show their placeholders.
            Some(SnapshotState::Loaded { covers: true }) if waited >= SNAPSHOT_WAIT => {
                exit_with_snapshot(&path);
                return true;
            }
            Some(SnapshotState::Failed(e)) => {
                eprintln!("requiem-gui: the page did not load: {e}");
                std::process::exit(1);
            }
            _ if waited >= SNAPSHOT_WAIT => {
                eprintln!(
                    "requiem-gui: the page did not load in {} s",
                    SNAPSHOT_WAIT.as_secs()
                );
                std::process::exit(1);
            }
            _ => {}
        }
        false
    });
}

/// Runs `f` every `SNAPSHOT_POLL`, once nothing else is pending: layout, deferred fills and
/// frames go first, which a slow software renderer would otherwise starve. Stops once `f`
/// returns true.
fn poll(mut f: impl FnMut() -> bool + 'static) {
    glib::timeout_add_local_full(SNAPSHOT_POLL, glib::Priority::LOW, move || {
        if f() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Writes the snapshot after the next frame is laid out: a widget waiting for its
/// allocation draws nothing, so a scroll just before would blank the picture.
fn exit_with_snapshot(path: &Path) {
    let clock = ui::with(|ui| {
        ui.window.queue_draw();
        ui.window.frame_clock()
    })
    .flatten();
    let Some(clock) = clock else {
        write_and_exit(path)
    };
    let path = path.to_owned();
    clock.connect_after_paint(move |_| write_and_exit(&path));
    clock.request_phase(gdk::FrameClockPhase::LAYOUT | gdk::FrameClockPhase::PAINT);
}

fn write_and_exit(path: &Path) -> ! {
    match ui::with(|ui| write_snapshot(&ui.root, path)) {
        Some(Ok(())) => std::process::exit(0),
        Some(Err(e)) => {
            eprintln!("requiem-gui: snapshot failed: {e}");
            std::process::exit(1);
        }
        None => std::process::exit(1),
    }
}

/// Renders `root` as the window shows it into a PNG at `path`.
fn write_snapshot(root: &impl IsA<gtk::Widget>, path: &Path) -> Result<(), String> {
    let (w, h) = (root.width(), root.height());
    if w <= 0 || h <= 0 {
        return Err("the window has no size".into());
    }
    let paintable = gtk::WidgetPaintable::new(Some(root));
    let snap = gtk::Snapshot::new();
    // The window's background, which the root draws through.
    snap.append_color(
        &gdk::RGBA::new(
            theme::rgb(theme::BG).0 as f32,
            theme::rgb(theme::BG).1 as f32,
            theme::rgb(theme::BG).2 as f32,
            1.0,
        ),
        &graphene::Rect::new(0.0, 0.0, w as f32, h as f32),
    );
    paintable.snapshot(&snap, f64::from(w), f64::from(h));
    let node = snap.to_node().ok_or("nothing to draw")?;
    let renderer = root
        .native()
        .map(|n| n.renderer())
        .ok_or("the window has no renderer")?
        .ok_or("the window has no renderer")?;
    let texture = renderer.render_texture(
        node,
        Some(&graphene::Rect::new(0.0, 0.0, w as f32, h as f32)),
    );
    texture.save_to_png(path).map_err(|e| e.to_string())
}
