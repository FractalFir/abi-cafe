mod cli;
mod error;
mod files;
mod fivemat;
mod harness;
mod log;
mod toolchains;

use error::*;
use files::Paths;
use harness::report::*;
use harness::test::*;
use harness::vals::*;
use harness::*;
use indexmap::IndexMap;
use toolchains::*;

use kdl_script::parse::LangRepr;
use std::error::Error;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::{debug, error, info};

pub type SortedMap<K, V> = std::collections::BTreeMap<K, V>;

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Human,
    Json,
    RustcJson,
}
impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = match self {
            OutputFormat::Human => "human",
            OutputFormat::Json => "json",
            OutputFormat::RustcJson => "rustc-json",
        };
        string.fmt(f)
    }
}
impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let val = match s {
            "human" => OutputFormat::Human,
            "json" => OutputFormat::Json,
            "rustc-json" => OutputFormat::RustcJson,
            _ => return Err(format!("unknown output format: {s}")),
        };
        Ok(val)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub output_format: OutputFormat,
    pub run_conventions: Vec<CallingConvention>,
    pub run_reprs: Vec<LangRepr>,
    pub run_toolchains: Vec<String>,
    pub run_pairs: Vec<(String, String)>,
    pub run_tests: Vec<String>,
    pub run_values: Vec<ValueGeneratorKind>,
    pub run_writers: Vec<WriteImpl>,
    pub run_selections: Vec<FunctionSelector>,
    pub minimizing_write_impl: WriteImpl,
    pub rustc_codegen_backends: Vec<(String, String)>,
    pub disable_builtin_tests: bool,
    pub disable_builtin_rules: bool,
    pub paths: Paths,
    pub debug: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("some tests failed")]
pub struct TestsFailed {}

fn main() -> Result<(), Box<dyn Error>> {
    let cfg = cli::make_app();
    debug!("parsed cli!");
    cfg.paths.init_dirs()?;

    let rt = tokio::runtime::Runtime::new().expect("failed to init tokio runtime");
    let _handle = rt.enter();

    // Grab all the tests
    let test_rules = harness::find_test_rules(&cfg)?;
    let test_sources = harness::find_tests(&cfg)?;
    let read_tasks = test_sources
        .into_iter()
        .map(|(test, test_file)| harness::spawn_read_test(&rt, test, test_file));

    // We could async pipeline this harder but it's nice to know all the tests upfront
    // Also we want it to be a hard error for any test to fail to load, as this indicates
    // an abi-cafe developer error
    let mut tests = SortedMap::new();
    let mut test_read_fails = false;
    for task in read_tasks {
        let res = rt.block_on(task).expect("failed to join on task");
        match res {
            Ok(test) => {
                tests.insert(test.name.clone(), test);
            }
            Err(e) => {
                test_read_fails = true;
                error!("{:?}", miette::Report::new(e));
            }
        }
    }
    if test_read_fails {
        Err(TestsFailed {})?;
    }
    debug!("loaded tests!");

    let harness = Arc::new(TestHarness::new(test_rules, tests, &cfg));
    debug!("initialized test harness!");

    // Run the tests
    let mut tasks = vec![];

    // The cruel bastard that is combinatorics... THE GOD LOOPS
    for test in harness.all_tests() {
        if !cfg.run_tests.is_empty() && !cfg.run_tests.contains(&test.name) {
            continue;
        }
        for &convention in &cfg.run_conventions {
            if !test.has_convention(convention) {
                continue;
            }
            for (caller_id, callee_id) in &cfg.run_pairs {
                if !cfg.run_toolchains.is_empty()
                    && !cfg.run_toolchains.iter().any(|x| x == caller_id)
                    && !cfg.run_toolchains.iter().any(|x| &**x == callee_id)
                {
                    continue;
                }
                for &repr in &cfg.run_reprs {
                    for &val_generator in &cfg.run_values {
                        for &val_writer in &cfg.run_writers {
                            for functions in &cfg.run_selections {
                                // Run the test!
                                let test_key = TestKey {
                                    test: test.name.to_owned(),
                                    caller: caller_id.to_owned(),
                                    callee: callee_id.to_owned(),
                                    options: TestOptions {
                                        convention,
                                        repr,
                                        val_writer,
                                        val_generator,
                                        functions: functions.clone(),
                                    },
                                };
                                let rules = harness.get_test_rules(&test_key);
                                let task = harness.clone().spawn_test(&rt, rules, test_key.clone());

                                tasks.push(task);
                            }
                        }
                    }
                }
            }
        }
    }
    // Join on all the tasks, and compute their results
    let reports = tasks
        .into_iter()
        .map(|task| {
            let results = rt.block_on(task).expect("failed to join task");
            report_test(results)
        })
        .collect::<Vec<_>>();

    // Compute the final report
    let mut full_report = compute_final_report(&cfg, &harness, reports);

    if full_report.failed() {
        generate_minimized_failures(&cfg, &harness, &rt, &mut full_report);
    }

    let mut output = std::io::stdout();
    match cfg.output_format {
        OutputFormat::Human => full_report.print_human(&harness, &mut output)?,
        OutputFormat::Json => full_report.print_json(&harness, &mut output)?,
        OutputFormat::RustcJson => full_report.print_rustc_json(&harness, &mut output)?,
    }

    if full_report.failed() {
        Err(TestsFailed {})?;
    }
    Ok(())
}

