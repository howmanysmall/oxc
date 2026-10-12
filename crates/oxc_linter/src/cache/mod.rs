//! Cache of lint results for `oxlint --cache`. See [`LintCache`].

mod file;
mod fingerprint;

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use cow_utils::CowUtils;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use oxc_diagnostics::{
    DiagnosticSender, DiagnosticService, NamedSource, OxcDiagnostic, SourceCode,
};

use crate::{ConfigStore, Message, PossibleFixes, suppression::DiffManager};

use file::{CacheEntry, CacheFile, CachedDiagnostic, FileStat};
pub use fingerprint::{Fingerprint, FingerprintHasher};

/// How [`LintCache`] detects changed files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheStrategy {
    /// Compare file size and modification time.
    #[default]
    Metadata,
    /// Compare a fingerprint of the file contents. Survives tools that touch files without
    /// changing them, such as `git clone`.
    Content,
}

impl FromStr for CacheStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "metadata" => Ok(Self::Metadata),
            "content" => Ok(Self::Content),
            _ => Err(format!("'{s}' is not a known cache strategy")),
        }
    }
}

/// Cache of lint results, used by `oxlint --cache` to lint only files that changed since the
/// previous run.
///
/// A file's cached result is reused when:
/// - the file is unchanged ([`CacheStrategy`]),
/// - the fingerprint of the [`Config`] that applies to the file is unchanged, and
/// - the settings fingerprint passed to [`LintCache::load`] is unchanged.
///
/// The settings fingerprint is built by the caller. `oxlint` includes command line options,
/// `--tsconfig`, the `tsgolint` executable, and the entry files of JS plugins.
///
/// Like ESLint, dependencies between files are not tracked, so results of rules that depend on
/// other files (type-aware rules, cross-module rules of the `import` plugin) can be stale. Changes
/// to modules imported by JS plugins, and to `tsconfig.json` files that are found automatically
/// rather than passed with `--tsconfig`, are not detected either.
///
/// Results are stored before suppressions are applied. Replayed results go through suppression
/// tracking like freshly linted ones, so suppression files keep working with the cache.
///
/// [`Config`]: crate::Config
pub struct LintCache {
    path: PathBuf,
    cwd: PathBuf,
    strategy: CacheStrategy,
    settings: Fingerprint,
    /// Entries loaded from the cache file.
    entries: FxHashMap<String, CacheEntry>,
    run: Mutex<RunState>,
}

impl std::fmt::Debug for LintCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LintCache")
            .field("path", &self.path)
            .field("strategy", &self.strategy)
            .finish_non_exhaustive()
    }
}

/// What happened to files during this run.
#[derive(Default)]
struct RunState {
    /// Keys of files whose cached results were replayed.
    hits: FxHashSet<String>,
    /// Files being linted.
    misses: FxHashMap<PathBuf, Miss>,
    /// Results of linted files.
    results: FxHashMap<PathBuf, FileResult>,
    /// Files that could not be read or written. Their results are not stored.
    failed: FxHashSet<PathBuf>,
    /// Set when a diagnostic could not be attributed to a file. No new results are stored, as
    /// any file could have been affected.
    has_unattributed_error: bool,
}

struct Miss {
    key: String,
    /// The file's state before linting. `None` if it could not be read.
    stat: Option<FileStat>,
    hash: Option<Fingerprint>,
    config: Fingerprint,
}

#[derive(Default)]
struct FileResult {
    seen: bool,
    diagnostics: Vec<CachedDiagnostic>,
}

/// A file whose cached results are replayed.
pub struct Hit {
    path: PathBuf,
    source_text: String,
    seen: bool,
    diagnostics: Vec<CachedDiagnostic>,
}

enum Lookup {
    Hit(String, Hit),
    Miss(Arc<OsStr>, Miss),
}

impl LintCache {
    /// Load the cache file at `path`. A missing, unreadable, or outdated cache file is treated
    /// as empty.
    ///
    /// `settings` must cover all settings besides the configuration that affect lint results,
    /// such as command line options.
    pub fn load(
        path: PathBuf,
        cwd: PathBuf,
        strategy: CacheStrategy,
        settings: Fingerprint,
    ) -> Self {
        let entries = CacheFile::load(&path, settings);
        Self { path, cwd, strategy, settings, entries, run: Mutex::default() }
    }

