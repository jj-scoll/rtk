//! Filters Go command output — test results, build errors, vet warnings.

use crate::core::guard::never_worse;
use crate::core::runner;
use crate::core::shell::{display_args, quote_word};
use crate::core::stream::{CaptureResult, exec_capture, exec_capture_unguarded};
use crate::core::tracking;
use crate::core::truncate::CAP_ERRORS;
use crate::core::utils::{resolved_command, truncate};
use crate::golangci_cmd;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GoTestEvent {
    #[serde(rename = "Time")]
    time: Option<String>,
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package")]
    package: Option<String>,
    #[serde(rename = "Test")]
    test: Option<String>,
    #[serde(rename = "Output")]
    output: Option<String>,
    #[serde(rename = "Elapsed")]
    elapsed: Option<f64>,
    #[serde(rename = "ImportPath")]
    import_path: Option<String>,
    #[serde(rename = "FailedBuild")]
    failed_build: Option<String>,
}

#[derive(Debug, Default)]
struct PackageResult {
    pass: usize,
    fail: usize,
    skip: usize,
    build_failed: bool,
    build_errors: Vec<String>,
    failed_tests: Vec<(String, Vec<String>)>, // (test_name, output_lines)
    package_failed: bool,                     // package-level failure (timeout, signal, etc.)
    package_fail_output: Vec<String>,         // output lines collected before the package fail
    fuzz_summary: Option<String>,
    result_line: Option<String>, // go's own `ok  \t<pkg>\t...` / `FAIL\t<pkg>\t...` line
}

fn forwarded_help(subcommand: &str, args: &[String], verbose: u8) -> Result<i32> {
    let mut forwarded = vec![OsString::from(subcommand)];
    forwarded.extend(args.iter().map(OsString::from));
    crate::core::runner::run_passthrough("go", &forwarded, verbose)
}

pub fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    if crate::core::runner::requests_help_args("go", args) {
        return forwarded_help("test", args, verbose);
    }
    let mut cmd = resolved_command("go");
    cmd.arg("test");

    let skip_json = args.iter().any(|a| a == "-json" || a.starts_with("-bench"));

    if !skip_json {
        cmd.arg("-json");
    }

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!(
            "Running: go test {}{}",
            if !skip_json { "-json " } else { "" },
            args.join(" ")
        );
    }

    let verbose_run = args
        .iter()
        .any(|a| a == "-v" || a == "-test.v" || a.starts_with("-v=") || a.starts_with("-test.v="));

    runner::run_filtered(
        cmd,
        "go test",
        &display_args(args),
        move |s: &str| {
            if skip_json {
                s.to_string()
            } else {
                filter_go_test_run(s, verbose_run)
            }
        },
        crate::core::runner::RunOptions::stdout_only().tee("go_test"),
    )
}

pub fn run_build(args: &[String], verbose: u8) -> Result<i32> {
    if crate::core::runner::requests_help_args("go", args) {
        return forwarded_help("build", args, verbose);
    }
    let mut cmd = resolved_command("go");
    cmd.arg("build");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: go build {}", args.join(" "));
    }

    runner::run_filtered_with_exit(
        cmd,
        "go build",
        &display_args(args),
        filter_go_build_with_exit,
        crate::core::runner::RunOptions::with_tee("go_build"),
    )
}

pub fn run_vet(args: &[String], verbose: u8) -> Result<i32> {
    if crate::core::runner::requests_help_args("go", args) {
        return forwarded_help("vet", args, verbose);
    }
    let mut cmd = resolved_command("go");
    cmd.arg("vet");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: go vet {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "go vet",
        &display_args(args),
        filter_go_vet,
        crate::core::runner::RunOptions::with_tee("go_vet"),
    )
}

pub fn run_other(args: &[OsString], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("go: no subcommand specified");
    }

    // Intercept: `go tool <known>` invocations for filtered output
    if let Some((tool, tool_args)) = match_go_tool(args) {
        match tool {
            GoTool::GolangciLint => return run_go_tool_golangci_lint(tool_args, verbose),
        }
    }

    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].to_string_lossy();
    let mut cmd = resolved_command("go");
    cmd.arg(&*subcommand);

    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: go {} ...", subcommand);
    }

    let captured =
        exec_capture(&mut cmd).with_context(|| format!("Failed to run go {}", subcommand))?;
    let raw = format!("{}\n{}", captured.stdout, captured.stderr);

    print!("{}", captured.stdout);
    eprint!("{}", captured.stderr);

    let label = format!("go {}", quote_word(&subcommand));
    timer.track(
        &label,
        &format!("rtk {label}"),
        &raw,
        &raw, // No filtering for unsupported commands
    );

    Ok(captured.exit_code)
}

/// Detect golangci-lint major version when invoked via `go tool`.
/// Returns 1 on any failure (safe fallback — v1 behaviour).
fn detect_go_tool_golangci_version() -> u32 {
    let mut cmd = resolved_command("go");
    cmd.arg("tool").arg("golangci-lint").arg("--version");

    match exec_capture_unguarded(&mut cmd) {
        Ok(captured) => {
            let version_text = if captured.stdout.trim().is_empty() {
                &captured.stderr
            } else {
                &captured.stdout
            };
            golangci_cmd::parse_major_version(version_text)
        }
        Err(_) => 1,
    }
}

fn has_golangci_format_flag(args: &[OsString]) -> bool {
    args.iter().any(|a| {
        let s = a.to_string_lossy();
        s == "--out-format"
            || s.starts_with("--out-format=")
            || s == "--output.json.path"
            || s.starts_with("--output.json.path=")
    })
}

/// Known `go tool` subcommands that RTK provides filtered output for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GoTool {
    GolangciLint,
}

impl GoTool {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "golangci-lint" => Some(Self::GolangciLint),
            _ => None,
        }
    }
}

/// If the first arg is `tool` identify if it is a tool we already handle.
fn match_go_tool(args: &[OsString]) -> Option<(GoTool, &[OsString])> {
    if args.first().map(|a| a == "tool").unwrap_or(false)
        && let Some(tool_arg) = args.get(1)
        && let Some(tool) = GoTool::from_name(&tool_arg.to_string_lossy())
    {
        return Some((tool, &args[2..]));
    }
    None
}

/// Run `go tool golangci-lint` and filter its output via the golangci JSON filter.
/// Reusing parts of golangci_cmd.
fn run_go_tool_golangci_lint(args: &[OsString], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let version = detect_go_tool_golangci_version();

    let mut cmd = resolved_command("go");
    cmd.arg("tool").arg("golangci-lint");

    let has_format = has_golangci_format_flag(args);

    if !has_format {
        if version >= 2 {
            cmd.arg("run").arg("--output.json.path").arg("stdout");
        } else {
            cmd.arg("run").arg("--out-format=json");
        }
    } else {
        cmd.arg("run");
    }

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        if version >= 2 {
            eprintln!("Running: go tool golangci-lint run --output.json.path stdout");
        } else {
            eprintln!("Running: go tool golangci-lint run --out-format=json");
        }
    }

    let CaptureResult {
        stdout,
        stderr,
        exit_code,
    } = exec_capture(&mut cmd).context("Failed to run go tool golangci-lint")?;
    let raw = format!("{}\n{}", stdout, stderr);

    // v2 outputs JSON on first line + trailing text; v1 outputs just JSON
    let json_output = if version >= 2 {
        stdout.lines().next().unwrap_or("")
    } else {
        &*stdout
    };

    let filtered = golangci_cmd::filter_golangci_json(json_output, version);
    let shown = never_worse(&raw, &filtered);
    println!("{}", shown);

    if !stderr.trim().is_empty() && verbose > 0 {
        eprintln!("{}", stderr.trim());
    }

    timer.track(
        "go tool golangci-lint",
        "rtk go tool golangci-lint",
        &raw,
        shown,
    );

    // golangci-lint: exit 0 = clean, exit 1 = lint issues found (not an error),
    // exit 2+ = config/build error, None = killed by signal (OOM, SIGKILL)
    Ok(if exit_code == 1 { 0 } else { exit_code })
}

