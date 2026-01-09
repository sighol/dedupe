use anyhow::Context;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateGroupFilter {
    All,
    OnlyGroupsWithScores,
    OnlyGroupsWithDeletables,
}

#[derive(Debug)]
pub struct Config {
    pub includes: Vec<PathBuf>,
    pub exclude_regex: Vec<regex::Regex>,
    pub min_size: u64,
    pub scores: Vec<Score>,
    pub groups_filter: DuplicateGroupFilter,
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

        for regex in self.exclude_regex.iter() {
            if path.to_str().is_some_and(|s| regex.is_match(s)) {
                return false;
            }
        }

        true
    }

    pub fn get_score(&self, path: &str) -> Option<i64> {
        for score in self.scores.iter() {
            if score.pattern.is_match(path) {
                return Some(score.score);
            }
        }
        None
    }
}

#[derive(Debug)]
pub struct Score {
    pub score: i64,
    pub pattern: regex::Regex,
}

impl Score {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let Some((score, regex)) = s.split_once('=') else {
            anyhow::bail!("Does does not contain =");
        };

        let score: i64 = match score.parse() {
            Ok(score) => score,
            e => e.context("Score is not a number")?,
        };

        let pattern = match regex::Regex::new(regex) {
            Ok(r) => r,
            e => e.context("Bad regex")?,
        };

        Ok(Self { score, pattern })
    }
}
