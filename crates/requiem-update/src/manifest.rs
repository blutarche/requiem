//! The signed manifest a release carries: its shape, its signature, and the checks that
//! decide whether this machine may install it.

use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::b64;

pub const FORMAT: u32 = 1;
pub const BUNDLE_ID: &str = "dev.blutarche.requiem";
pub const PLAYER_BUNDLE_ID: &str = "dev.blutarche.requiem.player";
/// For the manifest and its signature alike.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const MAX_ZIP_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub version: String,
    #[serde(rename = "minimumMacOS")]
    pub minimum_macos: String,
    pub architectures: Vec<String>,
    pub bundle_identifier: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

/// The machine a manifest must suit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// As manifests name it: `arm64`.
    pub arch: String,
    /// The running macOS, such as `26.6.2`.
    pub macos: String,
}

impl Target {
    pub fn this_machine() -> Target {
        Target {
            arch: match std::env::consts::ARCH {
                "aarch64" => "arm64".to_owned(),
                other => other.to_owned(),
            },
            macos: macos_version().unwrap_or_default(),
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_version() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated and `buf`/`len` describe a writable buffer.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let text = buf.get(..len)?.split(|&b| b == 0).next()?;
    String::from_utf8(text.to_vec()).ok()
}

#[cfg(not(target_os = "macos"))]
fn macos_version() -> Option<String> {
    None
}

pub fn zip_name(version: &str) -> String {
    format!("Requiem-{version}.zip")
}

/// `signature` is the `.sig` file: base64 of 64 bytes on one line. `public_key` is base64
/// of 32 bytes.
pub fn verify_signature(public_key: &str, manifest: &[u8], signature: &str) -> Result<(), String> {
    let key = b64::decode(public_key.trim())
        .filter(|k| k.len() == 32)
        .ok_or("no valid update key is built in")?;
    let line = signature.strip_suffix('\n').unwrap_or(signature);
    let sig = b64::decode(line)
        .filter(|s| s.len() == 64)
        .ok_or("the signature is not 64 bytes of base64 on one line")?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(manifest, &sig)
        .map_err(|_| "the manifest's signature does not match the update key".to_owned())
}

#[derive(Deserialize)]
struct FormatProbe {
    format: u32,
}

/// Parses a manifest whose signature has been checked, and checks that it suits `target`.
pub fn parse(bytes: &[u8], target: &Target) -> Result<(Manifest, Version), String> {
    let probe: FormatProbe =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest unreadable: {e}"))?;
    if probe.format != FORMAT {
        return Err(format!("manifest format {} is not {FORMAT}", probe.format));
    }
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest unreadable: {e}"))?;
    let version =
        Version::parse(&m.version).map_err(|e| format!("manifest version {:?}: {e}", m.version))?;
    if m.bundle_identifier != BUNDLE_ID {
        return Err(format!(
            "manifest is for {:?}, not {BUNDLE_ID}",
            m.bundle_identifier
        ));
    }
    if !m.architectures.contains(&target.arch) {
        return Err(format!(
            "release is for {}, not {}",
            m.architectures.join(", "),
            target.arch
        ));
    }
    if !at_least(&target.macos, &m.minimum_macos) {
        return Err(format!(
            "release needs macOS {}; this is {}",
            m.minimum_macos, target.macos
        ));
    }
    if m.file != zip_name(&m.version) {
        return Err(format!(
            "manifest names {:?}, not {}",
            m.file,
            zip_name(&m.version)
        ));
    }
    if !is_sha256_hex(&m.sha256) {
        return Err("manifest sha256 is not 64 lowercase hex characters".to_owned());
    }
    Ok((m, version))
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Dotted macOS versions, missing parts counting as 0. Unparsable ones are never at least
/// anything.
fn at_least(running: &str, minimum: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() };
    let (Some(mut a), Some(mut b)) = (parse(running), parse(minimum)) else {
        return false;
    };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a >= b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Key, manifest_json, target};

    fn json(version: &str) -> serde_json::Value {
        manifest_json(version, b"zip bytes")
    }

    #[test]
    fn accepts_a_good_signature() {
        let key = Key::new();
        let body = br#"{"format":1}"#;
        assert_eq!(
            verify_signature(&key.public(), body, &key.sign(body)),
            Ok(())
        );
    }

    #[test]
    fn rejects_a_one_bit_flip_in_the_manifest() {
        let key = Key::new();
        let body = serde_json::to_vec(&json("0.2.0")).unwrap();
        let sig = key.sign(&body);
        for i in [0, body.len() / 2, body.len() - 1] {
            let mut flipped = body.clone();
            flipped[i] ^= 1;
            assert!(
                verify_signature(&key.public(), &flipped, &sig).is_err(),
                "byte {i}"
            );
        }
    }

    #[test]
    fn rejects_another_key_and_a_bad_signature() {
        let key = Key::new();
        let body = b"manifest";
        let sig = key.sign(body);
        assert!(verify_signature(&Key::new().public(), body, &sig).is_err());
        let mut raw = b64::decode(sig.trim()).unwrap();
        raw[10] ^= 0x80;
        assert!(verify_signature(&key.public(), body, &b64::encode(&raw)).is_err());
        assert!(verify_signature(&key.public(), body, "").is_err());
        assert!(verify_signature(&key.public(), body, &format!("{}\n\n", sig.trim())).is_err());
        assert!(verify_signature(&key.public(), body, &b64::encode(&raw[..63])).is_err());
    }

    #[test]
    fn the_built_in_key_refuses_other_keys_and_an_empty_key_refuses_all() {
        let key = Key::new();
        let body = b"manifest";
        assert!(verify_signature(crate::key::PUBLIC_KEY, body, &key.sign(body)).is_err());
        let err = verify_signature("", body, &key.sign(body));
        assert_eq!(err, Err("no valid update key is built in".to_owned()));
    }

    #[test]
    fn parses_a_good_manifest() {
        let bytes = serde_json::to_vec(&json("0.2.0")).unwrap();
        let (m, v) = parse(&bytes, &target()).unwrap();
        assert_eq!(v, Version::new(0, 2, 0));
        assert_eq!(m.file, "Requiem-0.2.0.zip");
        assert_eq!(m.size, 9);
    }

    fn rejected(edit: impl FnOnce(&mut serde_json::Value), target: &Target) -> String {
        let mut j = json("0.2.0");
        edit(&mut j);
        parse(&serde_json::to_vec(&j).unwrap(), target).unwrap_err()
    }

    #[test]
    fn rejects_format_bundle_architecture_and_file_mismatches() {
        let t = target();
        let set = |k: &'static str, v: serde_json::Value| move |j: &mut serde_json::Value| j[k] = v;
        let err = |k, v| rejected(set(k, v), &t);
        assert!(err("format", 2.into()).contains("format 2"));
        assert!(err("bundleIdentifier", "dev.other.app".into()).contains("dev.other.app"));
        assert!(err("architectures", serde_json::json!(["x86_64"])).contains("x86_64"));
        assert!(err("file", "Requiem-0.2.1.zip".into()).contains("Requiem-0.2.1.zip"));
        assert!(err("file", "../Requiem-0.2.0.zip".into()).contains("names"));
        assert!(err("version", "two".into()).contains("version"));
        assert!(err("sha256", "AB".repeat(32).into()).contains("sha256"));
        assert!(err("minimumMacOS", "27.0".into()).contains("macOS 27.0"));
        let old = Target {
            macos: "15.7".into(),
            ..target()
        };
        assert!(rejected(|_| {}, &old).contains("needs macOS 26.0"));
        let partial = |j: &mut serde_json::Value| *j = serde_json::json!({"format": 1});
        assert!(rejected(partial, &t).contains("unreadable"));
    }

    #[test]
    fn macos_versions_compare_numerically() {
        assert!(at_least("26.0", "26.0"));
        assert!(at_least("26", "26.0.0"));
        assert!(at_least("26.10", "26.9"));
        assert!(!at_least("26.0", "26.0.1"));
        assert!(!at_least("", "26.0"));
        assert!(!at_least("26.x", "26.0"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_machine_reads_the_macos_version() {
        let t = Target::this_machine();
        assert_eq!(t.arch, "arm64");
        assert!(at_least(&t.macos, "11.0"), "{:?}", t.macos);
    }
}
