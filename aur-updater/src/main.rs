use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;
use regex::Regex;

#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug)]
#[command(author, version, about = "Robust AUR Monorepo Updater")]
struct Args {
    /// Automatically commit changes if updates are found
    #[arg(short, long)]
    commit: bool,

    /// Automatically git push to origin after a successful commit
    #[arg(short, long)]
    push: bool,

    /// Test-build the package with makepkg before committing
    #[arg(short, long)]
    build: bool,

    /// Do not sign the git commit (omits -S flag)
    #[arg(short, long)]
    no_sign: bool,

    /// Base directory or specific package folder
    #[arg(short, long, default_value = ".")]
    dir: String,

    /// Custom commit message suffix (appended after 'chore(pkgname): ')
    #[arg(short, long)]
    message: Option<String>,
}

fn main() {
    let args = Args::parse();
    let base_path = PathBuf::from(&args.dir);

    if !base_path.exists() {
        eprintln!("Error: Target directory '{}' does not exist.", args.dir);
        std::process::exit(1);
    }

    let mut monorepo_updated = false;

    // Is this a single package directory?
    if base_path.join("PKGBUILD").exists() {
        match process_package(&base_path, &args) {
            Ok(updated) => monorepo_updated = updated,
            Err(e) => eprintln!(" -> Error processing [{}]: {e}", base_path.display()),
        }
    } else {
        // Otherwise, treat it as the monorepo root and scan subdirectories
        match fs::read_dir(&base_path) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && path.join("PKGBUILD").exists() {
                        match process_package(&path, &args) {
                            Ok(updated) => {
                                if updated {
                                    monorepo_updated = true;
                                }
                            }
                            Err(e) => eprintln!(" -> Error processing [{}]: {e}", path.display()),
                        }
                    }
                }
            }
            Err(e) => eprintln!("Failed to read base directory: {e}"),
        }
    }

    // If any submodule pointers were committed, push the parent monorepo to GitHub
    if monorepo_updated && args.push {
        println!(" -> Pushing parent monorepo changes up to GitHub...");
        let push_status = Command::new("git")
            .args(["push", "origin", "HEAD"])
            .status();

        match push_status {
            Ok(s) if s.success() => println!(" -> Parent monorepo pushed successfully!"),
            _ => eprintln!(" -> Warning: Parent monorepo git push failed."),
        }
    }
}

