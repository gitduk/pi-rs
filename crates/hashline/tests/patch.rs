use std::collections::HashMap;

use hashline::{Blocks, Change, Error, NoBlocks, apply, parse, tag};

/// Explicit start→end pairs. hashline never parses source itself, so its own
/// tests should not either.
struct Fake(&'static [(usize, usize)]);

impl Blocks for Fake {
    fn end_of(&self, _path: &str, _content: &str, line: usize) -> Option<usize> {
        self.0.iter().find(|(s, _)| *s == line).map(|(_, e)| *e)
    }
}

fn files<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
    pairs.iter().copied().collect()
}

/// Apply `src` to a single file and return the new content.
fn edit(before: &str, ops: &str) -> Result<String, Error> {
    let src = format!("[a.rs#{}]\n{ops}", tag(before));
    let patch = parse(&src)?;
    let plan = apply(&patch, &files(&[("a.rs", before)]), &NoBlocks)?;
    match &plan.changes[0] {
        Change::Write { content, .. } => Ok(content.clone()),
        other => panic!("expected a write, got {other:?}"),
    }
}

const SRC: &str = "one\ntwo\nthree\nfour\n";

#[test]
fn replaces_an_inclusive_range() {
    assert_eq!(edit(SRC, "PUT 2.=3:\n+TWO\n").unwrap(), "one\nTWO\nfour\n");
}

#[test]
fn a_single_line_is_written_n_to_n() {
    assert_eq!(
        edit(SRC, "PUT 2.=2:\n+2\n").unwrap(),
        "one\n2\nthree\nfour\n"
    );
}

#[test]
fn body_length_is_independent_of_range_length() {
    assert_eq!(
        edit(SRC, "PUT 2.=2:\n+a\n+b\n+c\n").unwrap(),
        "one\na\nb\nc\nthree\nfour\n"
    );
    assert_eq!(edit(SRC, "PUT 2.=3:\n").unwrap(), "one\nfour\n");
}

#[test]
fn a_bare_plus_is_a_blank_line_and_whitespace_is_verbatim() {
    assert_eq!(
        edit(SRC, "PUT 2.=2:\n+\n+    indented\n").unwrap(),
        "one\n\n    indented\nthree\nfour\n"
    );
}

#[test]
fn inserts_at_the_head_and_the_tail() {
    assert_eq!(
        edit(SRC, "PUT <1:\n+zero\n").unwrap(),
        "zero\none\ntwo\nthree\nfour\n"
    );
    assert_eq!(
        edit(SRC, "PUT >$:\n+five\n").unwrap(),
        "one\ntwo\nthree\nfour\nfive\n"
    );
    assert_eq!(
        edit(SRC, "PUT >2:\n+2.5\n").unwrap(),
        "one\ntwo\n2.5\nthree\nfour\n"
    );
}

#[test]
fn later_hunks_keep_their_original_numbering() {
    // If the first hunk shifted the rest, `4.=4` would land on the wrong line.
    let out = edit(SRC, "PUT 1.=1:\n+a\n+b\n+c\nPUT 4.=4:\n+FOUR\n").unwrap();
    assert_eq!(out, "a\nb\nc\ntwo\nthree\nFOUR\n");
}

#[test]
fn a_stale_tag_is_rejected_before_anything_is_built() {
    let src = "[a.rs#0000]\nPUT 1.=1:\n+x\n";
    let patch = parse(src).unwrap();
    let err = apply(&patch, &files(&[("a.rs", SRC)]), &NoBlocks).unwrap_err();
    assert!(matches!(err, Error::StaleTag { .. }), "{err}");
    assert!(err.to_string().contains("Re-read it"), "{err}");
}

#[test]
fn one_stale_section_rejects_the_whole_patch() {
    let src = format!(
        "[a.rs#{}]\nPUT 1.=1:\n+A\n[b.rs#0000]\nPUT 1.=1:\n+B\n",
        tag(SRC)
    );
    let patch = parse(&src).unwrap();
    // a.rs is valid, but a partly-applied patch is worse than a rejected one.
    assert!(apply(&patch, &files(&[("a.rs", SRC), ("b.rs", SRC)]), &NoBlocks).is_err());
}