fn is_pipe_test_action(action: &str) -> bool {
    matches!(
        action,
        "start" | "run" | "pause" | "cont" | "pass" | "bench" | "fail" | "output" | "skip"
    )
}

fn pipe_event(line: &str) -> Option<GoTestEvent> {
    let event: GoTestEvent = serde_json::from_str(line).ok()?;
    let supported = match event.action.as_str() {
        "build-output" => event.import_path.is_some() && event.output.is_some(),
        "build-fail" => event.import_path.is_some(),
        action => is_pipe_test_action(action) && (action != "output" || event.output.is_some()),
    };
    if supported
        && is_pipe_test_action(&event.action)
        && event.package.is_none()
        && event.test.is_none()
        && event.output.is_none()
    {
        // Package-less lifecycle records can contain just Action/Time/Elapsed.
        // Unknown fields on a metadata-only log must not lose their diagnostics
        // merely because the log uses the same verb or timestamp as test2json.
        let fields: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line).ok()?;
        if fields
            .keys()
            .any(|key| !matches!(key.as_str(), "Action" | "Time" | "Elapsed"))
        {
            return None;
        }
    }
    supported.then_some(event)
}

fn pipe_test_evidence(event: &GoTestEvent) -> bool {
    // test2json may omit both Package and Time. Bare lifecycle records are
    // accepted within a stream, but a lone {"Action":"pass"} is not evidence.
    is_pipe_test_action(&event.action)
        && (event.package.is_some()
            || event.test.is_some()
            || event.time.is_some()
            || event.output.is_some()
            || event.elapsed.is_some())
}

fn pipe_event_line(index: usize, line: &str) -> &str {
    if index == 0 {
        line.strip_prefix('\u{feff}').unwrap_or(line).trim()
    } else {
        line.trim()
    }
}

/// Auto-detect only a leading Go stream, not events quoted inside other text.
/// Cold-cache downloads may precede events; arbitrary prose, Markdown fences and
/// diff headers must instead keep the whole input on the lossless fallback path.
/// Build events alone are not a test report and must retain their diagnostics.
pub(crate) fn is_go_test_json(input: &str) -> bool {
    for (index, line) in input.lines().enumerate() {
        let line = pipe_event_line(index, line);
        if line.is_empty() || line.starts_with("go: downloading ") {
            continue;
        }
        let Some(event) = pipe_event(line) else {
            return false;
        };
        if pipe_test_evidence(&event) {
            return true;
        }
    }
    false
}

/// Reduce piped test events while retaining unrelated or unrecognized records.
/// This adapter deliberately leaves the runner's reducer and completeness
/// semantics unchanged; a partial stream still reports only the events seen.
pub(crate) fn filter_go_test_pipe(input: &str) -> String {
    let records: Vec<_> = input
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, raw)| {
            let line = pipe_event_line(index, raw);
            (raw, line, pipe_event(line))
        })
        .collect();
    if !records
        .iter()
        .any(|(_, _, event)| event.as_ref().is_some_and(pipe_test_evidence))
    {
        return input.to_string();
    }

    // The reducer renders build output only for a matching package-level fail.
    // Unmatched build records must not disappear beside otherwise valid tests.
    let mut failed_packages = std::collections::HashSet::new();
    let repeated_build_failure = records
        .iter()
        .filter_map(|(_, _, event)| event.as_ref())
        .filter(|event| {
            event.action == "fail" && event.test.is_none() && event.failed_build.is_some()
        })
        .any(|event| !failed_packages.insert(event.package.as_deref().unwrap_or("unknown")));
    let mut represented_builds: HashMap<_, _> = records
        .iter()
        .enumerate()
        .filter_map(|(index, (_, _, event))| event.as_ref().map(|event| (index, event)))
        .filter(|(_, event)| event.action == "fail" && event.test.is_none())
        .filter_map(|(index, event)| event.failed_build.as_deref().map(|path| (path, index)))
        .collect();
    // A later build failure for the same package can replace earlier errors in
    // the reducer. Retain all build records when that representation is lossy.
    if repeated_build_failure {
        represented_builds.clear();
    }
    let mut events = String::new();
    let mut residual = String::new();
    let mut has_residual = false;
    for (index, (raw, line, event)) in records.iter().enumerate() {
        let consumed = event.as_ref().is_some_and(|event| {
            if matches!(event.action.as_str(), "build-output" | "build-fail") {
                event.import_path.as_deref().is_some_and(|path| {
                    represented_builds
                        .get(path)
                        .is_some_and(|fail_index| index < *fail_index)
                })
            } else {
                true
            }
        });
        if consumed {
            events.push_str(line);
            events.push('\n');
        } else {
            // This also keeps CRLF and an unterminated final fragment intact.
            residual.push_str(raw);
            has_residual |= !line.is_empty();
        }
    }
    let mut shown = filter_go_test_json(&events);
    if has_residual {
        shown.push('\n');
        shown.push_str(&residual);
    }
    shown
}

/// Parse go test -json output (NDJSON format)
/// What plain `go test` (without `-json`) would have printed for this event stream:
/// every output line under `-v`; otherwise the output of failed tests, package-level
/// output and build output, without the `=== RUN`-style progress frames.
fn plain_go_test_output(output: &str, verbose: bool) -> String {
    let mut plain = String::new();
    let mut pending: HashMap<(String, String), String> = HashMap::new();
    for line in output.lines() {
        let Ok(event) = serde_json::from_str::<GoTestEvent>(line.trim()) else {
            continue;
        };
        let package = event.package.clone().unwrap_or_default();
        match (event.action.as_str(), &event.test, &event.output) {
            // Without -v, go folds PASS and the coverage line into the package's ok line.
            ("output", None, Some(text))
                if !verbose && (text == "PASS\n" || text.starts_with("coverage: ")) => {}
            ("output" | "build-output", None, Some(text)) => plain.push_str(text),
            ("output", Some(_), Some(text)) if verbose => plain.push_str(text),
            ("output", Some(test), Some(text)) => {
                if !text.starts_with("=== ") {
                    pending
                        .entry((package, test.clone()))
                        .or_default()
                        .push_str(text);
                }
            }
            ("fail", Some(test), _) => {
                if let Some(text) = pending.remove(&(package, test.clone())) {
                    plain.push_str(&text);
                }
            }
            ("pass" | "skip", Some(test), _) => {
                pending.remove(&(package, test.clone()));
            }
            _ => {}
        }
    }
    plain
}

