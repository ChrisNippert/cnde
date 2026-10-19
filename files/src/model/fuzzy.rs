//! Fuzzy name matching for the search box.

/// Case-insensitive subsequence match. `None` if some query char is missing;
/// otherwise higher is a better match.
pub fn fuzzy_score(query: &str, name: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = name.chars().collect();
    let mut score = 0i32;
    let mut qi = 0;
    let mut last: Option<usize> = None;
    for (i, &c) in chars.iter().enumerate() {
        if qi == q.len() {
            break;
        }
        if !c.to_lowercase().eq(std::iter::once(q[qi])) {
            continue;
        }
        score += 10;
        match last {
            Some(l) if l + 1 == i => score += 15,
            Some(l) => score -= (i - l - 1).min(8) as i32,
            None => score -= i.min(8) as i32,
        }
        let boundary = i == 0
            || matches!(chars[i - 1], '-' | '_' | '.' | ' ')
            || (chars[i - 1].is_lowercase() && c.is_uppercase());
        if boundary {
            score += 12;
        }
        last = Some(i);
        qi += 1;
    }
    if qi < q.len() {
        return None;
    }
    Some(score - (chars.len() as i32) / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert_eq!(fuzzy_score("xyz", "abc"), None);
        assert_eq!(fuzzy_score("abc", "ab"), None);
        assert!(fuzzy_score("RS", "main.rs").is_some());
        assert!(fuzzy_score("mrs", "main.rs").is_some());
    }

    #[test]
    fn ranking() {
        let s = |q, n| fuzzy_score(q, n).unwrap();
        assert!(s("main", "main.rs") > s("main", "domain.rs"));
        assert!(s("fm", "file-manager") > s("fm", "affirm"));
        assert!(s("fm", "FileManager") > s("fm", "xfxmx"));
        assert!(s("doc", "doc") > s("doc", "doc_with_a_much_longer_name"));
        assert!(s("abc", "abc") > s("abc", "a_b_c"));
    }
}