fn compute_final_report(
    _cfg: &Config,
    harness: &Arc<TestHarness>,
    reports: Vec<TestReport>,
) -> FullReport {
    use TestConclusion::*;

    let mut expects = IndexMap::<TestKeyPattern, TestRulesPattern>::new();

    let mut num_tests = 0;
    let mut num_passed = 0;
    let mut num_busted = 0;
    let mut num_failed = 0;
    let mut num_skipped = 0;
    for report in &reports {
        num_tests += 1;
        match report.conclusion {
            Busted => num_busted += 1,
            Skipped => num_skipped += 1,
            Passed => num_passed += 1,
            Failed => {
                num_failed += 1;
                let pattern = harness.base_id(&report.key, None, "::");
                if let Ok(pattern) = pattern.parse() {
                    expects.insert(pattern, report.could_be.clone());
                }
            }
        }
    }

    let possible_rules = if expects.is_empty() {
        None
    } else {
        Some(ExpectFile {
            target: IndexMap::from_iter([(
                harness.toolchains.platform_info.target.clone(),
                expects,
            )]),
        })
    };

    FullReport {
        summary: TestSummary {
            num_tests,
            num_passed,
            num_busted,
            num_failed,
            num_skipped,
        },
        possible_rules,
        tests: reports,
    }
}

fn generate_minimized_failures(
    cfg: &Config,
    harness: &Arc<TestHarness>,
    rt: &tokio::runtime::Runtime,
    reports: &mut FullReport,
) {
    info!("minimizing failures...");
    let mut tasks = vec![];
    for (test_idx, report) in reports.tests.iter().enumerate() {
        let Some(check) = report.results.check.as_ref() else {
            continue;
        };
        // FIXME: certainly classes of run failure could also be minimized,
        // because we have information indicating there was an error in a specific func!
        for (subtest_idx, subtest) in check.subtest_checks.iter().enumerate() {
            let Err(failure) = &subtest.result else {
                continue;
            };

            let functions = match *failure {
                CheckFailure::ValMismatch {
                    func_idx,
                    arg_idx,
                    val_idx,
                    ..
                }
                | CheckFailure::TagMismatch {
                    func_idx,
                    arg_idx,
                    val_idx,
                    ..
                } => FunctionSelector::One {
                    idx: func_idx,
                    args: ArgSelector::One {
                        idx: arg_idx,
                        vals: ValSelector::One { idx: val_idx },
                    },
                },
            };

            let mut test_key = report.key.clone();
            test_key.options.functions = functions;
            test_key.options.val_writer = cfg.minimizing_write_impl;
            let mut rules = report.rules;
            rules.run = TestRunMode::Generate;

            let task = harness.clone().spawn_test(rt, rules, test_key);
            tasks.push((test_idx, subtest_idx, task));
        }
    }

    for (test_idx, subtest_idx, task) in tasks {
        let results = rt.block_on(task).expect("failed to join task");
        reports.tests[test_idx]
            .results
            .check
            .as_mut()
            .unwrap()
            .subtest_checks[subtest_idx]
            .minimized = results.source.and_then(|r| r.ok());
    }
}