    /// Split `files` into files that need linting and files whose cached results can be
    /// replayed.
    ///
    /// When `fix` is enabled, files with cached problems are linted again so that they get
    /// fixed. `silent` skips reading the source text of replayed files, which is only needed to
    /// print diagnostics.
    pub(crate) fn partition(
        &self,
        files: &[Arc<OsStr>],
        config_store: &ConfigStore,
        fix: bool,
        silent: bool,
    ) -> (Vec<Arc<OsStr>>, Vec<Hit>) {
        let lookups = files
            .par_iter()
            .map(|path| self.lookup(path, config_store, fix, silent))
            .collect::<Vec<_>>();

        let mut run = self.run.lock().expect("lint cache mutex poisoned");
        let mut files_to_lint = vec![];
        let mut hits = vec![];
        for lookup in lookups {
            match lookup {
                Lookup::Hit(key, hit) => {
                    run.hits.insert(key);
                    hits.push(hit);
                }
                Lookup::Miss(path, miss) => {
                    run.misses.insert(PathBuf::from(&path), miss);
                    files_to_lint.push(path);
                }
            }
        }
        (files_to_lint, hits)
    }

    fn lookup(
        &self,
        path: &Arc<OsStr>,
        config_store: &ConfigStore,
        fix: bool,
        silent: bool,
    ) -> Lookup {
        let file_path = Path::new(path);
        let key = self.key(file_path);
        let stat = FileStat::read(file_path);
        let contents = match self.strategy {
            CacheStrategy::Metadata => None,
            CacheStrategy::Content => stat.and_then(|_| fs::read(file_path).ok()),
        };
        let hash = contents.as_deref().map(Fingerprint::of_bytes);
        let config = config_store.fingerprint(file_path);

        let entry = self.entries.get(&key).filter(|entry| {
            let unchanged = match self.strategy {
                CacheStrategy::Metadata => stat == Some(entry.stat()),
                CacheStrategy::Content => {
                    stat.is_some_and(|stat| stat.size == entry.size)
                        && hash.is_some()
                        && entry.hash == hash
                }
            };
            // Like ESLint, lint files with problems again when fixing, so they get fixed.
            unchanged && entry.config == config && (!fix || entry.diagnostics.is_empty())
        });
        let miss = |key| Lookup::Miss(Arc::clone(path), Miss { key, stat, hash, config });
        let Some(entry) = entry else {
            return miss(key);
        };

        let source_text = if entry.diagnostics.is_empty() || silent {
            Some(String::new())
        } else if let Some(contents) = contents {
            String::from_utf8(contents).ok()
        } else {
            fs::read_to_string(file_path).ok()
        };
        let Some(source_text) = source_text else {
            return miss(key);
        };

        let hit = Hit {
            path: file_path.to_path_buf(),
            source_text,
            seen: entry.seen,
            diagnostics: entry.diagnostics.clone(),
        };
        Lookup::Hit(key, hit)
    }

