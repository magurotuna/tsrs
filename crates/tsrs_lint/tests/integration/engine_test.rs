use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;
use tsrs_core::P;
use tsrs_lint::engine::{ConfigHost, find_tsconfig, is_mapper_written};
use tsrs_lint::protocol::Range;
use tsrs_spanmap::{Kind, Segment};
use tsrs_vfs::{FS, vfstest, wrapvfs};

#[test]
fn shared_project_discovery_reads_each_config_once() {
    let region = tsrs_core::arena::Region::new(1 << 20);
    let _scope = region.enter();
    let mut files = vec![
        (
            "/project/tsconfig.json".to_owned(),
            r#"{"files":[],"references":[{"path":"./a"},{"path":"./b"},{"path":"."}]}"#.to_owned(),
        ),
        (
            "/project/a/tsconfig.json".to_owned(),
            r#"{"compilerOptions":{"composite":true},"include":["*.ts"],"references":[{"path":".."}]}"#.to_owned(),
        ),
        (
            "/project/b/tsconfig.json".to_owned(),
            r#"{"include":["*.ts"]}"#.to_owned(),
        ),
        (
            "/project/invalid/tsconfig.json".to_owned(),
            r#"{"compilerOptions":"#.to_owned(),
        ),
        ("/project/loose.ts".to_owned(), "export {};".to_owned()),
    ];
    for directory in ["a", "b"] {
        for index in 0..100 {
            files.push((format!("/project/{directory}/file{index}.ts"), "export {};".to_owned()));
        }
    }
    let fs = P::new(vfstest::from_map(files, true)).get();
    let reads = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let count_reads = Arc::clone(&reads);
    let fs = wrapvfs::wrap(
        fs,
        wrapvfs::Replacements {
            read_file: Some(Box::new(move |path| {
                *count_reads.lock().unwrap().entry(path.to_owned()).or_default() += 1;
                fs.read_file(path)
            })),
            ..Default::default()
        },
    );
    let host = ConfigHost::new(Arc::new(fs), "/project".into());
    let mut configs = FxHashMap::default();
    for directory in ["a", "b"] {
        for index in 0..100 {
            assert_eq!(
                find_tsconfig(&format!("/project/{directory}/file{index}.ts"), &host, &mut configs),
                Some(format!("/project/{directory}/tsconfig.json")),
            );
        }
    }
    assert_eq!(find_tsconfig("/project/loose.ts", &host, &mut configs), None);
    assert_eq!(find_tsconfig("/project/loose.ts", &host, &mut configs), None);
    for name in ["first", "second"] {
        assert_eq!(
            find_tsconfig(&format!("/project/invalid/{name}.ts"), &host, &mut configs),
            None
        );
    }
    let reads = reads.lock().unwrap();
    for path in [
        "/project/tsconfig.json",
        "/project/a/tsconfig.json",
        "/project/b/tsconfig.json",
        "/project/invalid/tsconfig.json",
    ] {
        assert_eq!(reads.get(path), Some(&1), "{path}");
    }
}

#[test]
fn project_discovery_uses_canonical_file_membership() {
    let region = tsrs_core::arena::Region::new(1 << 20);
    let _scope = region.enter();
    let fs = vfstest::from_map(
        [
            (
                "/Project/tsconfig.json",
                r#"{"compilerOptions":{"composite":true},"files":["./src/../src/a.ts"]}"#,
            ),
            ("/Project/src/a.ts", "export {};"),
        ],
        false,
    );
    let host = ConfigHost::new(Arc::new(fs), "/project".into());
    let mut configs = FxHashMap::default();
    for directory in ["/PROJECT", "/project"] {
        assert_eq!(
            find_tsconfig(&format!("{directory}/src/A.ts"), &host, &mut configs),
            Some(format!("{directory}/tsconfig.json")),
        );
    }
}

#[test]
fn project_discovery_traverses_non_matching_composite_references() {
    let region = tsrs_core::arena::Region::new(1 << 20);
    let _scope = region.enter();
    let fs = vfstest::from_map(
        [
            (
                "/project/tsconfig.json",
                r#"{"compilerOptions":{"composite":true,"strict":true,"strictNullChecks":false},"files":[],"references":[{"path":"./app/tsconfig.app.json"},{"path":"./tool/tsconfig.tool.json"}]}"#,
            ),
            (
                "/project/app/tsconfig.app.json",
                r#"{"extends":"../tsconfig.json","include":["*.ts"],"references":[{"path":"../lib/tsconfig.lib.json"}]}"#,
            ),
            (
                "/project/tool/tsconfig.tool.json",
                r#"{"extends":"../tsconfig.json","include":["*.ts"],"references":[{"path":"../lib/tsconfig.lib.json"}]}"#,
            ),
            (
                "/project/lib/tsconfig.lib.json",
                r#"{"extends":"../tsconfig.json","include":["*.ts"],"exclude":["excluded.ts"],"references":[{"path":"../core/tsconfig.core.json"},{"path":"./tsconfig.lib.json"}]}"#,
            ),
            (
                "/project/core/tsconfig.core.json",
                r#"{"extends":"../tsconfig.json","include":["*.ts"],"references":[{"path":"../app/tsconfig.app.json"}]}"#,
            ),
            ("/project/app/index.ts", "export {};"),
            ("/project/tool/index.ts", "export {};"),
            ("/project/lib/index.ts", "export {};"),
            ("/project/core/index.ts", "export {};"),
            ("/project/lib/excluded.ts", "export {};"),
            ("/project/loose.ts", "export {};"),
        ],
        true,
    );
    let host = ConfigHost::new(Arc::new(fs), "/project".into());
    let mut configs = FxHashMap::default();
    for _ in 0..2 {
        for project in ["core", "lib", "tool", "app"] {
            let expected = format!("/project/{project}/tsconfig.{project}.json");
            assert_eq!(
                find_tsconfig(&format!("/project/{project}/index.ts"), &host, &mut configs),
                Some(expected.clone()),
            );
            let config =
                configs[&tsrs_core::tspath::to_path(&expected, "/project", true)].as_ref().unwrap();
            assert!(config.compiler_options().unwrap().strict.is_true());
            assert!(config.compiler_options().unwrap().strict_null_checks.is_false());
        }
        for file in ["loose.ts", "lib/excluded.ts"] {
            assert_eq!(find_tsconfig(&format!("/project/{file}"), &host, &mut configs), None);
        }
    }
    assert_eq!(configs.len(), 5);
}

