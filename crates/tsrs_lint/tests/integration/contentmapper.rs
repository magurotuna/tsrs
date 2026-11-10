// Content-mapped files (testdata/contentmapper/lint-mapper): a rule report moves to the original file when both its
// ends map into the original text, and is dropped when it is on code that the mapper generated. Without
// `--runExternalCode`, the TS18068 error of a tsconfig with `contentMappers` does not stop its program. Needs `node`.

use std::path::{Path, PathBuf};
use std::process::Command;

use tsrs_core::json::Value;
use tsrs_lint::protocol::{self, MessageType};

use super::headless::run;

const A_CMP: &str = "// ünïcödé: UTF-8 and UTF-16 offsets differ\nasync function load(): Promise<void> {}\nload();\n//!on load\n";
const B_CMP: &str =
    "export const x = 1;\n//!supplemental\nasync function later(): Promise<void> {}\nlater();\n";
const INDEX_TS: &str = "async function f(): Promise<void> {}\nf();\n";
const WITH_MAPPER: &str = r#"{"compilerOptions":{"strict":true,"noEmit":true,"module":"esnext","target":"es2022","types":[]},"contentMappers":[{"package":"lint-mapper","extensions":[".cmp"]}],"include":["src"]}"#;
const WITHOUT_MAPPER: &str = r#"{"compilerOptions":{"strict":true,"noEmit":true,"module":"esnext","target":"es2022","types":[]},"include":["src"]}"#;

fn has_node() -> bool {
    Command::new("node").arg("--version").output().is_ok_and(|output| output.status.success())
}

fn project(name: &str, tsconfig: &str) -> PathBuf {
    let base = std::env::current_dir().unwrap().join("target/lint-content-mapper").join(name);
    let _ = std::fs::remove_dir_all(&base);
    let mapper = base.join("node_modules/lint-mapper");
    std::fs::create_dir_all(&mapper).unwrap();
    std::fs::create_dir_all(base.join("src")).unwrap();
    let testdata =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/contentmapper/lint-mapper");
    for file in ["package.json", "mapper.mjs"] {
        std::fs::copy(testdata.join(file), mapper.join(file)).unwrap();
    }
    std::fs::write(base.join("tsconfig.json"), tsconfig).unwrap();
    for (file, text) in [("src/a.cmp", A_CMP), ("src/b.cmp", B_CMP), ("src/index.ts", INDEX_TS)] {
        std::fs::write(base.join(file), text).unwrap();
    }
    base
}

fn payload(base: &Path, report_semantic: bool, report_notices: bool) -> Value {
    let files = ["src/a.cmp", "src/b.cmp", "src/index.ts"]
        .map(|file| Value::String(base.join(file).to_string_lossy().into_owned()));
    let rules = ["no-floating-promises", "no-misused-promises"]
        .map(|rule| protocol::object([("name", Value::String(rule.into()))]));
    protocol::object([
        ("version", Value::Integer(2)),
        (
            "configs",
            Value::Array(vec![protocol::object([
                ("file_paths", Value::Array(files.to_vec())),
                ("rules", Value::Array(rules.to_vec())),
            ])]),
        ),
        ("report_semantic", Value::Bool(report_semantic)),
        ("report_notices", Value::Bool(report_notices)),
    ])
}

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Object(object) = value else { panic!("not an object: {value:?}") };
    object.get(key).unwrap_or(&Value::Null)
}

fn string(value: &Value) -> &str {
    if let Value::String(s) = value { s } else { "" }
}

fn number(value: &Value) -> u32 {
    match value {
        Value::Integer(n) => u32::try_from(*n).unwrap(),
        Value::Number(n) => *n as u32,
        _ => panic!("not a number: {value:?}"),
    }
}

