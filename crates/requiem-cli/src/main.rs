use std::fs::File;
use std::io::{self, BufReader};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use requiem_audio::{AudioEngine, AudioOptions};
use requiem_cli::app;
use requiem_core::{EngineEventSink, Session, SessionConfig};
use requiem_provider_ytmusic::{YtMusicOptions, YtMusicProvider};

const USAGE: &str = "usage: requiem [--script <file>] [--prefer-aac] [--help]";

struct Args {
    script: Option<String>,
    prefer_aac: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        script: None,
        prefer_aac: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => return Ok(None),
            "--prefer-aac" => args.prefer_aac = true,
            "--script" => {
                args.script = Some(it.next().ok_or("--script needs a file")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    // The engine must exist before `Session::spawn`, whose factory cannot fail, so events
    // reach the session through a slot the factory fills.
    let slot: Arc<OnceLock<EngineEventSink>> = Arc::new(OnceLock::new());
    let forward: EngineEventSink = {
        let slot = slot.clone();
        Arc::new(move |ev| {
            if let Some(sink) = slot.get() {
                sink(ev);
            }
        })
    };
    let engine = match AudioEngine::new(forward, AudioOptions::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: no audio output: {e}");
            return ExitCode::from(1);
        }
    };
    let provider = Arc::new(YtMusicProvider::new(YtMusicOptions {
        prefer_aac: args.prefer_aac,
        ..YtMusicOptions::default()
    }));
    let session = Session::spawn(
        vec![provider],
        move |sink| {
            let _ = slot.set(sink);
            Box::new(engine)
        },
        SessionConfig::default(),
    );

    let script = args.script.is_some();
    let result = match &args.script {
        Some(path) => match File::open(path) {
            Ok(f) => app::run(&session, BufReader::new(f), io::stdout(), true),
            Err(e) => {
                eprintln!("error: {path}: {e}");
                session.shutdown();
                return ExitCode::from(1);
            }
        },
        None => app::run(&session, io::stdin().lock(), io::stdout(), script),
    };
    session.shutdown();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
