#[cfg(any(target_os = "macos", target_os = "linux"))]
fn main() {
    use requiem_gui::args::{self, ArgsError};

    // Run by an updating player on a staged copy: loading this far proves the binary and its
    // frameworks load. It must not create the application.
    if std::env::args().nth(1).as_deref() == Some("--update-self-test") {
        return;
    }
    let args = match args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(ArgsError::Help) => {
            println!("{}", args::USAGE);
            return;
        }
        Err(ArgsError::Bad(msg)) => {
            eprintln!("requiem-gui: {msg}\n{}", args::USAGE);
            std::process::exit(2);
        }
    };
    #[cfg(target_os = "macos")]
    requiem_gui::app::run(args);
    #[cfg(target_os = "linux")]
    std::process::exit(requiem_gui::gtk::run(args));
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!(
        "requiem-gui: the window runs on macOS and Linux only; use `requiem --connect` to drive \
         the player from a terminal"
    );
    std::process::exit(2);
}
