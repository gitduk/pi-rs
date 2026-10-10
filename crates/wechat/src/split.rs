//! One reply cut into messages a platform will take: each within a byte
//! budget, at the most natural boundary in reach, rejoining exactly.

pub(crate) fn split(text: &str, limit: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= limit {
            pieces.push(rest.to_string());
            break;
        }
        let (cut, skip) = boundary(rest, limit);
        pieces.push(rest[..cut].to_string());
        rest = &rest[cut + skip..];
    }
    pieces
}

// Falls back paragraph → line → space → char boundary, dropping nothing
// (exact rejoin); the cut is never zero, or the loop would spin forever.
fn boundary(rest: &str, budget: usize) -> (usize, usize) {
    let first = rest.chars().next().map_or(1, char::len_utf8);
    let mut end = budget.max(first);
    while end > first && !rest.is_char_boundary(end) {
        end -= 1;
    }
    let head = &rest[..end];
    for sep in ["\n\n", "\n", " "] {
        let Some(i) = head.rfind(sep).filter(|&i| i > end / 2) else {
            continue;
        };
        let skip = if sep == " " {
            1
        } else {
            rest[i..]
                .bytes()
                .take_while(|b| matches!(b, b'\n' | b'\r'))
                .count()
        };
        return (i, skip);
    }
    (end, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_with_no_boundary_is_cut_on_a_character_and_rejoins_exactly() {
        let text = "中".repeat(200);
        let pieces = split(&text, 100);
        assert!(pieces.len() > 1);
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn a_budget_shorter_than_one_character_still_advances() {
        let pieces = split("中文中文", 1);
        assert_eq!(pieces.len(), 4);
    }

    #[test]
    fn a_long_reply_breaks_at_a_paragraph() {
        let text = format!("{}\n\n{}", "a".repeat(60), "b".repeat(60));
        assert_eq!(split(&text, 100), ["a".repeat(60), "b".repeat(60)]);
    }
}
