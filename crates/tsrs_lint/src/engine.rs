//! Program construction and headless lint execution.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};

use tsrs_ast::{Diagnostic as TsDiagnostic, SourceFile};
use tsrs_compiler::{Context, Program, ProgramOptions};
use tsrs_contentmapper as contentmapper;
use tsrs_core::json::Value;
use tsrs_core::tspath;
use tsrs_core::{
    CompilerOptions, JsxEmit, Locale, ModuleKind, ModuleResolutionKind, P, ScriptTarget, Tristate,
};
use tsrs_spanmap::{Kind as SegmentKind, Segment};
use tsrs_tsoptions::{ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::{FS, bundled, cachedvfs, osvfs, wrapvfs};

use crate::payload::{ConfiguredRule, Payload};
use crate::protocol::{self, Diagnostic, MessageType, Range, RuleMessage};
use crate::rule::{RuleContext, visit_lint_nodes};

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub fix: bool,
    pub fix_suggestions: bool,
    pub debug_timings: bool,
    pub suppress_program_diagnostics: bool,
    /// tsc's `--runExternalCode`: tsconfig `contentMappers` are kept and their mapper processes run.
    pub run_external_code: bool,
}

#[derive(Default)]
struct RequestRegions {
    loaders: Arc<tsrs_compiler::ProgramRegions>,
}

pub struct ConfigHost {
    fs: Arc<dyn FS>,
    cwd: String,
    // The command-line options every tsconfig is parsed with.
    existing_options: Option<CompilerOptions>,
}

impl ConfigHost {
    pub fn new(fs: Arc<dyn FS>, cwd: String) -> Self {
        Self { fs, cwd, existing_options: None }
    }
}

impl ParseConfigHost for ConfigHost {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }
    fn get_current_directory(&self) -> &str {
        &self.cwd
    }
}

pub fn run(
    payload: &Payload,
    options: Options,
    cwd: &str,
    writer: &mut dyn Write,
) -> Result<(), String> {
    let overrides = Arc::new(payload.source_overrides.clone());
    let read_overrides = Arc::clone(&overrides);
    let directory_overrides = Arc::clone(&overrides);
    let entries_overrides = Arc::clone(&overrides);
    let stat_overrides = Arc::clone(&overrides);
    let realpath_overrides = Arc::clone(&overrides);
    let read_cache = tsrs_core::collections::SyncMap::<String, Option<String>>::default();
    let fs = wrapvfs::wrap(
        osvfs::fs(),
        wrapvfs::Replacements {
            file_exists: Some(Box::new(move |path| {
                overrides.contains_key(path) || osvfs::fs().file_exists(path)
            })),
            read_file: Some(Box::new(move |path| {
                if let Some(source) = read_overrides.get(path) {
                    return Some(source.clone());
                }
                if let Some(source) = read_cache.load(path) {
                    return source;
                }
                let source = osvfs::fs().read_file(path);
                read_cache.store(path.to_owned(), source.clone());
                source
            })),
            directory_exists: Some(Box::new(move |path| {
                if !directory_overrides.is_empty() {
                    let prefix = format!("{}/", tspath::normalize_path(path).trim_end_matches('/'));
                    if directory_overrides.keys().any(|file| file.starts_with(&prefix)) {
                        return true;
                    }
                }
                osvfs::fs().directory_exists(path)
            })),
            get_accessible_entries: Some(Box::new(move |path| {
                let mut entries = osvfs::fs().get_accessible_entries(path);
                if !entries_overrides.is_empty() {
                    let prefix = format!("{}/", tspath::normalize_path(path).trim_end_matches('/'));
                    for file in entries_overrides.keys() {
                        if let Some(relative) = file.strip_prefix(&prefix) {
                            if let Some((directory, _)) = relative.split_once('/') {
                                entries.directories.push(directory.into());
                            } else {
                                entries.files.push(relative.into());
                            }
                        }
                    }
                    entries.files.sort_unstable();
                    entries.files.dedup();
                    entries.directories.sort_unstable();
                    entries.directories.dedup();
                }
                entries
            })),
            stat: Some(Box::new(move |path| {
                stat_overrides.get(path).map_or_else(
                    || osvfs::fs().stat(path),
                    |source| {
                        Some(tsrs_vfs::FileInfo {
                            name: path.into(),
                            size: i64::try_from(source.len()).unwrap(),
                            mode: tsrs_vfs::FileMode::None,
                            mod_time: None,
                        })
                    },
                )
            })),
            realpath: Some(Box::new(move |path| {
                if realpath_overrides.contains_key(path) {
                    path.into()
                } else {
                    osvfs::fs().realpath(path)
                }
            })),
            ..Default::default()
        },
    );
    let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(cachedvfs::from(Arc::new(fs))));
    let host = ConfigHost {
        existing_options: options
            .run_external_code
            .then(|| CompilerOptions { run_external_code: Tristate::True, ..Default::default() }),
        ..ConfigHost::new(Arc::clone(&fs), cwd.into())
    };
    let region = tsrs_core::arena::Region::new_scratch(1 << 20);
    // Shared caches can receive loader allocations, so the loader regions outlive every request program.
    let regions = RequestRegions::default();
    let _scope = region.enter();
    run_in_region(payload, options, cwd, writer, fs, &host, &regions)
}

