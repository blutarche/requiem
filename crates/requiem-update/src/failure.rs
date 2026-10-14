use std::fmt;

/// What a failed install says. Each reason belongs to one [`Kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The staged bundle failed its layout, Info.plist or code-signature checks.
    NotBuiltLikeThis,
    /// A staged self-test failed.
    DidNotStart,
    /// The download failed, or did not match the manifest.
    DownloadMismatch,
    /// A self-test timed out, or the swap or the restart handoff failed.
    NotInTime,
}

impl Reason {
    pub fn text(self) -> &'static str {
        match self {
            Reason::NotBuiltLikeThis => "the new version is not built like this one",
            Reason::DidNotStart => "the new version did not start",
            Reason::DownloadMismatch => "the download did not match the release",
            Reason::NotInTime => "the new version did not start in time",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Reason::NotBuiltLikeThis | Reason::DidNotStart => Kind::Bad,
            Reason::DownloadMismatch | Reason::NotInTime => Kind::Transient,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The release itself is broken: that version with that zip is never tried again.
    Bad,
    /// May pass on another try, up to three.
    Transient,
}

/// A failed install. `detail` is for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub reason: Reason,
    pub detail: String,
}

impl Failure {
    pub fn new(reason: Reason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    pub fn kind(&self) -> Kind {
        self.reason.kind()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason.text(), self.detail)
    }
}