/// One line per frame: the file, the rule or message id, the original text of the range, and the number of labels,
/// fixes and suggestions.
fn describe(base: &Path, frames: &[(u8, Value)]) -> Vec<String> {
    frames
        .iter()
        .map(|(kind, value)| {
            if *kind == MessageType::Notice as u8 {
                return format!(
                    "notice {} {} {:?}",
                    string(field(value, "id")),
                    string(field(value, "tsconfig"))
                        .strip_prefix(&*base.to_string_lossy())
                        .unwrap(),
                    field(value, "extensions")
                );
            }
            assert_eq!(*kind, MessageType::Diagnostic as u8, "{value:?}");
            let path = string(field(value, "file_path"));
            let text = std::fs::read_to_string(path).unwrap();
            let range = field(value, "range");
            let excerpt = if *range == Value::Null {
                "<no range>".to_owned()
            } else {
                format!(
                    "{:?}",
                    &text[number(field(range, "pos")) as usize
                        ..number(field(range, "end")) as usize]
                )
            };
            let rule = field(value, "rule");
            let id = if *rule == Value::Null {
                string(field(field(value, "message"), "id"))
            } else {
                string(rule)
            };
            let count = |key| match field(value, key) {
                Value::Array(items) => items.len(),
                _ => 0,
            };
            format!(
                "{} {id} {excerpt} labels={} fixes={} suggestions={}",
                path.strip_prefix(&*base.to_string_lossy()).unwrap(),
                count("labeled_ranges"),
                count("fixes"),
                count("suggestions")
            )
        })
        .collect()
}

#[test]
fn rule_reports_move_to_the_original_file_or_are_dropped() {
    if !has_node() {
        return;
    }
    let base = project("reports", WITH_MAPPER);
    let (status, frames) =
        run(&payload(&base, false, true), &["--runExternalCode", "--fix", "--fix-suggestions"]);
    assert_eq!(status, 0, "{frames:?}");
    // Not reported: `__gen();` in the synthesized prefix, and the arrow function in `__on(() => load());`, which is
    // inside the atom that maps to `load` in `//!on load`.
    assert_eq!(
        describe(&base, &frames),
        [
            "/src/a.cmp no-floating-promises \"load()\" labels=1 fixes=0 suggestions=0",
            "/src/b.cmp no-floating-promises \"later()\" labels=1 fixes=0 suggestions=0",
            "/src/index.ts no-floating-promises \"f()\" labels=1 fixes=0 suggestions=2",
        ]
    );
}

#[test]
fn type_errors_without_an_original_location_are_grouped_at_the_start_of_the_file() {
    if !has_node() {
        return;
    }
    let base = project("type-errors", WITH_MAPPER);
    let (status, frames) = run(&payload(&base, true, true), &["--runExternalCode"]);
    assert_eq!(status, 0, "{frames:?}");
    let described = describe(&base, &frames);
    for file in ["/src/a.cmp", "/src/b.cmp"] {
        assert!(
            described.contains(&format!("{file} TS2882 \"\" labels=0 fixes=0 suggestions=0")),
            "{described:#?}"
        );
    }
    let grouped = frames
        .iter()
        .find(|(_, value)| string(field(field(value, "message"), "id")) == "TS2882")
        .unwrap();
    let help = string(field(field(&grouped.1, "message"), "help"));
    assert!(help.contains("code generated by the content mapper 'lint-mapper@1.0.0'"), "{help}");
    assert!(help.contains("Generated code: `import \"./missing-generated-module\";`"), "{help}");
}

#[test]
fn without_run_external_code_the_program_runs_without_its_mappers() {
    if !has_node() {
        return;
    }
    let base = project("no-flag", WITH_MAPPER);
    for report_notices in [true, false] {
        let (status, frames) = run(&payload(&base, false, report_notices), &[]);
        assert_eq!(status, 0, "{frames:?}");
        let mut expected = vec![
            "/src/index.ts no-floating-promises \"f()\" labels=1 fixes=0 suggestions=0".to_owned(),
        ];
        if report_notices {
            expected.insert(
                0,
                "notice content-mappers-require-run-external-code /tsconfig.json Array([String(\".cmp\")])"
                    .to_owned(),
            );
        }
        assert_eq!(describe(&base, &frames), expected);
    }
}

#[test]
fn files_that_no_mapper_transforms_are_skipped() {
    if !has_node() {
        return;
    }
    let base = project("no-mapper", WITHOUT_MAPPER);
    let (status, frames) = run(&payload(&base, true, true), &["--runExternalCode"]);
    assert_eq!(status, 0, "{frames:?}");
    assert_eq!(
        describe(&base, &frames),
        ["/src/index.ts no-floating-promises \"f()\" labels=1 fixes=0 suggestions=0"]
    );
}
