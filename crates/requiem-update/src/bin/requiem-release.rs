//! Makes and checks the signed update files of a Requiem release.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use requiem_update::b64;
use requiem_update::manifest::{
    self, BUNDLE_ID, FORMAT, MAX_MANIFEST_BYTES, MAX_ZIP_BYTES, Manifest, Target,
};
use ring::digest::{Context, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use semver::Version;

const USAGE: &str = "usage:
  requiem-release keygen --out <key file>
      make an update key; prints its public key for crates/requiem-update/src/key.rs
  requiem-release manifest --zip <dir>/Requiem-<version>.zip --version <version> --min-macos <version>
      write <dir>/requiem-update.json
  requiem-release sign --key <key file> --in <manifest>
      write <manifest>.sig
  requiem-release verify --dir <release folder> [--pubkey <base64>]
      check the folder's manifest, signature and zip as an installed app would";

const MANIFEST_NAME: &str = "requiem-update.json";
const ARCHITECTURE: &str = "arm64";

enum Error {
    Usage(String),
    Failed(String),
}

impl From<String> for Error {
    fn from(e: String) -> Self {
        Error::Failed(e)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((cmd, _)) if cmd == "--help" || cmd == "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some((cmd, rest)) => run(cmd, rest),
        None => Err(Error::Usage("no command".into())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(e)) => {
            eprintln!("error: {e}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Error::Failed(e)) => {
            eprintln!("requiem-release: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: &str, rest: &[String]) -> Result<(), Error> {
    match cmd {
        "keygen" => {
            let o = Options::parse(rest, &["--out"], &[])?;
            keygen(Path::new(o.get("--out")))
        }
        "manifest" => {
            let o = Options::parse(rest, &["--zip", "--version", "--min-macos"], &[])?;
            make_manifest(
                Path::new(o.get("--zip")),
                o.get("--version"),
                o.get("--min-macos"),
            )
        }
        "sign" => {
            let o = Options::parse(rest, &["--key", "--in"], &[])?;
            sign(Path::new(o.get("--key")), Path::new(o.get("--in")))
        }
        "verify" => {
            let o = Options::parse(rest, &["--dir"], &["--pubkey"])?;
            verify(
                Path::new(o.get("--dir")),
                o.0.get("--pubkey").map(String::as_str),
            )
        }
        other => Err(Error::Usage(format!("unknown command {other}"))),
    }
    .map_err(|e| match e {
        Error::Failed(e) => Error::Failed(format!("{cmd}: {e}")),
        usage => usage,
    })
}

struct Options(HashMap<String, String>);

impl Options {
    fn parse(args: &[String], required: &[&str], optional: &[&str]) -> Result<Options, Error> {
        let mut map = HashMap::new();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            if !required.contains(&flag.as_str()) && !optional.contains(&flag.as_str()) {
                return Err(Error::Usage(format!("unknown argument {flag}")));
            }
            let value = it
                .next()
                .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))?;
            if map.insert(flag.clone(), value.clone()).is_some() {
                return Err(Error::Usage(format!("{flag} given twice")));
            }
        }
        if let Some(missing) = required.iter().find(|r| !map.contains_key(**r)) {
            return Err(Error::Usage(format!("{missing} is required")));
        }
        Ok(Options(map))
    }

    fn get(&self, flag: &str) -> &str {
        &self.0[flag]
    }
}

fn keygen(out: &Path) -> Result<(), Error> {
    let mut seed = [0u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .map_err(|_| "the system has no randomness to give".to_owned())?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| "ring refused the generated seed".to_owned())?;
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => {
                format!("{} exists; refusing to overwrite a key", out.display())
            }
            _ => format!("{}: {e}", out.display()),
        })?;
    file.write_all(format!("{}\n", b64::encode(&seed)).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!(
        "wrote {}; its public key, for crates/requiem-update/src/key.rs:",
        out.display()
    );
    println!("{}", b64::encode(pair.public_key().as_ref()));
    Ok(())
}

fn make_manifest(zip: &Path, version: &str, min_macos: &str) -> Result<(), Error> {
    Version::parse(version).map_err(|e| format!("version {version:?}: {e}"))?;
    if min_macos.is_empty()
        || !min_macos
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(Error::Failed(format!(
            "--min-macos {min_macos:?} is not a macOS version"
        )));
    }
    let file = manifest::zip_name(version);
    if zip.file_name().is_none_or(|n| *n != *file) {
        return Err(Error::Failed(format!(
            "{} must be named {file}",
            zip.display()
        )));
    }
    let (size, sha256) = digest(zip)?;
    if size > MAX_ZIP_BYTES {
        return Err(Error::Failed(format!(
            "{} is {size} bytes, over {} MiB",
            zip.display(),
            MAX_ZIP_BYTES >> 20
        )));
    }
    let m = Manifest {
        format: FORMAT,
        version: version.to_owned(),
        minimum_macos: min_macos.to_owned(),
        architectures: vec![ARCHITECTURE.to_owned()],
        bundle_identifier: BUNDLE_ID.to_owned(),
        file,
        size,
        sha256,
    };
    let mut bytes = serde_json::to_vec_pretty(&m).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let out = zip.with_file_name(MANIFEST_NAME);
    fs::write(&out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{}", out.display());
    Ok(())
}

fn digest(path: &Path) -> Result<(u64, String), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut ctx = Context::new(&SHA256);
    let mut size = 0u64;
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        size += n as u64;
        ctx.update(&buf[..n]);
    }
    let hex = ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((size, hex))
}

