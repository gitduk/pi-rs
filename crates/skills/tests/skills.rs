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
