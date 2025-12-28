use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Config {
    pub includes: Vec<PathBuf>,
    pub excludes: Vec<glob::Pattern>,
    pub exclude_regex: Vec<regex::Regex>,
    pub min_size: u64,
}

impl Config {
    pub fn is_included(&self, path: &Path) -> bool {
        if path.is_symlink() {
            return false;
        }
        let mut is_included = false;
        for inc in self.includes.iter() {
            if path.starts_with(inc) {
                is_included = true;
                break;
            }
        }
        if !is_included {
            return false;
        }

        for exc in self.excludes.iter() {
            if exc.matches_path(path) {
                return false;
            }
        }

        for regex in self.exclude_regex.iter() {
            if path.to_str().map_or(false, |s| regex.is_match(s)) {
                return false;
            }
        }

        true
    }
}
