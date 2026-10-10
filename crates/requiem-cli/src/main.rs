use std::fs::File;
use std::io::{self, BufReader};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use requiem_audio::{AudioEngine, AudioOptions};
use requiem_cli::app::{self, Player};
use requiem_cli::remote::RemotePlayer;
use requiem_core::{EngineEventSink, Session, SessionConfig};
use requiem_provider_ytmusic::{YtMusicOptions, YtMusicProvider};

const USAGE: &str = "usage: requiem [--script <file>] [--connect] [--prefer-aac] [--help]\n  --connect  drive the requiemd daemon (started if not running) instead of playing in-process";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

struct Args {
    script: Option<String>,
    connect: bool,
    prefer_aac: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        script: None,
        connect: false,
        prefer_aac: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => return Ok(None),
            "--prefer-aac" => args.prefer_aac = true,
            "--connect" => args.connect = true,
            "--script" => {
                args.script = Some(it.next().ok_or("--script needs a file")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.connect && args.prefer_aac {
        return Err("--prefer-aac applies to the daemon; pass it to requiemd".into());
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

    if args.connect {
        return connect(&args);
    }
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

    let code = drive(&session, &args);
    session.shutdown();
    code
}

fn connect(args: &Args) -> ExitCode {
    let exe = std::env::current_exe().map(|p| p.with_file_name("requiemd"));
    let player = exe.and_then(|exe| RemotePlayer::connect_or_spawn(&exe, CONNECT_TIMEOUT));
    match player {
        Ok(player) => drive(&player, args),
        Err(e) => {
            eprintln!("error: cannot reach the daemon: {e}");
            ExitCode::from(1)
        }
    }
}

fn drive(player: &dyn Player, args: &Args) -> ExitCode {
    let script = args.script.is_some();
    let result = match &args.script {
        Some(path) => match File::open(path) {
            Ok(f) => app::run(player, BufReader::new(f), io::stdout(), true),
            Err(e) => {
                eprintln!("error: {path}: {e}");
                return ExitCode::from(1);
            }
        },
        None => app::run(player, io::stdin().lock(), io::stdout(), script),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
