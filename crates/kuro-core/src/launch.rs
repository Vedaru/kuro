//! Built-in launcher — kuro starts the game itself.
//!
//! Since WuWa 3.7 the client refuses to boot without a `-krqlv=<SD|HD|UHD>`
//! argument on its command line (`kuro: Use launcher to start game!`), and
//! that argument can only be delivered at `exec` time. Instead of asking some
//! other launcher to carry it, kuro owns the spawn and passes it directly —
//! no per-launcher config, no guessing where a launcher put its entry.
//!
//! ```text
//! proton run <game.exe> [-krqlv=<Q>]      (Proton, GE/dwproton preferred)
//! umu-run <game.exe> [-krqlv=<Q>]         (no Proton found)
//! ```
//!
//! The tier argument is only added for games that select assets by tier
//! ([`Game::uses_quality_tiers`]) — i.e. WuWa. PGR (Unity) launches plain.
//!
//! The prefix is the kuro-managed one under the install, reused on every
//! launch; override with `KURO_PREFIX`. Override Proton with `KURO_PROTON`.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use kuro_api::{Error, Game, Result};

use crate::quality::{self, Quality};
use crate::{state, steam};

/// kuro-managed Wine prefix, relative to the install. Reused across launches;
/// the game's settings live here.
pub const DEFAULT_PREFIX: &str = ".kuro_cache/wineprefix";

/// Override the Proton build (a directory containing `proton`).
pub const PROTON_ENV: &str = "KURO_PROTON";
/// Override the Wine prefix.
pub const PREFIX_ENV: &str = "KURO_PREFIX";

/// How kuro will start the game: what to run, with which argument, in which
/// prefix. Resolved up front so it can be shown before anything is spawned.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub game: Game,
    /// The install folder — where the launch-pid record lives, so a later
    /// [`kill`] can find this game.
    pub folder: PathBuf,
    /// The tier to pass as `-krqlv=`, or `None` for games that take no tier
    /// argument (PGR). See [`Game::uses_quality_tiers`].
    pub quality: Option<Quality>,
    /// Absolute path of the game executable.
    pub exe: PathBuf,
    /// Proton build root (the directory holding `proton`), if one was found.
    /// Preferred over `umu_run` when both exist.
    pub proton: Option<PathBuf>,
    /// An `umu-run` executable, used when no Proton was found.
    pub umu_run: Option<PathBuf>,
    /// Steam root, exported as `STEAM_COMPAT_CLIENT_INSTALL_PATH`.
    pub steam_root: Option<PathBuf>,
    /// `WINEPREFIX` / `STEAM_COMPAT_DATA_PATH`.
    pub prefix: PathBuf,
}

impl LaunchPlan {
    /// Which runner will be used, for logs.
    pub fn runner(&self) -> String {
        match (&self.proton, &self.umu_run) {
            (Some(dir), _) => dir.display().to_string(),
            (None, Some(p)) => p.display().to_string(),
            (None, None) => "none found".into(),
        }
    }

    /// The `-krqlv=` argument, or empty for games that take none.
    fn tier_arg(&self) -> String {
        match self.quality {
            Some(q) => quality::quality_arg(q),
            None => String::new(),
        }
    }

    /// One-line summary: argument, runner, prefix.
    pub fn brief(&self) -> String {
        let arg = self.tier_arg();
        let what = if arg.is_empty() {
            self.game.to_string()
        } else {
            arg
        };
        format!(
            "{} via {} (prefix {})",
            what,
            self.runner(),
            self.prefix.display()
        )
    }

    /// Multi-line plan for the CLI.
    pub fn describe(&self) -> String {
        let mut s = format!("{}", self.game);
        if let Some(q) = self.quality {
            s.push_str(&format!(" · {} ({})", q.as_arg(), q.label()));
        }
        s.push_str(&format!(
            "\n  runner: {}\n  exe:    {}\n  prefix: {}",
            self.runner(),
            self.exe.display(),
            self.prefix.display(),
        ));
        match (&self.proton, &self.umu_run) {
            (None, Some(_)) => s.push_str("\n  note:   no Proton found — falling back to umu-run"),
            (None, None) => {
                s.push_str("\n  note:   no Proton or umu-run found — set KURO_PROTON to a build")
            }
            _ => {}
        }
        s
    }
}

/// Resolve everything needed to launch `game` from `game_folder` at `quality`.
/// Does not spawn anything; call [`spawn`] for that.
pub fn plan(game_folder: &Path, game: Game, quality: Quality) -> Result<LaunchPlan> {
    let exe_rel = crate::game::find_game_exe(game_folder).ok_or_else(|| {
        Error::Patch(format!(
            "no game executable found in {}",
            game_folder.display()
        ))
    })?;
    let runners = resolve_runner();
    Ok(LaunchPlan {
        game,
        folder: game_folder.to_path_buf(),
        quality: game.uses_quality_tiers().then_some(quality),
        exe: game_folder.join(exe_rel),
        proton: runners.proton,
        umu_run: runners.umu_run,
        steam_root: steam::detect_steam().map(|s| s.steam_root),
        prefix: resolve_prefix(game_folder),
    })
}

