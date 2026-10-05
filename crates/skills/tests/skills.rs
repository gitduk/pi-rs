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
    let found = discover(&dir.path().join("skills"));

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
    let shelf = skills::Shelf::new(dir.path());
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