/// `rtk go test`: the JSON summary, unless plain `go test` would have printed less.
fn filter_go_test_run(output: &str, verbose: bool) -> String {
    let filtered = summarize_go_test_json(output, true);
    let plain = plain_go_test_output(output, verbose);
    never_worse(plain.trim_end(), &filtered).to_string()
}

/// The summary `rtk pipe -f go-test` prints: counts and failures only.
pub(crate) fn filter_go_test_json(output: &str) -> String {
    summarize_go_test_json(output, false)
}

/// Summarize a `go test -json` stream. With `result_lines`, go's own per-package
/// `ok`/`FAIL` lines close the summary, carrying the import path, `(cached)` and coverage.
fn summarize_go_test_json(output: &str, result_lines: bool) -> String {
    let mut packages: HashMap<String, PackageResult> = HashMap::new();
    let mut current_test_output: HashMap<(String, String), Vec<String>> = HashMap::new(); // (package, test) -> outputs
    let mut build_output: HashMap<String, Vec<String>> = HashMap::new(); // import_path -> error lines

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: GoTestEvent = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue, // Skip non-JSON lines
        };

        // Handle build-output/build-fail events (use ImportPath, no Package)
        match event.action.as_str() {
            "build-output" => {
                if let (Some(import_path), Some(output_text)) = (&event.import_path, &event.output)
                {
                    let text = output_text.trim_end().to_string();
                    if !text.is_empty() {
                        build_output
                            .entry(import_path.clone())
                            .or_default()
                            .push(text);
                    }
                }
                continue;
            }
            "build-fail" => {
                // build-fail has ImportPath — we'll handle it when the package-level fail arrives
                continue;
            }
            _ => {}
        }

        let package = event.package.unwrap_or_else(|| "unknown".to_string());
        let pkg_result = packages.entry(package.clone()).or_default();

        match event.action.as_str() {
            "pass" if event.test.is_some() => {
                pkg_result.pass += 1;
            }
            "fail" => {
                if let Some(test) = &event.test {
                    // Individual test failure
                    pkg_result.fail += 1;

                    // Collect output for failed test
                    let key = (package.clone(), test.clone());
                    let outputs = current_test_output.remove(&key).unwrap_or_default();
                    pkg_result.failed_tests.push((test.clone(), outputs));
                } else if event.failed_build.is_some() {
                    // Package-level build failure
                    pkg_result.build_failed = true;
                    // Collect build errors from the import path
                    if let Some(import_path) = &event.failed_build
                        && let Some(errors) = build_output.remove(import_path)
                    {
                        pkg_result.build_errors = errors;
                    }
                } else {
                    // Package-level failure without a specific test or build error
                    // (timeout, signal kill, panic before test execution, etc.)
                    pkg_result.package_failed = true;
                }
            }
            "skip" if event.test.is_some() => {
                pkg_result.skip += 1;
            }
            "output" => {
                if let Some(output_text) = &event.output {
                    if let Some(test) = &event.test {
                        // Keep the package's last fuzz progress line: Go prints the final stats there
                        if output_text.starts_with("fuzz: elapsed:") {
                            pkg_result.fuzz_summary = Some(output_text.trim_end().to_string());
                        }
                        // Collect output for current test
                        let key = (package.clone(), test.clone());
                        current_test_output
                            .entry(key)
                            .or_default()
                            .push(output_text.trim_end().to_string());
                    } else {
                        // Package-level output (timeout messages, signal info, etc.)
                        let trimmed = output_text.trim();
                        if output_text.starts_with("ok ") || output_text.starts_with("FAIL\t") {
                            pkg_result.result_line = Some(output_text.trim_end().to_string());
                        }
                        if !trimmed.is_empty() {
                            pkg_result.package_fail_output.push(trimmed.to_string());
                        }
                    }
                }
            }
            _ => {} // run, pause, cont, etc.
        }
    }

    // A failing subtest also fails its parent; count and list it once, unless the
    // parent printed a failure of its own.
    for pkg in packages.values_mut() {
        let names: Vec<String> = pkg.failed_tests.iter().map(|(n, _)| n.clone()).collect();
        let before = pkg.failed_tests.len();
        pkg.failed_tests.retain(|(name, outputs)| {
            let prefix = format!("{name}/");
            !names.iter().any(|n| n.starts_with(&prefix))
                || !select_go_test_failure_lines(outputs).is_empty()
        });
        pkg.fail -= before - pkg.failed_tests.len();
    }

    // Build summary
    let total_packages = packages.len();
    let total_pass: usize = packages.values().map(|p| p.pass).sum();
    let total_fail: usize = packages.values().map(|p| p.fail).sum();
    let total_skip: usize = packages.values().map(|p| p.skip).sum();
    let total_panic: usize = packages
        .values()
        .flat_map(|p| &p.failed_tests)
        .filter(|(_, outputs)| test_panicked(outputs))
        .count();
    let total_build_fail: usize = packages.values().filter(|p| p.build_failed).count();
    // Only count package-level fails for packages with no individual test or build failures.
    // go test -json emits a trailing package-level {"action":"fail"} after any test failure
    // too, but that event is just a cascade — the individual test failures are already counted.
    let total_pkg_fail: usize = packages
        .values()
        .filter(|p| p.package_failed && p.fail == 0 && !p.build_failed)
        .count();

    let has_failures = total_fail > 0 || total_build_fail > 0 || total_pkg_fail > 0;

    if !has_failures && total_pass == 0 {
        return "Go test: No tests found".to_string();
    }

    let total_fuzz = packages
        .values()
        .filter(|p| p.fuzz_summary.is_some())
        .count();
    if !has_failures && total_fuzz > 0 {
        return format_fuzz_summary(&packages, total_pass, total_fuzz);
    }

    if !has_failures {
        let mut result = format!(
            "Go test: {} passed in {} packages\n",
            total_pass, total_packages
        );
        if result_lines {
            push_result_lines(&mut result, &packages, |_| true);
        }
        return result.trim().to_string();
    }

    let mut result = String::new();
    result.push_str(&format!(
        "Go test: {} passed, {} failed",
        total_pass,
        total_fail - total_panic + total_build_fail + total_pkg_fail
    ));
    if total_panic > 0 {
        result.push_str(&format!(", {} panicked", total_panic));
    }
    if total_skip > 0 {
        result.push_str(&format!(", {} skipped", total_skip));
    }
    result.push_str(&format!(" in {} packages\n", total_packages));

    // Show package-level failures first (timeouts, signals, panics).
    // Skip packages that already have individual test-level failures — those are displayed
    // in the per-package section below and the package-level event is just a cascade.
    for (package, pkg_result) in packages.iter() {
        if !pkg_result.package_failed || pkg_result.fail > 0 || pkg_result.build_failed {
            continue;
        }

        result.push_str(&format!("\n{} [FAIL]\n", compact_package_name(package)));

        for line in &pkg_result.package_fail_output {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                result.push_str(&format!("  {}\n", trimmed));
            }
        }
    }

    // Show build failures
    for (package, pkg_result) in packages.iter() {
        if !pkg_result.build_failed {
            continue;
        }

        result.push_str(&format!(
            "\n{} [build failed]\n",
            compact_package_name(package)
        ));

        for line in &pkg_result.build_errors {
            let trimmed = line.trim();
            // Skip the "# package" header line
            if !trimmed.starts_with('#') && !trimmed.is_empty() {
                result.push_str(&format!("  {}\n", truncate(trimmed, 120)));
            }
        }
    }

    // Show failed tests grouped by package
    for (package, pkg_result) in packages.iter() {
        if pkg_result.fail == 0 {
            continue;
        }

        let panicked = pkg_result
            .failed_tests
            .iter()
            .filter(|(_, outputs)| test_panicked(outputs))
            .count();
        let panic_note = if panicked > 0 {
            format!(", {} panicked", panicked)
        } else {
            String::new()
        };
        result.push_str(&format!(
            "\n{} ({} passed, {} failed{})\n",
            compact_package_name(package),
            pkg_result.pass,
            pkg_result.fail - panicked,
            panic_note
        ));

        for (test, outputs) in &pkg_result.failed_tests {
            let label = if test_panicked(outputs) {
                "PANIC"
            } else {
                "FAIL"
            };
            result.push_str(&format!("  [{}] {}\n", label, test));

            for line in select_go_test_failure_lines(outputs) {
                result.push_str(&format!("     {}\n", line));
            }
        }
    }

    // go's own result line for every package not already shown with its package-level
    // output or build errors: the full import path, `(cached)` and coverage.
    if result_lines {
        result.push('\n');
        push_result_lines(&mut result, &packages, |p| {
            !p.build_failed && !(p.package_failed && p.fail == 0)
        });
    }

    result.trim().to_string()
}

