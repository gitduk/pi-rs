use serde_json::json;
use skills::{Load, discover};
use tool::{Ctx, Tool, ToolError, Workspace};

// A workspace of its own, so the gate under test is this call's and not
// whoever is running the suite's.
fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

// Skills live outside the workspace, so the workspace's gate doesn't cover
// them: the skill directory is its own boundary.
#[tokio::test]
async fn a_file_argument_cannot_leave_the_skill_directory() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("skills/thinking");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: thinking\ndescription: \"d\"\n---\n\n# Thinking\n",
    )
    .unwrap();
    // A real sibling skill to escape into; a missing file would fail with
    // NotFound before the boundary check is reached.
    let neighbour = dir.path().join("skills/commit");
    std::fs::create_dir_all(&neighbour).unwrap();
    std::fs::write(neighbour.join("SKILL.md"), "# Commit\n").unwrap();
    let found = discover(&[dir.path().join("skills")]);

    let (_w, c) = ctx();
    let r = Load::new(found.skills)
        .execute(
            json!({ "name": "thinking", "file": "../commit/SKILL.md" }),
            &c,
        )
        .await;
    assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
}

// Read live: a skill written while pi runs is loadable on the next turn, and
// the generation is what tells the command table to follow.
#[test]
fn a_skill_written_later_is_on_the_shelf_at_the_next_look() {
    use tool::Source as _;
    let dir = tempfile::tempdir().unwrap();
    let shelf = skills::Shelf::new([dir.path()]);
    assert!(shelf.tools().is_empty());
    let (_, before) = shelf.now();
    assert_eq!(shelf.now().1, before, "nothing changed, nothing moves");

    let skill = dir.path().join("haiku");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: write haiku\n---\nbody\n",
    )
    .unwrap();
    let (found, after) = shelf.now();
    assert_ne!(after, before);
    assert_eq!(found.skills[0].name, "haiku");
    assert!(
        shelf.tools()[0]
            .description()
            .contains("haiku: write haiku")
    );
}

// Two directories, read in order: the first to claim a name keeps it, and
// which one is offered is the caller's order to set.
#[test]
fn the_first_directory_to_claim_a_name_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("first"), dir.path().join("second"));
    for (base, description) in [(&first, "first"), (&second, "second's")] {
        let skill = base.join("commit");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }
    let found = discover(&[first.clone(), second.clone()]);
    assert_eq!(found.skills.len(), 1, "{:?}", found.skills);
    assert_eq!(found.skills[0].dir, first.join("commit"));
    assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
    assert!(
        found.problems[0].contains("already defined by"),
        "{:?}",
        found.problems
    );
}

#[tokio::test]
async fn loaded_skill_instructions_name_the_source_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review\ndescription: Review code.\n---\nRead the changed code.\n",
    )
    .unwrap();
    std::fs::write(skill_dir.join("checks.txt"), "Check changed interfaces.").unwrap();
    let found = discover(&[dir.path().to_path_buf()]);
    let (_w, c) = ctx();
    let loader = Load::new(found.skills);
    let out = loader
        .execute(json!({ "name": "review" }), &c)
        .await
        .unwrap();
    let text = out
        .content
        .iter()
        .filter_map(|c| match c {
            llm::message::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.starts_with(&format!(
        "Skill: `review`\nSource: {}\n\n",
        skill_dir.join("SKILL.md").display(),
    )));
    assert!(text.contains("Read the changed code."));
    assert!(text.contains("checks.txt"));
    assert!(!text.contains("description:"));
    let reference = loader
        .execute(json!({ "name": "review", "file": "checks.txt" }), &c)
        .await
        .unwrap();
    assert!(matches!(
        &reference.content[..],
        [llm::message::ToolResultContent::Text(t)] if t.text == "Check changed interfaces."
    ));
}

fn write_skill(dir: &std::path::Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), text).unwrap();
}