fn segment(
    virtual_start: u32,
    virtual_end: u32,
    original_start: u32,
    original_end: u32,
    kind: Kind,
) -> Segment {
    Segment { virtual_start, virtual_end, original_start, original_end, kind, ..Default::default() }
}

#[test]
fn mapper_written_text_is_text_without_a_verbatim_copy() {
    // astro2tsx: original "{\n\treturn x;\n}" (the `return` at 3), virtual "{\n\tthrow  x;\n}". The mapper wrote
    // `throw ` at [3, 9) between two verbatim segments. The report on `x` at [10, 11) maps, but the `throw` keyword
    // that the verdict depends on is mapper-written.
    let segments = [segment(0, 3, 0, 3, Kind::VERBATIM), segment(9, 14, 9, 14, Kind::VERBATIM)];
    assert!(is_mapper_written(&segments, Range { pos: 3, end: 8 }));
    assert!(is_mapper_written(&segments, Range { pos: 3, end: 9 }));
    // The `x` and the user's indentation in front of the keyword are verbatim.
    assert!(!is_mapper_written(&segments, Range { pos: 10, end: 11 }));
    assert!(!is_mapper_written(&segments, Range { pos: 1, end: 3 }));
    // One verbatim byte is enough: the range is not all mapper-written.
    assert!(!is_mapper_written(&segments, Range { pos: 2, end: 8 }));
    assert!(!is_mapper_written(&segments, Range { pos: 3, end: 10 }));
    // An empty range has no text.
    assert!(!is_mapper_written(&segments, Range { pos: 5, end: 5 }));
    // A file whose text is all generated.
    assert!(is_mapper_written(&[], Range { pos: 0, end: 5 }));
    // astro2tsx: original "{\n\treturn;\n}", virtual "{\n\tthrow undefined;\n}". The report on the statement at
    // [3, 19) starts where the first segment ends and ends in the second one, so it maps to `return;`. Its keyword
    // at [3, 8) is mapper-written, and the statement is not, because of its `;`.
    let segments = [segment(0, 3, 0, 3, Kind::VERBATIM), segment(18, 21, 9, 12, Kind::VERBATIM)];
    assert!(is_mapper_written(&segments, Range { pos: 3, end: 8 }));
    assert!(!is_mapper_written(&segments, Range { pos: 3, end: 19 }));
}

#[test]
fn text_inside_atoms_and_empty_segments_is_mapper_written() {
    // The Vue mapper: an event handler wrapper `: (...) => {\nreturn (` is one atom that is attributed to the tag
    // name, and `select(item)` is verbatim. Virtual: atom [10, 40), verbatim [40, 52), atom [52, 60).
    let segments = [
        segment(10, 40, 1, 3, Kind::ATOM),
        segment(40, 52, 20, 32, Kind::VERBATIM),
        segment(52, 60, 1, 3, Kind::ATOM),
    ];
    assert!(is_mapper_written(&segments, Range { pos: 32, end: 38 }));
    assert!(!is_mapper_written(&segments, Range { pos: 40, end: 46 }));
    assert!(is_mapper_written(&segments, Range { pos: 52, end: 53 }));
    // A whole atom or alias in the range is the user's token in another spelling: the Vue mapper writes `=>` for
    // `=&gt;` in an attribute, and `onClick` for `click`. Text inside it is still mapper-written.
    assert!(!is_mapper_written(&[segment(20, 22, 5, 10, Kind::ATOM)], Range { pos: 20, end: 22 }));
    assert!(!is_mapper_written(&[segment(0, 7, 1, 6, Kind::ALIAS)], Range { pos: 0, end: 7 }));
    assert!(is_mapper_written(&[segment(0, 7, 1, 6, Kind::ALIAS)], Range { pos: 1, end: 4 }));
    // An atom with an empty original range is code that the mapper wrote at one point, such as the `export default`
    // of a Vue component.
    assert!(is_mapper_written(&[segment(0, 14, 3, 3, Kind::ATOM)], Range { pos: 0, end: 14 }));
    // An empty verbatim segment (a navigation anchor of the Vue mapper) at the start of, inside, and at the end of
    // generated text covers no text.
    let anchors = [
        segment(0, 4, 0, 4, Kind::VERBATIM),
        segment(4, 4, 7, 7, Kind::VERBATIM),
        segment(6, 6, 8, 8, Kind::VERBATIM),
        segment(9, 9, 9, 9, Kind::VERBATIM),
        segment(9, 12, 9, 12, Kind::VERBATIM),
    ];
    assert!(is_mapper_written(&anchors, Range { pos: 4, end: 9 }));
    assert!(!is_mapper_written(&anchors, Range { pos: 4, end: 10 }));
    assert!(!is_mapper_written(&anchors, Range { pos: 3, end: 9 }));
}
