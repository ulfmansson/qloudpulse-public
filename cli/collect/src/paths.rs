//! Where the config file is when `--config` and `QLOUDPULSE_COLLECT_CONFIG` name none: `qloudpulse-collect.toml` in
//! the qloudpulse config directory (`$XDG_CONFIG_HOME/qloudpulse` or `~/.config/qloudpulse`, `%APPDATA%\qloudpulse`
//! on Windows), the directory `qloudpulse-stack` keeps its `credentials` in. A `qloudpulse-collect.toml` in the
//! current directory still wins, as it did before this default, so a config set up there keeps its key and its
//! pseudonyms. The files a config names by relative path sit beside it. What the client writes as it runs goes to
//! the state directory instead ([`state_dir`]), unless the config names `work_dir` or `output_dir`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The config file's name, in the current directory or the config directory.
pub const CONFIG_FILE: &str = "qloudpulse-collect.toml";

/// The config file to use when none is named.
pub fn default_config() -> PathBuf {
    resolve(
        Path::new(CONFIG_FILE).is_file(),
        |name| std::env::var_os(name),
        cfg!(windows),
    )
}

/// [`default_config`] with the current directory's file, the environment and the platform given.
fn resolve(in_current_dir: bool, var: impl Fn(&str) -> Option<OsString>, windows: bool) -> PathBuf {
    if in_current_dir {
        return CONFIG_FILE.into();
    }
    config_dir(var, windows).map_or_else(|| CONFIG_FILE.into(), |d| d.join(CONFIG_FILE))
}

/// The qloudpulse config directory, as `qloudpulse_stack::credentials::config_dir` finds it. Kept here too because
/// a build without the `upload` feature has no qloudpulse-stack in it. A relative value counts as unset.
fn config_dir(var: impl Fn(&str) -> Option<OsString>, windows: bool) -> Option<PathBuf> {
    let absolute = |v: OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let base = if windows {
        var("APPDATA").and_then(absolute)?
    } else {
        var("XDG_CONFIG_HOME")
            .and_then(absolute)
            .or_else(|| var("HOME").and_then(absolute).map(|h| h.join(".config")))?
    };
    Some(base.join("qloudpulse"))
}

/// The qloudpulse state directory, for what the client writes as it runs (raw snapshots, bundles, the delivery
/// state): `$XDG_STATE_HOME/qloudpulse` or `~/.local/state/qloudpulse`, `%LOCALAPPDATA%\qloudpulse` on Windows. Never
/// the config directory: what is there is config and secrets you back up, not data that churns. `None` with no home,
/// and always in this crate's unit tests, so they never write to the real one.
pub fn state_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    state_dir_from(|name| std::env::var_os(name), cfg!(windows))
}

fn state_dir_from(var: impl Fn(&str) -> Option<OsString>, windows: bool) -> Option<PathBuf> {
    let absolute = |v: OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let base = if windows {
        var("LOCALAPPDATA").and_then(absolute)?
    } else {
        var("XDG_STATE_HOME")
            .and_then(absolute)
            .or_else(|| var("HOME").and_then(absolute).map(|h| h.join(".local").join("state")))?
    };
    Some(base.join("qloudpulse"))
}

/// Where a source keeps its files in the state directory: one directory per `source_id`, so two configs on one
/// machine never share bundles or a delivery state.
pub fn source_state_dir(state_dir: &Path, source_id: &str) -> PathBuf {
    state_dir.join("collect").join(source_id)
}

/// Creates the config file's directory if it is missing: 0700 on Unix, since the key and the view token go there.
pub fn create_parent(config: &Path) -> std::io::Result<()> {
    let Some(dir) = config.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    if dir.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs.iter().map(|(k, v)| (k.to_string(), (*v).into())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn a_file_in_the_current_directory_wins() {
        let found = resolve(true, env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")]), false);
        assert_eq!(found, PathBuf::from(CONFIG_FILE));
    }

    #[test]
    fn xdg_config_home_then_home_dot_config() {
        let xdg = resolve(false, env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")]), false);
        assert_eq!(xdg, PathBuf::from("/xdg/qloudpulse/qloudpulse-collect.toml"));
        let home = resolve(false, env(&[("HOME", "/home/u")]), false);
        assert_eq!(
            home,
            PathBuf::from("/home/u/.config/qloudpulse/qloudpulse-collect.toml")
        );
    }

    #[test]
    fn a_relative_xdg_config_home_counts_as_unset() {
        let found = resolve(false, env(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/home/u")]), false);
        assert_eq!(
            found,
            PathBuf::from("/home/u/.config/qloudpulse/qloudpulse-collect.toml")
        );
    }

    #[test]
    fn appdata_on_windows() {
        let found = resolve(
            false,
            env(&[("APPDATA", "/appdata"), ("XDG_CONFIG_HOME", "/xdg")]),
            true,
        );
        assert_eq!(found, PathBuf::from("/appdata/qloudpulse/qloudpulse-collect.toml"));
    }

    #[test]
    fn no_home_falls_back_to_the_current_directory() {
        assert_eq!(resolve(false, env(&[]), false), PathBuf::from(CONFIG_FILE));
    }

    #[test]
    fn state_dir_is_xdg_state_home_then_local_state() {
        let xdg = state_dir_from(env(&[("XDG_STATE_HOME", "/st"), ("HOME", "/home/u")]), false);
        assert_eq!(xdg, Some(PathBuf::from("/st/qloudpulse")));
        let home = state_dir_from(env(&[("XDG_STATE_HOME", "rel"), ("HOME", "/home/u")]), false);
        assert_eq!(home, Some(PathBuf::from("/home/u/.local/state/qloudpulse")));
        let windows = state_dir_from(env(&[("LOCALAPPDATA", "/local"), ("HOME", "/home/u")]), true);
        assert_eq!(windows, Some(PathBuf::from("/local/qloudpulse")));
        assert_eq!(state_dir_from(env(&[]), false), None);
        assert_eq!(state_dir(), None, "unit tests never touch the real state directory");
    }

    #[test]
    fn create_parent_makes_a_private_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("a/qloudpulse/qloudpulse-collect.toml");
        create_parent(&config).unwrap();
        let dir = config.parent().unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        create_parent(&config).unwrap();
        create_parent(Path::new(CONFIG_FILE)).unwrap();
    }
}
