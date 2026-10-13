//! StatusNotifierItem tray icon and its dbusmenu, through ksni.

use crossbeam_channel::Sender;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::StandardItem;
use ksni::{MenuItem, Status, ToolTip};
use requiem_core::PlayState;

use crate::model::{Action, Snapshot, icon_name};

pub const ID: &str = "requiem";

pub struct Tray {
    snap: Snapshot,
    /// `None` once the handle is dropped.
    actions: Option<Sender<Action>>,
}

impl Tray {
    fn send(&self, action: Action) {
        if let Some(actions) = &self.actions {
            let _ = actions.send(action);
        }
    }

    fn item(label: &str, action: Action) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            activate: Box::new(move |t: &mut Self| t.send(action)),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        ID.into()
    }

    fn title(&self) -> String {
        "Requiem".into()
    }

    fn icon_name(&self) -> String {
        icon_name(self.snap.state).into()
    }

    fn status(&self) -> Status {
        match self.snap.state {
            PlayState::Stopped => Status::Passive,
            _ => Status::Active,
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.snap.headline().unwrap_or_default(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Action::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let playing = matches!(self.snap.state, PlayState::Playing | PlayState::Buffering);
        vec![
            StandardItem {
                label: self
                    .snap
                    .headline()
                    .unwrap_or_else(|| "Nothing playing".into()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            Self::item(if playing { "Pause" } else { "Play" }, Action::Toggle),
            Self::item("Next", Action::Next),
            Self::item("Previous", Action::Prev),
            MenuItem::Separator,
            Self::item("Quit", Action::Quit),
        ]
    }
}

pub struct TrayHandle(Handle<Tray>);

impl TrayHandle {
    pub fn start(actions: Sender<Action>) -> Result<Self, ksni::Error> {
        let tray = Tray {
            snap: Snapshot::default(),
            actions: Some(actions),
        };
        tray.spawn().map(Self)
    }

    pub fn update(&self, snap: &Snapshot) {
        self.0.update(|t| t.snap = snap.clone());
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        // ksni drops the tray only when its connection's tasks finish, which can be never once
        // their driver stops; the daemon's action thread waits for every sender to go.
        self.0.update(|t| t.actions = None);
        self.0.shutdown().wait();
    }
}