struct ProgramOwner {
    program: &'static Program,
    // Dropped after the program is freed.
    _content_mappers: Option<ContentMappers>,
}

impl Drop for ProgramOwner {
    fn drop(&mut self) {
        // SAFETY: each program is created once here, is never shared with another version, and all checker handles have been dropped.
        unsafe {
            tsrs_compiler::free_unshared_program(self.program);
        }
    }
}

// A program's content mapper project and host, closed in that order as the CLI's `ContentMapperCloser` does: a host
// that is still open keeps its mapper processes running.
struct ContentMappers {
    host: Arc<dyn contentmapper::Host>,
    project: Option<Arc<dyn contentmapper::Project>>,
}

impl ContentMappers {
    // The CLI's `new_content_mapper_host` and `get_content_mapper_project` (tsc.go:394), without a host for a
    // program that has no mappers.
    fn open(config: P<ParsedCommandLine>) -> Option<Self> {
        let options = config.compiler_options()?;
        // References into the command line's own list: the host keys the project by their addresses.
        let config: &'static ParsedCommandLine = config.get();
        if !options.run_external_code.is_true() || config.content_mappers().is_empty() {
            return None;
        }
        let host = contentmapper::new_host_with_options(
            Arc::new(contentmapper::ProcessSpawner),
            Locale(options.locale.clone()),
            contentmapper::HostOptions::default(),
        );
        let project = host.project(contentmapper::ProjectSpec {
            config_file_name: config.config_name().to_string(),
            mappers: config.content_mappers().iter().collect(),
            compiler_options: Some(options),
        });
        Some(Self { host, project })
    }
}

impl Drop for ContentMappers {
    fn drop(&mut self) {
        if let Some(project) = &self.project {
            let _ = project.close();
        }
        let _ = self.host.close();
    }
}

