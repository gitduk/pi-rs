use std::collections::HashMap;

use hashline::{
    Blocks, Change, Error, LinePos, NoBlocks, Op, Plan, Target, apply, first_changed_line,
    first_shifted_line, header, parse, tag, unified_patch,
};

// Explicit `naming row → extent` pairs. A real parser answers only for rows
// that open something, and the extent it answers with may start above the row
// named. hashline never parses source itself, so its own tests should not
// either.
struct Fake(&'static [(usize, (usize, usize))]);

impl Blocks for Fake {
    fn extent_of(&self, _path: &str, _content: &str, line: usize) -> Option<(usize, usize)> {
        self.0.iter().find(|(at, _)| *at == line).map(|(_, e)| *e)
    }
}

fn files<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
    pairs.iter().copied().collect()
}

// Apply `src` to a single file and return the new content.
fn edit(before: &str, ops: &str) -> Result<String, Error> {
    let src = format!("[a.rs#{}]\n{ops}", tag(before));
    let patch = parse(&src)?;
    let plan = apply(&patch, &files(&[("a.rs", before)]), &NoBlocks)?;
    match &plan.changes[0] {
        Change::Write { content, .. } => Ok(content.clone()),
        other => panic!("expected a write, got {other:?}"),
    }
}

// Apply `ops` to a single file and return the whole plan, hunks and all.
fn plan_for(before: &str, ops: &str) -> Plan {
    let src = format!("[a.rs#{}]\n{ops}", tag(before));
    apply(&parse(&src).unwrap(), &files(&[("a.rs", before)]), &NoBlocks).unwrap()
}

// The unified patch `ops` produces against `before`, as a string.
fn patch_of(before: &str, ops: &str) -> String {
    let plan = plan_for(before, ops);
    unified_patch(&plan.changes, &files(&[("a.rs", before)]))
}

const SRC: &str = "one\ntwo\nthree\nfour\n";

#[test]
fn replaces_an_inclusive_range() {
    assert_eq!(edit(SRC, "PUT 2-3:\n+TWO\n").unwrap(), "one\nTWO\nfour\n");
}

#[test]
fn body_length_is_independent_of_range_length() {
    assert_eq!(
        edit(SRC, "PUT 2:\n+a\n+b\n+c\n").unwrap(),
        "one\na\nb\nc\nthree\nfour\n"
    );
    assert_eq!(edit(SRC, "PUT 2-3:\n").unwrap(), "one\nfour\n");
}

#[test]
fn a_bare_plus_is_a_blank_line_and_whitespace_is_verbatim() {
    assert_eq!(
        edit(SRC, "PUT 2:\n+\n+    indented\n").unwrap(),
        "one\n\n    indented\nthree\nfour\n"
    );
}

#[test]
fn inserts_at_the_head_and_the_tail() {
    assert_eq!(
        edit(SRC, "PUT 1:UP:\n+zero\n").unwrap(),
        "zero\none\ntwo\nthree\nfour\n"
    );
    assert_eq!(
        edit(SRC, "PUT 4:DOWN:\n+five\n").unwrap(),
        "one\ntwo\nthree\nfour\nfive\n"
    );
    assert_eq!(
        edit(SRC, "PUT 2:DOWN:\n+2.5\n").unwrap(),
        "one\ntwo\n2.5\nthree\nfour\n"
    );
}

#[test]
fn later_hunks_keep_their_original_numbering() {
    // If the first hunk shifted the rest, `4-4` would land on the wrong line.
    let out = edit(SRC, "PUT 1:\n+a\n+b\n+c\nPUT 4:\n+FOUR\n").unwrap();
    assert_eq!(out, "a\nb\nc\ntwo\nthree\nFOUR\n");
}

#[test]
fn a_stale_tag_is_rejected_before_anything_is_built() {
    let src = "[a.rs#0000]\nPUT 1:\n+x\n";
    let patch = parse(src).unwrap();
    let err = apply(&patch, &files(&[("a.rs", SRC)]), &NoBlocks).unwrap_err();
    assert!(matches!(err, Error::StaleTag { .. }), "{err}");
    assert!(err.to_string().contains("Re-read it"), "{err}");
}

