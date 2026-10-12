use std::{borrow::Cow, fs, io, path::Path, time::UNIX_EPOCH};

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use oxc_diagnostics::{LabeledSpan, OxcDiagnostic, Severity};
use oxc_span::Span;

use super::Fingerprint;

/// Bump when the format of the cache file changes.
const FORMAT_VERSION: u32 = 2;

/// The cache file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CacheFile {
    version: u32,
    /// Fingerprint of the settings that affect the results of every file.
    settings: Fingerprint,
    /// Entries keyed by the file's path relative to the working directory.
    pub files: FxHashMap<String, CacheEntry>,
}

impl CacheFile {
    pub fn new(settings: Fingerprint, files: FxHashMap<String, CacheEntry>) -> Self {
        Self { version: FORMAT_VERSION, settings, files }
    }

    /// Load the entries of the cache file at `path`.
    /// A missing, unreadable, or outdated cache file is treated as empty.
    pub fn load(path: &Path, settings: Fingerprint) -> FxHashMap<String, CacheEntry> {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|file| file.version == FORMAT_VERSION && file.settings == settings)
            .map(|file| file.files)
            .unwrap_or_default()
    }

    /// Write the cache file. It is written to a temporary file first and then renamed, so that
    /// concurrent or interrupted runs never leave a partially written cache file behind.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir)?;
        let file_name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
        let temp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
        fs::write(&temp, bytes)?;
        fs::rename(&temp, path).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub size: u64,
    /// Modification time as `[seconds, nanoseconds]` since the Unix epoch.
    pub mtime: (u64, u32),
    /// Content fingerprint. Only stored with [`CacheStrategy::Content`].
    ///
    /// [`CacheStrategy::Content`]: super::CacheStrategy::Content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Fingerprint>,
    /// Fingerprint of the configuration the file was linted with.
    pub config: Fingerprint,
    /// Whether the file's lint results went through suppression tracking. Files that fail to
    /// parse do not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seen: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<CachedDiagnostic>,
}

/// File size and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub mtime: (u64, u32),
}

impl FileStat {
    pub fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self { size: metadata.len(), mtime: (mtime.as_secs(), mtime.subsec_nanos()) })
    }
}

impl CacheEntry {
    pub fn stat(&self) -> FileStat {
        FileStat { size: self.size, mtime: self.mtime }
    }
}

/// A diagnostic as stored in the cache file. Diagnostics are stored before suppressions are
/// applied; `suppressible` ones are passed through suppression tracking again when replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDiagnostic {
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    number: Option<String>,
    severity: CachedSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    help: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    labels: Vec<CachedLabel>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suppressible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CachedSeverity {
    Advice,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedLabel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    offset: u32,
    len: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    primary: bool,
}

impl CachedDiagnostic {
    pub fn new(diagnostic: &OxcDiagnostic, suppressible: bool) -> Self {
        let to_string = |s: &Option<Cow<'static, str>>| s.as_deref().map(ToString::to_string);
        Self {
            message: diagnostic.message.to_string(),
            scope: to_string(&diagnostic.code.scope),
            number: to_string(&diagnostic.code.number),
            severity: match diagnostic.severity {
                Severity::Advice => CachedSeverity::Advice,
                Severity::Warning => CachedSeverity::Warning,
                Severity::Error => CachedSeverity::Error,
            },
            help: to_string(&diagnostic.help),
            note: to_string(&diagnostic.note),
            url: to_string(&diagnostic.url),
            labels: diagnostic
                .labels
                .iter()
                .map(|label| CachedLabel {
                    label: label.label().map(ToString::to_string),
                    offset: label.offset(),
                    len: label.len(),
                    primary: label.primary(),
                })
                .collect(),
            suppressible,
        }
    }

    pub fn to_diagnostic(&self) -> OxcDiagnostic {
        let severity = match self.severity {
            CachedSeverity::Advice => Severity::Advice,
            CachedSeverity::Warning => Severity::Warning,
            CachedSeverity::Error => Severity::Error,
        };
        let mut diagnostic = OxcDiagnostic::error(self.message.clone()).with_severity(severity);
        diagnostic.code.scope = self.scope.clone().map(Cow::Owned);
        diagnostic.code.number = self.number.clone().map(Cow::Owned);
        diagnostic.help = self.help.clone().map(Cow::Owned);
        diagnostic.note = self.note.clone().map(Cow::Owned);
        diagnostic.url = self.url.clone().map(Cow::Owned);
        diagnostic.labels = self
            .labels
            .iter()
            .map(|label| {
                let span = Span::sized(label.offset, label.len);
                if label.primary {
                    LabeledSpan::new_primary_with_span(label.label.clone(), span)
                } else {
                    LabeledSpan::new_with_span(label.label.clone(), span)
                }
            })
            .collect();
        diagnostic
    }
}

#[cfg(test)]
mod test {
    use oxc_diagnostics::{LabeledSpan, OxcDiagnostic};
    use oxc_span::Span;

    use super::CachedDiagnostic;

    #[test]
    fn diagnostic_round_trips() {
        let diagnostic = OxcDiagnostic::warn("message")
            .with_error_code("eslint", "no-debugger")
            .with_help("help")
            .with_note("note")
            .with_url("https://oxc.rs")
            .with_labels([
                LabeledSpan::new_primary_with_span(Some("primary".into()), Span::new(4, 5)),
                LabeledSpan::new_with_span(None, Span::new(15, 16)),
            ]);

        let cached = CachedDiagnostic::new(&diagnostic, true);
        let json = serde_json::to_string(&cached).unwrap();
        let cached: CachedDiagnostic = serde_json::from_str(&json).unwrap();

        assert!(cached.suppressible);
        assert_eq!(cached.to_diagnostic(), diagnostic);
    }

    #[test]
    fn diagnostic_without_code_round_trips() {
        let diagnostic = OxcDiagnostic::error("Unexpected token");
        assert_eq!(CachedDiagnostic::new(&diagnostic, false).to_diagnostic(), diagnostic);
    }
}