fn run_in_region(
    payload: &Payload,
    options: Options,
    cwd: &str,
    writer: &mut dyn Write,
    fs: Arc<dyn FS>,
    host: &ConfigHost,
    regions: &RequestRegions,
) -> Result<(), String> {
    let mut programs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unmatched = Vec::new();
    let mut file_configs = BTreeMap::new();
    let mut timings: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for config in &payload.configs {
        for path in &config.file_paths {
            file_configs.insert(tspath::normalize_slashes(path), config.rules.as_slice());
        }
    }
    for rules in file_configs.values() {
        for rule in *rules {
            crate::registry::require_implemented(&rule.name)?;
        }
    }
    // tsrs-only: semantic reporting (oxlint `--type-check`) reports what `tsrs --noEmit` does for each tsconfig program:
    // every file it checks and the program's global diagnostics, where tsgolint reports only the files oxlint sent. One
    // `oxlint --type-aware --type-check` run can then replace a separate typecheck. Files oxlint sent that no tsconfig
    // includes are checked in an inferred program. Each file's type errors are reported once: a listed file by the
    // program it maps to, any other file by the first program that has it.
    let mut type_checked = FxHashSet::default();
    let mut config_cache = FxHashMap::default();
    for path in file_configs.keys() {
        let resolved = tspath::get_normalized_absolute_path(path, cwd);
        if payload.report_semantic {
            type_checked.insert(tspath::to_path(
                &resolved,
                cwd,
                fs.use_case_sensitive_file_names(),
            ));
        }
        if let Some(config_path) = find_tsconfig(&resolved, host, &mut config_cache) {
            programs.entry(config_path).or_default().push(path.clone());
        } else if has_script_extension(path) {
            unmatched.push(path.clone());
        }
        // A file in a foreign format (such as `.vue`) that no tsconfig includes has no content mapper to transform it:
        // the inferred program would parse it as TypeScript, so it is not linted.
    }
    drop(config_cache);
    'programs: for (config_path, files) in programs {
        // A content mapper that fails at run time (it does not start, or it fails to transform too many files) is
        // reported, and the program is built again without content mappers, as without `--runExternalCode`. The
        // compiler stops using a mapper after its fifth failed file, so the files that were loaded before that point
        // depend on the order of loading: linting that program would give different reports in each run.
        let mut without_mappers = false;
        let (program, mut content_mappers_disabled, disabled_extensions) = loop {
            let existing_options =
                if without_mappers { None } else { host.existing_options.as_ref() };
            let (config, diagnostics) = tsrs_tsoptions::get_parsed_command_line_of_config_file(
                &config_path,
                existing_options,
                None,
                host,
                None,
            );
            let mut content_mappers_disabled = false;
            let diagnostics =
                without_run_external_code_error(diagnostics, &mut content_mappers_disabled);
            if !diagnostics.is_empty() {
                for diagnostic in diagnostics {
                    report_config_error(diagnostic, &config_path, false, writer)?;
                }
                continue 'programs;
            }
            let mut config =
                config.ok_or_else(|| format!("couldn't read tsconfig at {config_path}"))?;
            config.errors = without_run_external_code_error(
                std::mem::take(&mut config.errors),
                &mut content_mappers_disabled,
            );
            if content_mappers_disabled && payload.report_notices && !without_mappers {
                report_content_mappers_disabled(&config_path, &config, writer)?;
            }
            let disabled_extensions = if content_mappers_disabled {
                content_mapper_extensions(&config)
            } else {
                Vec::new()
            };
            if !config.errors.is_empty() && !options.suppress_program_diagnostics {
                for diagnostic in &config.errors {
                    report_config_error(*diagnostic, &config_path, false, writer)?;
                }
                continue 'programs;
            }
            let program = create_program(
                config,
                Arc::clone(&fs),
                &tspath::get_directory_path(&config_path),
                true,
                Arc::clone(&regions.loaders),
            );
            if without_mappers {
                break (program, content_mappers_disabled, disabled_extensions);
            }
            let mapper_failures: Vec<_> = program
                .program
                .get_program_diagnostics()
                .into_iter()
                .filter(|diagnostic| is_content_mapper_failure(*diagnostic))
                .collect();
            if mapper_failures.is_empty() {
                break (program, content_mappers_disabled, disabled_extensions);
            }
            if !options.suppress_program_diagnostics {
                for diagnostic in mapper_failures {
                    report_config_error(diagnostic, &config_path, true, writer)?;
                }
            }
            drop(program);
            without_mappers = true;
        };
        let program_ref = program.program;
        let diagnostics = without_unmapped_file_errors(
            without_run_external_code_error(
                program_ref.get_program_diagnostics(),
                &mut content_mappers_disabled,
            ),
            &disabled_extensions,
        );
        if !diagnostics.is_empty() && !options.suppress_program_diagnostics {
            for diagnostic in diagnostics {
                report_config_error(diagnostic, &config_path, true, writer)?;
            }
            continue;
        }
        run_program(
            program_ref,
            &files,
            payload.report_semantic.then_some(&mut type_checked),
            payload,
            options,
            writer,
            &file_configs,
            &mut timings,
        )?;
        if payload.report_semantic {
            for diagnostic in program_ref.get_global_diagnostics(&Context::default()) {
                report_global_error(diagnostic, &config_path, writer)?;
            }
        }
    }
    if !unmatched.is_empty() {
        let config = tsrs_tsoptions::new_parsed_command_line(
            P::new(CompilerOptions {
                allow_js: Tristate::True,
                module: ModuleKind::ES_NEXT,
                module_resolution: ModuleResolutionKind::BUNDLER,
                target: ScriptTarget::ES2022,
                jsx: JsxEmit::REACT_JSX,
                allow_importing_ts_extensions: Tristate::True,
                strict_null_checks: Tristate::True,
                strict_function_types: Tristate::True,
                source_map: Tristate::True,
                es_module_interop: Tristate::True,
                allow_non_ts_extensions: Tristate::True,
                resolve_json_module: Tristate::True,
                ..Default::default()
            }),
            unmatched.clone(),
            Vec::new(),
            tspath::ComparePathsOptions {
                current_directory: cwd.into(),
                use_case_sensitive_file_names: fs.use_case_sensitive_file_names(),
            },
        );
        let program = create_program(config, fs, cwd, false, Arc::clone(&regions.loaders));
        run_program(
            program.program,
            &unmatched,
            None,
            payload,
            options,
            writer,
            &file_configs,
            &mut timings,
        )?;
    }
    if options.debug_timings {
        let mut records = timings.into_iter().collect::<Vec<_>>();
        records.sort_by(|(left_name, (left_duration, _)), (right_name, (right_duration, _))| {
            right_duration.cmp(left_duration).then_with(|| left_name.cmp(right_name))
        });
        protocol::write_message(
            writer,
            MessageType::Timing,
            &protocol::object([(
                "rules",
                Value::Array(
                    records
                        .into_iter()
                        .map(|(name, (duration, calls))| {
                            protocol::object([
                                ("rule_name", Value::String(name)),
                                ("duration", Value::Number(duration as f64)),
                                ("calls", Value::Number(calls as f64)),
                            ])
                        })
                        .collect(),
                ),
            )]),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn create_program(
    mut config: ParsedCommandLine,
    fs: Arc<dyn FS>,
    cwd: &str,
    references: bool,
    loader_regions: Arc<tsrs_compiler::ProgramRegions>,
) -> ProgramOwner {
    if let Some(checkers) = checker_count() {
        config.parsed_config.compiler_options = Some(P::new(CompilerOptions {
            checkers: Some(checkers),
            ..(*config.compiler_options().unwrap()).clone()
        }));
    }
    let config = P::new(config);
    let content_mappers = ContentMappers::open(config);
    // As in the CLI, `TSRS_LIB_PATH` overrides where the default libraries are read from.
    let host = tsrs_compiler::new_compiler_host(
        cwd,
        fs,
        &bundled::default_lib_path(),
        None,
        None,
        content_mappers.as_ref().and_then(|mappers| mappers.project.clone()),
    );
    let mut options = ProgramOptions::new(config, host);
    options.use_source_of_project_reference = references;
    options.regions = Some(loader_regions);
    // The checkers' memory goes back with the program (`free_unshared_program`), before the next program's is built.
    options.checker_regions = true;
    ProgramOwner { program: tsrs_compiler::new_program(options), _content_mappers: content_mappers }
}

/// `RAYON_NUM_THREADS` (oxlint's thread count) caps the checkers as well as the loader threads, else the compiler's
/// default for the machine and program applies.
fn checker_count() -> Option<i64> {
    std::env::var("RAYON_NUM_THREADS").ok().and_then(|value| value.parse().ok()).filter(|&n| n > 0)
}

// Lints the files on the program's checker pool: one thread per checker, each over the files the pool assigned to
// it (by import-graph locality under the fast preset), so a checker resolves the types of its own corner of the
// program instead of nearly all of it. Output keeps the order of `files`.
// `whole` (with semantic reporting): the files that already have a program to report their type errors. Also
// report the compiler diagnostics of the program's files outside that set, after `files`, and add them to it.
fn run_program(
    program: &'static Program,
    files: &[String],
    whole: Option<&mut FxHashSet<tspath::Path>>,
    payload: &Payload,
    options: Options,
    writer: &mut dyn Write,
    file_configs: &BTreeMap<String, &[ConfiguredRule]>,
    timings: &mut BTreeMap<String, (u64, u64)>,
) -> Result<(), String> {
    program.bind_source_files();
    let mut whole = whole;
    let mut work = Vec::with_capacity(files.len());
    for name in files {
        let Some(file) = program.get_source_file(name) else {
            // A file in a foreign format that no content mapper of this program transforms: nothing to lint.
            if !has_script_extension(name) {
                continue;
            }
            return Err(format!("Expected file '{name}' to be in program"));
        };
        let configured = file_configs.get(name).copied().unwrap_or_default();
        if payload.report_syntactic || payload.report_semantic || !configured.is_empty() {
            work.push((file, configured));
            // The other outputs of a content-mapped file (such as each `<script>` of an Astro component) are linted
            // with the same rules and reported on the original file.
            for &supplemental in file.supplemental_source_files() {
                if let Some(type_checked) = whole.as_deref_mut() {
                    type_checked.insert(supplemental.path().clone());
                }
                work.push((supplemental, configured));
            }
        }
    }
    if let Some(type_checked) = whole {
        for &file in program.get_source_files() {
            if !program.skip_type_checking(file, false) && type_checked.insert(file.path().clone())
            {
                work.push((file, &[]));
            }
        }
    }
    if work.is_empty() {
        return Ok(());
    }
    let lint_files: Vec<P<SourceFile>> = work.iter().map(|&(file, _)| file).collect();
    let outputs: Vec<Mutex<Vec<u8>>> = work.iter().map(|_| Mutex::new(Vec::new())).collect();
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let pool_timings: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());
    let pooled = program.for_each_checker_group(&lint_files, |checker, index, file| {
        let mut output = Vec::new();
        let mut file_timings = BTreeMap::new();
        let result = lint_one(
            program,
            file,
            work[index].1,
            payload,
            options,
            checker,
            &mut output,
            &mut file_timings,
        );
        if let Err(error) = result {
            errors.lock().unwrap().push(error);
        }
        *outputs[index].lock().unwrap() = output;
        if options.debug_timings {
            merge_timings(&mut pool_timings.lock().unwrap(), file_timings);
        }
    });
    if !pooled {
        return Err("the program has no checker pool".into());
    }
    if let Some(error) = errors.into_inner().unwrap().into_iter().next() {
        return Err(error);
    }
    for output in outputs {
        writer.write_all(&output.into_inner().unwrap()).map_err(|error| error.to_string())?;
    }
    merge_timings(timings, pool_timings.into_inner().unwrap());
    Ok(())
}

fn lint_one(
    program: &'static Program,
    file: P<SourceFile>,
    configured: &[ConfiguredRule],
    payload: &Payload,
    options: Options,
    checker: &mut tsrs_checker::Checker,
    output: &mut Vec<u8>,
    timings: &mut BTreeMap<String, (u64, u64)>,
) -> Result<(), String> {
    let context = Context::default();
    let mut unmapped = Vec::new();
    if payload.report_syntactic {
        for diagnostic in program.get_syntactic_diagnostics(&context, Some(file)) {
            report_type_error(diagnostic, file, output, &mut unmapped)?;
        }
    }
    if payload.report_semantic {
        let diagnostics = program.get_semantic_diagnostics_with_checker(&context, checker, file);
        for diagnostic in tsrs_compiler::sort_and_deduplicate_diagnostics(&diagnostics) {
            report_type_error(diagnostic, file, output, &mut unmapped)?;
        }
    }
    report_unmapped_type_errors(file, &unmapped, output)?;
    if configured.is_empty() {
        return Ok(());
    }
    if !file.is_content_mapped() {
        for diagnostic in lint_file(program, file, configured, options, checker, timings)? {
            emit(&diagnostic, output)?;
        }
        return Ok(());
    }
    // Rules see a content-mapped file's virtual text. A report is kept only when both its start and its end map into
    // the original file, so reports on code that only the mapper wrote are dropped.
    let Some(span_map) = file.span_map() else {
        // The empty placeholder of a failed transform: the mapper's error is already reported.
        return Ok(());
    };
    let segments = span_map.segments();
    let debug_mapping = std::env::var_os("TSRS_LINT_DEBUG_MAPPING").is_some();
    for diagnostic in lint_file(program, file, configured, options, checker, timings)? {
        if debug_mapping {
            print_mapping(&diagnostic, file.text(), &segments);
        }
        if let Some(diagnostic) = to_original_file(diagnostic, &segments, file) {
            emit(&diagnostic, output)?;
        }
    }
    Ok(())
}

/// `TSRS_LINT_DEBUG_MAPPING`: prints each rule report on a content-mapped file with its virtual text and the segments
/// that it touches to stderr, to find out why a report was kept or dropped.
fn print_mapping(diagnostic: &Diagnostic, text: &str, segments: &[Segment]) {
    let Some(range) = diagnostic.range else {
        return;
    };
    let touched: Vec<_> = segments
        .iter()
        .filter(|s| s.virtual_start <= range.end && s.virtual_end >= range.pos)
        .map(|s| (s.virtual_start, s.virtual_end, s.original_start, s.original_end, s.kind.0))
        .collect();
    eprintln!(
        "tsrs-lint mapping: {} virtual [{}, {}) {:?} segments (virtual start, end, original start, end, kind) {touched:?}",
        diagnostic.rule.as_deref().unwrap_or_default(),
        range.pos,
        range.end,
        text.get(range.pos as usize..range.end as usize).unwrap_or_default(),
    );
}

/// The script extensions that a program reads without a content mapper.
fn has_script_extension(path: &str) -> bool {
    const EXTENSIONS: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];
    let lower = path.to_ascii_lowercase();
    EXTENSIONS.iter().any(|extension| lower.ends_with(extension))
}

