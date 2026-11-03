//! Linux bubblewrap confinement for the `bash` tool (`--unshare-*` +
//! read-only system, read-write workspace).
//!
//! Default OFF (`[sandbox] enabled`): no behavior change, no false
//! promises. When enabled, every foreground and background `bash` command
//! runs under `bwrap`; a missing binary or failed namespace setup fails the
//! tool call LOUDLY (never silently unsandboxed). Network is unshared by
//! default (`network = false`); the whole profile opts out when builds
//! need to fetch. macOS Seatbelt / seccomp syscall filtering are future
//! work — this is namespace + bind-mount confinement, stated plainly.
//!
//! The builder (`argv`) is pure and unit-tested; `wrap` only assembles
//! `std::process::Command`.

use std::path::{Path, PathBuf};

/// Resolved sandbox profile (mirrors `config::SandboxConfig` 1:1).
#[derive(Debug, Clone)]
pub struct Profile {
    pub enabled: bool,
    /// When false (default) the sandbox gets `--unshare-net`.
    pub network: bool,
    /// Sandbox binary (default `bwrap`). Must be a bare path: extra flags
    /// are rejected rather than silently dropped.
    pub command: String,
}

impl Default for Profile {
    fn default() -> Self {
        Self { enabled: false, network: false, command: "bwrap".to_string() }
    }
}

impl Profile {
    pub fn from_config(cfg: &crate::config::SandboxConfig) -> Self {
        let command = if cfg.command.trim().is_empty() { "bwrap".to_string() } else { cfg.command.trim().to_string() };
        Self { enabled: cfg.enabled, network: cfg.network, command }
    }
}

/// Home tool caches the sandbox bind-mounts read-write so offline builds
/// keep working (cargo/rustup/npm/go). Everything else outside the
/// workspace stays invisible. Documented trade-off: a hostile command
/// could poison the user's own caches — the boundary is the host, not
/// the caches.
fn cache_binds(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    [".cargo", ".rustup", ".npm", "go"]
        .into_iter()
        .map(|d| home.join(d))
        .filter(|p| p.exists())
        .map(|p| (p.clone(), p))
        .collect()
}

/// Full argv with the sandbox binary at [0]. Pure: unit-test the policy.
pub fn argv(p: &Profile, workspace: &Path, dir: &Path, tmpdir: &Path, home: &Path) -> Result<Vec<String>, String> {
    if p.command.trim().is_empty() || p.command.split_whitespace().count() != 1 {
        return Err("sandbox: command must be a single binary path (no flags)".into());
    }
    let mut a = vec![p.command.clone()];
    // Identity + isolation. `--unshare-user` is hard (without it nothing
    // is isolated); cgroup gets the -try variant (fails on some hosts).
    a.extend(["--unshare-user", "--unshare-pid", "--unshare-uts", "--unshare-ipc", "--unshare-cgroup-try"]
        .into_iter().map(|s| s.to_string()));
    if !p.network {
        a.push("--unshare-net".into());
    }
    a.push("--die-with-parent".into());
    // Read-only system (+TLS roots for when network is on).
    for m in ["/usr", "/bin", "/lib", "/lib64", "/etc/resolv.conf", "/etc/ssl", "/etc/passwd"] {
        a.push("--ro-bind-try".into());
        a.push(m.into());
        a.push(m.into());
    }
    a.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].into_iter().map(|s| s.to_string()));
    // Read-write: workspace, exec dir (when it differs: allowlisted
    // external workdir), session TMPDIR, toolchain caches.
    let ws = workspace.display().to_string();
    a.extend(["--bind".into(), ws.clone(), ws]);
    if dir != workspace {
        let dd = dir.display().to_string();
        a.extend(["--bind-try".into(), dd.clone(), dd]);
    }
    let td = tmpdir.display().to_string();
    a.extend(["--bind".into(), td.clone(), td]);
    for (src, dst) in cache_binds(home) {
        a.extend(["--bind-try".into(), src.display().to_string(), dst.display().to_string()]);
    }
    a.extend(["--chdir".into(), dir.display().to_string()]);
    Ok(a)
}

/// Assemble the sandboxed command: `bwrap [flags] --chdir <dir> sh -c <cmd>`.
/// Env (TMPDIR, plugin extras) is applied by the caller on the result —
/// bwrap passes the environment through to the child.
pub fn wrap(p: &Profile, workspace: &Path, dir: &Path, tmpdir: &Path, cmd_str: &str) -> Result<std::process::Command, String> {
    if !p.enabled {
        return Err("sandbox: internal error (wrap called while disabled)".into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"));
    let mut a = argv(p, workspace, dir, tmpdir, &home)?;
    let bin = a.remove(0);
    a.extend(["sh".into(), "-c".into(), cmd_str.to_string()]);
    let mut cmd = std::process::Command::new(bin);
    cmd.args(&a);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof() -> Profile {
        Profile { enabled: true, network: false, command: "bwrap".into() }
    }

    fn home() -> PathBuf {
        let d = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(d.join(".cargo")).unwrap();
        d
    }

    #[test]
    fn argv_confines_and_binds_workspace() {
        let h = home();
        let a = argv(&prof(), Path::new("/w"), Path::new("/w/sub"), Path::new("/tmp/ses-1"), &h).unwrap();
        assert_eq!(a[0], "bwrap");
        for f in ["--unshare-user", "--unshare-pid", "--unshare-net", "--unshare-uts", "--unshare-ipc", "--die-with-parent"] {
            assert!(a.contains(&f.to_string()), "{a:?}");
        }
        // RO system, RW workspace + tmpdir.
        let ro = a.windows(3).any(|w| w == ["--ro-bind-try", "/usr", "/usr"]);
        assert!(ro, "{a:?}");
        let rw = a.windows(3).any(|w| w == ["--bind", "/w", "/w"]);
        assert!(rw, "{a:?}");
        // Existing cargo cache is shared; missing ones are skipped.
        assert!(a.windows(3).any(|w| w[0] == "--bind-try" && w[1].ends_with(".cargo")), "{a:?}");
        assert!(!a.iter().any(|x| x.ends_with(".rustup")), "{a:?}");
    }

    #[test]
    fn argv_network_opt_out() {
        let mut p = prof();
        p.network = true;
        let a = argv(&p, Path::new("/w"), Path::new("/w"), Path::new("/tmp/s"), Path::new("/nonexistent")).unwrap();
        assert!(!a.contains(&"--unshare-net".to_string()), "{a:?}");
    }

    #[test]
    fn argv_rejects_flags_in_command() {
        let mut p = prof();
        p.command = "bwrap --foo".into();
        assert!(argv(&p, Path::new("/w"), Path::new("/w"), Path::new("/t"), Path::new("/nonexistent")).is_err());
    }

    #[test]
    fn wrap_appends_shell_command() {
        let cmd = wrap(&prof(), Path::new("/w"), Path::new("/w"), Path::new("/t"), "echo hi").unwrap();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(&args[args.len() - 3..], ["sh", "-c", "echo hi"]);
        assert!(args.contains(&"--chdir".to_string()));
    }

    #[test]
    fn disabled_profile_never_wraps() {
        assert!(wrap(&Profile::default(), Path::new("/w"), Path::new("/w"), Path::new("/t"), "id").is_err());
    }
}
