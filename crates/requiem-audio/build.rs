fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    for dir in ["/opt/homebrew/opt/opus/lib", "/usr/local/opt/opus/lib"] {
        if std::path::Path::new(dir).exists() {
            println!("cargo:rustc-link-search=native={dir}");
            break;
        }
    }
}