/// Without `--runExternalCode`, TypeScript drops the `contentMappers` of a tsconfig and reports TS18068. The program
/// without the mappers is the program that oxlint lints when it does not run external code, so this error does not
/// stop the program. oxlint shows a notice instead (`report_content_mappers_disabled`).
fn without_run_external_code_error(
    diagnostics: Vec<P<TsDiagnostic>>,
    found: &mut bool,
) -> Vec<P<TsDiagnostic>> {
    let code = tsrs_diagnostics::CONTENT_MAPPERS_REQUIRE_THE_RUN_EXTERNAL_CODE_COMMAND_LINE_FLAG_TO_BE_ENABLED
        .code();
    let before = diagnostics.len();
    let diagnostics: Vec<_> = diagnostics.into_iter().filter(|d| d.code() != code).collect();
    *found |= diagnostics.len() != before;
    diagnostics
}

/// Without its content mappers, the compiler reports TS6054 (unsupported extension) for each file of a mapper extension
/// that `files` lists. Such a file is skipped, as a file of that extension that `include` matches, and the rest of the
/// program is linted.
fn without_unmapped_file_errors(
    diagnostics: Vec<P<TsDiagnostic>>,
    extensions: &[String],
) -> Vec<P<TsDiagnostic>> {
    if extensions.is_empty() {
        return diagnostics;
    }
    let code =
        tsrs_diagnostics::FILE_0_HAS_AN_UNSUPPORTED_EXTENSION_THE_ONLY_SUPPORTED_EXTENSIONS_ARE_1
            .code();
    diagnostics
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.code() != code
                || !diagnostic.message_args().first().is_some_and(|file| {
                    extensions.iter().any(|extension| file.ends_with(extension.as_str()))
                })
        })
        .collect()
}

