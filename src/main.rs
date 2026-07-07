use anyhow::{Result, anyhow};
use owo_colors::OwoColorize;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, Write},
    process::{Command, exit},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct Config {
    #[serde(default)]
    packages: Vec<String>,

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
    #[serde(default = "default_packages")]
    package_removal_confirmation_count: u32,
    #[serde(default = "default_true")]
    default_true_confirmation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            package_removal_confirmation_count: 0,
            default_true_confirmation: true,
        }
    }
}

fn default_packages() -> u32 {
    0
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

// Stdout is captured
fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program).args(args).output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Command failed ({} {}): {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// Stdout is inherited to our term
fn run_inherit(program: &str, args: &[&str]) -> Result<bool> {
    let status = Command::new(program).args(args).status()?;
    Ok(status.success())
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

    Ok(required_by_line
        .split_whitespace()
        .filter(|p| *p != "None")
        .map(|p| p.to_string())
        .collect())
}

/// Turn a newline separated command output into a Vec<String>, dropping blanks.
fn lines_to_vec(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn subtract(a: &[String], b: &[String]) -> Vec<String> {
    let set: HashSet<&String> = b.iter().collect();
    a.iter()
        .filter(|item| !set.contains(item))
        .cloned()
        .collect()
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
            match run_capture("pacman", &["-D", "--asdep", pkg]) {
                Ok(_) => logln!("Marked {} as a dependency", pkg),
                Err(e) => {
                    fail!("Could not mark {} as a dependency: {}", pkg, e);
                }
            }
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

    let mut args = vec!["-Rns", "--noconfirm"];
    args.extend(packages.iter().map(|p| p.as_str()));

    if run_inherit("pacman", &args)? {
        log!("Removed Packages: ");
        for p in packages {
            print!("{} ", p);
        }
        println!();
    } else {
        fail!("Failed to remove one or more packages");
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
            logln!(
                "{} has already been installed as a dependency, marking as explicit",
                pkg
            );
            match run_capture("pacman", &["-D", "--asexplicit", pkg.as_str()]) {
                Ok(_) => logln!("Marked {} as explicitly installed", pkg),
                Err(e) => {
                    fail!("Could not mark {} as explicitly installed: {}", pkg, e);
                }
            }
        } else {
            result.push(pkg);
        }
    }
    Ok(result)
}

fn install_packages(packages: &[String]) -> Result<()> {
    let mut args = vec!["-Syu", "--noconfirm"];
    if packages.is_empty() {
        logln!("Upgrading System");
    } else {
        log!("Upgrading System & Attempting to install: ");
        for p in packages {
            print!("{} ", p);
        }
        println!();
        args.extend(packages.iter().map(|p| p.as_str()));
    }
    if run_inherit("pacman", &args)? {
        if packages.is_empty() {
            logln!("Completed System Upgrade");
        } else {
            logln!("Completed System Upgrade & Installations");
        }
    } else if packages.is_empty() {
        fail!("System upgrade failed");
    } else {
        fail!("Installation/upgrade failed");
    }
    Ok(())
}

fn main() -> Result<()> {
    let arg = env::args().nth(1).expect("Failed to get json arg");
    let config: Config = serde_json::from_str(&arg).expect("Failed to parse config");

    // Remove unused deps
    let unused_deps_raw = run_capture("pacman", &["-Qtdq"]).unwrap_or_default(); // Nonzero error on
    // no orphans
    let unused_deps = subtract(&lines_to_vec(&unused_deps_raw), &config.ignore);
    if !unused_deps.is_empty()
        && check_package_warn_limit(
            &unused_deps,
            config.settings.package_removal_confirmation_count,
            config.settings.default_true_confirmation,
        )?
    {
        remove_packages(&unused_deps)?;
    }
    // Remove no longer specified packages
    let installed_raw = run_capture("pacman", &["-Qeq"])?;
    let installed = lines_to_vec(&installed_raw);
    let to_remove_candidates = subtract(&subtract(&installed, &config.packages), &config.ignore);
    if check_package_warn_limit(
        &to_remove_candidates,
        config.settings.package_removal_confirmation_count,
        config.settings.default_true_confirmation,
    )? {
        let to_remove = filter_removable(to_remove_candidates)?;
        remove_packages(&to_remove)?;
    }
    // Install packages
    let installed_raw = run_capture("pacman", &["-Qeq"])?; // Will have changed, after removals
    let installed = lines_to_vec(&installed_raw);
    let to_install = subtract(&config.packages, &installed);
    let to_install = filter_installables(to_install)?;
    install_packages(&to_install)?;
    Ok(())
}
