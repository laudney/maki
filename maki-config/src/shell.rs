use std::process::Command;

use crate::ShellPreference;

pub fn shell_command(cmd: &str, shell: &ShellPreference) -> Command {
    #[cfg(unix)]
    {
        let _ = shell;
        let mut command = Command::new("bash");
        command.arg("-c").arg(cmd);
        command
    }
    #[cfg(windows)]
    {
        windows::shell_command(cmd, shell)
    }
}

/// Compiled into tests on every OS, so Linux CI still checks the path logic.
#[cfg(any(windows, test))]
mod windows {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use crate::ShellPreference;

    const BASH_EXE: &str = "bash.exe";

    pub(super) fn shell_command(cmd: &str, shell: &ShellPreference) -> Command {
        let bash = match shell {
            ShellPreference::Auto => git_bash(),
            ShellPreference::Program(program) if is_cmd(program) => None,
            ShellPreference::Program(program) => Some(program.clone()),
        };
        let (program, flag) = match bash {
            Some(bash) => (bash, "-c"),
            None => (PathBuf::from("cmd.exe"), "/C"),
        };
        let mut command = Command::new(program);
        command.arg(flag).arg(cmd);
        command
    }

    fn is_cmd(program: &Path) -> bool {
        program
            .file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("cmd"))
    }

    /// We never take `bash.exe` straight from PATH, because on many machines
    /// the first one is WSL's launcher in System32 and the command would run
    /// inside Linux. The `bash.exe` that ships with Git for Windows is the one
    /// we want, so we look for `git.exe` and walk over to it.
    fn git_bash() -> Option<PathBuf> {
        let on_path = env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .map(|dir| dir.join("git.exe"))
                .find(|git| git.is_file())
        });
        on_path.and_then(|git| bash_next_to_git(&git)).or_else(|| {
            let bash = PathBuf::from(env::var_os("ProgramFiles")?)
                .join("Git")
                .join("bin")
                .join(BASH_EXE);
            bash.is_file().then_some(bash)
        })
    }

    /// The Git installer puts `cmd\` (or `bin\`) on PATH, and bash lives in
    /// `bin\`. Scoop puts a `shims\git.exe` on PATH, and the real install sits
    /// in `apps\git\current\`.
    fn bash_next_to_git(git: &Path) -> Option<PathBuf> {
        let dir = git.parent()?;
        let root = dir.parent()?;
        let dir_name = dir.file_name()?;
        let bin = if dir_name.eq_ignore_ascii_case("cmd") || dir_name.eq_ignore_ascii_case("bin") {
            root.join("bin")
        } else if dir_name.eq_ignore_ascii_case("shims") {
            root.join("apps").join("git").join("current").join("bin")
        } else {
            return None;
        };
        let bash = bin.join(BASH_EXE);
        bash.is_file().then_some(bash)
    }

    #[cfg(test)]
    mod tests {
        use std::fs;

        use test_case::test_case;

        use super::*;

        const CMD: &str = "echo hello";

        #[test_case("cmd", "cmd.exe", "/C" ; "cmd")]
        #[test_case("C:/Windows/System32/CMD.EXE", "cmd.exe", "/C" ; "cmd_full_path")]
        #[test_case("D:/tools/bash.exe", "D:/tools/bash.exe", "-c" ; "bash_like")]
        fn shell_command_for_program(program: &str, expected_program: &str, flag: &str) {
            let shell = ShellPreference::Program(PathBuf::from(program));
            let command = shell_command(CMD, &shell);
            assert_eq!(command.get_program(), expected_program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), [flag, CMD]);
        }

        #[test_case("Git/cmd/git.exe", Some("Git/bin/bash.exe") ; "installer_cmd")]
        #[test_case("Git/bin/git.exe", Some("Git/bin/bash.exe") ; "installer_bin")]
        #[test_case("scoop/shims/git.exe", Some("scoop/apps/git/current/bin/bash.exe") ; "scoop_shim")]
        #[test_case("tools/git.exe", None ; "unknown_layout")]
        fn bash_next_to_git_layouts(git: &str, bash: Option<&str>) {
            let dir = tempfile::tempdir().unwrap();
            let git = dir.path().join(git);
            let bash = bash.map(|bash| dir.path().join(bash));
            for file in [Some(&git), bash.as_ref()].into_iter().flatten() {
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, []).unwrap();
            }
            assert_eq!(bash_next_to_git(&git), bash);
        }
    }
}