/// Whether a program diagnostic reports a content mapper that failed at run time: it could not be initialized, or it
/// failed to transform too many files and is no longer used.
fn is_content_mapper_failure(diagnostic: P<TsDiagnostic>) -> bool {
    let code = diagnostic.code();
    code == tsrs_diagnostics::THE_CONTENT_MAPPER_0_FAILED_1_TIMES_AND_WILL_NOT_BE_USED.code()
        || code == tsrs_diagnostics::THE_CONTENT_MAPPER_0_COULD_NOT_BE_INITIALIZED.code()
}

/// The file extensions of the content mappers that a tsconfig lists, in order, without duplicates.
fn content_mapper_extensions(config: &ParsedCommandLine) -> Vec<String> {
    let mut extensions = Vec::new();
    if let Some(mappers) = config.raw.as_object().and_then(|raw| raw.get("contentMappers"))
        && let Some(mappers) = mappers.as_array()
    {
        for mapper in mappers {
            if let Some(list) = mapper
                .as_object()
                .and_then(|mapper| mapper.get("extensions"))
                .and_then(|list| list.as_array())
            {
                for extension in list {
                    if let Some(extension) = extension.as_str()
                        && !extensions.iter().any(|e| e == extension)
                    {
                        extensions.push(extension.to_owned());
                    }
                }
            }
        }
    }
    extensions
}

fn report_content_mappers_disabled(
    config_path: &str,
    config: &ParsedCommandLine,
    writer: &mut dyn Write,
) -> Result<(), String> {
    let extensions = content_mapper_extensions(config);
    protocol::write_message(
        writer,
        MessageType::Notice,
        &protocol::object([
            ("id", Value::String("content-mappers-require-run-external-code".into())),
            ("tsconfig", Value::String(config_path.into())),
            ("extensions", Value::Array(extensions.into_iter().map(Value::String).collect())),
        ]),
    )
    .map_err(|e| e.to_string())
}