fn process_package(dir: &Path, args: &Args) -> Result<bool, Box<dyn std::error::Error>> {
    let folder_name = dir.file_name().unwrap().to_string_lossy().to_string();
    let pkgbuild_path = dir.join("PKGBUILD");
    let content = fs::read_to_string(&pkgbuild_path)?;

    let current_version = extract_variable(&content, "pkgver").unwrap_or_default();
    let upstream_url = extract_variable(&content, "url").unwrap_or_default();

    let mut updated = false;
    let mut parent_repo_changed = false;

    println!("Checking [{folder_name}]...");

    // Check for pre-existing uncommitted manual modifications to the PKGBUILD
    let local_git_status = Command::new("git")
        .args(["status", "--porcelain", "PKGBUILD"])
        .current_dir(dir)
        .output()?;

    let has_local_changes = !local_git_status.stdout.is_empty();

    // 1. Route: Handle Pre-modified local configurations (Description/Pkgrel bumps)
    if has_local_changes {
        println!(
            " -> Local modifications detected in PKGBUILD. Bypassing upstream version checks."
        );
        updated = true;
    }
    // 2. Route: Handle VCS/Git packages
    else if folder_name.ends_with("-git") || content.contains("pkgver()") {
        println!(" -> VCS package detected. Running updpkgver...");
        let status = Command::new("updpkgver").current_dir(dir).status();
        let run_success = match status {
            Ok(s) => s.success(),
            _ => Command::new("makepkg")
                .args(["-od", "--noprepare"])
                .current_dir(dir)
                .status()?
                .success(),
        };

        if run_success {
            let git_diff = Command::new("git")
                .args(["diff", "--name-only", "PKGBUILD"])
                .current_dir(dir)
                .output()?;
            if git_diff.stdout.is_empty() {
                println!(" -> Up to date (VCS HEAD unchanged)");
            } else {
                updated = true;
            }
        }
    }
    // 3. Route: Handle Standard automated GitHub upgrades
    else if upstream_url.contains("github.com")
        && let Some(latest_version) = get_latest_github_version(&upstream_url)?
    {
        if latest_version == current_version {
            println!(" -> Up to date ({current_version})");
        } else {
            println!(" -> New version available: {current_version} -> {latest_version}");

            let updated_content = update_pkgbuild_version(&content, &latest_version);
            fs::write(&pkgbuild_path, updated_content)?;

            println!(" -> Regenerating integrity checksums via updpkgsums...");
            let checksum_status = Command::new("updpkgsums").current_dir(dir).status()?;
            if !checksum_status.success() {
                return Err(format!("updpkgsums failed for package '{folder_name}'").into());
            }
            updated = true;
        }
    }

    // Process Lifecycle execution block if updates/modifications are present
    if updated {
        // Regenerate .SRCINFO to match current state of the PKGBUILD
        println!(" -> Syncing and regenerating .SRCINFO...");
        let srcinfo_output = Command::new("makepkg")
            .arg("--printsrcinfo")
            .current_dir(dir)
            .output()?;
        if srcinfo_output.status.success() {
            fs::write(dir.join(".SRCINFO"), srcinfo_output.stdout)?;
        }

        // Optional: Pre-flight Build Test with aggressively isolated environment
        if args.build {
            println!(" -> [--build] Initiating sandbox test build via makepkg...");
            let mut build_cmd = Command::new("makepkg");
            build_cmd
                .args(["-s", "--noconfirm", "--needed", "-c", "-f", "-C"])
                .current_dir(dir);

            for (key, _) in std::env::vars() {
                if key.starts_with("CARGO") || key.starts_with("RUST") {
                    build_cmd.env_remove(&key);
                }
            }
            build_cmd.env_remove("LD_LIBRARY_PATH");
            build_cmd.env_remove("DYLD_LIBRARY_PATH");

            let build_status = build_cmd.status()?;
            if !build_status.success() {
                return Err(format!(
                    "Test build failed for package '{folder_name}'. Aborting Git sequence."
                )
                .into());
            }
            println!(" -> Test build completed successfully!");
        }

        // Git Verification & Lifecycle Action Block
        let git_status = Command::new("git")
            .args(["status", "--porcelain", "."])
            .current_dir(dir)
            .output()?;

        if !git_status.stdout.is_empty() {
            let fresh_content = fs::read_to_string(&pkgbuild_path)?;
            let final_version =
                extract_variable(&fresh_content, "pkgver").unwrap_or(current_version);
            println!("Ready to commit changes for [{folder_name}] at v{final_version}!");

            if args.commit {
                println!(" -> Staging and committing changes inside submodule...");
                Command::new("git")
                    .args(["add", "PKGBUILD", ".SRCINFO"])
                    .current_dir(dir)
                    .status()?;

                // Resolve commit message (custom vs automated fallback)
                let commit_msg = args.message.as_ref().map_or_else(
                    || format!("chore({folder_name}): bump to v{final_version}"),
                    |custom| format!("chore({folder_name}): {custom}"),
                );

                let mut commit_cmd = Command::new("git");
                commit_cmd
                    .args(["commit", "-m", &commit_msg])
                    .current_dir(dir);

                if !args.no_sign {
                    commit_cmd.arg("-S");
                }

                if commit_cmd.status()?.success() {
                    println!(" -> Submodule commit generated successfully.");

                    if args.push {
                        println!(" -> [--push] Pushing changes upstream to the AUR remote...");
                        let push_status = Command::new("git")
                            .args(["push", "origin", "HEAD"])
                            .current_dir(dir)
                            .status()?;

                        if push_status.success() {
                            println!(" -> Successfully pushed upstream to AUR.");
                        } else {
                            eprintln!(" -> Warning: Git push failed.");
                        }
                    }

                    println!(" -> Syncing submodule pointer reference in parent repository...");

                    // 1. Point this command to the monorepo root folder
                    let parent_add = Command::new("git")
                        .args(["add", &folder_name])
                        .current_dir(dir.parent().unwrap_or_else(|| Path::new(".")))
                        .status()?;

                    if parent_add.success() {
                        let mut parent_commit = Command::new("git");
                        parent_commit.args(["commit", "-m", &commit_msg]);

                        // 2. Point the commit command to the monorepo root folder too
                        parent_commit.current_dir(dir.parent().unwrap_or_else(|| Path::new(".")));

                        if !args.no_sign {
                            parent_commit.arg("-S");
                        }

                        if parent_commit.status()?.success() {
                            println!(" -> Parent repository pointer tracked successfully.");
                            parent_repo_changed = true;
                        }
                    }
                }
            }
        }
    }

    Ok(parent_repo_changed)
}

fn extract_variable(content: &str, var_name: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?m)^\s*{var_name}\s*=\s*(?:"([^"]+)"|([^\s]+))"#
    ))
    .unwrap();
    if let Some(caps) = re.captures(content) {
        return caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|m| m.as_str().to_string());
    }
    None
}

fn update_pkgbuild_version(content: &str, new_version: &str) -> String {
    let re_ver = Regex::new(r"(?m)^\s*pkgver=.*$").unwrap();
    let re_rel = Regex::new(r"(?m)^\s*pkgrel=.*$").unwrap();

    let updated = re_ver
        .replace(content, &format!(r#"pkgver="{new_version}""#))
        .into_owned();
    re_rel.replace(&updated, "pkgrel=1").into_owned()
}

fn get_latest_github_version(url: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let re = Regex::new(r"github\.com/([^/]+)/([^/]+)").unwrap();
    if let Some(caps) = re.captures(url) {
        let owner = caps.get(1).unwrap().as_str();
        let repo = caps
            .get(2)
            .unwrap()
            .as_str()
            .trim_end_matches(".git")
            .trim_end_matches('/');

        let api_url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
        let client = reqwest::blocking::Client::builder()
            .user_agent("aur-monorepo-updater")
            .build()?;

        let res = client.get(&api_url).send()?;
        if res.status().is_success() {
            let json: serde_json::Value = res.json()?;
            if let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) {
                return Ok(Some(tag.trim_start_matches('v').to_string()));
            }
        } else {
            // Fallback for repos that use Git tags but don't explicitly create GitHub Release entries
            let tags_url = format!("https://api.github.com/repos/{owner}/{repo}/tags");
            let res_tags = client.get(&tags_url).send()?;
            if res_tags.status().is_success() {
                let json: serde_json::Value = res_tags.json()?;
                if let Some(first_tag) = json
                    .get(0)
                    .and_then(|t| t.get("name"))
                    .and_then(|v| v.as_str())
                {
                    return Ok(Some(first_tag.trim_start_matches('v').to_string()));
                }
            }
        }
    }
    Ok(None)
}
