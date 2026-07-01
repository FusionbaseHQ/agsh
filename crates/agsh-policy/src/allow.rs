//! A default-deny command allowlist for confined agent sessions (`confine`).
//!
//! An `AllowPolicy` names the external commands a session may run. It is
//! intentionally minimal and **narrow-only**: `intersect` can shrink it but
//! never widen it, so a confined session can never grant itself more.
//!
//! Builtins are governed separately (they are the shell's own surface and funnel
//! any external targets back through the gated resolver); this policy decides
//! which *external* commands may be spawned. Entries match the command's
//! basename, e.g. `ls`, `df` (so `/usr/bin/ls` and `ls` are the same identity).

use std::collections::BTreeSet;

/// An immutable allowlist of external command basenames. Default-deny: a command
/// is permitted only if its basename is present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowPolicy {
    allowed: BTreeSet<String>,
}

impl AllowPolicy {
    /// Build a policy from command names (basenames are taken, blanks dropped).
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allowed = names
            .into_iter()
            .map(|n| basename(n.as_ref()).to_string())
            .filter(|n| !n.is_empty())
            .collect();
        Self { allowed }
    }

    /// Parse a comma/space-separated list (e.g. `"ls,df"` or `"ls df"`).
    pub fn parse_list(list: &str) -> Self {
        Self::from_names(list.split([',', ' ', '\t']).filter(|s| !s.is_empty()))
    }

    /// Whether a command (by name or path) is permitted.
    pub fn allows(&self, command: &str) -> bool {
        self.allowed.contains(basename(command))
    }

    /// Narrow this policy to the intersection with `names` (never widens).
    pub fn intersect<I, S>(&self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let requested = Self::from_names(names);
        Self {
            allowed: self
                .allowed
                .intersection(&requested.allowed)
                .cloned()
                .collect(),
        }
    }

    /// The allowed names, sorted (for messages and serialization).
    pub fn names(&self) -> Vec<String> {
        self.allowed.iter().cloned().collect()
    }

    /// A human-readable list for deny messages, e.g. `"df, ls"`.
    pub fn display_list(&self) -> String {
        self.names().join(", ")
    }

    /// Serialize back to a comma-separated list (for `AGSH_CONFINE`).
    pub fn to_list(&self) -> String {
        self.names().join(",")
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

/// The final path component of a command name (handles `/` and `\\`).
fn basename(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denies_everything() {
        let p = AllowPolicy::default();
        assert!(!p.allows("ls"));
        assert!(p.is_empty());
    }

    #[test]
    fn allows_listed_basenames_and_paths() {
        let p = AllowPolicy::parse_list("ls,df");
        assert!(p.allows("ls"));
        assert!(p.allows("df"));
        assert!(p.allows("/usr/bin/ls")); // path with allowed basename
        assert!(!p.allows("dh"));
        assert!(!p.allows("bash"));
    }

    #[test]
    fn intersect_only_narrows() {
        let p = AllowPolicy::parse_list("ls,df,cat");
        let n = p.intersect(["ls", "bash", "df"]); // bash not previously allowed
        assert!(n.allows("ls"));
        assert!(n.allows("df"));
        assert!(!n.allows("cat")); // dropped
        assert!(!n.allows("bash")); // never grantable
    }

    #[test]
    fn round_trips_through_list() {
        let p = AllowPolicy::parse_list("df, ls");
        assert_eq!(p.to_list(), "df,ls");
        assert_eq!(AllowPolicy::parse_list(&p.to_list()), p);
    }
}