/// Moves a rule report on a content-mapped file to the original file, or drops it when it is on code that the mapper
/// generated. The rule is Volar's for diagnostics (`SourceMap.findMatchingStartEnd` with `fallbackToAnyMatch`): both
/// the start and the end must map into the original text, preferably through the same segment. In addition, a
/// non-empty range must cover some mapped text: a range between two segments maps through their touching ends, but it
/// covers only generated code. A label that does not map is dropped, and the report is kept. Fixes and suggestions are
/// dropped: their edits are on the virtual text.
fn to_original_file(
    mut diagnostic: Diagnostic,
    segments: &[Segment],
    file: P<SourceFile>,
) -> Option<Diagnostic> {
    diagnostic.range = Some(original_lint_range(segments, diagnostic.range?)?);
    diagnostic.labeled_ranges.retain_mut(|label| {
        original_lint_range(segments, label.range).is_some_and(|range| {
            label.range = range;
            true
        })
    });
    diagnostic.fixes.clear();
    diagnostic.suggestions.clear();
    diagnostic.file_path = Some(file.original_file_name().into());
    Some(diagnostic)
}

fn original_lint_range(segments: &[Segment], range: Range) -> Option<Range> {
    // The compiler accepts only sorted segments that do not overlap, so they are also sorted by their ends.
    if range.end > range.pos
        && !segments[segments.partition_point(|s| s.virtual_end <= range.pos)..]
            .iter()
            .take_while(|s| s.virtual_start < range.end)
            .any(|s| s.virtual_end > s.virtual_start)
    {
        return None;
    }
    for segment in segments_at(segments, range.pos) {
        if let (Some(pos), Some(end)) =
            (map_offset(segment, range.pos), map_offset(segment, range.end))
            && end >= pos
        {
            return Some(Range { pos, end });
        }
    }
    // A start that is also the end of the previous segment maps through the next one first, and an end that is also
    // the start of the next segment maps through the previous one first, as Volar prefers. As in Volar's
    // `fallbackToAnyMatch`, the next mapped start is tried when no mapped end comes after a start.
    segments_at(segments, range.pos).rev().filter_map(|s| map_offset(s, range.pos)).find_map(
        |pos| {
            segments_at(segments, range.end)
                .filter_map(|s| map_offset(s, range.end))
                .find(|&end| end >= pos)
                .map(|end| Range { pos, end })
        },
    )
}

/// The segments whose virtual range contains `offset`, ends included, in order.
fn segments_at(segments: &[Segment], offset: u32) -> impl DoubleEndedIterator<Item = &Segment> {
    let after = segments.partition_point(|segment| segment.virtual_start <= offset);
    let mut first = after;
    while first > 0 && segments[first - 1].virtual_end >= offset {
        first -= 1;
    }
    segments[first..after].iter()
}

/// Volar's `translateOffset`: a verbatim segment maps each position, and a segment whose virtual and original texts
/// differ (an atom or an alias) maps only its two ends. So a report inside generated code that the mapper attributes
/// as a whole to some original text, such as an event handler wrapper attributed to its tag name, does not map.
fn map_offset(segment: &Segment, offset: u32) -> Option<u32> {
    if offset < segment.virtual_start || offset > segment.virtual_end {
        return None;
    }
    if segment.kind == SegmentKind::VERBATIM {
        return Some(segment.original_start + (offset - segment.virtual_start));
    }
    if offset == segment.virtual_start {
        Some(segment.original_start)
    } else if offset == segment.virtual_end {
        Some(segment.original_end)
    } else {
        None
    }
}

fn merge_timings(into: &mut BTreeMap<String, (u64, u64)>, from: BTreeMap<String, (u64, u64)>) {
    for (name, (duration, calls)) in from {
        let entry = into.entry(name).or_default();
        entry.0 = entry.0.saturating_add(duration);
        entry.1 += calls;
    }
}