fn sign(key: &Path, manifest_path: &Path) -> Result<(), Error> {
    let seed = read_capped(key, 1024)?;
    let seed = String::from_utf8(seed)
        .ok()
        .and_then(|s| b64::decode(s.strip_suffix('\n').unwrap_or(&s)))
        .filter(|s| s.len() == 32)
        .ok_or_else(|| {
            format!(
                "{} is not a key: 32 bytes of base64 on one line",
                key.display()
            )
        })?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| format!("{} is not a key", key.display()))?;
    let bytes = read_capped(manifest_path, MAX_MANIFEST_BYTES)?;
    let m: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a manifest: {e}", manifest_path.display()))?;
    if m.format != FORMAT {
        return Err(Error::Failed(format!(
            "{} has format {}, not {FORMAT}",
            manifest_path.display(),
            m.format
        )));
    }
    let sig = pair.sign(&bytes);
    let mut out = manifest_path.as_os_str().to_owned();
    out.push(".sig");
    let out = PathBuf::from(out);
    fs::write(&out, format!("{}\n", b64::encode(sig.as_ref())))
        .map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!(
        "signed {} {} with public key {}",
        m.version,
        manifest_path.display(),
        b64::encode(pair.public_key().as_ref())
    );
    println!("{}", out.display());
    Ok(())
}

fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > max {
        return Err(format!("{}: over {max} bytes", path.display()));
    }
    Ok(bytes)
}

fn verify(dir: &Path, pubkey: Option<&str>) -> Result<(), Error> {
    let manifest_path = dir.join(MANIFEST_NAME);
    let bytes = read_capped(&manifest_path, MAX_MANIFEST_BYTES)?;
    let sig = read_capped(
        &dir.join(format!("{MANIFEST_NAME}.sig")),
        MAX_MANIFEST_BYTES,
    )?;
    let sig = String::from_utf8(sig).map_err(|_| "the signature file is not text".to_owned())?;
    let key = pubkey.unwrap_or(requiem_update::key::PUBLIC_KEY);
    manifest::verify_signature(key, &bytes, &sig)?;
    let (m, version) = manifest::parse(&bytes, &Target::this_machine())?;
    println!("signature: ok, {version}");
    stage(dir, &m)?;
    println!("zip, layout, Info.plists, code signatures and self-tests: ok");
    Ok(())
}

#[cfg(target_os = "macos")]
fn stage(dir: &Path, m: &Manifest) -> Result<(), Error> {
    use requiem_update::{fetch, macos};

    let staging = macos::Staging::create(dir)?;
    let zip = staging.path().join(&m.file);
    let result = fetch::copy_checked(&dir.join(&m.file), m, &zip)
        .and_then(|()| macos::stage(&staging, &zip, &m.version).map(drop));
    let path = staging.path().to_owned();
    if let Err(e) = staging.remove() {
        eprintln!("requiem-release: {}: {e}", path.display());
    }
    result.map_err(|f| Error::Failed(f.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn stage(_dir: &Path, _m: &Manifest) -> Result<(), Error> {
    Err(Error::Failed(requiem_update::UNSUPPORTED.to_owned()))
}