/// Launch the game from `plan`, detached. Returns the child PID; a background
/// thread reaps it on exit so it never lingers as a zombie.
///
/// The child starts a fresh process group ([`CommandExt::process_group`]), so
/// the whole Proton→wine→client tree shares one group that a later [`kill`]
/// can signal without touching kuro's own shell. Its id is recorded under
/// `.kuro_cache/` — kuro exits right after spawning, so the pid cannot be kept
/// in memory for the "game hung on shutdown" case.
pub fn spawn(plan: &LaunchPlan) -> Result<u32> {
    let arg = plan.quality.map(quality::quality_arg);
    let mut cmd = match &plan.proton {
        Some(dir) => {
            let proton = dir.join("proton");
            if !proton.is_file() {
                return Err(Error::Patch(format!(
                    "no proton binary at {}",
                    proton.display()
                )));
            }
            // Proton creates the prefix on first run; making the directory
            // first keeps a typo'd KURO_PREFIX from silently "working".
            std::fs::create_dir_all(&plan.prefix)?;
            let mut c = Command::new(proton);
            c.arg("run").arg(&plan.exe);
            c.env("WINEPREFIX", &plan.prefix);
            c.env("STEAM_COMPAT_DATA_PATH", &plan.prefix);
            c.env("STEAM_COMPAT_CLIENT_INSTALL_PATH", client_install_path(plan));
            if let Some(arg) = &arg {
                c.arg(arg);
            }
            c
        }
        None => {
            // umu-run picks its own prefix (by GAMEID); KURO_PREFIX does not
            // apply on this path. Fail with advice rather than a bare ENOENT
            // when the HOME scan turned up nothing either.
            let umu = plan.umu_run.clone().ok_or_else(|| {
                Error::Patch(
                    "no Proton found and no umu-run on disk — install umu-launcher \
                     or point KURO_PROTON at a Proton directory"
                        .into(),
                )
            })?;
            let mut c = Command::new(umu);
            c.arg(&plan.exe);
            if let Some(arg) = &arg {
                c.arg(arg);
            }
            c
        }
    };
    // ACE behaves better when it thinks it is on a Deck; the same tweak the
    // community recommends as a launch option.
    cmd.env("steamdeck", "1");
    if let Some(dir) = plan.exe.parent() {
        cmd.current_dir(dir);
    }
    // A fresh group per launch: the child becomes its leader, so `pid` is
    // both its pid and the group id `kill` signals.
    cmd.process_group(0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd.spawn()?;
    let pid = child.id();
    // Best-effort: a read-only install must still launch.
    let _ = record_pid(&plan.folder, pid);
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// What a [`kill`] attempt did.
#[derive(Debug, Clone, Copy)]
pub struct KillReport {
    /// The recorded process-group id.
    pub pid: u32,
    /// The group was still alive when signalled.
    pub was_running: bool,
}

/// Force-kill the game kuro last launched for `game_folder` — SIGKILL to the
/// whole process group, the way out of a client that hangs on shutdown.
///
/// Only one process is recorded per install (the newest launch), so this
/// targets the current game rather than any stale one. A group that has
/// already exited is reported, not treated as an error, and its record is
/// cleared.
pub fn kill(game_folder: &Path) -> Result<KillReport> {
    let path = state::game_pid_file(game_folder);
    let pid = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| {
            Error::Patch(format!(
                "no game launched by kuro for {} — nothing to kill",
                game_folder.display()
            ))
        })?;

    // A negative pid addresses the whole group. Probe with signal 0 first so
    // an already-exited game is reported rather than signalled blindly.
    let was_running = unsafe { signal(-pid, 0) } == 0;
    if was_running {
        unsafe { signal(-pid, SIGKILL) };
    }
    let _ = std::fs::remove_file(&path);
    Ok(KillReport {
        pid: pid as u32,
        was_running,
    })
}

/// Record the launched process-group id, creating the cache dir if needed.
fn record_pid(game_folder: &Path, pid: u32) -> std::io::Result<()> {
    let path = state::game_pid_file(game_folder);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pid.to_string())
}

/// SIGKILL, for [`kill`]. The value Linux defines for it.
const SIGKILL: i32 = 9;

