use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct PathCache {
    entries: BTreeMap<String, PathBuf>,
}

impl PathCache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn insert(&mut self, name: impl Into<String>, path: PathBuf) {
        self.entries.insert(name.into(), path);
    }

    pub fn get(&self, name: &str) -> Option<&PathBuf> {
        self.entries.get(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
