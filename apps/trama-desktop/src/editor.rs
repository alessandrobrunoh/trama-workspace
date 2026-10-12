//! Safe, shell-free launch argument planning for checkout actions.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    MissingDirectory(PathBuf),
    MissingZedCli,
}

/// Build a launch plan only for an existing checkout directory.
pub fn zed_plan(path: &Path, path_env: Option<&std::ffi::OsStr>) -> Result<LaunchPlan, PlanError> {
    let checkout = canonical_checkout(path)?;
    if let Some(cli) = find_executable("zed", path_env) {
        return Ok(LaunchPlan {
            program: cli,
            args: vec![checkout],
        });
    }
    #[cfg(target_os = "macos")]
    {
        let open = PathBuf::from("/usr/bin/open");
        if open.is_file() {
            return Ok(LaunchPlan {
                program: open,
                args: vec![PathBuf::from("-a"), PathBuf::from("Zed"), checkout],
            });
        }
    }
    Err(PlanError::MissingZedCli)
}

pub fn finder_plan(path: &Path) -> Result<LaunchPlan, PlanError> {
    Ok(LaunchPlan {
        program: PathBuf::from("/usr/bin/open"),
        args: vec![PathBuf::from("-R"), canonical_checkout(path)?],
    })
}

/// Called only from a background executor after an explicit user action.
pub fn launch(plan: LaunchPlan) -> Result<(), String> {
    let output = std::process::Command::new(&plan.program)
        .args(&plan.args)
        .output()
        .map_err(|error| format!("Could not open the copy: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(1024)
            .collect();
        Err(format!("Open failed: {}", detail.trim()))
    }
}

fn canonical_checkout(path: &Path) -> Result<PathBuf, PlanError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| PlanError::MissingDirectory(path.to_path_buf()))?;
    if !canonical.is_dir() {
        return Err(PlanError::MissingDirectory(path.to_path_buf()));
    }
    Ok(canonical)
}

fn find_executable(name: &str, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_env?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file() && is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_directory(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "trama-editor-test-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn checkout_paths_are_passed_as_single_arguments() {
        let directory = test_directory("spaces");
        let checkout = directory.join("repo with spaces - branch");
        std::fs::create_dir(&checkout).unwrap();
        let plan = zed_plan(&checkout, Some(OsStr::new(""))).unwrap();
        assert_eq!(plan.args.last().unwrap(), &checkout.canonicalize().unwrap());
        assert_eq!(
            finder_plan(&checkout).unwrap().args[1],
            checkout.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_checkout_is_rejected_before_planning() {
        let missing = PathBuf::from("/path/that/does/not/exist");
        assert_eq!(
            zed_plan(&missing, Some(OsStr::new(""))),
            Err(PlanError::MissingDirectory(missing.clone()))
        );
        assert_eq!(
            finder_plan(&missing),
            Err(PlanError::MissingDirectory(missing))
        );
    }

    #[test]
    fn executable_lookup_requires_a_regular_executable() {
        let directory = test_directory("not-executable");
        let candidate = directory.join("zed");
        std::fs::write(&candidate, "not executable").unwrap();
        assert_eq!(find_executable("zed", Some(directory.as_os_str())), None);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
