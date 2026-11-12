//! Built-in skills supplied by the terminal surface.

/// Instructions for the one surface a person configures in a file of its own.
pub fn builtins() -> Vec<skills::Skill> {
    vec![
        skills::Skill::builtin(include_str!("../skills/statusline/SKILL.md")).unwrap_or_else(
            || {
                let why = skills::frontmatter(include_str!("../skills/statusline/SKILL.md"))
                    .err()
                    .unwrap_or_default();
                panic!("the statusline skill is not usable: {why}")
            },
        ),
    ]
}
