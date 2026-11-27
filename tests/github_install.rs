use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "github-install-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_failure(output: &Output, expected: &str) {
    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(expected), "{error}");
}

#[test]
fn github_install_rejects_unsafe_inputs_and_conflicting_options() {
    let dir = Directory::new();
    for repository in [
        "https://user:secret@github.com/owner/repo",
        "https://github.com.evil.example/owner/repo",
        "https://github.com/owner/repo?x=y",
        "owner/../repo",
        "-option/repo",
        "file:///tmp/repo",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(["install", "--git", repository])
            .env("COG_HOME", dir.0.join("home"))
            .output()
            .unwrap();
        assert_failure(&output, "GitHub repository");
    }
    for (args, expected) in [
        (vec!["install", "--rev", "v1"], "--rev requires --git"),
        (
            vec!["install", "--git", "owner/repo", "--path", "."],
            "choose either --path or --git",
        ),
        (
            vec!["install", "--git", "owner/repo", "--rev", "-bad"],
            "invalid Git revision",
        ),
        (
            vec!["check", "--git", "owner/repo"],
            "only valid with `install`",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(args)
            .env("COG_HOME", dir.0.join("home"))
            .output()
            .unwrap();
        assert_failure(&output, expected);
    }
}

#[cfg(unix)]
#[test]
fn github_install_uses_revision_and_existing_local_store_without_network() {
    let dir = Directory::new();
    let repository = dir.0.join("remotes/example/math.git");
    std::fs::create_dir_all(&repository).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    dir.write(
        "remotes/example/math.git/package.kdl",
        "package \"integer-math\" version=\"0.1.0\" kind=\"library\"\n",
    );
    dir.write(
        "remotes/example/math.git/src/lib.cog",
        "use integer_math;\n",
    );
    dir.write(
        "remotes/example/math.git/src/integer_math.cog",
        "public fn square(value: int) -> int { value * value }\n",
    );
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Cog Test",
        "-c",
        "user.email=cog@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "first",
    ]);
    git(&["tag", "v1"]);
    let commit = Command::new("git")
        .current_dir(&repository)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let commit = String::from_utf8(commit.stdout).unwrap();
    dir.write(
        "remotes/example/math.git/package.kdl",
        "package \"integer-math\" version=\"0.2.0\" kind=\"library\"\n",
    );
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Cog Test",
        "-c",
        "user.email=cog@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "second",
    ]);

    let temp = dir.0.join("home/tmp");
    let config = dir.0.join("empty-gitconfig");
    std::fs::write(&config, "").unwrap();
    let prefix = format!("file://{}/", dir.0.join("remotes").display());
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(args)
            .env("COG_HOME", dir.0.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &config)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("url.{prefix}.insteadOf"))
            .env("GIT_CONFIG_VALUE_0", "https://github.com/")
            .output()
            .unwrap()
    };
    let installed = run(&[
        "install",
        "--git",
        "example/math",
        "--rev",
        "v1",
        "--locked",
    ]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let details = String::from_utf8_lossy(&installed.stderr);
    assert!(details.contains("integer-math 0.1.0 library package"));
    assert!(details.contains(commit.trim()));
    let installed_again = run(&[
        "install",
        "--git",
        "https://github.com/example/math.git",
        "--rev",
        "v1",
    ]);
    assert!(
        installed_again.status.success(),
        "{}",
        String::from_utf8_lossy(&installed_again.stderr)
    );
    let by_commit = run(&["install", "--git", "example/math", "--rev", commit.trim()]);
    assert!(
        by_commit.status.success(),
        "{}",
        String::from_utf8_lossy(&by_commit.stderr)
    );
    let latest = run(&["install", "--git", "example/math"]);
    assert!(
        latest.status.success(),
        "{}",
        String::from_utf8_lossy(&latest.stderr)
    );
    assert!(String::from_utf8_lossy(&latest.stderr).contains("integer-math 0.2.0 library package"));
    assert_failure(
        &run(&["install", "--git", "example/math", "--rev", "missing"]),
        "git failed",
    );
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);

    dir.write(
        "consumer/package.kdl",
        "package \"consumer\" version=\"0.1.0\"\n",
    );
    dir.write(
        "consumer/dependencies.kdl",
        "arithmetic package=\"integer-math\" version=\"0.1.0\"\n",
    );
    dir.write("consumer/src/main.cog", "@!include(\"std\") @!include(\"arithmetic\") use io; use integer_math from arithmetic; fn main() { io::println(integer_math::square(12))!!; }\n");
    let output = run(&["run", dir.0.join("consumer").to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"144\n");

    let commit_tag = |name: &str| {
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.name=Cog Test",
            "-c",
            "user.email=cog@example.invalid",
            "commit",
            "--quiet",
            "-m",
            name,
        ]);
        git(&["tag", name]);
    };
    let prefix = "remotes/example/math.git";
    dir.write(
        &format!("{prefix}/package.kdl"),
        "package \"outer\" version=\"0.3.0\" kind=\"library\"\n",
    );
    dir.write(
        &format!("{prefix}/dependencies.kdl"),
        "inner path=\"vendor/inner\"\n",
    );
    dir.write(
        &format!("{prefix}/vendor/inner/package.kdl"),
        "package \"inner\" version=\"1.0.0\" kind=\"library\"\n",
    );
    dir.write(&format!("{prefix}/vendor/inner/src/lib.cog"), "");
    commit_tag("within-checkout");
    let safe = run(&[
        "install",
        "--git",
        "example/math",
        "--rev",
        "within-checkout",
    ]);
    assert!(
        safe.status.success(),
        "{}",
        String::from_utf8_lossy(&safe.stderr)
    );

    dir.write(
        "outside/package.kdl",
        "package \"outside\" version=\"1.0.0\" kind=\"library\"\n",
    );
    dir.write(
        &format!("{prefix}/vendor/inner/dependencies.kdl"),
        &format!("outside path=\"{}\"\n", dir.0.join("outside").display()),
    );
    commit_tag("escape");
    assert_failure(
        &run(&["install", "--git", "example/math", "--rev", "escape"]),
        "dependency escapes the checkout",
    );
    std::fs::remove_file(repository.join("vendor/inner/dependencies.kdl")).unwrap();

    std::os::unix::fs::symlink(dir.0.join("outside"), repository.join("link")).unwrap();
    dir.write(
        &format!("{prefix}/dependencies.kdl"),
        "outside path=\"link\"\n",
    );
    commit_tag("symlink");
    assert_failure(
        &run(&["install", "--git", "example/math", "--rev", "symlink"]),
        "contains a symbolic link",
    );
    std::fs::remove_file(repository.join("link")).unwrap();

    dir.write(&format!("{prefix}/local.cogpkg"), "not opened");
    dir.write(
        &format!("{prefix}/dependencies.kdl"),
        "archive archive=\"local.cogpkg\"\n",
    );
    commit_tag("archive");
    assert_failure(
        &run(&["install", "--git", "example/math", "--rev", "archive"]),
        "does not support archive dependencies",
    );

    dir.write(
        &format!("{prefix}/package.kdl"),
        "package \"outer\" version=\"0.4.0\" kind=\"library\"\n",
    );
    dir.write(
        &format!("{prefix}/dependencies.kdl"),
        "arithmetic package=\"integer-math\" version=\"0.1.0\"\n",
    );
    dir.write(&format!("{prefix}/src/lib.cog"), "");
    commit_tag("installed-dependency");
    let safe = run(&[
        "install",
        "--git",
        "example/math",
        "--rev",
        "installed-dependency",
    ]);
    assert!(
        safe.status.success(),
        "{}",
        String::from_utf8_lossy(&safe.stderr)
    );
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn github_install_ignores_repository_environment_and_checks_out_inside_cog_home() {
    use std::os::unix::fs::PermissionsExt;

    let dir = Directory::new();
    let git = |repository: &std::path::Path, args: &[&str]| {
        let output = Command::new("git")
            .current_dir(repository)
            .args([
                "-c",
                "user.name=Cog Test",
                "-c",
                "user.email=cog@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let source = dir.0.join("remotes/example/math.git");
    dir.write(
        "remotes/example/math.git/package.kdl",
        "package \"integer-math\" version=\"0.1.0\" kind=\"library\"\n",
    );
    dir.write("remotes/example/math.git/src/lib.cog", "");
    let victim = dir.0.join("victim");
    dir.write("victim/precious.txt", "unchanged\n");
    for repository in [&source, &victim] {
        git(repository, &["init", "--quiet", "--initial-branch=main"]);
        git(repository, &["add", "."]);
        git(repository, &["commit", "--quiet", "-m", "first"]);
    }
    let victim_state = || {
        (
            git(&victim, &["symbolic-ref", "HEAD"]),
            git(&victim, &["rev-parse", "HEAD"]),
            git(&victim, &["status", "--porcelain"]),
            std::fs::read(victim.join(".git/index")).unwrap(),
        )
    };
    let before = victim_state();

    // Observe what every Git subprocess receives through a wrapper on PATH.
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let log = dir.0.join("git.log");
    let wrapper = dir.0.join("bin/git");
    dir.write(
        "git-wrapper",
        &format!(
            "#!/bin/sh\n\
             env | grep '^GIT_' >> '{log}'\n\
             for last; do :; done\n\
             if [ \"$2\" = --quiet ] && [ \"$1\" = clone ]; then\n\
             echo \"checkout=$last\" >> '{log}'\n\
             ls -ld \"$(dirname \"$last\")\" >> '{log}'\n\
             fi\n\
             exec '{git}' \"$@\"\n",
            log = log.display(),
            git = real_git.display(),
        ),
    );
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    // A child process writes the executable, so no concurrently forked test can
    // inherit a writable handle to it.
    let copied = Command::new("sh")
        .args(["-c", "cp \"$0\" \"$1\" && chmod 755 \"$1\""])
        .arg(dir.0.join("git-wrapper"))
        .arg(&wrapper)
        .status()
        .unwrap();
    assert!(copied.success());
    let path = std::env::join_paths(
        std::iter::once(wrapper.parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();

    let config = dir.0.join("empty-gitconfig");
    std::fs::write(&config, "").unwrap();
    let prefix = format!("file://{}/", dir.0.join("remotes").display());
    let home = dir.0.join("home");
    let victim_git = victim.join(".git");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(args)
            .env("PATH", &path)
            .env("COG_HOME", &home)
            .env("GIT_DIR", &victim_git)
            .env("GIT_WORK_TREE", &victim)
            .env("GIT_INDEX_FILE", victim_git.join("index"))
            .env("GIT_COMMON_DIR", &victim_git)
            .env("GIT_OBJECT_DIRECTORY", victim_git.join("objects"))
            .env("GIT_NAMESPACE", "elsewhere")
            // User transport and configuration settings stay in effect.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &config)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("url.{prefix}.insteadOf"))
            .env("GIT_CONFIG_VALUE_0", "https://github.com/")
            .output()
            .unwrap()
    };

    let installed = run(&["install", "--git", "example/math"]);
    assert_eq!(victim_state(), before);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let seen = std::fs::read_to_string(&log).unwrap();
    for removed in [
        "GIT_DIR=",
        "GIT_WORK_TREE=",
        "GIT_INDEX_FILE=",
        "GIT_COMMON_DIR=",
        "GIT_OBJECT_DIRECTORY=",
        "GIT_NAMESPACE=",
    ] {
        assert!(!seen.contains(removed), "{removed}: {seen}");
    }
    assert!(seen.contains("GIT_CONFIG_GLOBAL="), "{seen}");
    let temporary = home.join("tmp");
    assert!(
        seen.contains(&format!("checkout={}/", temporary.display())),
        "{seen}"
    );
    assert!(seen.contains("\ndrwx------"), "{seen}");
    assert_eq!(
        std::fs::metadata(&temporary).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);

    assert_failure(
        &run(&["install", "--git", "example/math", "--rev", "missing"]),
        "git failed",
    );
    assert_eq!(victim_state(), before);
    assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
}
