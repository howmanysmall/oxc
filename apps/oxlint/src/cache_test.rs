//! Tests for `--cache`.
//!
//! Most tests prove that a cached result was replayed by replacing the cached message of every
//! diagnostic with `FROM CACHE` and checking whether it shows up in the output.

use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use serde_json::Value;

use crate::cli::{CliRunResult, CliRunner, lint_command};

const DEBUGGER: &str = "debugger;\n";
const CLEAN: &str = "export {};\n";
const CACHE_FILE: &str = ".oxlintcache";
const SUPPRESSIONS_FILE: &str = "oxlint-suppressions.json";

fn run(cwd: &Path, args: &[&str]) -> (String, CliRunResult) {
    // Single thread, like `Tester`, so output order is deterministic.
    let _ = rayon::ThreadPoolBuilder::new().num_threads(1).build_global();
    let mut all_args = vec!["-f", "unix", "-D", "no-debugger"];
    all_args.extend(args);
    let options = lint_command().run_inner(all_args.as_slice()).unwrap();
    let mut output = Vec::new();
    let result = CliRunner::new(options, None).with_cwd(cwd.to_path_buf()).run(&mut output);
    (String::from_utf8(output).unwrap(), result)
}

/// A directory with `a.js`, which has a problem, and `b.js`, which has none.
fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.js"), DEBUGGER).unwrap();
    fs::write(dir.path().join("b.js"), CLEAN).unwrap();
    dir
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn tamper(cache_file: &Path) {
    let mut cache = read_json(cache_file);
    for entry in cache["files"].as_object_mut().unwrap().values_mut() {
        if let Some(diagnostics) = entry.get_mut("diagnostics") {
            for diagnostic in diagnostics.as_array_mut().unwrap() {
                diagnostic["message"] = Value::from("FROM CACHE");
            }
        }
    }
    fs::write(cache_file, serde_json::to_vec(&cache).unwrap()).unwrap();
}

fn set_mtime(path: &Path, mtime: SystemTime) {
    fs::File::options().write(true).open(path).unwrap().set_modified(mtime).unwrap();
}

fn assert_result(result: &CliRunResult, expected: &CliRunResult, output: &str) {
    assert_eq!(format!("{result:?}"), format!("{expected:?}"), "{output}");
}

#[test]
fn caches_and_replays() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);

    let (uncached, _) = run(dir.path(), &[]);
    let (cold, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintFoundErrors, &cold);
    assert_eq!(cold, uncached);

    let cache = read_json(&cache_file);
    assert_eq!(cache["files"]["a.js"]["diagnostics"].as_array().unwrap().len(), 1);
    assert!(cache["files"]["b.js"].get("diagnostics").is_none());

    let (warm, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintFoundErrors, &warm);
    assert_eq!(warm, cold);

    tamper(&cache_file);
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(output.contains("FROM CACHE"), "{output}");
}

#[test]
fn changed_file_is_linted_again() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    fs::write(dir.path().join("a.js"), "debugger;\ndebugger;\n").unwrap();
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
    assert!(output.contains("2 problems"), "{output}");

    fs::write(dir.path().join("a.js"), CLEAN).unwrap();
    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
}

#[test]
fn metadata_strategy_compares_size_and_mtime() {
    let dir = setup();
    let file = dir.path().join("a.js");
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    // Same size and modification time: the cached result is used.
    let mtime = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, "// hello\n\n").unwrap();
    set_mtime(&file, mtime);
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(output.contains("FROM CACHE"), "{output}");

    // Only the modification time changed: the file is linted again.
    fs::write(&file, DEBUGGER).unwrap();
    set_mtime(&file, mtime + Duration::from_secs(10));
    tamper(&cache_file);
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
}

#[test]
fn content_strategy_compares_content() {
    let dir = setup();
    let file = dir.path().join("a.js");
    let cache_file = dir.path().join(CACHE_FILE);
    let args = ["--cache", "--cache-strategy", "content"];
    run(dir.path(), &args);
    tamper(&cache_file);

    // Only the modification time changed: the cached result is used.
    let mtime = fs::metadata(&file).unwrap().modified().unwrap();
    set_mtime(&file, mtime + Duration::from_secs(10));
    let (output, _) = run(dir.path(), &args);
    assert!(output.contains("FROM CACHE"), "{output}");

    // Same size and modification time but different content: the file is linted again.
    fs::write(&file, "// hello\n\n").unwrap();
    set_mtime(&file, mtime + Duration::from_secs(10));
    let (output, result) = run(dir.path(), &args);
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
}

