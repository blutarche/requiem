//! Linux release metadata uses a separate endpoint so format-1 macOS clients keep working.

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::manifest::{BUNDLE_ID, MAX_MANIFEST_BYTES, MAX_ZIP_BYTES, is_sha256_hex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub platform: String,
    pub architecture: String,
    pub bundle_identifier: String,
    pub version: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

pub fn archive_name(version: &str, arch: &str) -> String {
    format!("requiem-{version}-linux-{arch}.tar.gz")
}

pub fn manifest_name(arch: &str) -> Result<String, String> {
    check_arch(arch)?;
    Ok(format!("requiem-update-linux-{arch}.json"))
}

fn check_arch(arch: &str) -> Result<(), String> {
    match arch {
        "aarch64" | "x86_64" => Ok(()),
        _ => Err(format!("unsupported Linux architecture {arch:?}")),
    }
}

/// The caller must authenticate the exact bytes before parsing them.
pub fn parse(bytes: &[u8], arch: &str) -> Result<(Manifest, Version), String> {
    check_arch(arch)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("Linux manifest exceeds size limit".into());
    }
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let version = Version::parse(&m.version).map_err(|e| e.to_string())?;
    if m.format != 1 || m.platform != "linux" || m.bundle_identifier != BUNDLE_ID {
        return Err("Linux manifest format, platform or application mismatch".into());
    }
    if m.architecture != arch {
        return Err(format!("release is for {}, not {arch}", m.architecture));
    }
    if m.file != archive_name(&m.version, arch) {
        return Err("Linux archive name mismatch".into());
    }
    if m.size == 0 || m.size > MAX_ZIP_BYTES || !is_sha256_hex(&m.sha256) {
        return Err("Linux archive size or SHA-256 is invalid".into());
    }
    Ok((m, version))
}

pub fn verify(
    bytes: &[u8],
    signature: &str,
    key: &str,
    arch: &str,
) -> Result<(Manifest, Version), String> {
    crate::manifest::verify_signature(key, bytes, signature)?;
    parse(bytes, arch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Key;

    fn manifest() -> Manifest {
        Manifest {
            format: 1,
            platform: "linux".into(),
            architecture: "aarch64".into(),
            bundle_identifier: BUNDLE_ID.into(),
            version: "0.3.12".into(),
            file: archive_name("0.3.12", "aarch64"),
            size: 123,
            sha256: "ab".repeat(32),
        }
    }

    #[test]
    fn signed_linux_manifest_is_architecture_specific() {
        let key = Key::new();
        let bytes = serde_json::to_vec(&manifest()).unwrap();
        let sig = key.sign(&bytes);
        assert!(verify(&bytes, &sig, &key.public(), "aarch64").is_ok());
        assert!(verify(&bytes, &sig, &key.public(), "x86_64").is_err());
        assert!(verify(&bytes, &sig, &Key::new().public(), "aarch64").is_err());
        let mut changed = bytes.clone();
        changed[10] ^= 1;
        assert!(verify(&changed, &sig, &key.public(), "aarch64").is_err());
    }

    #[test]
    fn rejects_paths_platforms_and_unbounded_archives() {
        for edit in 0..7 {
            let mut m = manifest();
            match edit {
                0 => m.file = "../requiem.tar.gz".into(),
                1 => m.platform = "macos".into(),
                2 => m.size = MAX_ZIP_BYTES + 1,
                3 => m.size = 0,
                4 => m.sha256 = "AB".repeat(32),
                5 => m.bundle_identifier = "other".into(),
                _ => m.format = 2,
            }
            assert!(parse(&serde_json::to_vec(&m).unwrap(), "aarch64").is_err());
        }
        assert!(manifest_name("../x").is_err());
    }
}
