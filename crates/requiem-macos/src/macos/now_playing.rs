use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use crossbeam_channel::Sender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, Message};
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{
    MainThreadMarker, NSCopying, NSDictionary, NSMutableDictionary, NSNumber, NSString,
};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommand,
    MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use requiem_core::PlayState;

use super::{art, is_active};
use crate::{PlayerView, UiAction};

/// Decoded size of the cover handed to MediaPlayer: Control Center and Raycast draw it up to
/// about 300 pt, so 600 px is sharp on Retina without holding a full-size image.
const ARTWORK_PX: u32 = 600;

/// The artwork for the URL the published info is currently about.
#[derive(Default)]
struct Cover {
    url: Option<String>,
    artwork: Option<Retained<MPMediaItemArtwork>>,
}

/// `MPNowPlayingInfoCenter` plus the `MPRemoteCommandCenter` handlers.
pub(super) struct NowPlaying {
    center: Retained<MPNowPlayingInfoCenter>,
    /// Read by remote handlers, which MediaPlayer does not promise to call on the main thread.
    active: Arc<AtomicBool>,
    targets: Vec<(Retained<MPRemoteCommand>, Retained<AnyObject>)>,
    /// Shared with the callback that adds a cover once it has loaded.
    cover: Rc<RefCell<Cover>>,
}

fn artwork_for(image: &NSImage) -> Retained<MPMediaItemArtwork> {
    let image = image.retain();
    let ptr = NonNull::from(&*image);
    // The handler returns `image`, which the block keeps alive for as long as the artwork
    // holds the block; MediaPlayer scales it to whatever size it asks for.
    let handler = RcBlock::new(move |_: CGSize| {
        let _keep = &image;
        ptr
    });
    let side = f64::from(ARTWORK_PX);
    // SAFETY: the handler returns a valid image pointer, kept alive by the block's capture.
    unsafe {
        MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            CGSize::new(side, side),
            &handler,
        )
    }
}

/// Adds `artwork` to the info already published, leaving elapsed time and rate as they are.
fn add_artwork(center: &MPNowPlayingInfoCenter, artwork: &MPMediaItemArtwork) {
    // SAFETY: plain getter on the shared info center.
    let Some(current) = (unsafe { center.nowPlayingInfo() }) else {
        return;
    };
    let info = NSMutableDictionary::dictionaryWithDictionary(&current);
    // SAFETY: `MPMediaItemPropertyArtwork` is an immutable string constant, and its documented
    // value is an `MPMediaItemArtwork`.
    unsafe {
        info.setObject_forKey(
            artwork as &AnyObject,
            ProtocolObject::from_ref(MPMediaItemPropertyArtwork) as &ProtocolObject<dyn NSCopying>,
        );
        center.setNowPlayingInfo(Some(&info));
    }
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
            cover: Rc::default(),
        }
    }

    /// Elapsed time is set only here, i.e. only when the view changed; macOS extrapolates
    /// it from the rate in between.
    pub(super) fn show(&self, view: Option<&PlayerView>) {
        let Some(view) = view else {
            self.active.store(false, Ordering::Relaxed);
            *self.cover.borrow_mut() = Cover::default();
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
        let artwork = self.artwork(view.artwork_url.as_deref());
        // SAFETY: an immutable string constant from MediaPlayer.
        let k_artwork = unsafe { MPMediaItemPropertyArtwork };
        if let Some(artwork) = &artwork {
            keys.push(k_artwork);
            values.push(artwork);
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

    /// The cover for `url` if it is loaded. Otherwise requests it and, when it arrives for a
    /// view still showing `url`, adds it to the published info.
    fn artwork(&self, url: Option<&str>) -> Option<Retained<MPMediaItemArtwork>> {
        let mut cover = self.cover.borrow_mut();
        if cover.url.as_deref() != url {
            *cover = Cover {
                url: url.map(str::to_owned),
                artwork: None,
            };
        }
        if let Some(artwork) = &cover.artwork {
            return Some(artwork.clone());
        }
        let url = url?;
        // `show` runs on the main thread; without a marker there is no image to load.
        let mtm = MainThreadMarker::new()?;
        let (shared, center, wanted) =
            (Rc::clone(&self.cover), self.center.clone(), url.to_owned());
        let image = art::load(mtm, url, ARTWORK_PX, move |image| {
            let mut cover = shared.borrow_mut();
            if cover.url.as_deref() != Some(&wanted) || cover.artwork.is_some() {
                return;
            }
            let artwork = artwork_for(image);
            add_artwork(&center, &artwork);
            cover.artwork = Some(artwork);
        })?;
        let artwork = artwork_for(&image);
        cover.artwork = Some(artwork.clone());
        Some(artwork)
    }

    pub(super) fn close(&self) {
        for (command, target) in &self.targets {
            // SAFETY: `target` is the token `addTargetWithHandler` returned for `command`.
            unsafe { command.removeTarget(Some(target)) };
        }
        self.show(None);
    }
}