#[test]
fn settings_change_invalidates_cache() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    // A different rule severity from the command line.
    let (output, result) = run(dir.path(), &["--cache", "-W", "no-debugger"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
}

#[test]
fn config_change_invalidates_cache() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    let config = dir.path().join(".oxlintrc.json");
    fs::write(&config, r#"{ "rules": { "no-console": "error" } }"#).unwrap();
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    // Rewriting the configuration with the same content keeps the cache.
    fs::write(&config, r#"{ "rules": { "no-console": "error" } }"#).unwrap();
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(output.contains("FROM CACHE"), "{output}");

    fs::write(&config, r#"{ "rules": { "no-console": "warn" } }"#).unwrap();
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
}

#[test]
fn settings_change_in_config_invalidates_cache() {
    // `settings` other than those of built-in plugins are read by rules and JS plugins, but are
    // not serialized with the rest of the configuration.
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    let config = dir.path().join(".oxlintrc.json");
    fs::write(&config, r#"{ "settings": { "react-hooks": { "additionalEffectHooks": "a" } } }"#)
        .unwrap();
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    fs::write(&config, r#"{ "settings": { "react-hooks": { "additionalEffectHooks": "b" } } }"#)
        .unwrap();
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
}

#[test]
fn extended_config_change_invalidates_cache() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    let base = dir.path().join("base.json");
    fs::write(&base, r#"{ "rules": { "no-console": "error" } }"#).unwrap();
    fs::write(dir.path().join(".oxlintrc.json"), r#"{ "extends": ["./base.json"] }"#).unwrap();
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    fs::write(&base, r#"{ "rules": { "no-console": "warn" } }"#).unwrap();
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
}

#[test]
fn nested_config_change_only_invalidates_its_files() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("c.js"), DEBUGGER).unwrap();
    let nested_config = nested.join(".oxlintrc.json");
    fs::write(&nested_config, r#"{ "rules": { "no-debugger": "error" } }"#).unwrap();
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    fs::write(&nested_config, r#"{ "rules": { "no-debugger": "error", "no-console": "warn" } }"#)
        .unwrap();
    let (output, _) = run(dir.path(), &["--cache"]);
    assert!(output.contains("a.js:1:1: FROM CACHE"), "{output}");
    assert!(!output.contains("c.js:1:1: FROM CACHE"), "{output}");
    assert!(output.contains("c.js:1:1:"), "{output}");
}

#[test]
fn replay_keeps_diagnostic_order() {
    let dir = tempfile::tempdir().unwrap();
    let cache_file = dir.path().join(CACHE_FILE);
    fs::write(dir.path().join("a.js"), "// eslint-disable-next-line no-console\ndebugger;\n")
        .unwrap();
    // The rule error is reported while linting, the unused directive after all files are linted.
    let args = ["--cache", "--report-unused-disable-directives"];
    let (cold, _) = run(dir.path(), &args);

    // Number the cached messages, to check that the next run replays them in the same order.
    let mut cache = read_json(&cache_file);
    let diagnostics = cache["files"]["a.js"]["diagnostics"].as_array_mut().unwrap();
    assert_eq!(diagnostics.len(), 2, "{cold}");
    for (i, diagnostic) in diagnostics.iter_mut().enumerate() {
        diagnostic["message"] = Value::from(format!("FROM CACHE {i}"));
    }
    fs::write(&cache_file, serde_json::to_vec(&cache).unwrap()).unwrap();

    let (warm, _) = run(dir.path(), &args);
    let locations = |output: &str| {
        output
            .lines()
            .filter_map(|line| line.split_once(": "))
            .map(|(location, _)| location.to_string())
            .collect::<Vec<_>>()
    };
    assert!(warm.contains("FROM CACHE 0") && warm.contains("FROM CACHE 1"), "{warm}");
    assert_eq!(locations(&warm), locations(&cold), "cold:\n{cold}\nwarm:\n{warm}");
}

#[test]
fn output_options_do_not_invalidate_cache() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    let (output, _) = run(dir.path(), &["--cache", "--quiet", "--max-warnings", "3"]);
    assert!(output.contains("FROM CACHE"), "{output}");
}

#[test]
fn running_without_cache_deletes_cache_file() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);
    assert!(cache_file.is_file());

    run(dir.path(), &[]);
    assert!(!cache_file.exists());
}

#[test]
fn fixed_files_are_not_cached() {
    let dir = setup();
    let file = dir.path().join("a.js");
    let cache_file = dir.path().join(CACHE_FILE);

    let (output, result) = run(dir.path(), &["--cache", "--fix-suggestions"]);
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
    assert_ne!(fs::read_to_string(&file).unwrap(), DEBUGGER);
    let cache = read_json(&cache_file);
    assert!(cache["files"].get("a.js").is_none(), "{cache}");
    assert!(cache["files"].get("b.js").is_some(), "{cache}");

    run(dir.path(), &["--cache"]);
    assert!(read_json(&cache_file)["files"].get("a.js").is_some());
}

#[test]
fn files_with_problems_are_linted_again_when_fixing() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);
    tamper(&cache_file);

    let (output, result) = run(dir.path(), &["--cache", "--fix-suggestions"]);
    assert!(!output.contains("FROM CACHE"), "{output}");
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
    assert_ne!(fs::read_to_string(dir.path().join("a.js")).unwrap(), DEBUGGER);
}

#[test]
fn cache_location() {
    let dir = setup();

    run(dir.path(), &["--cache", "--cache-location", "custom-cache"]);
    assert!(dir.path().join("custom-cache").is_file());
    assert!(!dir.path().join(CACHE_FILE).exists());

    run(dir.path(), &["--cache", "--cache-location", "cache-dir/"]);
    let files = fs::read_dir(dir.path().join("cache-dir"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].starts_with(".cache_"), "{files:?}");

    // Running without `--cache` deletes the cache file, but never the directory.
    run(dir.path(), &["--cache-location", "cache-dir/"]);
    assert!(dir.path().join("cache-dir").is_dir());
    assert_eq!(fs::read_dir(dir.path().join("cache-dir")).unwrap().count(), 0);
}

#[test]
fn invalid_cache_file_is_ignored() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    fs::write(&cache_file, "not json").unwrap();

    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintFoundErrors, &output);
    assert!(read_json(&cache_file)["files"].get("a.js").is_some());
}

