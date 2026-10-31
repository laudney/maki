use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub mod grep;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileEntry {
    pub path: String,
    pub groups: Vec<GrepMatchGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatchGroup {
    pub lines: Vec<GrepLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLine {
    pub line_nr: usize,
    pub text: String,
    pub is_match: bool,
}

impl GrepLine {
    pub fn matched(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: true,
        }
    }

    pub fn context(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            line_nr,
            text: text.into(),
            is_match: false,
        }
    }
}

impl GrepMatchGroup {
    pub fn single(line_nr: usize, text: impl Into<String>) -> Self {
        Self {
            lines: vec![GrepLine::matched(line_nr, text)],
        }
    }

    pub fn match_count(&self) -> usize {
        self.lines.iter().filter(|l| l.is_match).count()
    }
}

impl GrepFileEntry {
    pub fn match_count(&self) -> usize {
        self.groups.iter().map(|g| g.match_count()).sum()
    }
}

pub fn resolve_path(path: &str) -> Result<String, String> {
    let home_dir = crate::paths::home();
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = home_dir.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.join(rest).to_string_lossy().into_owned()
    } else if path == "~" {
        let home = home_dir.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.to_string_lossy().into_owned()
    } else {
        path.to_string()
    };

    if Path::new(&expanded).is_relative() {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        Ok(cwd.join(&expanded).to_string_lossy().into_owned())
    } else {
        Ok(expanded)
    }
}

pub fn resolve_search_path(path: Option<&str>) -> Result<String, String> {
    match path {
        Some(p) => resolve_path(p),
        None => env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("cwd error: {e}")),
    }
}

/// Convenience wrapper that always respects gitignore.
pub fn walk_builder(root: &str, patterns: &[&str]) -> Result<WalkBuilder, String> {
    walk_builder_opts(root, patterns, true)
}

/// `.git` is always excluded, even when `gitignore` is false.
pub fn walk_builder_opts(
    root: &str,
    patterns: &[&str],
    gitignore: bool,
) -> Result<WalkBuilder, String> {
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    ob.add("!.git").expect("!.git is a valid glob");

    for p in patterns {
        ob.add(p)
            .map_err(|e| format!("invalid glob pattern: {e}"))?;
    }

    let overrides = ob
        .build()
        .map_err(|e| format!("invalid glob pattern: {e}"))?;

    let mut wb = WalkBuilder::new(root);
    wb.hidden(false).overrides(overrides);
    if !gitignore {
        wb.ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    Ok(wb)
}

pub fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

pub fn truncate_bytes(line: &str, max_bytes: usize) -> String {
    if line.len() > max_bytes {
        let boundary = line.floor_char_boundary(max_bytes);
        format!("{}...", &line[..boundary])
    } else {
        line.to_owned()
    }
}