#[test]
fn overlapping_ranges_are_rejected() {
    let err = edit(SRC, "PUT 1.=2:\n+a\nPUT 2.=3:\n+b\n").unwrap_err();
    assert!(matches!(err, Error::Overlap { overlap: 2, .. }), "{err}");
    assert!(
        err.to_string().contains("may never touch the same one"),
        "{err}"
    );
}

#[test]
fn an_insertion_buried_in_a_replaced_span_is_rejected() {
    let err = edit(SRC, "PUT 1.=3:\n+a\nPUT >2:\n+stray\n").unwrap_err();
    assert!(matches!(err, Error::Overlap { .. }), "{err}");
}

#[test]
fn out_of_range_names_the_real_length() {
    let err = edit(SRC, "PUT 9.=9:\n+x\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "a.rs has 4 lines, so 9.=9 names lines that do not exist"
    );
}

#[test]
fn cut_and_paste_moves_lines_within_a_file() {
    let out = edit(SRC, "CUT 1.=1 @first\nPUT >4 @first\n").unwrap();
    assert_eq!(out, "two\nthree\nfour\none\n");
}

#[test]
fn an_unlabeled_cut_feeds_the_anonymous_register() {
    assert_eq!(
        edit(SRC, "CUT 1.=1\nPUT >3\n").unwrap(),
        "two\nthree\none\nfour\n"
    );
}

#[test]
fn a_register_flows_between_files() {
    let a = "keep\nmoveme\n";
    let b = "target\n";
    let src = format!(
        "[a.rs#{}]\nCUT 2.=2 @fn\n[b.rs#{}]\nPUT <1 @fn\n",
        tag(a),
        tag(b)
    );
    let patch = parse(&src).unwrap();
    let plan = apply(&patch, &files(&[("a.rs", a), ("b.rs", b)]), &NoBlocks).unwrap();

    assert_eq!(
        plan.changes[0],
        Change::Write {
            path: "a.rs".into(),
            content: "keep\n".into(),
            landed: vec![]
        }
    );
    assert_eq!(
        plan.changes[1],
        Change::Write {
            path: "b.rs".into(),
            content: "moveme\ntarget\n".into(),
            landed: vec![hashline::Landed { start: 1, end: 1 }],
        }
    );
}

#[test]
fn pasting_an_unfilled_register_is_an_error() {
    let err = edit(SRC, "PUT >1 @nope\n").unwrap_err();
    assert!(matches!(err, Error::UnknownRegister { .. }), "{err}");
}

