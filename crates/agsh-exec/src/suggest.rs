//! Deterministic "did you mean" suggestions and install hints for the
//! command-not-found path. No network and no LLM: pure edit distance over known
//! command names plus a small static install-hint table.

use std::collections::BTreeSet;
use std::fs;

/// Optimal string alignment (Damerau-Levenshtein) distance: like Levenshtein but
/// counts an adjacent transposition (e.g. `gti` -> `git`) as a single edit, since
/// transpositions are among the most common typos.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

/// Up to three closest candidates to `target` within an edit-distance threshold
/// that scales with the target length, sorted by distance then name.
pub fn did_you_mean(target: &str, candidates: impl Iterator<Item = String>) -> Vec<String> {
    let target_len = target.chars().count();
    let max = if target_len <= 4 { 1 } else { 2 };
    let mut scored: Vec<(usize, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for cand in candidates {
        if cand == target || cand.is_empty() || !seen.insert(cand.clone()) {
            continue;
        }
        // Skip trivial single-char builtins (".", ":", "[") unless the target is
        // itself that short — they are noise as suggestions.
        if cand.chars().count() < 2 && target_len >= 2 {
            continue;
        }
        // Cheap length prefilter before the O(n*m) distance.
        if target.len().abs_diff(cand.len()) > max {
            continue;
        }
        let d = edit_distance(target, &cand);
        if d <= max {
            scored.push((d, cand));
        }
    }
    scored.sort();
    scored.into_iter().map(|(_, c)| c).take(3).collect()
}

/// Install hint for a well-known tool, covering Homebrew and apt.
pub fn install_hint(name: &str) -> Option<&'static str> {
    let hint = match name {
        "rg" => "brew install ripgrep  |  apt install ripgrep",
        "fd" => "brew install fd  |  apt install fd-find",
        "jq" => "brew install jq  |  apt install jq",
        "yq" => "brew install yq",
        "bat" => "brew install bat  |  apt install bat",
        "exa" | "eza" => "brew install eza",
        "htop" => "brew install htop  |  apt install htop",
        "tree" => "brew install tree  |  apt install tree",
        "tmux" => "brew install tmux  |  apt install tmux",
        "gh" => "brew install gh  |  see https://cli.github.com",
        "docker" => "install Docker Desktop  |  apt install docker.io",
        "kubectl" => "brew install kubectl  |  see https://kubernetes.io/docs/tasks/tools",
        "cargo" | "rustc" => "install Rust via https://rustup.rs",
        "node" | "npm" => "brew install node  |  see https://nodejs.org",
        "python" | "python3" => "brew install python  |  apt install python3",
        "go" => "brew install go  |  see https://go.dev/dl",
        _ => return None,
    };
    Some(hint)
}

/// Executable file names found across the directories of `path_var` (`$PATH`).
pub fn path_executables(path_var: &str) -> Vec<String> {
    let mut names = Vec::new();
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                names.push(name);
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_counts_transposition_as_one() {
        assert_eq!(edit_distance("git", "git"), 0);
        assert_eq!(edit_distance("gti", "git"), 1); // transposition = 1 edit
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn suggests_close_matches() {
        let cands = ["git", "grep", "gcc", "ls"].iter().map(|s| s.to_string());
        let got = did_you_mean("gti", cands);
        assert!(got.contains(&"git".to_string()));
        assert!(!got.contains(&"ls".to_string()));
    }

    #[test]
    fn no_suggestion_when_far() {
        let cands = ["git", "ls"].iter().map(|s| s.to_string());
        assert!(did_you_mean("zzzzzzzz", cands).is_empty());
    }

    #[test]
    fn install_hints() {
        assert!(install_hint("rg").unwrap().contains("ripgrep"));
        assert!(install_hint("definitely-not-a-tool").is_none());
    }
}
