use std::time::Duration;

use requiem_core::{AudioSource, Engine, EngineEvent, EngineEventSink, FailureKind};

use crate::AudioError;

type Factory<E> = Box<dyn FnMut() -> Result<E, AudioError> + Send>;

/// An engine that may not exist yet, so a machine without audio output still runs the rest of
/// the player. Each `load` opens it again while it is missing; until then the load fails with
/// `FailureKind::Output` and the other calls do nothing.
///
/// `load` blocks no longer than the factory does (for `AudioEngine::new`, as long as opening
/// the device takes).
pub struct LazyEngine<E> {
    engine: Option<E>,
    factory: Factory<E>,
    sink: EngineEventSink,
    volume: Option<f32>,
}

impl<E: Engine> LazyEngine<E> {
    /// `engine` is what the first attempt at the factory gave, if it succeeded. `sink` is where
    /// a failed load is reported.
    pub fn new(
        sink: EngineEventSink,
        factory: impl FnMut() -> Result<E, AudioError> + Send + 'static,
        engine: Option<E>,
    ) -> Self {
        Self {
            engine,
            factory: Box::new(factory),
            sink,
            volume: None,
        }
    }
}

impl<E: Engine> Engine for LazyEngine<E> {
    fn load(&mut self, r#gen: u64, src: AudioSource, paused: bool) {
        if self.engine.is_none() {
            match (self.factory)() {
                Ok(mut e) => {
                    if let Some(v) = self.volume {
                        e.set_volume(v);
                    }
                    self.engine = Some(e);
                }
                Err(e) => {
                    (self.sink)(EngineEvent::Failed {
                        r#gen,
                        kind: FailureKind::Output,
                        detail: format!("no audio output: {e}"),
                    });
                    return;
                }
            }
        }
        if let Some(e) = &mut self.engine {
            e.load(r#gen, src, paused);
        }
    }

    fn pause(&mut self) {
        if let Some(e) = &mut self.engine {
            e.pause();
        }
    }

    fn resume(&mut self) {
        if let Some(e) = &mut self.engine {
            e.resume();
        }
    }

    fn seek(&mut self, to: Duration) {
        if let Some(e) = &mut self.engine {
            e.seek(to);
        }
    }

    fn set_volume(&mut self, v: f32) {
        self.volume = Some(v);
        if let Some(e) = &mut self.engine {
            e.set_volume(v);
        }
    }

    fn stop(&mut self) {
        if let Some(e) = &mut self.engine {
            e.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use requiem_core::{ByteSource, FormatHint};

    use super::*;

    type Calls = Arc<Mutex<Vec<String>>>;

    struct Fake(Calls);

    impl Engine for Fake {
        fn load(&mut self, r#gen: u64, _: AudioSource, paused: bool) {
            self.0.lock().unwrap().push(format!("load {gen} {paused}"));
        }
        fn pause(&mut self) {
            self.0.lock().unwrap().push("pause".into());
        }
        fn resume(&mut self) {
            self.0.lock().unwrap().push("resume".into());
        }
        fn seek(&mut self, to: Duration) {
            self.0.lock().unwrap().push(format!("seek {to:?}"));
        }
        fn set_volume(&mut self, v: f32) {
            self.0.lock().unwrap().push(format!("volume {v}"));
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().push("stop".into());
        }
    }

    struct Empty(Cursor<Vec<u8>>);

    impl std::io::Read for Empty {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl std::io::Seek for Empty {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    impl ByteSource for Empty {
        fn byte_len(&self) -> Option<u64> {
            Some(0)
        }
    }

    fn source() -> AudioSource {
        AudioSource::Encoded {
            bytes: Box::new(Empty(Cursor::new(Vec::new()))),
            hint: FormatHint::WebmOpus,
        }
    }

    fn events() -> (EngineEventSink, Arc<Mutex<Vec<EngineEvent>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink: EngineEventSink = {
            let got = got.clone();
            Arc::new(move |ev| got.lock().unwrap().push(ev))
        };
        (sink, got)
    }

    fn no_output() -> Result<Fake, AudioError> {
        Err(AudioError("none".into()))
    }

    #[test]
    fn load_without_output_fails_with_output_kind() {
        let (sink, got) = events();
        let mut e = LazyEngine::new(sink, no_output, None);
        e.load(7, source(), false);
        assert_eq!(
            *got.lock().unwrap(),
            vec![EngineEvent::Failed {
                r#gen: 7,
                kind: FailureKind::Output,
                detail: "no audio output: none".into(),
            }]
        );
    }

    #[test]
    fn other_calls_without_output_do_nothing() {
        let (sink, got) = events();
        let mut e = LazyEngine::new(sink, no_output, None);
        e.pause();
        e.resume();
        e.seek(Duration::from_secs(3));
        e.stop();
        e.set_volume(0.5);
        assert!(got.lock().unwrap().is_empty());
    }

    #[test]
    fn output_that_appears_gets_the_stored_volume_then_the_load() {
        let (sink, got) = events();
        let calls = Calls::default();
        let tries = Arc::new(Mutex::new(0));
        let factory = {
            let calls = calls.clone();
            let tries = tries.clone();
            move || {
                let mut n = tries.lock().unwrap();
                *n += 1;
                if *n < 2 {
                    no_output()
                } else {
                    Ok(Fake(calls.clone()))
                }
            }
        };
        let mut e = LazyEngine::new(sink, factory, None);
        e.set_volume(0.25);
        e.set_volume(0.5);
        e.load(1, source(), true);
        assert_eq!(got.lock().unwrap().len(), 1);
        assert!(calls.lock().unwrap().is_empty());

        e.load(2, source(), true);
        e.pause();
        e.set_volume(0.75);
        assert_eq!(got.lock().unwrap().len(), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            ["volume 0.5", "load 2 true", "pause", "volume 0.75"]
        );
        assert_eq!(*tries.lock().unwrap(), 2);
    }

    #[test]
    fn a_held_engine_is_not_reopened() {
        let (sink, _) = events();
        let calls = Calls::default();
        let mut e = LazyEngine::new(
            sink,
            || -> Result<Fake, AudioError> { panic!("reopened") },
            Some(Fake(calls.clone())),
        );
        e.load(1, source(), false);
        assert_eq!(*calls.lock().unwrap(), ["load 1 false"]);
    }
}