fn lint_file(
    program: &'static Program,
    file: P<SourceFile>,
    configured: &[ConfiguredRule],
    options: Options,
    checker: &mut tsrs_checker::Checker,
    timings: &mut BTreeMap<String, (u64, u64)>,
) -> Result<Vec<Diagnostic>, String> {
    let mut diagnostics = Vec::new();
    let mut rules = Vec::with_capacity(configured.len());
    for config in configured {
        let start = options.debug_timings.then(Instant::now);
        let mut rule = crate::registry::create_rule(config)?;
        rule.initialize(&mut RuleContext {
            source_file: file,
            program,
            checker,
            rule_name: &config.name,
            options,
            diagnostics: &mut diagnostics,
        });
        if let Some(start) = start {
            let entry = timings.entry(config.name.clone()).or_default();
            entry.0 = entry
                .0
                .saturating_add(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
            entry.1 += 1;
        }
        rules.push((config.name.as_str(), rule));
    }
    let mut listeners = FxHashMap::default();
    visit_lint_nodes(file, &mut |listener, node| {
        let active: &mut Vec<usize> = listeners.entry(listener).or_insert_with(|| {
            rules
                .iter()
                .enumerate()
                .filter_map(|(index, (_, rule))| rule.listens_to(listener).then_some(index))
                .collect()
        });
        for &index in active.iter() {
            let (name, rule) = &mut rules[index];
            let start = options.debug_timings.then(Instant::now);
            let mut context = RuleContext {
                source_file: file,
                program,
                checker,
                rule_name: name,
                options,
                diagnostics: &mut diagnostics,
            };
            rule.visit(listener, node, &mut context);
            if let Some(start) = start {
                let entry = timings.entry((*name).into()).or_default();
                entry.0 = entry
                    .0
                    .saturating_add(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
                entry.1 += 1;
            }
        }
    });
    Ok(diagnostics)
}

// `unmapped` collects the type errors of a content-mapped file that have no location in the original file.
fn report_type_error(
    diagnostic: P<TsDiagnostic>,
    file: P<SourceFile>,
    writer: &mut dyn Write,
    unmapped: &mut Vec<P<TsDiagnostic>>,
) -> Result<(), String> {
    if diagnostic.file().is_none_or(|f| f.file_name() != file.file_name()) {
        return Ok(());
    }
    let range = original_range(diagnostic, file);
    if range.is_none() && file.span_map().is_some() && diagnostic.loc().is_some() {
        unmapped.push(diagnostic);
        return Ok(());
    }
    emit(
        &Diagnostic {
            range,
            message: RuleMessage {
                id: format!("TS{}", diagnostic.code()),
                description: diagnostic.localize(),
                help: String::new(),
            },
            file_path: Some(file.original_file_name().into()),
            ..Default::default()
        },
        writer,
    )
}

// As TypeScript's language server does, the type errors in code that the mapper generated are reported as one
// diagnostic at the start of the original file. The help lists each error with its line of generated code, as the
// CLI's diagnostic writer shows that code.
fn report_unmapped_type_errors(
    file: P<SourceFile>,
    unmapped: &[P<TsDiagnostic>],
    writer: &mut dyn Write,
) -> Result<(), String> {
    let Some(&first) = unmapped.first() else {
        return Ok(());
    };
    let mapper = file.content_mapper();
    let mut help = format!(
        "This location is in code generated by the content mapper '{mapper}' and has no corresponding location in the original file."
    );
    let message = if let [diagnostic] = unmapped {
        let _ = write!(
            help,
            "\nGenerated code: `{}`",
            generated_line(file.text(), diagnostic.pos().unwrap_or_default())
        );
        RuleMessage { id: format!("TS{}", first.code()), description: first.localize(), help }
    } else {
        for diagnostic in unmapped {
            let _ = write!(
                help,
                "\nTS{}: {}\n  Generated code: `{}`",
                diagnostic.code(),
                diagnostic.localize(),
                generated_line(file.text(), diagnostic.pos().unwrap_or_default())
            );
        }
        RuleMessage {
            id: "generated-code".into(),
            description: format!(
                "{} type errors are in code that the content mapper '{mapper}' generated.",
                unmapped.len()
            ),
            help,
        }
    };
    emit(
        &Diagnostic {
            range: Some(Range { pos: 0, end: 0 }),
            message,
            file_path: Some(file.original_file_name().into()),
            ..Default::default()
        },
        writer,
    )
}

fn generated_line(text: &str, pos: u32) -> String {
    let mut pos = (pos as usize).min(text.len());
    while !text.is_char_boundary(pos) {
        pos -= 1;
    }
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i);
    let line = text[start..end].trim();
    if line.chars().count() > 120 {
        format!("{}...", line.chars().take(120).collect::<String>())
    } else {
        line.to_owned()
    }
}

// A diagnostic of the whole program, such as a missing global type, which `tsrs --noEmit` reports without a location:
// reported on the program's tsconfig, like a configuration error.
fn report_global_error(
    diagnostic: P<TsDiagnostic>,
    config_path: &str,
    writer: &mut dyn Write,
) -> Result<(), String> {
    emit(
        &Diagnostic {
            range: diagnostic_range(diagnostic),
            message: RuleMessage {
                id: format!("TS{}", diagnostic.code()),
                description: diagnostic.localize(),
                help: String::new(),
            },
            file_path: Some(
                diagnostic
                    .file()
                    .map_or_else(|| config_path.to_owned(), |f| f.file_name().to_owned()),
            ),
            ..Default::default()
        },
        writer,
    )
}

fn report_config_error(
    diagnostic: P<TsDiagnostic>,
    config_path: &str,
    program_diagnostic: bool,
    writer: &mut dyn Write,
) -> Result<(), String> {
    let mut help = diagnostic.localize();
    if program_diagnostic && help.contains("Please remove it from your configuration.") {
        help.push_str(
            "\nSee https://github.com/oxc-project/tsgolint/issues/351 for more information.",
        );
    }
    emit(
        &Diagnostic {
            range: diagnostic_range(diagnostic),
            message: RuleMessage {
                id: "tsconfig-error".into(),
                description: "Invalid tsconfig".into(),
                help,
            },
            file_path: Some(
                diagnostic
                    .file()
                    .map_or_else(|| config_path.to_owned(), |f| f.file_name().to_owned()),
            ),
            ..Default::default()
        },
        writer,
    )
}

fn diagnostic_range(diagnostic: P<TsDiagnostic>) -> Option<Range> {
    Some(Range { pos: diagnostic.pos()?, end: diagnostic.end()? })
}

// A compiler diagnostic on a content-mapped file has a range in the mapper's virtual text: as the CLI's diagnostic
// writer does, it is reported at the original range, or on the whole file when it lies in synthesized code. A
// mapper's own diagnostics (they have a source) already carry original ranges.
fn original_range(diagnostic: P<TsDiagnostic>, file: P<SourceFile>) -> Option<Range> {
    let Some(span_map) = file.span_map().filter(|_| diagnostic.source().is_empty()) else {
        return diagnostic_range(diagnostic);
    };
    let (loc, fidelity) = span_map.virtual_to_original_span(diagnostic.loc()?);
    if fidelity == tsrs_spanmap::Fidelity::None {
        return None;
    }
    Some(Range { pos: loc.source_pos()?, end: loc.source_end()? })
}

pub(crate) fn emit(diagnostic: &Diagnostic, writer: &mut dyn Write) -> Result<(), String> {
    protocol::write_message(writer, MessageType::Diagnostic, &diagnostic.to_json())
        .map_err(|e| e.to_string())
}

#[expect(clippy::implicit_hasher, reason = "Public for the integration tests.")]
pub fn find_tsconfig(
    file: &str,
    host: &ConfigHost,
    configs: &mut FxHashMap<tspath::Path, Option<ParsedCommandLine>>,
) -> Option<String> {
    let mut next = compute_config_file_name(file, false, host);
    let mut visited = FxHashSet::default();
    let file_path = tspath::to_path(file, &host.cwd, host.fs.use_case_sensitive_file_names());
    while let Some(config_path) = next {
        let mut queue = VecDeque::from([config_path.clone()]);
        let mut disable_solution_searching = false;
        while let Some(path) = queue.pop_front() {
            if !visited.insert(path.clone()) {
                continue;
            }
            let config_path_key =
                tspath::to_path(&path, &host.cwd, host.fs.use_case_sensitive_file_names());
            let config = configs.entry(config_path_key).or_insert_with(|| {
                let (config, diagnostics) = tsrs_tsoptions::get_parsed_command_line_of_config_file(
                    &path,
                    host.existing_options.as_ref(),
                    None,
                    host,
                    None,
                );
                if !without_run_external_code_error(diagnostics, &mut false).is_empty() {
                    return None;
                }
                config.filter(|config| {
                    config.config_file.is_none_or(|file| file.source_file.diagnostics().is_empty())
                })
            });
            if let Some(config) = config {
                if path == config_path {
                    disable_solution_searching = config
                        .compiler_options()
                        .is_some_and(|options| options.disable_solution_searching.is_true());
                }
                if !config.file_names().is_empty()
                    && (config
                        .compiler_options()
                        .is_none_or(|options| !options.composite.is_true())
                        || config.possibly_matches_file_name(file))
                    && config.file_names_by_path().contains_key(&file_path)
                {
                    return Some(path);
                }
                queue.extend(config.resolved_project_reference_paths().iter().cloned());
            }
        }
        if disable_solution_searching {
            return None;
        }
        next = compute_config_file_name(&config_path, true, host);
    }
    None
}

fn compute_config_file_name(file: &str, skip_directory: bool, host: &ConfigHost) -> Option<String> {
    let mut directory = tspath::get_directory_path(file);
    let mut skip_tsconfig = skip_directory;
    let mut skip_jsconfig = skip_directory && !file.ends_with("/tsconfig.json");
    loop {
        for (name, skip) in [("tsconfig.json", skip_tsconfig), ("jsconfig.json", skip_jsconfig)] {
            if !skip {
                let path = tspath::combine_paths(&directory, &[name]);
                if host.fs.file_exists(&path) {
                    return Some(path);
                }
            }
        }
        if directory.ends_with("/node_modules") {
            return None;
        }
        let parent = tspath::get_directory_path(&directory);
        if parent == directory {
            return None;
        }
        directory = parent;
        skip_tsconfig = false;
        skip_jsconfig = false;
    }
}

#[cfg(test)]
mod tests {
    use tsrs_spanmap::{Kind, Segment};

    use super::original_lint_range;
    use crate::protocol::Range;

    fn segment(
        virtual_start: u32,
        virtual_end: u32,
        original_start: u32,
        original_end: u32,
        kind: Kind,
    ) -> Segment {
        Segment {
            virtual_start,
            virtual_end,
            original_start,
            original_end,
            kind,
            ..Default::default()
        }
    }

    #[test]
    fn the_fallback_tries_each_mapped_start() {
        // original "f()\nf", virtual "declare function f(): Promise<void>;\nf();\n": a generated atom maps to the empty
        // range at 0, the `f` of `f()` maps to the `f` at 4, and `()` maps to the `()` at 1. The report on `f()` at
        // virtual [37, 40) starts where the atom ends and where the `f` segment starts. Through the `f` segment, the
        // start is 4, after every mapped end, so the start through the atom (0) is used, as Volar's
        // `fallbackToAnyMatch` does.
        let segments = [
            segment(0, 37, 0, 0, Kind::ATOM),
            segment(37, 38, 4, 5, Kind::VERBATIM),
            segment(38, 40, 1, 3, Kind::VERBATIM),
        ];
        assert_eq!(
            original_lint_range(&segments, Range { pos: 37, end: 40 }),
            Some(Range { pos: 0, end: 3 })
        );
    }
}