fn text_of(out: &tool::ToolOutput) -> String {
    out.content
        .iter()
        .filter_map(|c| match c {
            llm::message::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_body_is_what_follows_the_header() {
    assert_eq!(skills::body("---\nname: a\n---\n\nhello\n"), "hello\n");
    assert_eq!(skills::body("no header"), "no header");
}

#[test]
fn a_builtin_needs_a_usable_name_and_a_description() {
    const TEXT: &str = "---\nname: helper\ndescription: helps\n---\nbody\n";
    let skill = skills::Skill::builtin(TEXT).unwrap();
    assert_eq!(
        (skill.name.as_str(), skill.description.as_str()),
        ("helper", "helps")
    );
    assert_eq!(skill.text().unwrap(), TEXT);
    assert_eq!(skills::Skill::builtin("---\ndescription: d\n---\n"), None);
    assert_eq!(
        skills::Skill::builtin("---\nname: a/b\ndescription: d\n---\n"),
        None
    );
}

#[test]
fn a_skill_on_disk_reads_its_text_from_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let text = "---\ndescription: d\n---\nbody\n";
    write_skill(&dir.path().join("disk"), text);
    let found = discover(&[dir.path().to_path_buf()]);
    assert_eq!(found.skills[0].text().unwrap(), text);
}

// Folded to one line of at most 1000 bytes, cut only between words, except
// a single word too long to fit, which is cut where the limit falls.
#[test]
fn a_description_is_folded_and_capped_at_word_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ("a".repeat(500), "b".repeat(499));
    write_skill(
        &dir.path().join("fits"),
        &format!("---\ndescription: \"{a}\\n  {b} c\"\n---\n"),
    );
    // The space between them is the byte that does not fit.
    write_skill(
        &dir.path().join("spills"),
        &format!("---\ndescription: {a} {}\n---\n", "b".repeat(500)),
    );
    write_skill(
        &dir.path().join("long"),
        &format!("---\ndescription: {} d\n---\n", "x".repeat(1001)),
    );
    let found = discover(&[dir.path().to_path_buf()]);
    let described = |name: &str| {
        let skill = found.skills.iter().find(|s| s.name == name).unwrap();
        skill.description.clone()
    };
    assert_eq!(described("fits"), format!("{a} {b}"));
    assert_eq!(described("spills"), a);
    assert_eq!(described("long"), "x".repeat(1000));
}

// One past the read limit is reported; exactly at it is read.
#[test]
fn a_skill_md_over_the_read_limit_is_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    for (name, len) in [
        ("at", tool::limit::MAX_BYTES),
        ("over", tool::limit::MAX_BYTES + 1),
    ] {
        let skill = dir.path().join(name);
        write_skill(&skill, "---\ndescription: d\n---\n");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(skill.join("SKILL.md"))
            .unwrap();
        file.set_len(len).unwrap();
    }
    let found = discover(&[dir.path().to_path_buf()]);
    let names: Vec<_> = found.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["at"]);
    assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
    assert!(found.problems[0].contains("over"), "{:?}", found.problems);
}

// Skills are looked for three directories below each skills directory; the
// shelf's change check looks exactly as deep, so a deeper one moves nothing.
#[test]
fn skills_sit_at_most_three_directories_deep() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(&dir.path().join("a/b/three"), "---\ndescription: d\n---\n");
    let shelf = skills::Shelf::new([dir.path()]);
    let (found, before) = shelf.now();
    let names: Vec<_> = found.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["three"]);

    write_skill(&dir.path().join("a/b/c/four"), "---\ndescription: d\n---\n");
    assert_eq!(discover(&[dir.path().to_path_buf()]).skills.len(), 1);
    assert_eq!(shelf.now().1, before);
}

#[test]
fn a_builtin_is_offered_unless_a_skill_on_disk_has_its_name() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(&dir.path().join("taken"), "---\ndescription: disk\n---\n");
    let builtin = |text: &'static str| skills::Skill::builtin(text).unwrap();
    let shelf = skills::Shelf::new([dir.path()])
        .with_builtin(builtin("---\nname: taken\ndescription: builtin\n---\n"))
        .with_builtin(builtin("---\nname: free\ndescription: builtin\n---\n"));
    let found = shelf.now().0;
    let seen: Vec<_> = found
        .skills
        .iter()
        .map(|s| (s.name.as_str(), s.description.as_str()))
        .collect();
    assert_eq!(seen, [("free", "builtin"), ("taken", "disk")]);
}

#[tokio::test]
async fn a_builtin_skill_serves_only_its_compiled_in_files() {
    let skill = |files: Vec<(String, &'static str)>| {
        skills::Skill::builtin("---\nname: kit\ndescription: d\n---\nbody\n")
            .unwrap()
            .with_files(files)
    };
    let (_w, c) = ctx();
    let with_files = Load::new(vec![skill(vec![("ref.md".into(), "reference")])]);
    let out = with_files
        .execute(json!({ "name": "kit", "file": "ref.md" }), &c)
        .await
        .unwrap();
    assert_eq!(text_of(&out), "reference");
    let missing = with_files
        .execute(json!({ "name": "kit", "file": "other.md" }), &c)
        .await;
    assert!(
        matches!(&missing, Err(ToolError::Invalid(m)) if m.contains("Available: ref.md")),
        "{missing:?}"
    );

    let bare = Load::new(vec![skill(Vec::new())]);
    let none = bare
        .execute(json!({ "name": "kit", "file": "ref.md" }), &c)
        .await;
    assert!(
        matches!(&none, Err(ToolError::Invalid(m)) if m.contains("has no files")),
        "{none:?}"
    );
}

// The read limit holds when loading, too: a file that grew past it since
// discovery is refused, one exactly at it is served.
#[tokio::test]
async fn loading_refuses_files_past_the_read_limit() {
    use tool::limit::MAX_BYTES;
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("big");
    write_skill(&skill, "---\ndescription: d\n---\n");
    std::fs::write(skill.join("ref.txt"), "").unwrap();
    let loader = Load::new(discover(&[dir.path().to_path_buf()]).skills);
    let (_w, c) = ctx();
    let grow = |path: std::path::PathBuf, len: u64| {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_len(len).unwrap();
    };

    for (args, path) in [
        (json!({ "name": "big" }), skill.join("SKILL.md")),
        (
            json!({ "name": "big", "file": "ref.txt" }),
            skill.join("ref.txt"),
        ),
    ] {
        grow(path.clone(), MAX_BYTES);
        let at = text_of(&loader.execute(args.clone(), &c).await.unwrap());
        assert!(!at.contains("read limit"), "{path:?}");
        grow(path.clone(), MAX_BYTES + 1);
        let over = text_of(&loader.execute(args, &c).await.unwrap());
        assert!(over.contains("read limit"), "{path:?}: {over}");
    }
}