/// `kill(2)` via the C library std already links, so kuro stays free of a
/// `libc` dependency for this one signal. Negative `pid` means "process
/// group". Unix-only — the game is only ever launched on Linux (Proton/umu).
///
/// # Safety
/// `pid` must address a pid or process group the caller is allowed to signal.
unsafe fn signal(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

/// The `STEAM_COMPAT_CLIENT_INSTALL_PATH` to export to Proton: an explicit env
/// override wins, then a detected Steam root, else the kuro-managed prefix.
///
/// Proton reads this variable unconditionally while preparing a prefix and
/// aborts with a `KeyError` if it is unset, even though every use of it (the
/// `legacycompat` dlls, `appcache`) is guarded, so the directory need not
/// exist. Falling back to the prefix keeps Proton working on a machine with no
/// Steam at all, without assuming any Steam location.
fn client_install_path(plan: &LaunchPlan) -> PathBuf {
    std::env::var_os("STEAM_COMPAT_CLIENT_INSTALL_PATH")
        .map(PathBuf::from)
        .or_else(|| plan.steam_root.clone())
        .unwrap_or_else(|| plan.prefix.clone())
}

/// Resolve a runner: `KURO_PROTON` wins, else walk `$HOME` for a `proton`
/// script and an `umu-run` (see [`steam::find_runners`]) — no path assumed.
fn resolve_runner() -> steam::Runners {
    if let Some(dir) = std::env::var_os(PROTON_ENV) {
        let mut r = steam::find_runners();
        r.proton = Some(PathBuf::from(dir));
        return r;
    }
    steam::find_runners()
}

/// The prefix to use: `KURO_PREFIX`, else the kuro-managed one under the
/// install (reused across launches).
pub fn resolve_prefix(game_folder: &Path) -> PathBuf {
    match std::env::var_os(PREFIX_ENV) {
        Some(p) => PathBuf::from(p),
        None => game_folder.join(DEFAULT_PREFIX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_install() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kuro-launch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("Client/Binaries/Win64");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("Client-Win64-Shipping.exe"), b"MZ").unwrap();
        dir
    }

    #[test]
    fn plan_carries_the_exe_quality_and_prefix() {
        let dir = temp_install();
        let p = plan(&dir, Game::WuWa, Quality::Uhd).unwrap();
        assert!(p.exe.ends_with("Client/Binaries/Win64/Client-Win64-Shipping.exe"));
        assert!(p.brief().contains("-krqlv=UHD"));
        assert_eq!(p.prefix, dir.join(DEFAULT_PREFIX));
        assert_eq!(p.folder, dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pid that cannot exist (above any kernel `pid_max`) is reported as not
    /// running, never signalled — and its stale record is cleared.
    #[test]
    fn kill_reports_a_stale_record_without_signalling() {
        let dir = temp_install();
        record_pid(&dir, 2_000_000_000).unwrap();
        let r = kill(&dir).unwrap();
        assert_eq!(r.pid, 2_000_000_000);
        assert!(!r.was_running);
        assert!(!state::game_pid_file(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing recorded → a clear error, not a blind signal.
    #[test]
    fn kill_without_a_record_errors() {
        let dir = temp_install();
        assert!(kill(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SIGKILL reaches a real process group recorded by a launch.
    #[test]
    fn kill_ends_a_real_process_group() {
        let dir = temp_install();
        let mut cmd = Command::new("sleep");
        cmd.arg("30").process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        record_pid(&dir, child.id()).unwrap();

        let r = kill(&dir).unwrap();
        assert_eq!(r.pid, child.id());
        assert!(r.was_running);
        let _ = child.wait(); // reap; SIGKILL has no clean-exit path
        assert!(!state::game_pid_file(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pgr_plan_carries_no_tier_argument() {
        // Unity layout: PGR.exe sits at the install root.
        let dir = std::env::temp_dir().join(format!("kuro-launch-pgr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PGR.exe"), b"MZ").unwrap();
        let p = plan(&dir, Game::Pgr, Quality::Hd).unwrap();
        assert!(p.quality.is_none());
        assert!(!p.brief().contains("-krqlv"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Proton aborts without `STEAM_COMPAT_CLIENT_INSTALL_PATH` set. With no
    /// detected Steam root (no Steam installed) the kuro-managed prefix is
    /// used, so the launch still works; a detected root wins over it.
    #[test]
    fn client_install_path_defaults_to_prefix_without_steam() {
        let dir = temp_install();
        let mut p = plan(&dir, Game::WuWa, Quality::Hd).unwrap();
        std::env::remove_var("STEAM_COMPAT_CLIENT_INSTALL_PATH");
        p.steam_root = None;
        assert_eq!(client_install_path(&p), p.prefix);
        p.steam_root = Some(PathBuf::from("/a/steam/root"));
        assert_eq!(client_install_path(&p), PathBuf::from("/a/steam/root"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_without_exe_errors() {
        let dir = std::env::temp_dir().join(format!("kuro-launch-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(plan(&dir, Game::Pgr, Quality::Hd).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
