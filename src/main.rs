use anyhow::{Result, anyhow};
use git2::{FetchOptions, Repository, ResetType, build::RepoBuilder};
use owo_colors::OwoColorize;
use schemars::JsonSchema;
use serde::Deserialize;
use std::process::exit;
use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct Pkgbuild {
    base: String,
    #[serde(default)]
    artifacts: Vec<String>,
    git: Option<String>,
    rpc: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Calibre {
    Simple(String),
    Complex(Pkgbuild),
}
impl Calibre {
    pub fn complexify(self) -> Pkgbuild {
        match self {
            Self::Simple(base) => Pkgbuild {
                artifacts: vec![base.clone()], // The base package is the only artifact, assumed
                base,
                git: None,
                rpc: None,
            },
            Self::Complex(mut pkgbuild) => {
                if pkgbuild.artifacts.is_empty() {
                    // Hence, artifacts is optional
                    pkgbuild.artifacts.push(pkgbuild.base.clone());
                }
                pkgbuild
            }
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Config {
    #[serde(default)]
    packages: Vec<String>,

    #[serde(default)]
    pkgbuilds: Vec<Calibre>,

    #[serde(default)]
    ignore: Vec<String>,

    #[serde(default)]
    settings: Settings,
}

// Kinda annoying pattern of default for individual fields, and a default function
// We need to do this however because the impl Default is for Settings from nothing
// whereas the individual fields is for JsonSchema to be able to construct it from partial
#[derive(Debug, Deserialize, JsonSchema, Clone)]
struct Settings {
    #[serde(default = "default_elevation")]
    elevation: String,
    #[serde(default = "default_packages")]
    package_removal_confirmation_count: u32,
    #[serde(default = "default_true")]
    default_true_confirmation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            elevation: default_elevation(),
            package_removal_confirmation_count: 0,
            default_true_confirmation: true,
        }
    }
}

fn default_packages() -> u32 {
    0
}
fn default_elevation() -> String {
    "sudo".to_string()
}
fn default_true() -> bool {
    true
}

macro_rules! input {
    ($($arg:tt)*) => {
        print!("{} {}", "[INPUT]".purple().bold(), format!($($arg)*))
    };
}

macro_rules! warnln {
    ($($arg:tt)*) => {
        println!("{} {}", "[WARNING]".yellow().bold(), format!($($arg)*))
    };
}

macro_rules! logln {
    ($($arg:tt)*) => {
        println!("{} {}", "[LOG]".cyan().bold(), format!($($arg)*))
    };
}

macro_rules! log {
    ($($arg:tt)*) => {
        print!("{} {}", "[LOG]".cyan().bold(), format!($($arg)*))
    };
}

macro_rules! fail {
    ($($arg:tt)*) => {{
        println!("{} {}", "[FAIL]".red().bold(), format!($($arg)*));
        exit(1);
    }};
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();
static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
const AUR_URL: &str = "https://aur.archlinux.org";

fn init_settings(settings: Settings) {
    let _ = SETTINGS.set(settings);
}

fn settings() -> &'static Settings {
    SETTINGS.get().expect("settings not initialised")
}

fn client() -> &'static reqwest::blocking::Client {
    CLIENT.get_or_init(reqwest::blocking::Client::new)
}

fn get_aur_git(base: &str) -> String {
    format!("{}/{}.git", AUR_URL, base)
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    run_capture_in(program, None, args)
}

fn run_inherit(program: &str, args: &[&str]) -> Result<bool> {
    Ok(Command::new(program).args(args).status()?.success())
}

// Gets the packages that depends on the target
fn get_required_by(package: &str) -> Result<Vec<String>> {
    let raw = run_capture("pacman", &["-Qi", package])?;

    let required_by_line = raw
        .lines()
        .find(|line| line.starts_with("Required By"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim())
        .unwrap_or("None");

    Ok(required_by_line.split_whitespace().filter(|p| *p != "None").map(|p| p.to_string()).collect())
}

/// Turn a newline separated command output into a Vec<String>, dropping blanks.
fn lines_to_vec(raw: &str) -> Vec<String> {
    raw.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

fn subtract(a: &[String], b: &[String]) -> Vec<String> {
    let set: HashSet<&String> = b.iter().collect();
    a.iter().filter(|item| !set.contains(item)).cloned().collect()
}

fn check_package_warn_limit(packages: &[String], limit: u32, default: bool) -> Result<bool> {
    if (packages.len() as u32) <= limit {
        return Ok(true);
    }

    warnln!("{} packages will be removed:", packages.len());
    for pkg in packages {
        println!("  - {}", pkg);
    }

    let prompt = if default { "[Y/n]" } else { "[y/N]" };
    input!("Proceed? {}: ", prompt);
    io::stdout().flush().ok();

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();

    if answer.is_empty() {
        return Ok(default);
    }

    Ok(answer == "y" || answer == "yes")
}

// Filter the packages to remove, to ones that aren't depended on by anything else.
// Also, marks them as a dependency
fn filter_removable(to_remove: Vec<String>) -> Result<Vec<String>> {
    // We continuously loop over the to_remove, and check if any package cannot be removed.
    // If so, we remove those packages, and then go again, until we eventually reach
    // a state where all packages can be removed
    // Eg. A depends on B, and C depends on A. AB is being removed. This means we don't remove B
    // A in pass 1 would be marked as non-removeable, so we move to pass 2, where B is then marked
    // as non-removeable, since A is no longer in the to_remove list
    let mut required_by_map: HashMap<String, Vec<String>> = HashMap::new();
    for pkg in &to_remove {
        let required_by = get_required_by(pkg)?;
        required_by_map.insert(pkg.clone(), required_by);
    }

    let mut to_remove: HashSet<String> = to_remove.into_iter().collect();

    loop {
        let mut newly_excluded = Vec::new();

        for pkg in &to_remove {
            let required_by = required_by_map.get(pkg).expect("cached above");

            // Check if there are any packages that depend on this pkg, that aren't also being
            // removed
            let can_be_removed = required_by.iter().all(|dep| to_remove.contains(dep));
            if !can_be_removed {
                newly_excluded.push(pkg.clone());
            }
        }

        if newly_excluded.is_empty() {
            break;
        }

        for pkg in &newly_excluded {
            to_remove.remove(pkg);

            let required_by = required_by_map.get(pkg).expect("cached above");
            warnln!("Unable to remove {} as the following depend upon it:", pkg);
            for dep in required_by {
                println!("  - {}", dep);
            }
            logln!("Marking {} install reason as dependency", pkg);
            run_capture(&settings().elevation, &["pacman", "-D", "--asdep", pkg]).map_err(|err| anyhow!("Could not mark {} as a dependency: {}", pkg, err))?;
            logln!("Marked {} as a dependency", pkg);
        }
    }

    Ok(to_remove.into_iter().collect())
}

fn remove_packages(packages: &[String]) -> Result<()> {
    if packages.is_empty() {
        return Ok(());
    }

    log!("Removing: ");
    for p in packages {
        print!("{} ", p);
    }
    println!();

    let mut args = vec!["pacman", "-Rns", "--noconfirm"];
    args.extend(packages.iter().map(|p| p.as_str()));

    if run_inherit(&settings().elevation, &args)? {
        log!("Removed packages: ");
        for p in packages {
            print!("{} ", p);
        }
        println!();
    } else {
        return Err(anyhow!("Failed to remove one or more packages"));
    }

    Ok(())
}

// Check if the to_install packages are already installed as deps, and if so,
// simply just mark em as explicit
fn filter_installables(to_install: Vec<String>) -> Result<Vec<String>> {
    let installed_raw = run_capture("pacman", &["-Qq"])?;
    let installed: HashSet<String> = lines_to_vec(&installed_raw).into_iter().collect();

    let mut result = Vec::new();
    for pkg in to_install {
        if installed.contains(&pkg) {
            logln!("{} has already been installed as a dependency, marking as explicit", pkg);
            run_capture(&settings().elevation, &["pacman", "-D", "--asexplicit", pkg.as_str()]).map_err(|err| anyhow!("Could not mark {} as explicitly installed: {}", pkg, err))?;
            logln!("Marked {} as explicitly installed", pkg);
        } else {
            result.push(pkg);
        }
    }
    Ok(result)
}

fn install_packages(packages: &[String]) -> Result<()> {
    let mut args = vec!["pacman", "-Syu", "--noconfirm"];
    if packages.is_empty() {
        logln!("Upgrading system");
    } else {
        log!("Upgrading system & attempting to install: ");
        for p in packages {
            print!("{} ", p);
        }
        println!();
        args.extend(packages.iter().map(|p| p.as_str()));
    }
    if run_inherit(&settings().elevation, &args)? {
        if packages.is_empty() {
            logln!("Completed system upgrade");
        } else {
            logln!("Completed system upgrade & installations");
        }
    } else if packages.is_empty() {
        return Err(anyhow!("System upgrade failed"));
    } else {
        return Err(anyhow!("Installation/upgrade failed"));
    }
    Ok(())
}

fn run_capture_in(program: &str, path: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    if let Some(path) = path {
        command.current_dir(path);
    }
    let output = command.args(args).output()?;
    if !output.status.success() {
        return Err(anyhow!("Command failed ({} {}): {}", program, args.join(" "), String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn makepkg(path: &Path, args: &[&str]) -> Result<bool> {
    Ok(Command::new("makepkg").current_dir(path).env("PACMAN_AUTH", &settings().elevation).args(args).status()?.success())
}

fn update_pkgbuild(path: &Path) -> Result<()> {
    let repo = Repository::open(path)?;
    // Only shallow fetch
    let mut fetch_options = FetchOptions::new();
    fetch_options.depth(1);
    repo.find_remote("origin")?.fetch(&["HEAD"], Some(&mut fetch_options), None)?;

    // Reset the repo to the fetch head,
    let fetch_head = repo.find_reference("FETCH_HEAD")?;
    let fetch_head = fetch_head.target().ok_or_else(|| anyhow!("FETCH_HEAD does not point to an object"))?;
    repo.reset(&repo.find_object(fetch_head, None)?, ResetType::Hard, None)?;
    Ok(())
}

#[derive(Deserialize)]
struct RpcResponse {
    resultcount: usize,
    results: Vec<AurResult>,
}

#[derive(Deserialize)]
struct AurResult {
    #[serde(rename = "Version")]
    version: String,
}

fn get_rpc_version(artifact: &str) -> Result<String> {
    // The RPC takes the name of a real package, not the package base, so we just use the first
    // artifact (all artifacts should have the same version, anyway)
    let response: RpcResponse = client().get(format!("{}/rpc/v5/info", AUR_URL)).query(&[("arg[]", artifact)]).send()?.error_for_status()?.json()?;
    if response.resultcount == 0 {
        return Err(anyhow!("Could not retrieve RPC version for {}", artifact));
    }
    Ok(response.results.first().ok_or_else(|| anyhow!("RPC returned no result for {}", artifact))?.version.clone())
}

fn srcinfo_version(path: &Path) -> Result<String> {
    let srcinfo = run_capture_in("makepkg", Some(path), &["--printsrcinfo"])?;
    // lines is borrowed, hence no lines to vec
    let value = |key: &str| srcinfo.lines().find_map(|line| line.trim().split_once(" = ").filter(|(name, _)| *name == key).map(|(_, value)| value));
    let pkgver = value("pkgver").ok_or_else(|| anyhow!("Missing pkgver"))?;
    let pkgrel = value("pkgrel").ok_or_else(|| anyhow!("Missing pkgrel"))?;
    Ok(match value("epoch") {
        // Epoch overrides normal versioning, and is optional
        Some(epoch) => format!("{}:{}-{}", epoch, pkgver, pkgrel),
        None => format!("{}-{}", pkgver, pkgrel),
    })
}

fn install_pkgbuild(path: &Path, artifacts: &[String]) -> Result<()> {
    if !makepkg(path, &["-s", "--noconfirm"])? {
        return Err(anyhow!("makepkg failed in {}", path.display()));
    }

    // Gives absolute paths
    let package_list = run_capture_in("makepkg", Some(path), &["--packagelist"])?;
    let mut packages = Vec::new();
    for package in lines_to_vec(&package_list) {
        // -p means a file on disk, not an installed one
        let details = run_capture("pacman", &["-Qp", &package])?;
        let name = details.split_whitespace().next().ok_or_else(|| anyhow!("Could not inspect package {}", package))?;
        if artifacts.iter().any(|artifact| artifact == name) {
            packages.push(package);
        }
    }
    if packages.len() != artifacts.len() {
        return Err(anyhow!("Could not find every artifact built in {}", path.display()));
    }
    let mut args = vec!["pacman", "-U", "--noconfirm"];
    args.extend(packages.iter().map(|package| package.as_str()));
    if !run_inherit(&settings().elevation, &args)? {
        return Err(anyhow!("Failed to install artifacts for {}", path.display()));
    }
    Ok(())
}

fn run() -> Result<()> {
    if rustix::process::geteuid().is_root() {
        return Err(anyhow!("depac must not be run as root, makepkg disallows it"));
    }
    let path = std::env::args().nth(1).ok_or_else(|| anyhow!("Configuration path not given"))?;
    let config: Config = serde_json::from_str(&std::fs::read_to_string(path)?).map_err(|err| anyhow!("Failed to parse config: {}", err))?;
    init_settings(config.settings);
    let pkgbuilds: Vec<Pkgbuild> = config.pkgbuilds.into_iter().map(|calibre| calibre.complexify()).collect(); // Normalises to the complex form
    let mut desired_packages = config.packages.clone();
    desired_packages.extend(pkgbuilds.iter().flat_map(|pkgbuild| pkgbuild.artifacts.iter().cloned()));

    // Remove unused deps
    let unused_deps_raw = run_capture("pacman", &["-Qtdq"]).unwrap_or_default(); // Nonzero error on no orphans
    let unused_deps = subtract(&lines_to_vec(&unused_deps_raw), &config.ignore);
    if !unused_deps.is_empty() && check_package_warn_limit(&unused_deps, settings().package_removal_confirmation_count, settings().default_true_confirmation)? {
        remove_packages(&unused_deps)?;
    }
    // Remove no longer specified packages
    let installed_raw = run_capture("pacman", &["-Qeq"])?;
    let installed = lines_to_vec(&installed_raw);
    let to_remove_candidates = subtract(&subtract(&installed, &desired_packages), &config.ignore);
    if check_package_warn_limit(&to_remove_candidates, settings().package_removal_confirmation_count, settings().default_true_confirmation)? {
        let to_remove = filter_removable(to_remove_candidates)?;
        remove_packages(&to_remove)?;
    }
    // Install packages
    let installed_raw = run_capture("pacman", &["-Qeq"])?; // Will have changed, after removals
    let installed = lines_to_vec(&installed_raw);
    let to_install = subtract(&config.packages, &installed);
    let to_install = filter_installables(to_install)?;
    install_packages(&to_install)?;

    // PKGBUILDs
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache) => PathBuf::from(cache),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("Bruh"))?).join(".cache"),
    }
    .join("depac");
    std::fs::create_dir_all(&cache)?;
    // Remove the cloned repos, if they're no longer needed, or their url is wrong
    for repo in std::fs::read_dir(&cache)? {
        let repo = repo?;
        let pkgbuild = pkgbuilds.iter().find(|pkgbuild| repo.file_name().to_string_lossy() == pkgbuild.base);
        let removal_reason = match pkgbuild {
            Some(pkgbuild) => {
                let git = match &pkgbuild.git {
                    Some(git) => git.clone(),
                    None => get_aur_git(&pkgbuild.base),
                };
                match Repository::open(repo.path()) {
                    Ok(repository) => match repository.find_remote("origin") {
                        Ok(remote) => match remote.url() {
                            Ok(url) if url != git => Some(format!("git URL changed: {} -> {}", url, git)),
                            Ok(_) => None,
                            Err(err) => Some(format!("could not read origin URL: {}", err)),
                        },
                        Err(err) => Some(format!("could not find origin remote: {}", err)),
                    },
                    Err(err) => Some(format!("invalid Git repository: {}", err)),
                }
            }
            None => Some("removed explicitly".to_string()),
        };
        if let Some(removal_reason) = removal_reason {
            logln!("Removing pkgbuild repository: {} ({})", repo.path().display(), removal_reason);
            std::fs::remove_dir_all(repo.path())?;
        }
    }

    let mut rpc_checked = 0;
    let mut git_checked = 0;
    for pkgbuild in &pkgbuilds {
        let path = cache.join(&pkgbuild.base);
        let git = pkgbuild.git.clone().unwrap_or_else(|| get_aur_git(&pkgbuild.base));
        let rpc = pkgbuild.rpc.unwrap_or(pkgbuild.git.is_none());
        if rpc {
            rpc_checked += 1;
        } else {
            git_checked += 1;
        }
        let cloned = !path.exists();
        if cloned {
            logln!("Cloning: {}", pkgbuild.base);
            let mut fetch_options = FetchOptions::new();
            fetch_options.depth(1); // Shallow clone
            RepoBuilder::new().fetch_options(fetch_options).clone(&git, &path)?;
        }

        let build_reason = if cloned {
            Some("new clone".to_string())
        } else {
            let version = if rpc {
                get_rpc_version(pkgbuild.artifacts.first().unwrap())?
            } else {
                update_pkgbuild(&path)?;
                // Must run all steps upto pkgver, so pkgver() can correctly derive the new version
                run_capture_in("makepkg", Some(&path), &["--nobuild", "--noconfirm"])?;
                srcinfo_version(&path)?
            };

            let mut build_reason = None;
            for artifact in &pkgbuild.artifacts {
                let installed = match run_capture("pacman", &["-Q", artifact]) {
                    Ok(installed) => installed,
                    Err(_) => {
                        build_reason = Some(format!("{} is not installed", artifact));
                        break;
                    }
                };
                let installed = installed.split_whitespace().nth(1).ok_or_else(|| anyhow!("Could not determine installed version of {}", artifact))?;
                match alpm::vercmp(installed, version.as_str()) {
                    std::cmp::Ordering::Less => {
                        build_reason = Some(format!("version: {} <- {}", installed, version));
                        break;
                    }
                    std::cmp::Ordering::Equal => {}
                    std::cmp::Ordering::Greater => {
                        warnln!("Local {} newer than RPC ({} <- {}), try disabling RPC", artifact, installed, version);
                    }
                }
            }
            build_reason
        };

        if let Some(build_reason) = build_reason {
            if rpc && !cloned {
                update_pkgbuild(&path)?;
            }
            logln!("Building {} ({})", pkgbuild.base, build_reason);
            install_pkgbuild(&path, &pkgbuild.artifacts)?;
        }
    }
    logln!("Completed pkgbuilds upgrades (rpc checks: {}, git checks: {})", rpc_checked, git_checked);
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        fail!("{}", err);
    }
}
