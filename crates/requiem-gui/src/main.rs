#[cfg(target_os = "macos")]
fn main() {
    use requiem_gui::args::{self, ArgsError};

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
    requiem_gui::app::run(args);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("requiem-gui: the GUI is macOS-only in v1; use `requiem --connect`");
    std::process::exit(2);
}
