use hashline::{Blocks, Change, Error, Mark, Row, apply, parse, unified_patch};
use std::collections::HashMap;

// A fake resolver: constructs are a table of (opening row, start, end), so a
// test states exactly what the parser would have said.
struct Fake {
    extents: HashMap<usize, (usize, usize)>,
}

impl Blocks for Fake {
    fn extent_of(&self, _p: &str, _c: &str, line: usize) -> Option<(usize, usize)> {
        self.extents.get(&line).copied()
    }

    fn openings(&self, _p: &str, _c: &str) -> Vec<usize> {
        let mut v: Vec<usize> = self.extents.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

fn blocks(pairs: &[(usize, usize, usize)]) -> Fake {
    Fake {
        extents: pairs.iter().map(|(o, s, e)| (*o, (*s, *e))).collect(),
    }
}

// Parse, apply to `content` in `a.txt`, and hand back the result.
fn run(content: &str, patch: &str, pairs: &[(usize, usize, usize)]) -> Result<String, Error> {
    let patch = parse(patch)?;
    let fake = blocks(pairs);
    let files: HashMap<&str, &str> = HashMap::from([("a.txt", content)]);
    let plan = apply(&patch, &files, &fake)?;
    match &plan.changes[..] {
        [Change::Write { content, .. }] => Ok(content.clone()),
        other => unreachable!("{other:?}"),
    }
}

fn refused<T>(r: &Result<T, Error>) -> &Error {
    r.as_ref().err().unwrap()
}

#[test]
fn sections_split_on_blank_lines_and_carry_marks() {
    let patch = parse("[a.rs]\n-old\n+new\n\n=keep\n+added\n").unwrap();
    assert_eq!(patch.sections.len(), 1);
    assert_eq!(patch.sections[0].groups.len(), 2);
    assert_eq!(
        patch.sections[0].groups[0],
        vec![
            Row::Mark(Mark::Del, "old".into()),
            Row::Mark(Mark::Add, "new".into()),
        ]
    );
    assert_eq!(
        patch.sections[0].groups[1],
        vec![
            Row::Mark(Mark::Keep, "keep".into()),
            Row::Mark(Mark::Add, "added".into()),
        ]
    );
}

#[test]
fn a_header_with_a_tag_is_refused_with_the_fix_named() {
    let r = parse("[a.rs#A1B2]\n+new\n");
    let err = refused(&r);
    assert!(err.to_string().contains("no TAG any more"), "{err}");
}

#[test]
fn content_starting_with_a_marker_needs_no_escape() {
    let patch = parse("[a.rs]\n--x\n").unwrap();
    assert_eq!(
        patch.sections[0].groups[0],
        vec![Row::Mark(Mark::Del, "-x".into())]
    );
}

#[test]
fn a_replace_matches_once_and_swaps_the_rows() {
    let out = run("a\nb\nc\n", "[a.txt]\n-b\n+B\n", &[]).unwrap();
    assert_eq!(out, "a\nB\nc\n");
}

#[test]
fn a_lone_delete_removes_its_rows() {
    let out = run("a\nb\nc\n", "[a.txt]\n-b\n", &[]).unwrap();
    assert_eq!(out, "a\nc\n");
}

#[test]
fn a_delete_disambiguated_by_keep_context_hits_the_first() {
    // The case that named the design: two identical rows, delete the first.
    let out = run("abc\n\nabc\n", "[a.txt]\n-abc\n=\n=abc\n", &[]).unwrap();
    assert_eq!(out, "\nabc\n");
}

#[test]
fn an_insert_after_the_anchor_lands_below_it() {
    let out = run("a\nb\n", "[a.txt]\n=a\n+new\n", &[]).unwrap();
    assert_eq!(out, "a\nnew\nb\n");
}

#[test]
fn an_insert_before_the_anchor_lands_above_it() {
    let out = run("a\nb\n", "[a.txt]\n+new\n=b\n", &[]).unwrap();
    assert_eq!(out, "a\nnew\nb\n");
}

#[test]
fn adds_between_keeps_land_between_them() {
    let out = run("a\nc\n", "[a.txt]\n=a\n+b\n=c\n", &[]).unwrap();
    assert_eq!(out, "a\nb\nc\n");
}

#[test]
fn a_star_row_scopes_the_operation_to_one_construct() {
    // Two identical bodies; the scope picks the second construct.
    let src = "fn a() {\n    same\n}\n\nfn b() {\n    same\n}\n";
    let out = run(
        src,
        "[a.txt]\n@fn b()\n-    same\n+    other\n",
        &[(1, 1, 3), (5, 5, 7)],
    )
    .unwrap();
    assert_eq!(out, "fn a() {\n    same\n}\n\nfn b() {\n    other\n}\n");
}

#[test]
fn plus_rows_hugging_a_star_insert_before_the_construct() {
    let src = "fn a() {\n}\n";
    let out = run(src, "[a.txt]\n+fn b() {}\n*fn a()\n", &[(1, 1, 2)]).unwrap();
    assert_eq!(out, "fn b() {}\nfn a() {\n}\n");
}

#[test]
fn a_star_with_rows_below_inserts_after_the_construct() {
    let src = "fn a() {\n}\n";
    let out = run(src, "[a.txt]\n*fn a()\n+fn b() {}\n", &[(1, 1, 2)]).unwrap();
    assert_eq!(out, "fn a() {\n}\nfn b() {}\n");
}

#[test]
fn an_append_renders_as_an_append_in_the_unified_patch() {
    let patch = parse("[a.txt]\n*fn a()\n+fn b() {}\n").unwrap();
    let fake = blocks(&[(1, 1, 2)]);
    let files: HashMap<&str, &str> = HashMap::from([("a.txt", "fn a() {\n}\n")]);
    let plan = apply(&patch, &files, &fake).unwrap();
    let out = unified_patch(&plan.changes, &files);
    assert_eq!(
        out,
        "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,3 @@\n fn a() {\n }\n+fn b() {}\n"
    );
}

#[test]
fn plus_rows_under_an_at_replace_the_whole_construct() {
    let src = "fn a() {\n}\n";
    let out = run(src, "[a.txt]\n@fn a()\n+fn b() {}\n", &[(1, 1, 2)]).unwrap();
    assert_eq!(out, "fn b() {}\n");
}

#[test]
fn a_star_sandwich_inserts_on_both_sides() {
    let src = "fn a() {\n}\n";
    let out = run(src, "[a.txt]\n+top();\n*fn a()\n+bottom();\n", &[(1, 1, 2)]).unwrap();
    assert_eq!(out, "top();\nfn a() {\n}\nbottom();\n");
}

#[test]
fn a_star_holding_marks_points_to_at() {
    let r = run("fn a() {\n}\n", "[a.txt]\n*fn a()\n-x\n", &[(1, 1, 2)]);
    match refused(&r) {
        Error::Syntax { what, .. } => assert!(what.contains("`@`"), "{what}"),
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn an_at_scope_with_no_operation_refuses() {
    let r = run("fn a() {\n}\n", "[a.txt]\n@fn a()\n", &[(1, 1, 2)]);
    match refused(&r) {
        Error::Syntax { what, .. } => assert!(what.contains("names no operation"), "{what}"),
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn insert_after_the_close_goes_through_the_last_row_anchor() {
    let src = "fn a() {\n}\n";
    let out = run(src, "[a.txt]\n=}\n+fn b() {}\n", &[(1, 1, 2)]).unwrap();
    assert_eq!(out, "fn a() {\n}\nfn b() {}\n");
}

#[test]
fn a_no_match_refusal_names_the_closest_row() {
    let r = run("alpha\nbeta\n", "[a.txt]\n-alpga\n", &[]);
    match refused(&r) {
        Error::NoMatch { line, text, .. } => {
            assert_eq!(*line, 1);
            assert_eq!(text, "alpha");
        }
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn an_ambiguous_match_refuses_with_every_candidate() {
    let r = run("x\nz\nx\nz\nx\n", "[a.txt]\n=z\n-x\n", &[]);
    match refused(&r) {
        Error::Ambiguous { n, detail, .. } => {
            assert_eq!(*n, 2);
            assert!(detail.contains("2-3") && detail.contains("4-5"), "{detail}");
        }
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn an_ambiguous_match_grows_context_until_it_tells_candidates_apart() {
    let src = "fn f() {\n    b();\n    a();\n    b();\n    a();\n    b();\n}\n";
    let r = run(src, "[a.txt]\n=    a();\n-    b();\n", &[(1, 1, 7)]);
    match refused(&r) {
        Error::Ambiguous { detail, .. } => {
            assert!(detail.contains("in `fn f() {`"), "{detail}");
            assert!(
                detail.matches("preceded by `    b();`").count() == 2,
                "{detail}"
            );
            assert!(detail.contains("before that `fn f() {`"), "{detail}");
            assert!(detail.contains("before that `    a();`"), "{detail}");
        }
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn an_ambiguous_match_names_the_construct_of_each_candidate() {
    let src =
        "fn a() {\n    one\n    same\n    other\n}\n\nfn b() {\n    two\n    same\n    other\n}\n";
    let r = run(
        src,
        "[a.txt]\n=    same\n-    other\n",
        &[(1, 1, 5), (7, 7, 11)],
    );
    match refused(&r) {
        Error::Ambiguous { detail, .. } => {
            assert!(detail.contains("in `fn a() {`"), "{detail}");
            assert!(detail.contains("in `fn b() {`"), "{detail}");
            assert!(detail.contains("preceded by `    one`"), "{detail}");
            assert!(detail.contains("preceded by `    two`"), "{detail}");
        }
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn a_sweep_without_a_scope_runs_file_wide() {
    let src = "fn a() {\n    dbg!(x);\n    keep;\n}\n\nfn b() {\n    dbg!(y);\n    keep;\n}\n";
    let out = run(src, "[a.txt]\n-dbg!(\n", &[(1, 1, 4), (6, 6, 9)]).unwrap();
    assert_eq!(out, "fn a() {\n    keep;\n}\n\nfn b() {\n    keep;\n}\n");
}

#[test]
fn a_sweep_takes_every_matching_row_in_its_construct() {
    let src = "fn a() {\n    dbg!(x);\n    keep;\n}\n\nfn b() {\n    dbg!(y);\n    keep;\n}\n";
    let out = run(src, "[a.txt]\n@fn b()\n-dbg!(\n", &[(1, 1, 4), (6, 6, 9)]).unwrap();
    assert_eq!(
        out,
        "fn a() {\n    dbg!(x);\n    keep;\n}\n\nfn b() {\n    keep;\n}\n"
    );
}

#[test]
fn a_sweep_matching_nothing_points_at_the_nearest_row() {
    let r = run(
        "fn a() {\n    keep;\n}\n",
        "[a.txt]\n@fn a()\n-dbgzz\n",
        &[(1, 1, 3)],
    );
    assert!(matches!(refused(&r), Error::NoMatch { .. }), "{:?}", r);
}

#[test]
fn a_bare_minus_deletes_the_whole_construct() {
    let out = run(
        "fn a() {\n    x\n    y\n}\n",
        "[a.txt]\n@fn a()\n-\n",
        &[(1, 1, 4)],
    )
    .unwrap();
    assert_eq!(out, "");
}

#[test]
fn a_bare_minus_without_a_scope_refuses() {
    let r = run("a\n\nb\n", "[a.txt]\n-\n", &[]);
    match refused(&r) {
        Error::Syntax { what, .. } => assert!(what.contains("bare"), "{what}"),
        other => unreachable!("{other:?}"),
    }
}

#[test]
fn a_star_that_resolves_to_nothing_refuses() {
    let r = run("a\n", "[a.txt]\n+new\n*fn missing()\n", &[]);
    let err = refused(&r);
    assert!(matches!(err, Error::NoConstruct { .. }), "{err}");
}

#[test]
fn overlapping_operations_refuse_the_patch() {
    let r = run("a\nb\nc\n", "[a.txt]\n-a\n-b\n\n=b\n-c\n", &[]);
    let err = refused(&r);
    assert!(matches!(err, Error::Overlap { .. }), "{err}");
}

#[test]
fn a_pure_add_group_without_an_anchor_refuses() {
    let r = run("a\n", "[a.txt]\n+new\n", &[]);
    let err = refused(&r);
    assert!(err.to_string().contains("anchor"), "{err}");
}

#[test]
fn crlf_rows_and_the_trailing_newline_survive() {
    let out = run("a\r\nb\r\n", "[a.txt]\n-a\n+A\n", &[]).unwrap();
    assert_eq!(out, "A\r\nb\r\n");
}

#[test]
fn an_empty_patch_is_refused() {
    assert!(matches!(refused(&parse("")), Error::Empty));
}

#[test]
fn landed_records_are_handed_back_for_the_report() {
    let patch = parse("[a.txt]\n-b\n+B\n").unwrap();
    let fake = blocks(&[]);
    let files: HashMap<&str, &str> = HashMap::from([("a.txt", "a\nb\nc\n")]);
    let plan = apply(&patch, &files, &fake).unwrap();
    match &plan.changes[..] {
        [Change::Write { landed, .. }] => {
            assert_eq!(landed.len(), 1);
            assert_eq!(landed[0].took, vec!["b".to_string()]);
            assert_eq!(landed[0].gave(), 1);
        }
        other => unreachable!("{other:?}"),
    }
}

// Two sections for one file stack: the second anchors on what the first
// wrote — each landing separately would let the later overwrite the earlier
// on disk. `run` itself asserts a single Write.
#[test]
fn two_sections_for_one_file_stack_into_one_write() {
    assert_eq!(
        run(
            "one\ntwo\nthree\n",
            "[a.txt]\n=one\n+ONE\n\n[a.txt]\n=three\n+THREE",
            &[],
        )
        .unwrap(),
        "one\nONE\ntwo\nthree\nTHREE\n"
    );
}