fn push_result_lines(
    result: &mut String,
    packages: &HashMap<String, PackageResult>,
    include: impl Fn(&PackageResult) -> bool,
) {
    let mut lines: Vec<&str> = packages
        .values()
        .filter(|p| include(p))
        .filter_map(|p| p.result_line.as_deref())
        .collect();
    lines.sort_by_key(|line| line.split('\t').nth(1).unwrap_or(line));
    for line in lines {
        result.push_str(line);
        result.push('\n');
    }
}

fn test_panicked(outputs: &[String]) -> bool {
    outputs
        .iter()
        .any(|line| line.trim_start().starts_with("panic:"))
}

fn format_fuzz_summary(
    packages: &HashMap<String, PackageResult>,
    total_pass: usize,
    total_fuzz: usize,
) -> String {
    let mut result = format!(
        "Go test: {} passed, {} fuzz in {} packages\n",
        total_pass,
        total_fuzz,
        packages.len()
    );
    let mut fuzzed: Vec<(&String, &String)> = packages
        .iter()
        .filter_map(|(pkg, r)| r.fuzz_summary.as_ref().map(|s| (pkg, s)))
        .collect();
    fuzzed.sort();
    for (package, summary) in fuzzed {
        result.push_str(&format!(
            "{}\n  {}\n",
            compact_package_name(package),
            summary
        ));
    }
    result.trim().to_string()
}

fn select_go_test_failure_lines(outputs: &[String]) -> Vec<String> {
    const MAX_FAILURE_LINES: usize = 8;
    let is_frame_noise = |line: &str| {
        line.is_empty()
            || line.starts_with("=== ")
            || line.starts_with("--- FAIL")
            || line.starts_with("--- PASS")
    };
    let goroot = goroot_prefix(outputs);
    let mut relevant = Vec::new();
    let mut keep_next_context_line = false;
    let mut in_panic = false;

    let mut i = 0;
    while i < outputs.len() && relevant.len() < MAX_FAILURE_LINES {
        let trimmed = outputs[i].trim();
        i += 1;

        if in_panic {
            // A panic's stack: keep the function/location pairs in the repo and its
            // dependencies; the runtime and testing frames from GOROOT say nothing new.
            if trimmed.starts_with("goroutine ") {
                continue;
            }
            if let Some(location) = outputs.get(i).map(|l| l.trim())
                && is_stack_frame_location(location)
            {
                i += 1;
                if !goroot.is_some_and(|root| location.starts_with(root)) {
                    relevant.push(trimmed.to_string());
                    relevant.push(location.to_string());
                }
                continue;
            }
        }

        if is_frame_noise(trimmed) {
            keep_next_context_line = false;
            continue;
        }

        let is_location = is_go_test_location_line(trimmed);
        let is_failure = is_go_test_failure_line(trimmed);

        if is_location || is_failure || keep_next_context_line {
            relevant.push(trimmed.to_string());
            keep_next_context_line = is_location;
        } else {
            keep_next_context_line = false;
        }
        in_panic |= trimmed.starts_with("panic:");
    }

    if relevant.is_empty()
        && let Some(line) = outputs
            .iter()
            .map(|line| line.trim())
            .find(|line| !is_frame_noise(line))
    {
        relevant.push(line.to_string());
    }

    relevant
}

/// The `<GOROOT>/src/` prefix of a panic's stack, read off the testing package's
/// own frame, which every test panic passes through.
fn goroot_prefix(outputs: &[String]) -> Option<&str> {
    outputs.iter().find_map(|line| {
        let line = line.trim();
        let at = line.find("/src/testing/testing.go:")?;
        Some(&line[..at + "/src/".len()])
    })
}

/// The location line under a stack frame's function: `<path>.go:<line> +0x<pc>`.
fn is_stack_frame_location(line: &str) -> bool {
    is_go_test_location_line(line) && (line.starts_with('/') || line.contains(" +0x"))
}

fn is_go_test_location_line(line: &str) -> bool {
    if let Some((_, rest)) = line.split_once(".go:") {
        rest.chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    } else {
        false
    }
}

fn is_go_test_failure_line(line: &str) -> bool {
    let lower = line.to_lowercase();

    lower.starts_with("panic:")
        || lower.starts_with("error:")
        || lower.contains(" error:")
        || lower.contains("expected")
        || lower.contains("got")
        || lower.contains("want")
        || lower.contains("actual")
        || lower.contains("assert")
        || lower.contains("mismatch")
        || lower.contains("unexpected")
        || lower.contains("fatal")
        || line.starts_with("at ")
}

/// Filter go build output - show only errors
pub(crate) fn filter_go_build(output: &str) -> String {
    filter_go_build_with_exit(output, 0)
}

fn filter_go_build_with_exit(output: &str, exit_code: i32) -> String {
    let mut errors: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if is_go_build_error_line(trimmed) {
            errors.push(trimmed.to_string());
        }
    }

    if errors.is_empty() {
        return if exit_code == 0 {
            "Go build: Success".to_string()
        } else {
            format_go_build_failure(output, exit_code)
        };
    }

    let mut result = String::new();
    result.push_str(&format!("Go build: {} errors\n", errors.len()));

    const MAX_GO_BUILD_ERRORS: usize = CAP_ERRORS;
    for (i, error) in errors.iter().take(MAX_GO_BUILD_ERRORS).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, truncate(error, 120)));
    }

    if errors.len() > MAX_GO_BUILD_ERRORS {
        result.push_str(&format!(
            "\n… +{} more errors\n",
            errors.len() - MAX_GO_BUILD_ERRORS
        ));
        let all_errors = errors.join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all_errors, "go-build", MAX_GO_BUILD_ERRORS + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

fn format_go_build_failure(output: &str, exit_code: i32) -> String {
    let lines: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    if lines.is_empty() {
        return format!("Go build: failed (exit {})", exit_code);
    }

    let mut result = String::new();
    result.push_str(&format!("Go build: failed (exit {})\n", exit_code));
    result.push_str("═══════════════════════════════════════\n");

    const MAX_GO_BUILD_ERRORS: usize = CAP_ERRORS;
    for (i, line) in lines.iter().take(MAX_GO_BUILD_ERRORS).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, truncate(line, 120)));
    }

    if lines.len() > MAX_GO_BUILD_ERRORS {
        result.push_str(&format!(
            "\n… +{} more output lines\n",
            lines.len() - MAX_GO_BUILD_ERRORS
        ));
    }

    result.trim().to_string()
}

