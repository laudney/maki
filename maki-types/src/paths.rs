//! Paths as a tool row shows them, relative to the working directory or home.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[doc(hidden)]
pub static CWD: LazyLock<Option<PathBuf>> = LazyLock::new(|| env::current_dir().ok());
#[doc(hidden)]
pub static HOME: LazyLock<Option<PathBuf>> = LazyLock::new(maki_storage::paths::home);

#[doc(hidden)]
pub fn relative_path(path: &str) -> String {
    let p = Path::new(path);
    if let Some(cwd) = CWD.as_deref()
        && let Ok(rel) = p.strip_prefix(cwd)
    {
        return format_rel("", ".", rel);
    }
    if let Some(home) = HOME.as_deref()
        && let Ok(rel) = p.strip_prefix(home)
    {
        return format_rel("~/", "~", rel);
    }
    path.to_string()
}

fn format_rel(prefix: &str, fallback: &str, rel: &Path) -> String {
    let s = rel.to_string_lossy();
    if s.is_empty() {
        fallback.into()
    } else {
        format!("{prefix}{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = maki_storage::paths::home().unwrap();

        let cases: &[(&str, &str)] = &[
            (&format!("{}/src/main.rs", cwd.display()), "src/main.rs"),
            (&cwd.to_string_lossy(), "."),
            (
                &format!("{}/.config/something.toml", home.display()),
                "~/.config/something.toml",
            ),
            ("/etc/hosts", "/etc/hosts"),
        ];
        for (input, expected) in cases {
            assert_eq!(relative_path(input), *expected, "input: {input}");
        }

        let no_partial = format!("{}sibling/file.txt", home.display());
        assert_eq!(relative_path(&no_partial), no_partial);
    }
}
