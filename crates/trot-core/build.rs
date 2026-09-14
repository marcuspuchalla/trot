use std::process::Command;
fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    let dirty = Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .ok()
        .map(|s| (!s.success()).to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=TROT_BUILD_COMMIT={sha}");
    println!("cargo:rustc-env=TROT_BUILD_DIRTY={dirty}");
}