#[test]
fn deleted_files_are_removed_from_cache() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--cache"]);

    // Files not linted in this run are kept while they exist.
    fs::remove_file(dir.path().join("b.js")).unwrap();
    run(dir.path(), &["--cache", "a.js"]);
    let cache = read_json(&cache_file);
    assert!(cache["files"].get("a.js").is_some(), "{cache}");
    assert!(cache["files"].get("b.js").is_none(), "{cache}");
}

#[test]
fn suppressions_apply_to_cached_results() {
    let dir = setup();
    let cache_file = dir.path().join(CACHE_FILE);
    run(dir.path(), &["--suppress-all"]);
    assert!(dir.path().join(SUPPRESSIONS_FILE).is_file());

    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
    // Results are cached before suppressions are applied.
    let cache = read_json(&cache_file);
    assert_eq!(cache["files"]["a.js"]["diagnostics"].as_array().unwrap().len(), 1, "{cache}");

    tamper(&cache_file);
    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintSucceeded, &output);

    // Without the suppressions, the replayed result is reported.
    fs::remove_file(dir.path().join(SUPPRESSIONS_FILE)).unwrap();
    let (output, result) = run(dir.path(), &["--cache"]);
    assert!(output.contains("a.js:1:1: FROM CACHE"), "{output}");
    assert_result(&result, &CliRunResult::LintFoundErrors, &output);
}

#[test]
fn pruning_suppressions_uses_cached_results() {
    let dir = setup();
    let file = dir.path().join("a.js");
    let suppressions = dir.path().join(SUPPRESSIONS_FILE);
    fs::write(&file, "debugger;\ndebugger;\n").unwrap();
    run(dir.path(), &["--suppress-all"]);
    assert_eq!(read_json(&suppressions)["a.js"]["no-debugger"]["count"], 2);

    // One problem is fixed. This run caches the file.
    fs::write(&file, DEBUGGER).unwrap();
    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintUnprunedSuppressions, &output);
    tamper(&dir.path().join(CACHE_FILE));

    // Pruning sees the cached result of the unchanged file.
    run(dir.path(), &["--cache", "--prune-suppressions"]);
    assert_eq!(read_json(&suppressions)["a.js"]["no-debugger"]["count"], 1);
    let cache = read_json(&dir.path().join(CACHE_FILE));
    assert_eq!(cache["files"]["a.js"]["diagnostics"][0]["message"], "FROM CACHE", "{cache}");
    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintSucceeded, &output);
}

#[cfg(unix)]
#[test]
fn unreadable_files_are_not_cached() {
    use std::os::unix::fs::PermissionsExt;

    let dir = setup();
    let unreadable = dir.path().join("c.js");
    fs::write(&unreadable, CLEAN).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&unreadable).is_ok() {
        // Running as root.
        return;
    }

    let (output, result) = run(dir.path(), &["--cache"]);
    assert_result(&result, &CliRunResult::LintFoundErrors, &output);
    let cache = read_json(&dir.path().join(CACHE_FILE));
    assert!(cache["files"].get("c.js").is_none(), "{cache}");
    assert!(cache["files"].get("a.js").is_some(), "{cache}");
    assert!(cache["files"].get("b.js").is_some(), "{cache}");
}

#[test]
#[cfg(not(target_endian = "big"))]
fn type_aware_results_are_cached() {
    // `tsgolint` is found through the fixture's `node_modules`, so lint the fixture in place and
    // keep the cache file outside it.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cli/tsgolint");
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_file = cache_dir.path().join("cache");
    let args = [
        "--type-aware",
        "--cache",
        "--cache-location",
        cache_file.to_str().unwrap(),
        "no-floating-promises.ts",
    ];

    let (cold, _) = run(&fixture, &args);
    assert!(cold.contains("Promises must be awaited"), "{cold}");

    tamper(&cache_file);
    let (warm, result) = run(&fixture, &args);
    assert!(warm.contains("FROM CACHE [Error/typescript(no-floating-promises)]"), "{warm}");
    assert_result(&result, &CliRunResult::LintFoundErrors, &warm);
}
