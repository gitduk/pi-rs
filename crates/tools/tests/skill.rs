mod common;

use common::ctx;
use serde_json::json;
use tools::skills::discover_from;
use tools::{Tool, ToolError};

// Skills live outside the workspace, so the workspace's gate does not cover
// them: the skill directory is its own boundary, and a `file:` argument
// cannot walk out of it.
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
    let found = discover_from(&[dir.path().join("skills")]);

    let (_w, c) = ctx();
    let r = tools::skill::SkillTool::new(found.skills)
        .execute(
            json!({ "name": "thinking", "file": "../commit/SKILL.md" }),
            &c,
        )
        .await;
    assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
}
