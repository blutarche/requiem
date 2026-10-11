use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use requiem_audio::{AudioEngine, AudioOptions};
use requiem_cli::app::{self, Format, Player};
use requiem_cli::remote::{RemotePlayer, daemon_exe};
use requiem_core::{EngineEventSink, Session, SessionConfig};
use requiem_provider_ytmusic::{YtMusicOptions, YtMusicProvider};

const USAGE: &str = "usage: requiem [--script <file|->] [--connect [--json]] [--prefer-aac] [--help]\n  --connect  drive the requiemd daemon (started if not running) instead of playing in-process\n  --script -  read the script from stdin\n  --json     with --connect: print one JSON object per line instead of text";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

struct Args {
    script: Option<String>,
    connect: bool,
    json: bool,
    prefer_aac: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        script: None,
        connect: false,
        json: false,
        prefer_aac: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => return Ok(None),
            "--prefer-aac" => args.prefer_aac = true,
            "--connect" => args.connect = true,
            "--json" => args.json = true,
            "--script" => {
                args.script = Some(it.next().ok_or("--script needs a file")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.connect && args.prefer_aac {
        return Err("--prefer-aac applies to the daemon; pass it to requiemd".into());
    }
    if args.json && !args.connect {
        return Err("--json needs --connect".into());
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
    let exe = std::env::current_exe().map(|p| daemon_exe(&p, Path::exists));
    let player = exe.and_then(|exe| RemotePlayer::connect_or_spawn(&exe, CONNECT_TIMEOUT));
    match player {
        Ok(player) => drive(&player, args),
        Err(e) => {
            fail(args, &format!("cannot reach the daemon: {e}"));
            ExitCode::from(1)
        }
    }
}

/// Reports a failure before any command ran: on stdout as a JSON object in JSON mode, else
/// on stderr.
fn fail(args: &Args, message: &str) {
    if args.json {
        println!(
            "{}",
            serde_json::json!({ "type": "error", "message": message })
        );
    } else {
        eprintln!("error: {message}");
    }
}

fn drive(player: &dyn Player, args: &Args) -> ExitCode {
    let format = if args.json {
        Format::Json
    } else {
        Format::Text
    };
    let result = match args.script.as_deref() {
        Some("-") => app::run_with_format(player, io::stdin().lock(), io::stdout(), true, format),
        Some(path) => match File::open(path) {
            Ok(f) => app::run_with_format(player, BufReader::new(f), io::stdout(), true, format),
            Err(e) => {
                fail(args, &format!("{path}: {e}"));
                return ExitCode::from(1);
            }
        },
        None => app::run_with_format(player, io::stdin().lock(), io::stdout(), false, format),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            fail(args, &e.to_string());
            ExitCode::from(1)
        }
    }
}