fn is_go_build_error_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_lowercase();

    // Go download/progress lines often contain package names like pkg/errors,
    // xerrors, or multierror. These are not compilation failures.
    if lower.starts_with("go: downloading ")
        || lower.starts_with("go: finding ")
        || lower.starts_with("go: extracting ")
    {
        return false;
    }

    // Package headers are context, not errors by themselves.
    if trimmed.starts_with('#') {
        return false;
    }

    // Canonical compiler/config error locations: file:line:col: ...
    let is_go_config_location = !lower.starts_with("go: ")
        && (lower.contains("go.mod:") || lower.contains("go.work:") || lower.contains("go.sum:"));
    if trimmed.contains(".go:") || is_go_config_location {
        return true;
    }

    // Some compiler/module failures do not include a file.go:line:col location.
    let non_file_error_prefixes = [
        "undefined: ",
        "cannot use ",
        "cannot find package ",
        "no required module provides package ",
        "missing go.sum entry for module providing package ",
        "found packages ",
        "go: go.mod file not found in current directory or any parent directory",
        "go: cannot load module ",
        "go: build failed",
        "go: error ",
        "error: ",
        "pattern ",
        "go: updates to go.mod needed",
        "go: inconsistent vendoring",
        "no go files in ",
    ];

    non_file_error_prefixes
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || lower.contains("import cycle not allowed")
        || lower.contains("build constraints exclude all go files")
        || lower.contains("function main is undeclared in the main package")
}