#[test]
fn rem_deletes_and_refuses_company() {
    let src = format!("[a.rs#{}]\nREM\n", tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    assert_eq!(
        plan.changes[0],
        Change::Remove {
            path: "a.rs".into()
        }
    );

    let src = format!("[a.rs#{}]\nREM\nPUT 1.=1:\n+x\n", tag(SRC));
    let err = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap_err();
    assert!(matches!(err, Error::RemoveWithOps { .. }), "{err}");
}

#[test]
fn mv_carries_the_edited_content_to_the_destination() {
    let src = format!("[a.rs#{}]\nPUT 1.=1:\n+ONE\nMV lib/a.rs\n", tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    assert_eq!(
        plan.changes[0],
        Change::Rename {
            from: "a.rs".into(),
            to: "lib/a.rs".into(),
            content: "ONE\ntwo\nthree\nfour\n".into(),
            landed: vec![hashline::Landed { start: 1, end: 1 }],
        }
    );
}

#[test]
fn a_file_without_a_trailing_newline_keeps_it_that_way() {
    assert_eq!(edit("a\nb", "PUT 1.=1:\n+A\n").unwrap(), "A\nb");
    assert_eq!(edit("a\nb\n", "PUT 1.=1:\n+A\n").unwrap(), "A\nb\n");
}

#[test]
fn an_empty_file_accepts_a_head_insert() {
    assert_eq!(edit("", "PUT <1:\n+first\n").unwrap(), "first\n");
    assert_eq!(edit("", "PUT >$:\n+first\n").unwrap(), "first\n");
}

#[test]
fn unified_diff_habits_are_named_rather_than_guessed_at() {
    let err = edit(SRC, "PUT 1.=1:\n+new\n-one\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("`-` rows are not valid"), "{err}");
    // Both ways out, because the row is written by a model that wants one of
    // them: a deletion, or a literal line that begins with `-`.
    assert!(err.contains("CUT N.=M"), "{err}");
    assert!(err.contains("`+- item`"), "{err}");
}

#[test]
fn a_bare_line_number_is_the_single_line_range() {
    assert_eq!(edit(SRC, "PUT 2:\n+B\n").unwrap(), "one\nB\nthree\nfour\n");
    assert_eq!(edit(SRC, "CUT 2\n").unwrap(), "one\nthree\nfour\n");
    // The long form still means the same thing.
    assert_eq!(edit(SRC, "CUT 2.=2\n").unwrap(), "one\nthree\nfour\n");
}

#[test]
fn a_trailing_colon_on_a_bodyless_op_is_tolerated() {
    // A `:` on a `CUT` is noise. Refusing it put the complaint on the colon and
    // hid the row underneath, which is the mistake that actually matters.
    assert_eq!(edit(SRC, "CUT 2:\n").unwrap(), "one\nthree\nfour\n");
    assert_eq!(
        edit(SRC, "CUT 2.=2 @held:\n").unwrap(),
        "one\nthree\nfour\n"
    );

    let err = edit(SRC, "CUT 2:\n-  two\n").unwrap_err().to_string();
    assert!(err.contains("take no body rows"), "{err}");
    assert!(err.contains("PUT N.=M:"), "{err}");
}

#[test]
fn a_plus_row_under_a_bodyless_op_says_which_op() {
    let err = edit(SRC, "CUT 2\n+two\n").unwrap_err().to_string();
    assert!(err.contains("take no body rows"), "{err}");
}

/// Apply `ops` with a resolver that knows the given start→end pairs.
fn edit_blocks(before: &str, ops: &str, pairs: &'static [(usize, usize)]) -> Result<String, Error> {
    let src = format!("[a.rs#{}]\n{ops}", tag(before));
    let plan = apply(&parse(&src)?, &files(&[("a.rs", before)]), &Fake(pairs))?;
    match &plan.changes[0] {
        Change::Write { content, .. } => Ok(content.clone()),
        other => panic!("expected a write, got {other:?}"),
    }
}

#[test]
fn a_block_op_replaces_through_the_construct_it_names() {
    // `2*` covers lines 2-3; the body length is unrelated to the range.
    assert_eq!(
        edit_blocks(SRC, "PUT 2*:\n+X\n", &[(2, 3)]).unwrap(),
        "one\nX\nfour\n"
    );
    assert_eq!(
        edit_blocks(SRC, "CUT 2*\n", &[(2, 3)]).unwrap(),
        "one\nfour\n"
    );
}

#[test]
fn an_insert_after_a_block_lands_past_its_closing_line() {
    let out = edit_blocks(SRC, "PUT >2*:\n+after\n", &[(2, 3)]).unwrap();
    assert_eq!(out, "one\ntwo\nthree\nafter\nfour\n");
}

#[test]
fn a_block_and_a_range_still_may_not_overlap() {
    let err = edit_blocks(SRC, "PUT 1*:\n+a\nPUT 2.=2:\n+b\n", &[(1, 2)]).unwrap_err();
    assert!(matches!(err, Error::Overlap { .. }), "{err}");
}

#[test]
fn a_block_op_on_a_line_that_opens_nothing_is_rejected() {
    // Guessing here would rewrite code nobody looked at.
    let err = edit_blocks(SRC, "PUT 3*:\n+x\n", &[(2, 3)]).unwrap_err();
    assert!(matches!(err, Error::NoBlockAt { line: 3, .. }), "{err}");
    assert!(
        err.to_string().contains("Name the lines with `N.=M`"),
        "{err}"
    );
}

#[test]
fn a_caller_with_no_parser_reports_that_rather_than_guessing() {
    let err = edit(SRC, "PUT 1*:\n+x\n").unwrap_err();
    assert!(matches!(err, Error::NoBlockAt { .. }), "{err}");
}

#[test]
fn crossing_the_two_target_forms_is_named_as_the_stray_dot_it_is() {
    // A model that has just written `N.=M` reaches for `N.*`. Complaining that
    // the line number is missing points it at the part it got right.
    let err = parse("[f.rs#AAAA]\nCUT 50.*\n").unwrap_err();
    let said = err.to_string();
    assert!(said.contains("stray `.`"), "{said}");
    assert!(said.contains("`50*`"), "{said}");

    // The complaint it replaces still fires where it is true.
    let err = parse("[f.rs#AAAA]\nCUT abc*\n").unwrap_err();
    assert!(
        err.to_string().contains("needs a line number before `*`"),
        "{err}"
    );
    let err = parse("[f.rs#AAAA]\nCUT 0*\n").unwrap_err();
    assert!(err.to_string().contains("numbered from 1"), "{err}");

    // `>N*` carried its own copy of the same complaint, and its fix is `>50*`.
    let err = parse("[f.rs#AAAA]\nPUT >50.*:\n+x\n").unwrap_err();
    assert!(err.to_string().contains("`>50*`"), "{err}");

    // A bare zero is the one-line form, not a malformed range; the old wording
    // told the model to write what it had just written.
    for spec in ["CUT 0", "CUT 00"] {
        let err = parse(&format!("[f.rs#AAAA]\n{spec}\n")).unwrap_err();
        assert!(err.to_string().contains("numbered from 1"), "{err}");
    }
}

#[test]
fn a_missing_section_header_is_reported_with_its_line() {
    let err = parse("PUT 1.=1:\n+x\n").unwrap_err();
    assert!(matches!(err, Error::Syntax { line: 1, .. }), "{err}");
    assert!(err.to_string().contains("before any `[path#TAG]`"), "{err}");
}

#[test]
fn paths_lists_what_the_caller_must_load() {
    let src = "[a.rs#AAAA]\nREM\n[b.rs#BBBB]\nREM\n[a.rs#AAAA]\nREM\n";
    assert_eq!(parse(src).unwrap().paths(), vec!["a.rs", "b.rs"]);
}

#[test]
fn landed_reports_new_numbering_so_a_second_edit_needs_no_re_read() {
    let src = format!(
        "[a.rs#{}]\nPUT 1.=1:\n+a\n+b\n+c\nPUT >4:\n+tail\n",
        tag(SRC)
    );
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    let Change::Write {
        content, landed, ..
    } = &plan.changes[0]
    else {
        panic!()
    };

    assert_eq!(content, "a\nb\nc\ntwo\nthree\nfour\ntail\n");
    // Lines 1-3 are the replacement; line 7 is the appended tail.
    assert_eq!(
        landed,
        &vec![
            hashline::Landed { start: 1, end: 3 },
            hashline::Landed { start: 7, end: 7 }
        ]
    );
}

#[test]
fn a_verb_less_op_line_names_the_repair() {
    let err = edit(SRC, "1.=2:\n+x\n").unwrap_err().to_string();
    assert!(
        err.contains("every op line starts with PUT, CUT, MV or REM"),
        "{err}"
    );
    assert!(err.contains("did you mean `PUT 1.=2:`?"), "{err}");

    let err = edit(SRC, ">3:\n+x\n").unwrap_err().to_string();
    assert!(err.contains("did you mean `PUT >3:`?"), "{err}");
}

#[test]
fn a_line_pasted_from_read_output_is_recognized_as_such() {
    let err = edit(SRC, "2:two\n+new\n").unwrap_err().to_string();
    assert!(
        err.contains("that is a line from a read, not an op"),
        "{err}"
    );
    assert!(err.contains("`PUT N*:`"), "{err}");
}
