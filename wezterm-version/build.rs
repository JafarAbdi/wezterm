use std::path::PathBuf;

/// The files whose change moves HEAD to another commit.
///
/// `HEAD` lives in the git dir of this worktree; the branch it names lives
/// in the common git dir, either as a loose file or inside `packed-refs`.
/// A packed branch becomes a loose file on its next commit, so the nearest
/// existing directory above the loose path is watched until the file exists.
/// Only existing paths are returned: cargo reruns the script on every build
/// for a path that is missing.
fn head_inputs(repo: &git2::Repository) -> Vec<PathBuf> {
    let mut inputs = vec![repo.path().join("HEAD")];
    let branch = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|head| head.symbolic_target().map(str::to_string));
    if let Some(branch) = branch {
        let common = repo.commondir();
        inputs.push(common.join("packed-refs"));
        let loose = common.join(&branch);
        inputs.extend(
            loose
                .ancestors()
                .find(|path| path.exists())
                .map(PathBuf::from),
        );
    }
    inputs.retain(|path| path.exists());
    inputs
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // If a file named `.tag` is present, we'll take its contents for the
    // version number that we report in wezterm -h.
    let mut ci_tag = String::new();
    if let Ok(tag) = std::fs::read("../.tag") {
        if let Ok(s) = String::from_utf8(tag) {
            ci_tag = s.trim().to_string();
            println!("cargo:rerun-if-changed=../.tag");
        }
    } else {
        // Otherwise we'll derive it from the git information

        if let Ok(repo) = git2::Repository::discover(".") {
            for path in head_inputs(&repo) {
                println!("cargo:rerun-if-changed={}", path.display());
            }

            if let Ok(output) = std::process::Command::new("git")
                .args(&[
                    "-c",
                    "core.abbrev=8",
                    "show",
                    "-s",
                    "--format=%cd-%h",
                    "--date=format:%Y%m%d-%H%M%S",
                ])
                .output()
            {
                let info = String::from_utf8_lossy(&output.stdout);
                ci_tag = info.trim().to_string();
            }
        }
    }

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());

    println!("cargo:rustc-env=WEZTERM_TARGET_TRIPLE={}", target);
    println!("cargo:rustc-env=WEZTERM_CI_TAG={}", ci_tag);
}
