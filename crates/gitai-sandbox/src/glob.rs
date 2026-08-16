//! Path globbing for scope enforcement.
//!
//! Small on purpose. The patterns it has to handle come from a planner model
//! and from config, and they are all of the form `src/**`, `**/*_test.rs`,
//! `docs/*.md`. A full glob crate would add a dependency for syntax nobody
//! writes here.
//!
//! Supported: `*` (no separator), `**` (any, including separators), `?`.
//! Matching is on `/`-separated paths, which is what git reports on every
//! platform.

/// Does `path` match `pattern`?
pub fn matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = path.chars().collect();
    match_from(&pat, 0, &txt, 0)
}

fn match_from(pat: &[char], mut pi: usize, txt: &[char], mut ti: usize) -> bool {
    while pi < pat.len() {
        match pat[pi] {
            '*' => {
                let double = pi + 1 < pat.len() && pat[pi + 1] == '*';
                let next_pi = if double { pi + 2 } else { pi + 1 };

                // `**/` should also match zero directories, so `**/a.rs`
                // matches a top-level `a.rs`.
                if double
                    && next_pi < pat.len()
                    && pat[next_pi] == '/'
                    && match_from(pat, next_pi + 1, txt, ti)
                {
                    return true;
                }

                for skip in ti..=txt.len() {
                    if !double && txt[ti..skip].contains(&'/') {
                        break;
                    }
                    if match_from(pat, next_pi, txt, skip) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= txt.len() || txt[ti] == '/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            c => {
                if ti >= txt.len() || txt[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == txt.len()
}

/// True when `path` matches any pattern. An empty pattern list matches nothing.
pub fn matches_any(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| matches(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_single_star() {
        assert!(matches("src/main.rs", "src/main.rs"));
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(!matches("src/*.rs", "src/a/main.rs"));
        assert!(!matches("src/*.rs", "src/main.py"));
    }

    #[test]
    fn double_star_crosses_directories() {
        assert!(matches("src/**", "src/a/b/c.rs"));
        assert!(matches("src/**", "src/a.rs"));
        assert!(matches("**/*_test.rs", "crates/core/src/thing_test.rs"));
        assert!(matches("**/tests/**", "a/b/tests/c/d.rs"));
    }

    #[test]
    fn leading_double_star_matches_at_the_root_too() {
        assert!(matches("**/*.md", "README.md"));
        assert!(matches("**/*.md", "docs/guide/README.md"));
    }

    #[test]
    fn question_mark_does_not_cross_a_separator() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "a/c"));
    }

    #[test]
    fn non_matches_stay_non_matches() {
        assert!(!matches("src/**", "docs/a.md"));
        assert!(!matches("src/**", "src"));
        assert!(!matches("", "a"));
        assert!(matches("", ""));
    }

    #[test]
    fn matches_any_needs_at_least_one_pattern() {
        assert!(!matches_any(&[], "src/a.rs"));
        let pats = vec!["docs/**".to_string(), "src/**".to_string()];
        assert!(matches_any(&pats, "src/a.rs"));
        assert!(!matches_any(&pats, "Cargo.toml"));
    }

    #[test]
    fn adjacent_stars_do_not_blow_up() {
        assert!(matches("**/**/*.rs", "a/b/c.rs"));
        assert!(matches("*", "file.rs"));
        assert!(!matches("*", "dir/file.rs"));
    }
}
