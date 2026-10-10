use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use crossbeam_channel::Sender;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};
use requiem_core::PlayState;

use super::is_active;
use crate::{PlayerView, UiAction};

/// `MPNowPlayingInfoCenter` plus the `MPRemoteCommandCenter` handlers.
pub(super) struct NowPlaying {
    center: Retained<MPNowPlayingInfoCenter>,
    /// Read by remote handlers, which MediaPlayer does not promise to call on the main thread.
    active: Arc<AtomicBool>,
    targets: Vec<(Retained<MPRemoteCommand>, Retained<AnyObject>)>,
}

fn status(sent: bool) -> MPRemoteCommandHandlerStatus {
    if sent {
        MPRemoteCommandHandlerStatus::Success
    } else {
        MPRemoteCommandHandlerStatus::CommandFailed
    }
}

fn add_handler(
    command: Retained<MPRemoteCommand>,
    handler: impl Fn(&MPRemoteCommandEvent) -> MPRemoteCommandHandlerStatus + 'static,
) -> (Retained<MPRemoteCommand>, Retained<AnyObject>) {
    let block = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        // SAFETY: MediaPlayer passes a valid event that lives for the duration of the call.
        handler(unsafe { event.as_ref() })
    });
    // SAFETY: the block matches MediaPlayer's handler signature, and `command` belongs to the
    // shared command center.
    let target = unsafe {
        command.setEnabled(true);
        command.addTargetWithHandler(&block)
    };
    (command, target)
}

impl NowPlaying {
    pub(super) fn new(actions: Sender<UiAction>) -> Self {
        let active = Arc::new(AtomicBool::new(false));
        // SAFETY: plain getters on MediaPlayer's shared singletons.
        let (center, commands) = unsafe {
            (
                MPNowPlayingInfoCenter::defaultCenter(),
                MPRemoteCommandCenter::sharedCommandCenter(),
            )
        };
        // SAFETY: plain getters on the shared command center.
        let (play, pause, toggle, next, prev, seek) = unsafe {
            (
                commands.playCommand(),
                commands.pauseCommand(),
                commands.togglePlayPauseCommand(),
                commands.nextTrackCommand(),
                commands.previousTrackCommand(),
                Retained::into_super(commands.changePlaybackPositionCommand()),
            )
        };

        let send = {
            let actions = actions.clone();
            move |action| actions.send(action).is_ok()
        };
        // `UiAction` has only a toggle, so play and pause toggle only out of the other state.
        let toggle_unless = |want_active: bool| {
            let (send, active) = (send.clone(), Arc::clone(&active));
            move |_: &MPRemoteCommandEvent| {
                if active.load(Ordering::Relaxed) == want_active {
                    return MPRemoteCommandHandlerStatus::Success;
                }
                status(send(UiAction::Toggle))
            }
        };
        let simple = |action: UiAction| {
            let send = send.clone();
            move |_: &MPRemoteCommandEvent| status(send(action))
        };

        let targets = vec![
            add_handler(play, toggle_unless(true)),
            add_handler(pause, toggle_unless(false)),
            add_handler(toggle, simple(UiAction::Toggle)),
            add_handler(next, simple(UiAction::Next)),
            add_handler(prev, simple(UiAction::Prev)),
            add_handler(seek, move |event| {
                let Some(event) = event.downcast_ref::<MPChangePlaybackPositionCommandEvent>()
                else {
                    return MPRemoteCommandHandlerStatus::CommandFailed;
                };
                // SAFETY: plain getter on the event MediaPlayer passed in.
                let secs = unsafe { event.positionTime() };
                let to = Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or_default();
                status(send(UiAction::Seek(to)))
            }),
        ];

        Self {
            center,
            active,
            targets,
        }
    }

    /// Elapsed time is set only here, i.e. only when the view changed; macOS extrapolates
    /// it from the rate in between.
    pub(super) fn show(&self, view: Option<&PlayerView>) {
        let Some(view) = view else {
            self.active.store(false, Ordering::Relaxed);
            // SAFETY: `nil` clears the info; the state setter takes a plain enum.
            unsafe {
                self.center.setNowPlayingInfo(None);
                self.center
                    .setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            }
            return;
        };
        self.active.store(is_active(view.state), Ordering::Relaxed);

        let title = NSString::from_str(&view.title);
        let artist = NSString::from_str(&view.artists);
        let album = view.album.as_deref().map(NSString::from_str);
        let duration = view.duration.map(|d| NSNumber::new_f64(d.as_secs_f64()));
        let elapsed = NSNumber::new_f64(view.position.as_secs_f64());
        let rate = if view.state == PlayState::Playing {
            1.0
        } else {
            0.0
        };
        let rate = NSNumber::new_f64(rate);

        // SAFETY: the keys are immutable string constants from MediaPlayer.
        let (k_title, k_artist, k_album, k_duration, k_elapsed, k_rate) = unsafe {
            (
                MPMediaItemPropertyTitle,
                MPMediaItemPropertyArtist,
                MPMediaItemPropertyAlbumTitle,
                MPMediaItemPropertyPlaybackDuration,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                MPNowPlayingInfoPropertyPlaybackRate,
            )
        };
        let mut keys: Vec<&NSString> = vec![k_title, k_artist, k_elapsed, k_rate];
        let mut values: Vec<&AnyObject> = Vec::with_capacity(6);
        values.push(&title);
        values.push(&artist);
        values.push(&elapsed);
        values.push(&rate);
        if let Some(album) = &album {
            keys.push(k_album);
            values.push(album);
        }
        if let Some(duration) = &duration {
            keys.push(k_duration);
            values.push(duration);
        }
        let info = NSDictionary::from_slices(&keys, &values);

        // Buffering reports `Playing` at rate 0 so Control Center offers pause, as the menu does.
        let state = match view.state {
            PlayState::Playing | PlayState::Buffering => MPNowPlayingPlaybackState::Playing,
            PlayState::Paused => MPNowPlayingPlaybackState::Paused,
            PlayState::Stopped => MPNowPlayingPlaybackState::Stopped,
        };
        // SAFETY: the info dictionary maps MediaPlayer keys to the NSString and NSNumber
        // values those keys document.
        unsafe {
            self.center.setNowPlayingInfo(Some(&info));
            self.center.setPlaybackState(state);
        }
    }

    pub(super) fn close(&self) {
        for (command, target) in &self.targets {
            // SAFETY: `target` is the token `addTargetWithHandler` returned for `command`.
            unsafe { command.removeTarget(Some(target)) };
        }
        self.show(None);
    }
}