/// Filter go vet output - show issues
fn filter_go_vet(output: &str) -> String {
    let mut issues: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // Collect issue lines (vet reports issues with file:line:col format)
        if !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.contains(".go:") {
            issues.push(trimmed.to_string());
        }
    }

    if issues.is_empty() {
        return "Go vet: No issues found".to_string();
    }

    let mut result = String::new();
    result.push_str(&format!("Go vet: {} issues\n", issues.len()));

    const MAX_GO_VET_ISSUES: usize = CAP_ERRORS;
    for (i, issue) in issues.iter().take(MAX_GO_VET_ISSUES).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, truncate(issue, 120)));
    }

    if issues.len() > MAX_GO_VET_ISSUES {
        result.push_str(&format!(
            "\n… +{} more issues\n",
            issues.len() - MAX_GO_VET_ISSUES
        ));
        let all_issues = issues.join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all_issues, "go-vet", MAX_GO_VET_ISSUES + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

/// Compact package name (remove long paths)
fn compact_package_name(package: &str) -> String {
    // Remove common module prefixes like github.com/user/repo/
    if let Some(pos) = package.rfind('/') {
        package[pos + 1..].to_string()
    } else {
        package.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detection_requires_leading_stream_evidence() {
        let event = r#"{"Action":"pass","Package":"example.com/proj","Test":"TestOK"}"#;
        for prefix in [
            "",
            "\u{feff}",
            " \r\n",
            "go: downloading example.com/dep v1.0.0\n",
        ] {
            assert!(is_go_test_json(&format!("{prefix}{event}")));
        }
        for prefix in [
            "diff --git a/report b/report\n ",
            "```json\n",
            "# Notes\n\n",
            "Diagnostic example:\n",
        ] {
            assert!(!is_go_test_json(&format!("{prefix}{event}")));
        }
        assert!(!is_go_test_json("go: downloading example.com/dep v1.0.0\n"));
        assert!(!is_go_test_json(r#"{"Action":"start"}"#));
    }

    // Fixtures: real `go test -json` captures (Go 1.27.3), trimmed to the failing tests
    // and a few passing ones, with home and module paths sanitized.
    const CACHED_COVER_JSON: &str =
        include_str!("../../../tests/fixtures/go_test_cached_cover.json");
    const SUBTEST_FAIL_JSON: &str =
        include_str!("../../../tests/fixtures/go_test_subtest_fail.json");
    const PANIC_JSON: &str = include_str!("../../../tests/fixtures/go_test_panic.json");
    const SETUP_FAIL_JSON: &str =
        include_str!("../../../tests/fixtures/go_test_package_setup_fail.json");

    #[test]
    fn go_test_pass_keeps_each_package_result_line() {
        // `go test -cover ./internal/...` served from the cache.
        let out = summarize_go_test_json(CACHED_COVER_JSON, true);
        for line in [
            "ok  \tgithub.com/example/broodhall/internal/container\t(cached)\tcoverage: 98.9% of statements",
            "ok  \tgithub.com/example/broodhall/internal/pytext\t(cached)\tcoverage: 98.8% of statements",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
    }

    #[test]
    fn go_test_failure_keeps_the_whole_assertion_and_passing_package_lines() {
        // `go test ./internal/...`: one subtest of TestCmd fails, pytext passes from cache.
        let out = summarize_go_test_json(SUBTEST_FAIL_JSON, true);
        assert!(
            out.contains(r#"want ["ls"], [{./a /x} {~/b /y} {$H /z}]"#),
            "assertion cut short:\n{out}"
        );
        assert!(
            out.contains("FAIL\tgithub.com/example/broodhall/internal/container\t"),
            "{out}"
        );
        assert!(
            out.contains("ok  \tgithub.com/example/broodhall/internal/pytext\t(cached)"),
            "{out}"
        );
        // The failing subtest is counted once, not again as its parent TestCmd.
        assert!(out.contains(", 1 failed"), "{out}");
        assert!(!out.contains("[FAIL] TestCmd\n"), "{out}");
    }

    #[test]
    fn go_test_panic_keeps_the_repo_frame_and_says_it_panicked() {
        let out = filter_go_test_json(PANIC_JSON);
        assert!(out.contains("internal/pytext/cache_test.go:9"), "{out}");
        assert!(
            out.contains("panic: assignment to entry in nil map"),
            "{out}"
        );
        assert!(!out.contains("/src/testing/testing.go"), "{out}");
        assert!(out.contains("1 panicked"), "{out}");
        assert!(out.contains("[PANIC] TestDecodeCacheWarm"), "{out}");
    }

    #[test]
    fn go_test_package_failure_line_is_not_truncated() {
        let out = filter_go_test_json(SETUP_FAIL_JSON);
        assert!(out.contains("(set MEMORY_EXT_DIR)"), "{out}");
        assert!(
            out.contains("FAIL\tgithub.com/example/broodhall/cmd/memory\t0.023s"),
            "{out}"
        );
    }

    #[test]
    fn go_test_run_output_is_never_larger_than_plain_go_test() {
        for (json, verbose) in [
            (CACHED_COVER_JSON, false),
            (CACHED_COVER_JSON, true),
            (SUBTEST_FAIL_JSON, false),
            (PANIC_JSON, false),
            (SETUP_FAIL_JSON, false),
        ] {
            let plain = plain_go_test_output(json, verbose);
            let out = filter_go_test_run(json, verbose);
            assert!(
                crate::core::tracking::estimate_tokens(&out)
                    <= crate::core::tracking::estimate_tokens(&plain),
                "rtk output larger than plain go test:\n{out}\n---\n{plain}"
            );
        }
        // Non-verbose plain output of a passing run is just go's result lines.
        let plain = plain_go_test_output(CACHED_COVER_JSON, false);
        assert!(!plain.contains("=== RUN"), "{plain}");
        assert!(
            plain.contains("ok  \tgithub.com/example/broodhall/internal/pytext"),
            "{plain}"
        );
    }

    #[test]
    fn test_filter_go_test_all_pass() {
        let output = r#"{"Time":"2024-01-01T10:00:00Z","Action":"run","Package":"example.com/foo","Test":"TestBar"}
{"Time":"2024-01-01T10:00:01Z","Action":"output","Package":"example.com/foo","Test":"TestBar","Output":"=== RUN   TestBar\n"}
{"Time":"2024-01-01T10:00:02Z","Action":"pass","Package":"example.com/foo","Test":"TestBar","Elapsed":0.5}
{"Time":"2024-01-01T10:00:02Z","Action":"pass","Package":"example.com/foo","Elapsed":0.5}"#;

        let result = filter_go_test_json(output);
        assert!(result.contains("Go test"));
        assert!(result.contains("1 passed"));
        assert!(result.contains("1 packages"));
    }

    #[test]
    fn test_filter_go_test_with_failures() {
        let output = r#"{"Time":"2024-01-01T10:00:00Z","Action":"run","Package":"example.com/foo","Test":"TestFail"}
{"Time":"2024-01-01T10:00:01Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"=== RUN   TestFail\n"}
{"Time":"2024-01-01T10:00:02Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"    Error: expected 5, got 3\n"}
{"Time":"2024-01-01T10:00:03Z","Action":"fail","Package":"example.com/foo","Test":"TestFail","Elapsed":0.5}
{"Time":"2024-01-01T10:00:03Z","Action":"fail","Package":"example.com/foo","Elapsed":0.5}"#;

        let result = filter_go_test_json(output);
        assert!(result.contains("1 failed"));
        assert!(result.contains("TestFail"));
        assert!(result.contains("expected 5, got 3"));
    }

    #[test]
    fn test_filter_go_test_preserves_file_location_and_followup_context() {
        let output = r#"{"Time":"2024-01-01T10:00:00Z","Action":"run","Package":"example.com/foo","Test":"TestFail"}
{"Time":"2024-01-01T10:00:01Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"=== RUN   TestFail\n"}
{"Time":"2024-01-01T10:00:02Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"    foo_test.go:42:\n"}
{"Time":"2024-01-01T10:00:03Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"        values differ after normalization\n"}
{"Time":"2024-01-01T10:00:04Z","Action":"fail","Package":"example.com/foo","Test":"TestFail","Elapsed":0.5}
{"Time":"2024-01-01T10:00:04Z","Action":"fail","Package":"example.com/foo","Elapsed":0.5}"#;

        let result = filter_go_test_json(output);
        assert!(result.contains("foo_test.go:42:"));
        assert!(result.contains("values differ after normalization"));
    }

    #[test]
    fn test_filter_go_test_timeout_package_fail() {
        // When go test times out, the JSON stream has a package-level "fail"
        // with no Test field and no FailedBuild field. This should be reported
        // as a failure, not "No tests found".
        let output = r#"{"Time":"2024-01-01T10:00:00Z","Action":"start","Package":"example.com/foo"}
{"Time":"2024-01-01T10:01:03Z","Action":"output","Package":"example.com/foo","Output":"*** Test killed with quit: ran too long (1m3s).\n"}
{"Time":"2024-01-01T10:01:03Z","Action":"output","Package":"example.com/foo","Output":"FAIL\texample.com/foo\t63.001s\n"}
{"Time":"2024-01-01T10:01:03Z","Action":"fail","Package":"example.com/foo","Elapsed":63.003}"#;

        let result = filter_go_test_json(output);
        assert!(
            result.contains("1 failed"),
            "Expected '1 failed' in output, got: {}",
            result
        );
        assert!(
            !result.contains("No tests found"),
            "Should not say 'No tests found' on timeout, got: {}",
            result
        );
        assert!(
            result.contains("FAIL"),
            "Expected failure output in summary, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_go_test_no_double_count_on_test_failure() {
        // go test -json always emits a package-level {"action":"fail"} after each
        // test-level failure. The package-level event is a cascade, not an additional
        // failure. The summary header must show "1 failed", not "2 failed".
        let output = r#"{"Time":"2024-01-01T10:00:00Z","Action":"run","Package":"example.com/foo","Test":"TestFail"}
{"Time":"2024-01-01T10:00:01Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"=== RUN   TestFail\n"}
{"Time":"2024-01-01T10:00:02Z","Action":"output","Package":"example.com/foo","Test":"TestFail","Output":"    Error: expected 5, got 3\n"}
{"Time":"2024-01-01T10:00:03Z","Action":"fail","Package":"example.com/foo","Test":"TestFail","Elapsed":0.5}
{"Time":"2024-01-01T10:00:03Z","Action":"fail","Package":"example.com/foo","Elapsed":0.5}"#;

        let result = filter_go_test_json(output);
        // The summary header must say "1 failed", not "2 failed" (no double-counting).
        assert!(
            result.starts_with("Go test: 0 passed, 1 failed"),
            "Expected header 'Go test: 0 passed, 1 failed', got: {}",
            result
        );
        assert!(result.contains("TestFail"));
        assert!(result.contains("expected 5, got 3"));
        // The package must NOT appear twice (once as "[FAIL]" and once with test details).
        assert_eq!(
            result.matches("foo").count(),
            1,
            "Package name should appear exactly once, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_go_test_timeout_with_signal_quit_output() {
        // Exact reproduction of the scenario from issue #958: the signal: quit line
        // appears as a separate JSON output event.
        let output = r#"{"Action":"start","Package":"example.com/pkg"}
{"Action":"output","Package":"example.com/pkg","Output":"*** Test killed with quit: ran too long (1m30s).\n"}
{"Action":"output","Package":"example.com/pkg","Output":"signal: quit\n"}
{"Action":"output","Package":"example.com/pkg","Output":"FAIL\texample.com/pkg\t90.000s\n"}
{"Action":"fail","Package":"example.com/pkg","Elapsed":90.001}"#;

        let result = filter_go_test_json(output);
        assert!(
            result.starts_with("Go test: 0 passed, 1 failed"),
            "Expected 'Go test: 0 passed, 1 failed', got: {}",
            result
        );
        assert!(
            !result.contains("No tests found"),
            "Must not say 'No tests found' on timeout, got: {}",
            result
        );
        assert!(
            result.contains("Test killed with quit"),
            "Should show the timeout message, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_go_test_timeout_with_passing_tests_before_kill() {
        // Some tests pass before the package times out.
        // Summary should show both pass and fail counts.
        let output = r#"{"Action":"run","Package":"example.com/foo","Test":"TestFast"}
{"Action":"pass","Package":"example.com/foo","Test":"TestFast","Elapsed":0.001}
{"Action":"run","Package":"example.com/foo","Test":"TestHang"}
{"Action":"output","Package":"example.com/foo","Output":"*** Test killed with quit: ran too long (30s).\n"}
{"Action":"fail","Package":"example.com/foo","Elapsed":30.001}"#;

        let result = filter_go_test_json(output);
        assert!(
            result.starts_with("Go test: 1 passed, 1 failed"),
            "Expected 'Go test: 1 passed, 1 failed', got: {}",
            result
        );
        assert!(
            !result.contains("No tests found"),
            "Must not say 'No tests found', got: {}",
            result
        );
        assert!(
            result.contains("Test killed with quit"),
            "Should show timeout message, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_go_build_success() {
        let output = "";
        let result = filter_go_build(output);
        assert!(result.contains("Go build"));
        assert!(result.contains("Success"));
    }

    #[test]
    fn test_filter_go_build_errors() {
        let output = r#"# example.com/foo
main.go:10:5: undefined: missingFunc
main.go:15:2: cannot use x (type int) as type string"#;

        let result = filter_go_build(output);
        assert!(result.contains("2 errors"));
        assert!(result.contains("undefined: missingFunc"));
        assert!(result.contains("cannot use x"));
    }

    #[test]
    fn test_filter_go_build_ignores_download_lines_with_error_in_package_names() {
        let output = r#"go: downloading github.com/go-errors/errors v1.5.1
go: finding module for package example.com/foo
go: extracting github.com/pkg/errors v0.9.1
go: downloading github.com/pkg/errors v0.9.1
go: downloading github.com/hashicorp/go-multierror v1.1.1
go: downloading golang.org/x/xerrors v0.0.0-20220907171357-04be3eba64a2"#;

        let result = filter_go_build(output);
        assert_eq!(result, "Go build: Success");
    }

    #[test]
    fn test_is_go_build_error_line_recognizes_real_compiler_errors() {
        assert!(is_go_build_error_line("undefined: missingFunc"));
        assert!(is_go_build_error_line("cannot find package \"foo/bar\""));
        assert!(is_go_build_error_line(
            "found packages a (a.go) and b (b.go) in /tmp/rtk-go-build-probe-mix"
        ));
        assert!(is_go_build_error_line(
            "imports example.com/cycle/a: import cycle not allowed"
        ));
        assert!(is_go_build_error_line(
            "package example.com/buildtag: build constraints exclude all Go files in /tmp/rtk-go-build-probe-buildtag"
        ));
        assert!(is_go_build_error_line(
            "go.mod:3: invalid go version 'not-a-version': must match format 1.23.0"
        ));
        assert!(is_go_build_error_line(
            "go.work:1: invalid go version 'not-a-version': must match format 1.23.0"
        ));
        assert!(is_go_build_error_line(
            "go: go.mod file not found in current directory or any parent directory; see 'go help modules'"
        ));
        assert!(is_go_build_error_line("no Go files in /tmp/example"));
        assert!(is_go_build_error_line(
            "go: cannot load module missing listed in go.work file: open missing/go.mod: no such file or directory"
        ));
        assert!(is_go_build_error_line(
            "runtime.main_main·f: function main is undeclared in the main package"
        ));
        assert!(is_go_build_error_line(
            "main.go:10:5: undefined: missingFunc"
        ));
        assert!(is_go_build_error_line("error: failed to load module"));
        assert!(!is_go_build_error_line(
            "go: downloading github.com/pkg/errors v0.9.1"
        ));
        assert!(!is_go_build_error_line(
            "go: finding module for package example.com/foo"
        ));
        assert!(!is_go_build_error_line(
            "go: extracting github.com/pkg/errors v0.9.1"
        ));
        assert!(!is_go_build_error_line("# example.com/foo"));
    }

    #[test]
    fn test_filter_go_build_preserves_non_file_error_shapes() {
        let output = r#"undefined: missingFunc
cannot find package "foo/bar"
found packages a (a.go) and b (b.go) in /tmp/rtk-go-build-probe-mix
imports example.com/cycle/a: import cycle not allowed
package example.com/buildtag: build constraints exclude all Go files in /tmp/rtk-go-build-probe-buildtag
runtime.main_main·f: function main is undeclared in the main package"#;

        let result = filter_go_build(output);
        assert!(result.contains("6 errors"));
        assert!(result.contains("undefined: missingFunc"));
        assert!(result.contains("cannot find package \"foo/bar\""));
        assert!(result.contains("found packages a (a.go) and b (b.go)"));
        assert!(result.contains("import cycle not allowed"));
        assert!(result.contains("build constraints exclude all Go files"));
        assert!(result.contains("function main is undeclared in the main package"));
    }

    #[test]
    fn test_filter_go_build_preserves_go_config_parse_errors() {
        let output = r#"go: errors parsing go.mod:
go.mod:3: invalid go version 'not-a-version': must match format 1.23.0
go: errors parsing go.work:
go.work:1: invalid go version 'not-a-version': must match format 1.23.0"#;

        let result = filter_go_build(output);
        assert!(result.contains("2 errors"));
        assert!(result.contains("go.mod:3: invalid go version"));
        assert!(result.contains("go.work:1: invalid go version"));
        assert!(!result.contains("go: errors parsing go.mod:"));
        assert!(!result.contains("go: errors parsing go.work:"));
    }

    #[test]
    fn test_filter_go_build_preserves_module_root_and_workspace_errors() {
        let output = r#"go: go.mod file not found in current directory or any parent directory; see 'go help modules'
no Go files in /tmp/example
go: cannot load module missing listed in go.work file: open missing/go.mod: no such file or directory"#;

        let result = filter_go_build(output);
        assert!(result.contains("3 errors"));
        assert!(
            result.contains("go.mod file not found in current directory or any parent directory")
        );
        assert!(result.contains("no Go files in /tmp/example"));
        assert!(result.contains("go: cannot load module missing listed in go.work file"));
    }

    #[test]
    fn test_filter_go_build_preserves_package_pattern_errors() {
        let output = r#"pattern ./...: directory prefix . does not contain main module or its selected dependencies
pattern ./...: directory prefix . does not contain modules listed in go.work or their selected dependencies"#;

        let result = filter_go_build(output);
        assert!(result.contains("2 errors"));
        assert!(result.contains("does not contain main module"));
        assert!(result.contains("does not contain modules listed in go.work"));
        assert!(!result.contains("Success"));
    }

    #[test]
    fn test_filter_go_build_nonzero_exit_never_reports_success() {
        let output = "opaque go build failure from stderr";

        let result = filter_go_build_with_exit(output, 1);
        assert!(result.contains("Go build: failed (exit 1)"));
        assert!(result.contains(output));
        assert!(!result.contains("Success"));
    }

    #[test]
    fn test_filter_go_vet_no_issues() {
        let output = "";
        let result = filter_go_vet(output);
        assert!(result.contains("Go vet"));
        assert!(result.contains("No issues found"));
    }

    #[test]
    fn test_filter_go_vet_with_issues() {
        let output = r#"main.go:42:2: Printf format %d has arg x of wrong type string
utils.go:15:5: unreachable code"#;

        let result = filter_go_vet(output);
        assert!(result.contains("2 issues"));
        assert!(result.contains("Printf format"));
        assert!(result.contains("unreachable code"));
    }

    #[test]
    fn test_compact_package_name() {
        assert_eq!(compact_package_name("github.com/user/repo/pkg"), "pkg");
        assert_eq!(compact_package_name("example.com/foo"), "foo");
        assert_eq!(compact_package_name("simple"), "simple");
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn test_match_go_tool_golangci_lint() {
        let args = os(&["tool", "golangci-lint", "run", "./..."]);
        let (tool, rest) = match_go_tool(&args).expect("should match");
        assert_eq!(tool, GoTool::GolangciLint);
        assert_eq!(rest.len(), 2); // ["run", "./..."]
    }

    #[test]
    fn test_match_go_tool_bare() {
        let args = os(&["tool", "golangci-lint"]);
        let (tool, rest) = match_go_tool(&args).expect("should match");
        assert_eq!(tool, GoTool::GolangciLint);
        assert!(rest.is_empty());
    }

    #[test]
    fn test_match_go_tool_rejects_unknown() {
        assert!(match_go_tool(&os(&["tool", "pprof"])).is_none());
        assert!(match_go_tool(&os(&["tool"])).is_none());
        assert!(match_go_tool(&os(&["test", "./..."])).is_none());
        assert!(match_go_tool(&os(&[])).is_none());
    }

    #[test]
    fn test_has_golangci_format_flag_v1() {
        assert!(has_golangci_format_flag(&os(&["--out-format=json"])));
        assert!(has_golangci_format_flag(&os(&[
            "./...",
            "--out-format",
            "json"
        ])));
    }

    #[test]
    fn test_has_golangci_format_flag_v2() {
        assert!(has_golangci_format_flag(&os(&[
            "--output.json.path",
            "stdout"
        ])));
        assert!(has_golangci_format_flag(&os(&[
            "--output.json.path=stdout"
        ])));
    }

    #[test]
    fn test_has_golangci_format_flag_absent() {
        assert!(!has_golangci_format_flag(&os(&["run", "./..."])));
        assert!(!has_golangci_format_flag(&os(&[])));
        assert!(!has_golangci_format_flag(&os(&["--fix"])));
    }

    #[test]
    fn test_filter_go_test_fuzz_keeps_last_elapsed_line() {
        let output = r#"{"Action":"start","Package":"example.com/foo"}
{"Action":"run","Package":"example.com/foo","Test":"FuzzBar"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"=== RUN   FuzzBar\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"fuzz: elapsed: 0s, gathering baseline coverage: 0/6 completed\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"fuzz: elapsed: 3s, execs: 224325 (74749/sec), new interesting: 0 (total: 6)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"fuzz: elapsed: 10s, execs: 842077 (78542/sec), new interesting: 0 (total: 6)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"--- PASS: FuzzBar (10.10s)\n"}
{"Action":"pass","Package":"example.com/foo","Test":"FuzzBar","Elapsed":10.1}
{"Action":"output","Package":"example.com/foo","Output":"PASS\n"}
{"Action":"pass","Package":"example.com/foo","Elapsed":10.414}"#;

        let result = filter_go_test_json(output);

        assert_eq!(
            result,
            "Go test: 1 passed, 1 fuzz in 1 packages\nfoo\n  fuzz: elapsed: 10s, execs: 842077 (78542/sec), new interesting: 0 (total: 6)"
        );
    }

    #[test]
    fn test_filter_go_test_fuzz_with_unit_tests() {
        let output = r#"{"Action":"pass","Package":"example.com/foo","Test":"TestUnit","Elapsed":0.01}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"fuzz: elapsed: 3s, execs: 100000 (33333/sec), new interesting: 1 (total: 5)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzBar","Output":"fuzz: elapsed: 5s, execs: 200000 (40000/sec), new interesting: 2 (total: 6)\n"}
{"Action":"pass","Package":"example.com/foo","Test":"FuzzBar","Elapsed":5.0}
{"Action":"pass","Package":"example.com/foo","Elapsed":5.1}"#;

        let result = filter_go_test_json(output);

        assert!(result.starts_with("Go test: 2 passed, 1 fuzz in 1 packages"));
        assert!(result.contains("execs: 200000"));
        assert!(!result.contains("execs: 100000"));
    }

    #[test]
    fn test_filter_go_test_failing_fuzz_reports_failure_not_fuzz_summary() {
        let output = r#"{"Action":"start","Package":"example.com/foo"}
{"Action":"run","Package":"example.com/foo","Test":"FuzzRev"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"=== RUN   FuzzRev\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"fuzz: elapsed: 0s, gathering baseline coverage: 0/3 completed\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"fuzz: elapsed: 0s, execs: 1234 (12345/sec), new interesting: 1 (total: 4)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"--- FAIL: FuzzRev (0.11s)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"    --- FAIL: FuzzRev (0.00s)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"        rev_test.go:17: Reverse produced invalid UTF-8 string \"\\x9c\"\n"}
{"Action":"fail","Package":"example.com/foo","Test":"FuzzRev","Elapsed":0.11}
{"Action":"output","Package":"example.com/foo","Output":"FAIL\n"}
{"Action":"fail","Package":"example.com/foo","Elapsed":0.12}"#;

        let result = filter_go_test_json(output);

        assert!(
            result.starts_with("Go test: 0 passed, 1 failed"),
            "got: {}",
            result
        );
        assert!(
            result.contains("Reverse produced invalid UTF-8"),
            "got: {}",
            result
        );
        assert!(!result.contains("fuzz in"), "got: {}", result);
    }

    #[test]
    fn test_filter_go_test_fuzz_package_failure_reports_failure_not_fuzz_summary() {
        // TestMain exits non-zero after a green fuzz run: no test-level `fail`, only the package one
        let output = r#"{"Action":"start","Package":"example.com/foo"}
{"Action":"run","Package":"example.com/foo","Test":"FuzzRev"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"fuzz: elapsed: 3s, execs: 199077 (66340/sec), new interesting: 0 (total: 59)\n"}
{"Action":"output","Package":"example.com/foo","Test":"FuzzRev","Output":"fuzz: elapsed: 4s, execs: 199077 (0/sec), new interesting: 0 (total: 59)\n"}
{"Action":"pass","Package":"example.com/foo","Test":"FuzzRev","Elapsed":4.0}
{"Action":"output","Package":"example.com/foo","Output":"PASS\n"}
{"Action":"output","Package":"example.com/foo","Output":"teardown failed\n"}
{"Action":"output","Package":"example.com/foo","Output":"exit status 3\n"}
{"Action":"output","Package":"example.com/foo","Output":"FAIL\texample.com/foo\t4.052s\n"}
{"Action":"fail","Package":"example.com/foo","Elapsed":4.05}"#;

        let result = filter_go_test_json(output);

        assert!(
            result.starts_with("Go test: 1 passed, 1 failed in 1 packages"),
            "got: {}",
            result
        );
        assert!(result.contains("foo [FAIL]"), "got: {}", result);
        assert!(result.contains("teardown failed"), "got: {}", result);
        assert!(!result.contains("fuzz in"), "got: {}", result);
    }
}
