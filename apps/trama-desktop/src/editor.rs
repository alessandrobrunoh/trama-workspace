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

/// Zed release channels, in preference order. Each ships its CLI inside the
/// bundle at `Contents/MacOS/cli`.
const ZED_BUNDLES: [&str; 3] = ["Zed.app", "Zed Preview.app", "Zed Nightly.app"];

/// Build a launch plan only for an existing checkout directory.
pub fn zed_plan(path: &Path, path_env: Option<&std::ffi::OsStr>) -> Result<LaunchPlan, PlanError> {
    // App bundles exist only on macOS; elsewhere `zed` must be on PATH.
    let mut app_dirs = Vec::new();
    if cfg!(target_os = "macos") {
        app_dirs.push(PathBuf::from("/Applications"));
        if let Some(home) = std::env::var_os("HOME") {
            app_dirs.push(PathBuf::from(home).join("Applications"));
        }
    }
    zed_plan_in(path, path_env, &app_dirs)
}

/// `zed` on PATH first, then the CLI of an installed Zed bundle (any channel),
/// then `open -a <bundle>` when a bundle has no usable CLI.
fn zed_plan_in(
    path: &Path,
    path_env: Option<&std::ffi::OsStr>,
    app_dirs: &[PathBuf],
) -> Result<LaunchPlan, PlanError> {
    let checkout = canonical_checkout(path)?;
    if let Some(cli) = find_executable("zed", path_env) {
        return Ok(LaunchPlan {
            program: cli,
            args: vec![checkout],
        });
    }
    let bundles: Vec<PathBuf> = ZED_BUNDLES
        .iter()
        .flat_map(|name| app_dirs.iter().map(move |dir| dir.join(name)))
        .filter(|bundle| bundle.is_dir())
        .collect();
    if let Some(cli) = bundles
        .iter()
        .map(|bundle| bundle.join("Contents/MacOS/cli"))
        .find(|cli| cli.is_file() && is_executable(cli))
    {
        return Ok(LaunchPlan {
            program: cli,
            args: vec![checkout],
        });
    }
    let open = PathBuf::from("/usr/bin/open");
    match bundles.into_iter().next() {
        Some(bundle) if open.is_file() => Ok(LaunchPlan {
            program: open,
            args: vec![PathBuf::from("-a"), bundle, checkout],
        }),
        _ => Err(PlanError::MissingZedCli),
    }
}

/// Reveal the checkout in the platform file manager: Finder on macOS,
/// Explorer on Windows, the default handler (`xdg-open`) elsewhere.
pub fn finder_plan(path: &Path) -> Result<LaunchPlan, PlanError> {
    let checkout = canonical_checkout(path)?;
    Ok(if cfg!(target_os = "macos") {
        LaunchPlan {
            program: PathBuf::from("/usr/bin/open"),
            args: vec![PathBuf::from("-R"), checkout],
        }
    } else if cfg!(windows) {
        LaunchPlan {
            program: PathBuf::from("explorer.exe"),
            args: vec![checkout],
        }
    } else {
        LaunchPlan {
            program: PathBuf::from("xdg-open"),
            args: vec![checkout],
        }
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
    let canonical =
        dunce::canonicalize(path).map_err(|_| PlanError::MissingDirectory(path.to_path_buf()))?;
    if !canonical.is_dir() {
        return Err(PlanError::MissingDirectory(path.to_path_buf()));
    }
    Ok(canonical)
}

fn find_executable(name: &str, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    // No executable bit on Windows: only executable extensions count.
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_owned()]
    };
    std::env::split_paths(path_env?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
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
        // A fake `zed` on PATH, so the test does not depend on an install.
        let bin = directory.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let zed = bin.join(if cfg!(windows) { "zed.exe" } else { "zed" });
        std::fs::write(&zed, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&zed, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plan = zed_plan(&checkout, Some(bin.as_os_str())).unwrap();
        assert_eq!(plan.program, zed);
        assert_eq!(
            plan.args.last().unwrap(),
            &dunce::canonicalize(&checkout).unwrap()
        );
        assert_eq!(
            finder_plan(&checkout).unwrap().args.last().unwrap(),
            &dunce::canonicalize(&checkout).unwrap()
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

    #[cfg(target_os = "macos")]
    fn fake_bundle(apps: &Path, name: &str, with_cli: bool) -> PathBuf {
        let macos = apps.join(name).join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        if with_cli {
            use std::os::unix::fs::PermissionsExt;
            let cli = macos.join("cli");
            std::fs::write(&cli, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        apps.join(name)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn zed_preview_bundle_cli_is_used_without_zed_on_path() {
        let directory = test_directory("preview");
        let apps = directory.join("Applications");
        let checkout = directory.join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let bundle = fake_bundle(&apps, "Zed Preview.app", true);
        let plan = zed_plan_in(&checkout, Some(OsStr::new("")), &[apps.clone()]).unwrap();
        assert_eq!(plan.program, bundle.join("Contents/MacOS/cli"));
        assert_eq!(plan.args, vec![dunce::canonicalize(&checkout).unwrap()]);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stable_zed_is_preferred_and_bundle_without_cli_opens_by_path() {
        let directory = test_directory("stable");
        let apps = directory.join("Applications");
        let checkout = directory.join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let stable = fake_bundle(&apps, "Zed.app", false);
        fake_bundle(&apps, "Zed Preview.app", false);
        let plan = zed_plan_in(&checkout, Some(OsStr::new("")), &[apps.clone()]).unwrap();
        assert_eq!(plan.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(plan.args[..2], [PathBuf::from("-a"), stable]);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn no_zed_install_is_reported() {
        let directory = test_directory("none");
        let checkout = directory.join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        assert_eq!(
            zed_plan_in(
                &checkout,
                Some(OsStr::new("")),
                &[directory.join("Applications")]
            ),
            Err(PlanError::MissingZedCli)
        );
        std::fs::remove_dir_all(directory).unwrap();
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