#[test]
fn one_stale_section_rejects_the_whole_patch() {
    let src = format!(
        "[a.rs#{}]\nPUT 1:\n+A\n[b.rs#0000]\nPUT 1:\n+B\n",
        tag(SRC)
    );
    let patch = parse(&src).unwrap();
    // a.rs is valid, but a partly-applied patch is worse than a rejected one.
    assert!(apply(&patch, &files(&[("a.rs", SRC), ("b.rs", SRC)]), &NoBlocks).is_err());
}

#[test]
fn overlapping_ranges_are_rejected() {
    let err = edit(SRC, "PUT 1-2:\n+a\nPUT 2-3:\n+b\n").unwrap_err();
    assert!(matches!(err, Error::Overlap { overlap: 2, .. }), "{err}");
    assert!(
        err.to_string().contains("may never touch the same one"),
        "{err}"
    );
}

#[test]
fn an_insertion_buried_in_a_replaced_span_is_rejected() {
    let err = edit(SRC, "PUT 1-3:\n+a\nPUT 2:DOWN:\n+stray\n").unwrap_err();
    assert!(matches!(err, Error::Overlap { .. }), "{err}");
}

#[test]
fn out_of_range_names_the_real_length() {
    let err = edit(SRC, "PUT 9:\n+x\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "a.rs has 4 lines, so 9 names lines that do not exist"
    );
}

#[test]
fn cut_and_paste_moves_lines_within_a_file() {
    let out = edit(SRC, "CUT 1 @first\nPUT 4:DOWN @first\n").unwrap();
    assert_eq!(out, "two\nthree\nfour\none\n");
}

#[test]
fn an_unlabeled_cut_feeds_the_anonymous_register() {
    assert_eq!(
        edit(SRC, "CUT 1\nPUT 3:DOWN @\n").unwrap(),
        "two\nthree\none\nfour\n"
    );
}

#[test]
fn a_register_flows_between_files() {
    let a = "keep\nmoveme\n";
    let b = "target\n";
    let src = format!(
        "[a.rs#{}]\nCUT 2 @fn\n[b.rs#{}]\nPUT 1:UP @fn\n",
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
            // A cut gives nothing, so `end` sits below `start`. What it took
            // is the whole of what happened, and is reported as such.
            landed: vec![hashline::Landed {
                start: 2,
                end: 1,
                took: vec!["moveme".into()],
                took_at: 2,
            }]
        }
    );
    assert_eq!(
        plan.changes[1],
        Change::Write {
            path: "b.rs".into(),
            content: "moveme\ntarget\n".into(),
            landed: vec![hashline::Landed {
                start: 1,
                end: 1,
                took: vec![],
                took_at: 1,
            }],
        }
    );
}

#[test]
fn pasting_an_unfilled_register_is_an_error() {
    let err = edit(SRC, "PUT 1:DOWN @nope\n").unwrap_err();
    assert!(matches!(err, Error::UnknownRegister { .. }), "{err}");
}