    /// Send the cached results of `hits`, passing them through suppression tracking like
    /// freshly linted results.
    pub(crate) fn replay(
        &self,
        hits: Vec<Hit>,
        diff_manager: &DiffManager,
        tx_error: &DiagnosticSender,
    ) {
        hits.into_par_iter().for_each(|hit| {
            let diagnostics = if hit.seen && !diff_manager.skip() {
                let messages = hit
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.suppressible)
                    .map(|diagnostic| Message::new(diagnostic.to_diagnostic(), PossibleFixes::None))
                    .collect::<Vec<_>>();
                // Suppression tracking keeps a subset of the messages in their order. Merge it
                // back, so diagnostics are reported in the order they were recorded.
                let mut kept =
                    diff_manager.collect_file(&hit.path, &self.cwd, messages).into_iter();
                let mut next_kept = kept.next();
                let mut diagnostics = Vec::with_capacity(hit.diagnostics.len());
                for cached in &hit.diagnostics {
                    let diagnostic = cached.to_diagnostic();
                    if !cached.suppressible {
                        diagnostics.push(diagnostic);
                    } else if next_kept.as_ref().is_some_and(|message| message.error == diagnostic)
                    {
                        diagnostics.push(diagnostic);
                        next_kept = kept.next();
                    }
                }
                diagnostics
            } else {
                hit.diagnostics.iter().map(CachedDiagnostic::to_diagnostic).collect()
            };

            if !diagnostics.is_empty() {
                let source = DiagnosticService::named_source(&self.cwd, &hit.path, "");
                let source =
                    Arc::new(NamedSource::new(source.name().unwrap_or_default(), hit.source_text));
                let diagnostics = diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.with_source_code(Arc::clone(&source)))
                    .collect();
                tx_error.send(diagnostics).expect("Failed to send diagnostics");
            }
        });
    }

    /// Record diagnostics of a file being linted.
    ///
    /// `suppressible` diagnostics are those that go through suppression tracking. Recording
    /// them marks the file as seen by suppression tracking, even if there are none.
    pub(crate) fn record<'a>(
        &self,
        path: &Path,
        diagnostics: impl IntoIterator<Item = &'a OxcDiagnostic>,
        suppressible: bool,
    ) {
        let diagnostics = diagnostics
            .into_iter()
            .map(|diagnostic| CachedDiagnostic::new(diagnostic, suppressible))
            .collect::<Vec<_>>();
        let mut run = self.run.lock().expect("lint cache mutex poisoned");
        let result = run.results.entry(path.to_path_buf()).or_default();
        result.seen |= suppressible;
        result.diagnostics.extend(diagnostics);
    }

    /// Record that a file could not be read or written, so its results are not stored.
    pub(crate) fn record_failure(&self, path: &Path) {
        self.run.lock().expect("lint cache mutex poisoned").failed.insert(path.to_path_buf());
    }

    /// Record an error that is not attributed to a file, so no new results are stored.
    pub(crate) fn record_unattributed_error(&self) {
        self.run.lock().expect("lint cache mutex poisoned").has_unattributed_error = true;
    }

    /// Write the cache file with the results of this run.
    ///
    /// Results of a file are only stored if the file did not change while it was linted. This
    /// also excludes files changed by fixes, which are linted again in the next run, like ESLint
    /// does.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache file cannot be written.
    ///
    /// # Panics
    ///
    /// Panics if a thread recording results panicked.
    pub fn save(&self) -> io::Result<()> {
        let mut run = self.run.lock().expect("lint cache mutex poisoned");
        let run = &mut *run;

        let miss_keys = run.misses.values().map(|miss| miss.key.as_str()).collect::<FxHashSet<_>>();
        // Keep entries of files that were not linted in this run while they exist.
        let mut entries = self
            .entries
            .par_iter()
            .filter(|(key, _)| {
                run.hits.contains(*key)
                    || (!miss_keys.contains(key.as_str()) && self.cwd.join(key).exists())
            })
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect::<FxHashMap<_, _>>();

        if !run.has_unattributed_error {
            let results = &run.results;
            let failed = &run.failed;
            let new_entries = run
                .misses
                .par_iter()
                .filter(|(path, _)| !failed.contains(*path))
                .filter_map(|(path, miss)| {
                    let stat = miss.stat?;
                    if FileStat::read(path) != Some(stat) {
                        return None;
                    }
                    let result = results.get(path);
                    let entry = CacheEntry {
                        size: stat.size,
                        mtime: stat.mtime,
                        hash: miss.hash,
                        config: miss.config,
                        seen: result.is_some_and(|result| result.seen),
                        diagnostics: result
                            .map(|result| result.diagnostics.clone())
                            .unwrap_or_default(),
                    };
                    Some((miss.key.clone(), entry))
                })
                .collect::<Vec<_>>();
            entries.extend(new_entries);
        }

        CacheFile::new(self.settings, entries).save(&self.path)
    }

    /// The key of the file at `path` in the cache file: its path relative to the working
    /// directory, with `/` separators.
    fn key(&self, path: &Path) -> String {
        let path = path.strip_prefix(&self.cwd).unwrap_or(path).to_string_lossy();
        path.cow_replace('\\', "/").into_owned()
    }
}
