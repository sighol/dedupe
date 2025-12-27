use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Config {
    pub includes: Vec<PathBuf>,
    pub excludes: Vec<glob::Pattern>,
}

impl Config {
    pub fn is_included(&self, path: &Path) -> bool {
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

        true
    }
}