#[test]
fn rem_deletes_and_refuses_company() {
    let src = format!("[a.rs#{}]\nRM\n", tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    assert_eq!(
        plan.changes[0],
        Change::Remove {
            path: "a.rs".into()
        }
    );

    let src = format!("[a.rs#{}]\nRM\nPUT 1:\n+x\n", tag(SRC));
    let err = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap_err();
    assert!(matches!(err, Error::RemoveWithOps { .. }), "{err}");
}

#[test]
fn mv_carries_the_edited_content_to_the_destination() {
    let src = format!("[a.rs#{}]\nPUT 1:\n+ONE\nMV lib/a.rs\n", tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    assert_eq!(
        plan.changes[0],
        Change::Rename {
            from: "a.rs".into(),
            to: "lib/a.rs".into(),
            content: "ONE\ntwo\nthree\nfour\n".into(),
            landed: vec![hashline::Landed {
                start: 1,
                end: 1,
                took: vec!["one".into()],
                took_at: 1,
            }],
        }
    );
}

#[test]
fn a_file_without_a_trailing_newline_keeps_it_that_way() {
    assert_eq!(edit("a\nb", "PUT 1:\n+A\n").unwrap(), "A\nb");
    assert_eq!(edit("a\nb\n", "PUT 1:\n+A\n").unwrap(), "A\nb\n");
}

#[test]
fn an_empty_file_accepts_a_head_insert() {
    // `1:UP` is the one insertion a file with no lines has, and the reason
    // `$` was never needed: every other position names a line, and there
    // are none.
    assert_eq!(edit("", "PUT 1:UP:\n+first\n").unwrap(), "first\n");
    assert!(edit("", "PUT 1:DOWN:\n+first\n").is_err());
}

#[test]
fn unified_diff_habits_are_named_rather_than_guessed_at() {
    let err = edit(SRC, "PUT 1:\n+new\n-one\n").unwrap_err().to_string();
    assert!(err.contains("is not a deletion"), "{err}");
    // Both ways out, because the row is written by a model that wants one of
    // them: a deletion, or a literal line that begins with `-`.
    assert!(err.contains("CUT N-M"), "{err}");
    assert!(err.contains("`+- item`"), "{err}");
}

#[test]
fn a_single_line_is_written_without_suffix() {
    // `N` is the one address that takes no suffix: `PUT 2:` replaces one
    // line, and `N-N` — the old spelling of a single line — is refused with
    // the shorter form named, so the model repairs it in one turn.
    assert_eq!(edit(SRC, "PUT 2:\n+B\n").unwrap(), "one\nB\nthree\nfour\n");
    assert_eq!(edit(SRC, "CUT 2\n").unwrap(), "one\nthree\nfour\n");

    let err = edit(SRC, "PUT 2-2:\n+B\n").unwrap_err().to_string();
    assert!(err.contains("a single line is `2`"), "{err}");
    assert!(err.contains("`PUT 2-2`"), "{err}");
}

#[test]
fn a_trailing_colon_on_a_bodyless_op_is_tolerated() {
    // A `:` on a `CUT` is noise. Refusing it put the complaint on the colon and
    // hid the row underneath, which is the mistake that actually matters.
    assert_eq!(edit(SRC, "CUT 2:\n").unwrap(), "one\nthree\nfour\n");
    assert_eq!(edit(SRC, "CUT 2 @held:\n").unwrap(), "one\nthree\nfour\n");

    let err = edit(SRC, "CUT 2:\n-  two\n").unwrap_err().to_string();
    assert!(err.contains("take no body rows"), "{err}");
    assert!(err.contains("PUT N-M:"), "{err}");
}

#[test]
fn a_plus_row_under_a_bodyless_op_says_which_op() {
    let err = edit(SRC, "CUT 2\n+two\n").unwrap_err().to_string();
    assert!(err.contains("take no body rows"), "{err}");
}

// Apply `ops` with a resolver that knows the given start→end pairs.
fn edit_blocks(
    before: &str,
    ops: &str,
    pairs: &'static [(usize, (usize, usize))],
) -> Result<String, Error> {
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
        edit_blocks(SRC, "PUT 2*:\n+X\n", &[(2, (2, 3))]).unwrap(),
        "one\nX\nfour\n"
    );
    assert_eq!(
        edit_blocks(SRC, "CUT 2*\n", &[(2, (2, 3))]).unwrap(),
        "one\nfour\n"
    );
}

#[test]
fn a_context_row_under_a_body_is_told_to_widen_not_to_delete() {
    // The sequence this replaces: the row was called invalid, the model
    // deleted it, and the next patch failed on the brace it had just dropped.
    let err = edit(SRC, "PUT 2:\n+TWO\nthree\n").unwrap_err().to_string();
    assert!(err.contains("widen the address"), "{err}");
    // A row pasted out of a view is a different mistake and keeps its own
    // advice; nothing was open above it to make it look like context.
    let err = edit(SRC, "2: two\n").unwrap_err().to_string();
    assert!(err.contains("a line from a read"), "{err}");
    assert!(!err.contains("widen the address"), "{err}");
}

#[test]
fn a_block_op_covers_the_annotations_the_resolver_gave_it() {
    // Naming the construct's own row, not its doc comment's: the resolver
    // reports the whole extent and the op must take all of it. Keeping only
    // the end left the annotations in place, and a body that carried its own
    // wrote them twice.
    const ANNOTATED: &str = "///doc\nfn f() {\n}\ntail\n";
    assert_eq!(
        edit_blocks(ANNOTATED, "PUT 2*:\n+///new\n+fn f() {}\n", &[(2, (1, 3))]).unwrap(),
        "///new\nfn f() {}\ntail\n"
    );
    // And the same extent whichever of its rows is named.
    assert_eq!(
        edit_blocks(ANNOTATED, "CUT 1*\n", &[(1, (1, 3))]).unwrap(),
        "tail\n"
    );
}

#[test]
fn a_block_op_that_widens_onto_another_hunk_is_an_overlap() {
    // The start moving up is what makes this reachable: `2*` looks clear of
    // line 1 until the resolver reports the annotation above it.
    let err = edit_blocks(SRC, "PUT 1:\n+a\nPUT 2*:\n+b\n", &[(2, (1, 3))]).unwrap_err();
    assert!(matches!(err, Error::Overlap { .. }), "{err}");
}

#[test]
fn an_insert_after_a_range_is_anchored_where_it_was_written() {
    // `took_at` is the original line a hunk acted on, and two consumers read it
    // as one: the shift boundary and the address a refusal prints back. A
    // `:DOWN` sharing its anchor with the range's end is the case that named
    // the range's start instead, reporting a shift above where anything moved.
    let src = "[a.rs#(tag)]\nPUT 2-4:\n+A\n+B\n+C\nPUT 4:DOWN\n+tail\n"
        .replace("(tag)", &tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    let Change::Write { landed, .. } = &plan.changes[0] else {
        panic!("expected a write")
    };
    let inserted = landed.last().expect("the insertion");
    assert_eq!(inserted.took_at, 4, "anchored at the range's end, not its start");
    // The replace is one-for-one, so only the insertion moves anything.
    assert_eq!(first_shifted_line(&plan.changes), Some(4));
}

#[test]
fn a_shift_is_reported_only_when_numbering_actually_moved() {
    let plan = |ops: &str| {
        let src = format!("[a.rs#{}]\n{ops}", tag(SRC));
        apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap()
    };
    // One-for-one: every address the model holds is still good.
    assert_eq!(first_shifted_line(&plan("PUT 2:\n+TWO\n").changes), None);
    // Original numbering, so it can be compared with what the model wrote.
    assert_eq!(
        first_shifted_line(&plan("PUT 2:\n+A\n+B\n").changes),
        Some(2)
    );
    assert_eq!(first_shifted_line(&plan("CUT 3\n").changes), Some(3));
    assert_eq!(
        first_shifted_line(&plan("PUT 1:\n+ONE\nPUT 3:DOWN\n+x\n").changes),
        Some(3)
    );
}

#[test]
fn an_insert_after_a_block_lands_past_its_closing_line() {
    let out = edit_blocks(SRC, "PUT 2*:DOWN:\n+after\n", &[(2, (2, 3))]).unwrap();
    assert_eq!(out, "one\ntwo\nthree\nafter\nfour\n");
}

#[test]
fn a_block_and_a_range_still_may_not_overlap() {
    let err = edit_blocks(SRC, "PUT 1*:\n+a\nPUT 2:\n+b\n", &[(1, (1, 2))]).unwrap_err();
    assert!(matches!(err, Error::Overlap { .. }), "{err}");
}

#[test]
fn a_block_op_on_a_line_that_opens_nothing_is_rejected() {
    // Guessing here would rewrite code nobody looked at.
    let err = edit_blocks(SRC, "PUT 3*:\n+x\n", &[(2, (2, 3))]).unwrap_err();
    assert!(matches!(err, Error::NoBlockAt { line: 3, .. }), "{err}");
    assert!(
        err.to_string().contains("Name the lines with `N-M`"),
        "{err}"
    );
}

#[test]
fn a_caller_with_no_parser_reports_that_rather_than_guessing() {
    let err = edit(SRC, "PUT 1*:\n+x\n").unwrap_err();
    assert!(matches!(err, Error::NoBlockAt { .. }), "{err}");
}

#[test]
fn a_spelling_this_grammar_dropped_is_simply_not_an_address() {
    // No transitional advice per old form. Every one of these is answered by
    // the same sentence, which names the whole grammar — and nothing has to be
    // deleted later once no model writes them.
    let says = |ops: &str| {
        parse(&format!("[f.rs#AAAA]\n{ops}\n"))
            .unwrap_err()
            .to_string()
    };
    for ops in [
        "CUT 2.=3",    // the old range separator
        "PUT <2:\n+x", // the old prefix positions
        "PUT >2:\n+x",
        "PUT >2*:\n+x",
        "PUT >$:\n+x", // the old file tail
        "CUT 2.*",     // the two old forms crossed
        "PUT 2<:\n+x", // the old gaps after the number
        "PUT 2>:\n+x",
        "PUT 2*>:\n+x",
        "CUT 2>",
        "CUT 2:UP",    // direction lives on PUT; CUT takes lines or a block
        "CUT abc",     // never valid under either
    ] {
        assert!(
            says(ops).contains("an address is"),
            "{ops}: {}",
            says(ops)
        );
    }

    // Complaints that are about the number rather than the shape stay their
    // own: naming the grammar would not help a zero or a backwards range.
    for ops in ["CUT 0-1", "CUT 00-1", "CUT 0<"] {
        assert!(
            says(ops).contains("numbered from 1"),
            "{ops}: {}",
            says(ops)
        );
    }
    assert!(says("CUT 3-2").contains("runs backwards"));
    assert!(says("CUT 3-").contains("needs both ends"));
}

#[test]
fn what_the_grammar_prints_is_what_the_grammar_reads() {
    // Every address form there is, and the direction forms PUT teaches on top.
    // A view renders addresses through `Display` and the model hands them
    // straight back to `parse`, so the two being inverse is the property, not
    // a coincidence two files maintain.
    let cases = [
        Target::Range { start: 3, end: 7 },
        Target::Range { start: 3, end: 3 },
        Target::Block { line: 3 },
    ];
    for want in cases {
        let shown = want.to_string();
        // The door a view uses to ask "is this one of mine".
        assert_eq!(Target::read(&shown), Some(want), "`{shown}` did not read back");
        let patch = parse(&format!("[f.rs#AAAA]\nCUT {shown}\n"))
            .unwrap_or_else(|e| panic!("`{shown}` does not parse: {e}"));
        let got = match &patch.sections[0].ops[0] {
            Op::Cut { target, .. } => *target,
            other => panic!("`{shown}` parsed as {other:?}"),
        };
        assert_eq!(got, want, "`{shown}` round-tripped to something else");
    }

    // Direction reads back to the site it names: above/below a line, past
    // either edge of a range, above a block or past where it closes.
    for (spec, kind, n) in [
        ("3:UP", "before", 3),
        ("3:DOWN", "after", 3),
        ("2-4:UP", "before", 2),
        ("2-4:DOWN", "after", 4),
        ("2*:UP", "before", 2),
        ("4*:DOWN", "afterblock", 4),
    ] {
        let patch = parse(&format!("[f.rs#AAAA]\nPUT {spec}:\n+x\n")).unwrap();
        let ok = match &patch.sections[0].ops[0] {
            Op::InsertBefore { line, .. } => kind == "before" && *line == n,
            Op::InsertAfter { at, .. } => match (kind, at) {
                ("after", LinePos::At(m)) => *m == n,
                ("afterblock", LinePos::AfterBlock(m)) => *m == n,
                _ => false,
            },
            _ => false,
        };
        assert!(ok, "`PUT {spec}:` parsed to the wrong site");
    }
}

#[test]
fn a_put_direction_is_up_down_or_nothing() {
    let err = edit(SRC, "PUT 2:LEFT:\n+x\n").unwrap_err().to_string();
    assert!(
        err.contains("after the colon, expected `UP`, `DOWN` or nothing"),
        "{err}"
    );
    assert!(edit(SRC, "PUT 2:UP:\n+up\n").is_ok());
    assert!(edit(SRC, "PUT 2:DOWN:\n+down\n").is_ok());
}
#[test]
fn a_register_pastes_at_every_address_that_takes_a_body() {
    let says = |ops: &str| parse(&format!("[f.rs#AAAA]\n{ops}\n"));
    for spec in [
        "PUT 1:UP @h",
        "PUT 4:DOWN @h",
        "PUT 1 @h",
        "PUT 2*:DOWN @h",
        "PUT 2* @h",
    ] {
        assert!(says(&format!("CUT 2-3 @h\n{spec}")).is_ok(), "{spec}");
    }
}

#[test]
fn a_missing_section_header_is_reported_with_its_line() {
    let err = parse("PUT 1:\n+x\n").unwrap_err();
    assert!(matches!(err, Error::Syntax { line: 1, .. }), "{err}");
    assert!(err.to_string().contains("before any `[path#TAG]`"), "{err}");
}

#[test]
fn paths_lists_what_the_caller_must_load() {
    let src = "[a.rs#AAAA]\nRM\n[b.rs#BBBB]\nRM\n[a.rs#AAAA]\nRM\n";
    assert_eq!(parse(src).unwrap().paths(), vec!["a.rs", "b.rs"]);
}

#[test]
fn landed_reports_new_numbering_so_a_second_edit_needs_no_re_read() {
    let src = format!(
        "[a.rs#{}]\nPUT 1:\n+a\n+b\n+c\nPUT 4:DOWN:\n+tail\n",
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
            hashline::Landed {
                start: 1,
                end: 3,
                took: vec!["one".into()],
                took_at: 1,
            },
            hashline::Landed {
                start: 7,
                end: 7,
                took: vec![],
                took_at: 4,
            }
        ]
    );
}

#[test]
fn a_later_hunk_numbers_its_displaced_lines_in_the_original_file() {
    // The first hunk puts two lines where one stood, so the second hunk's
    // rows sit one line lower in the new file than they did in the old one.
    // `took_at` keeps the original numbering; `start` is where they are now.
    let src = format!("[a.rs#{}]\nPUT 1:\n+a\n+b\nCUT 3:\n", tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    let Change::Write { landed, .. } = &plan.changes[0]
    else {
        panic!()
    };
    assert_eq!(landed[1].took, vec!["three"]);
    assert_eq!(landed[1].took_at, 3);
    assert_eq!(landed[1].start, 4);
}

#[test]
fn a_verb_less_op_line_names_the_repair() {
    let err = edit(SRC, "1-2:\n+x\n").unwrap_err().to_string();
    assert!(
        err.contains("every op line starts with PUT, CUT, MV or RM"),
        "{err}"
    );
    assert!(err.contains("did you mean `PUT 1-2:`?"), "{err}");

    let err = edit(SRC, "3*:\n+x\n").unwrap_err().to_string();
    assert!(err.contains("did you mean `PUT 3*:`?"), "{err}");
}
#[test]
fn the_old_rem_spelling_names_the_new_verb() {
    let err = edit(SRC, "REM\n").unwrap_err().to_string();
    assert!(
        err.contains("every op line starts with PUT, CUT, MV or RM"),
        "{err}"
    );
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

#[test]
fn crlf_files_keep_their_ending_and_new_rows_join_with_it() {
    let before = "one\r\ntwo\r\nthree\r\n";
    assert_eq!(
        edit(before, "PUT 2:\n+TWO\n").unwrap(),
        "one\r\nTWO\r\nthree\r\n"
    );
    assert_eq!(
        edit(before, "PUT 1:UP:\n+zero\n").unwrap(),
        "zero\r\none\r\ntwo\r\nthree\r\n"
    );
    assert_eq!(
        edit(before, "CUT 2 @x\nPUT 1:UP: @x\n").unwrap(),
        "two\r\none\r\nthree\r\n"
    );
}

#[test]
fn a_crlf_file_without_a_trailing_newline_keeps_it_that_way() {
    assert_eq!(
        edit("one\r\ntwo", "PUT 1:\n+ONE\n").unwrap(),
        "ONE\r\ntwo"
    );
}

#[test]
fn unified_patch_is_standard_and_carries_real_line_numbers() {
    let plan = plan_for(SRC, "PUT 2-3:\n+TWO\n+THREE\n");
    assert_eq!(
        patch_of(SRC, "PUT 2-3:\n+TWO\n+THREE\n"),
        "--- a/a.rs\n+++ b/a.rs\n@@ -1,4 +1,4 @@\n one\n-two\n-three\n+TWO\n+THREE\n four\n"
    );
    assert_eq!(first_changed_line(&plan.changes), Some(2));
}

#[test]
fn unified_patch_keeps_neighbouring_hunks_from_sharing_context() {
    // The head insertion takes lines 1-3 as context; the deletion then has no
    // context left before it, so it names only its own row.
    assert_eq!(
        patch_of(SRC, "PUT 1:UP:\n+zero\nCUT 4:\n"),
        "--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,4 @@\n+zero\n one\n two\n three\n@@ -4,1 +4,0 @@\n-four\n"
    );
    assert_eq!(first_changed_line(&plan_for(SRC, "PUT 1:UP:\n+zero\nCUT 4:\n").changes), Some(1));
}

#[test]
fn unified_patch_skips_hunks_that_changed_nothing_and_deleted_files() {
    assert_eq!(patch_of(SRC, "PUT 2:\n+two\n"), "");
    assert_eq!(first_changed_line(&plan_for(SRC, "PUT 2:\n+two\n").changes), None);

    let removed = format!("[a.rs#{}]\nRM\n", tag(SRC));
    let plan = apply(&parse(&removed).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    assert_eq!(unified_patch(&plan.changes, &files(&[("a.rs", SRC)])), "");
    assert_eq!(first_changed_line(&plan.changes), None);
}

#[test]
fn unified_patch_names_both_sides_of_a_rename() {
    let src = format!("[a.rs#{}]\nPUT 1:\n+ONE\nMV lib/a.rs\n", tag(SRC));
    let plan = apply(&parse(&src).unwrap(), &files(&[("a.rs", SRC)]), &NoBlocks).unwrap();
    assert_eq!(
        unified_patch(&plan.changes, &files(&[("a.rs", SRC)])),
        "--- a/a.rs\n+++ b/lib/a.rs\n@@ -1,4 +1,4 @@\n-one\n+ONE\n two\n three\n four\n"
    );
}

#[test]
fn first_changed_line_anchors_a_pure_deletion_before_it() {
    assert_eq!(first_changed_line(&plan_for(SRC, "CUT 4:\n").changes), Some(3));
    assert_eq!(first_changed_line(&plan_for(SRC, "CUT 1:\n").changes), Some(1));
}

#[test]
fn what_prints_a_header_and_what_reads_one_are_the_same_grammar() {
    // Seven views printed this shape by hand, and the model has to copy one
    // back into a patch for an edit to land at all. A printer that drifts from
    // this parser breaks editing with nothing to show for it.
    for path in ["a.rs", "crates/x/src/lib.rs", "a b/c#d.rs"] {
        let src = format!("{}\nPUT 1:\n+X\n", header(path, &tag(SRC)));
        let patch = parse(&src).unwrap_or_else(|e| panic!("`{path}`: {e}"));
        assert_eq!(patch.sections[0].path, path);
        assert_eq!(patch.sections[0].tag, tag(SRC));
    }
}
