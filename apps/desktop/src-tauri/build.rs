// There is deliberately NO default backend URL. A build that does not set
// PETAL_BACKEND_URL bakes nothing, and `token::backend_base_url` returns
// MissingEnv. Official releases set it explicitly in the release recipe.
//
// Do not reintroduce a hosted fallback: this repository is public, and a
// baked default means every third-party build silently mints tokens against
// the maintainers' LiveKit/Vercel deployment. See docs/SELF_HOSTING.md.
const SYSTEM_SWIFT_RUNTIME_RPATH: &str = "/usr/lib/swift";

fn main() {
    // The Rust unit-test harness (`cargo test --lib`) imports
    // `TaskDialogIndirect`, which exists only in Common Controls v6. The
    // shipped `desktop` binary gets the v6 manifest from tauri-build
    // (`cargo:rustc-link-arg-bins` via tauri-winres), but test harnesses get
    // none, so they bind comctl32 v5 and fail to LOAD with 0xc0000139 before
    // any test runs. `/MANIFESTDEPENDENCY` merges the v6 dependency into the
    // linker's manifest instead of adding a second RT_MANIFEST resource, so
    // it works for the `--lib` harness AND coexists with tauri-winres's
    // manifest on the `desktop` binary (plain `cargo:rustc-link-arg` is
    // required: the scoped `-tests`/`-lib` variants never reach the
    // `cargo test --lib` harness).
    //
    // Note: CI's rust-gate also documents that the repo `.cargo/config.toml`
    // must set `+crt-static` for the MSVC target (issue #673) -- without it
    // the prebuilt MT libwebrtc fails the link with LNK2038.
    #[cfg(target_os = "windows")]
    println!("cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'");
    println!("cargo:rerun-if-env-changed=PETAL_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=PETAL_BUILD_DATE");
    println!("cargo:rerun-if-env-changed=PETAL_BACKEND_URL");
    println!("cargo:rerun-if-env-changed=PETAL_ALLOW_NO_BACKEND");
    println!("cargo:rerun-if-env-changed=PETAL_SENTRY_DSN");
    println!("cargo:rerun-if-env-changed=PETAL_POSTHOG_KEY");
    println!("cargo:rerun-if-env-changed=PETAL_POSTHOG_HOST");
    println!("cargo:rerun-if-env-changed=PETAL_COCKPIT_FRONTEND_PROVENANCE");
    println!("cargo:rerun-if-env-changed=PETAL_OFFICIAL_SOURCE_SHA_FULL");
    println!("cargo:rerun-if-env-changed=PETAL_OFFICIAL_SOURCE_STATE");
    println!("cargo:rerun-if-env-changed=PETAL_SOURCE_PROVENANCE_WRAPPED");
    emit_git_watch_paths();

    let commit = std::env::var("PETAL_GIT_COMMIT").unwrap_or_else(|_| git_commit());
    let source_sha_full = official_source_sha_full();
    let build_date = std::env::var("PETAL_BUILD_DATE").unwrap_or_else(|_| build_date());

    println!("cargo:rustc-env=PETAL_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=PETAL_SOURCE_SHA_FULL={source_sha_full}");
    println!("cargo:rustc-env=PETAL_BUILD_DATE={build_date}");

    // A direct QA binary must carry provenance for the generated cockpit
    // source/status pages. The runtime refuses the unverified fallback before
    // it creates a test window, preventing stale/missing embedded frontend
    // assets from masquerading as a capture failure (#262/#315).
    if is_qa_cockpit_binary() {
        let provenance = std::env::var("PETAL_COCKPIT_FRONTEND_PROVENANCE")
            .unwrap_or_else(|_| "unverified".to_string());
        println!("cargo:rustc-env=PETAL_COCKPIT_FRONTEND_PROVENANCE={provenance}");
    }

    // Bake the backend URL so `option_env!("PETAL_BACKEND_URL")` in token.rs
    // resolves it with no runtime env. An explicit non-empty value is baked;
    // an explicitly empty value is the local-LiveKit opt-out; an ABSENT value
    // bakes nothing, and token.rs reports MissingEnv (debug/test builds fall
    // through to the local dev mint, release builds surface a setup error).
    let configured_backend = match std::env::var("PETAL_BACKEND_URL") {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(value),
        Err(_) => None,
    };
    match configured_backend {
        Some(url) => println!("cargo:rustc-env=PETAL_BACKEND_URL={}", url.trim()),
        // HARD FAIL for release builds. This used to be only a
        // `cargo:warning`, which is trivially lost in tauri build output --
        // and 0.8.2 shipped that way: no baked URL, so EVERY join failed with
        // "no token backend is configured" on a signed, notarized, published
        // release. A warning is not a gate; do not downgrade this back.
        // Escape hatch for a deliberate backend-less release build:
        // PETAL_ALLOW_NO_BACKEND=1.
        None if std::env::var("PROFILE").as_deref() == Ok("release")
            && std::env::var("PETAL_ALLOW_NO_BACKEND").as_deref() != Ok("1") =>
        {
            // Deliberately names NO example host: `windowsBootstrap.test.ts`
            // asserts this file never mentions a hosted backend, so that a
            // public-repo build can't quietly default to the maintainers'
            // deployment. Point users at the doc instead of a URL.
            panic!(
                "PETAL_BACKEND_URL is not set: this release build could not mint tokens and \
                 every join would fail (this is exactly how 0.8.2 shipped broken). Set it to \
                 your own token backend -- see docs/SELF_HOSTING.md. To build a backend-less \
                 release on purpose, set PETAL_ALLOW_NO_BACKEND=1."
            );
        }
        None => {}
    }

    // Bake the Sentry DSN (#281) so `option_env!("PETAL_SENTRY_DSN")` in
    // logging.rs resolves it with no runtime env -- required because a
    // notarized `.app` launched via `open`/Dock/Spotlight has no shell env
    // for a runtime var to land in. Unlike PETAL_BACKEND_URL there is no
    // default to fall back to for a release build with none supplied:
    // absent is absent, and that's the correct (and only sane) behavior for
    // every local/CI build that doesn't explicitly supply the release repo
    // secret -- crash reporting is simply off, not pointed at a bogus DSN.
    // `cargo:rerun-if-env-changed` above is load-bearing: without it, cargo
    // has no reason to know a bare env-var change (no source edit) should
    // invalidate this crate's build cache, and a stale binary could
    // silently keep an old (or absent) DSN baked in.
    let explicit_sentry_dsn = std::env::var("PETAL_SENTRY_DSN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(dsn) = explicit_sentry_dsn {
        println!("cargo:rustc-env=PETAL_SENTRY_DSN={}", dsn.trim());
    }

    // Bake the PostHog project token the same way: a notarized `.app` has no
    // shell env, so a runtime-only var never reaches real users. Absent is
    // the correct local/CI state (product events simply do not fire). Never
    // panic a release build on a missing key — unlike PETAL_BACKEND_URL,
    // analytics is optional. Do not print the token.
    let explicit_posthog_key = std::env::var("PETAL_POSTHOG_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(key) = explicit_posthog_key {
        println!("cargo:rustc-env=PETAL_POSTHOG_KEY={}", key.trim());
    }
    let explicit_posthog_host = std::env::var("PETAL_POSTHOG_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(host) = explicit_posthog_host {
        println!("cargo:rustc-env=PETAL_POSTHOG_HOST={}", host.trim());
    }

    // `livekit`/`webrtc-sys` require the `-ObjC` linker flag on macOS (their
    // prebuilt libwebrtc registers Objective-C categories that need whole-
    // archive loading, or `Room::connect` crashes at runtime with
    // `+[NSString stringForAbslStringView:]: unrecognized selector`).
    // `webrtc-sys`'s own build.rs *does* emit `cargo:rustc-link-arg=-ObjC`,
    // but per Cargo's documented semantics that only applies to `-ObjC`
    // being added to targets built by `webrtc-sys`'s own package -- it does
    // NOT propagate to a downstream binary/example crate (like this one)
    // that merely depends on it as an rlib. So it must be re-emitted here,
    // from our own crate's build script, to actually reach the final
    // `desktop` bin/example link lines. Scoped to macOS only so it can't
    // leak into other targets' builds.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");

        // The QA builder may use full Xcode for static compatibility archives,
        // but the launched cockpit must resolve Swift from macOS itself. This
        // avoids CommandLineTools/system duplicate Swift classes (#315).
        if is_qa_cockpit_binary() {
            println!(
                "cargo:rustc-link-arg-bin=desktop=-Wl,-rpath,{}",
                SYSTEM_SWIFT_RUNTIME_RPATH
            );
        }
    }

    #[cfg(not(test))]
    {
        ensure_frontend_dist();
        tauri_build::build()
    }
}

/// Tauri's CLI runs `beforeBuildCommand` before invoking Cargo, but a direct
/// `cargo build` does not. A custom-protocol build must still have current
/// static assets to embed; otherwise it either fails on a clean checkout or
/// leaves developers running an old frontend. Development builds launched by
/// `tauri dev` disable `custom-protocol`, so they continue to use `devUrl`.
fn ensure_frontend_dist() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !matches!(target_os.as_str(), "windows" | "macos" | "linux")
        || std::env::var_os("CARGO_FEATURE_CUSTOM_PROTOCOL").is_none()
    {
        return;
    }

    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let frontend_dir = manifest_dir
        .parent()
        .expect("src-tauri must have a frontend parent directory");
    let frontend_inputs = [
        frontend_dir.join("src"),
        frontend_dir.join("static"),
        frontend_dir.join("package.json"),
        frontend_dir.join("package-lock.json"),
        frontend_dir.join("svelte.config.js"),
        frontend_dir.join("vite.config.js"),
        frontend_dir.join("tsconfig.json"),
    ];

    for input in &frontend_inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    let dist_entry = frontend_dir.join("build").join("index.html");
    let dist_modified = modified_at(&dist_entry);
    let needs_build = dist_modified.is_none()
        || frontend_inputs
            .iter()
            .any(|input| path_is_newer_than(input, dist_modified));
    if !needs_build {
        return;
    }

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let status = std::process::Command::new(npm)
        .args(["run", "build"])
        .current_dir(frontend_dir)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to start `{npm} run build` in {}: {error}",
                frontend_dir.display()
            )
        });
    assert!(
        status.success(),
        "`npm run build` failed while preparing Tauri frontend assets"
    );
    assert!(
        dist_entry.is_file(),
        "`npm run build` succeeded but {} was not created",
        dist_entry.display()
    );
}

fn modified_at(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn path_is_newer_than(path: &std::path::Path, baseline: Option<std::time::SystemTime>) -> bool {
    let Some(baseline) = baseline else {
        return true;
    };
    if modified_at(path).is_some_and(|modified| modified > baseline) {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .any(|entry| path_is_newer_than(&entry.path(), Some(baseline)))
}

fn is_canonical_full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_source_state(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Only the official source-provenance wrapper may provide a trusted SHA.
/// Direct Cargo builds intentionally remain unverified: Cargo has no native
/// invalidation event for a newly-created unknown untracked input.
fn official_source_sha_full() -> String {
    official_source_sha_full_from(
        std::env::var("PETAL_OFFICIAL_SOURCE_SHA_FULL")
            .ok()
            .as_deref(),
        std::env::var("PETAL_OFFICIAL_SOURCE_STATE").ok().as_deref(),
        std::env::var("PETAL_SOURCE_PROVENANCE_WRAPPED")
            .ok()
            .as_deref(),
    )
}

fn official_source_sha_full_from(
    sha: Option<&str>,
    state: Option<&str>,
    wrapped_marker: Option<&str>,
) -> String {
    sha.filter(|value| is_canonical_full_sha(value))
        .filter(|_| {
            state.is_some_and(is_canonical_source_state)
                && wrapped_marker.is_some_and(|marker| Some(marker) == state)
        })
        .map(str::to_owned)
        .unwrap_or_else(|| "unverified".to_string())
}

/// Cargo must invalidate provenance when a linked-worktree HEAD moves, its
/// index changes, or any tracked source becomes dirty/clean. Never guess a
/// `.git` path: `--git-path` resolves both ordinary and linked worktrees.
fn emit_git_watch_paths() {
    let mut paths = git_watch_paths_at(std::path::Path::new("."));
    paths.sort();
    paths.dedup();
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_watch_paths_at(workdir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for git_path in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_path_at(workdir, git_path) {
            paths.push(path);
        }
    }
    if let Some(reference) = command_output_at(workdir, "git", &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path_at(workdir, &reference) {
            paths.push(path);
        }
    }
    if let Some(root) = command_output_at(workdir, "git", &["rev-parse", "--show-toplevel"]) {
        let root = std::path::PathBuf::from(root);
        if let Some(files) =
            command_output_bytes_at(workdir, "git", &["ls-files", "--full-name", "-z"])
        {
            for name in files
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
            {
                if let Ok(name) = std::str::from_utf8(name) {
                    paths.push(root.join(name));
                }
            }
        }
    }
    paths
}

fn git_path_at(workdir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    command_output_at(
        workdir,
        "git",
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )
    .map(std::path::PathBuf::from)
    .filter(|path| path.exists())
}

fn is_qa_cockpit_binary() -> bool {
    std::env::var_os("CARGO_FEATURE_COCKPIT_PRIVILEGED").is_some()
        && std::env::var("PROFILE").as_deref() == Ok("debug")
}

#[cfg(test)]
mod tests {
    use super::{git_watch_paths_at, official_source_sha_full_from, SYSTEM_SWIFT_RUNTIME_RPATH};
    use std::fs;
    use std::process::Command;

    #[test]
    fn qa_runtime_uses_the_os_swift_runtime() {
        assert_eq!(SYSTEM_SWIFT_RUNTIME_RPATH, "/usr/lib/swift");
    }

    #[test]
    fn official_source_sha_requires_a_complete_state_bound_wrapper_triple() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let state = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let other_state = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        assert_eq!(
            official_source_sha_full_from(Some(sha), Some(state), Some(state)),
            sha
        );
        for (candidate_sha, candidate_state, marker) in [
            (Some(sha), None, None),
            (None, Some(state), None),
            (None, None, Some(state)),
            (Some(sha), Some("malformed"), Some("malformed")),
            (Some(sha), Some(state), None),
            (Some(sha), Some(state), Some(other_state)),
            (
                Some("ABC3456789abcdef0123456789abcdef01234567"),
                Some(state),
                Some(state),
            ),
        ] {
            assert_eq!(
                official_source_sha_full_from(candidate_sha, candidate_state, marker),
                "unverified"
            );
        }
    }

    #[test]
    fn linked_worktree_watches_real_head_index_ref_and_sources() {
        let root = temp_repo("linked");
        let linked = root.with_extension("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                linked.to_str().unwrap(),
                "-b",
                "linked-test",
            ],
        );
        let paths = git_watch_paths_at(&linked);
        assert!(paths.iter().any(|path| {
            path.to_string_lossy().contains(".git/worktrees")
                && path.file_name().is_some_and(|name| name == "HEAD")
        }));
        assert!(paths.iter().any(|path| {
            path.to_string_lossy().contains(".git/worktrees")
                && path.file_name().is_some_and(|name| name == "index")
        }));
        assert!(paths
            .iter()
            .any(|path| path.ends_with("refs/heads/linked-test")));
        let expected_source = linked.join("tracked.txt").canonicalize().unwrap();
        assert!(paths
            .iter()
            .any(|path| path.canonicalize().ok().as_ref() == Some(&expected_source)));
        git(&root, &["worktree", "remove", linked.to_str().unwrap()]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_cargo_provenance_is_fresh_and_tracks_normal_and_linked_worktrees() {
        let root = temp_repo("nested fresh space");
        assert_nested_transitions(&root, &root.join("apps/nested"));

        let linked = root.with_extension("nested-linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                linked.to_str().unwrap(),
                "-b",
                "nested-fresh-linked",
            ],
        );
        assert_nested_transitions(&linked, &linked.join("apps/nested"));
        git(&root, &["worktree", "remove", linked.to_str().unwrap()]);
        fs::remove_dir_all(root).unwrap();
    }

    fn assert_nested_transitions(root: &std::path::Path, package: &std::path::Path) {
        let watched = git_watch_paths_at(package);
        let expected = package.join("src/main.rs").canonicalize().unwrap();
        assert!(watched
            .iter()
            .any(|path| path.canonicalize().ok().as_ref() == Some(&expected)));

        let (_, raw_sha, raw_payload) = cargo_run_nested_verbose(package, ProvenanceMode::Direct);
        assert_eq!(raw_sha, "unverified");
        assert_eq!(raw_payload, "base");

        let (_, first_sha, first_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Trusted);
        let first_count = nested_run_count(package);
        assert_ne!(first_sha, "unverified");
        assert_eq!(first_payload, "base");
        let (_, raw_clean_sha, raw_clean_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(raw_clean_sha, "unverified");
        assert_eq!(raw_clean_payload, "base");
        let raw_clean_count = nested_run_count(package);
        assert_eq!(raw_clean_count, first_count + 1);
        let (second, noop_sha, noop_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(noop_sha, "unverified");
        assert_eq!(noop_payload, "base");
        assert!(
            second.contains("Fresh nested-provenance-fresh-test"),
            "unchanged raw/local-CI invocation was not Fresh:\n{}",
            second
        );
        assert_eq!(nested_run_count(package), raw_clean_count);

        let untracked = package.join("untracked-payload.rs");
        fs::write(
            &untracked,
            "pub const PROVENANCE_PAYLOAD: &str = \"untracked\";\n",
        )
        .unwrap();
        let (_, untracked_sha, untracked_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(untracked_sha, "unverified");
        assert_eq!(untracked_payload, "untracked");
        let untracked_count = nested_run_count(package);
        assert_eq!(untracked_count, raw_clean_count + 1);
        assert_require_clean_refuses(package);

        fs::write(
            &untracked,
            "pub const PROVENANCE_PAYLOAD: &str = \"edited-untracked\";\n",
        )
        .unwrap();
        let (_, edited_untracked_sha, edited_untracked_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(edited_untracked_sha, "unverified");
        assert_eq!(edited_untracked_payload, "edited-untracked");
        let edited_untracked_count = nested_run_count(package);
        assert_eq!(edited_untracked_count, untracked_count + 1);

        let (_, raw_untracked_sha, raw_untracked_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Direct);
        assert_eq!(raw_untracked_sha, "unverified");
        assert_eq!(raw_untracked_payload, "edited-untracked");

        fs::remove_file(&untracked).unwrap();
        let (_, restored_untracked_sha, restored_untracked_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Trusted);
        assert_eq!(restored_untracked_sha, first_sha);
        assert_eq!(restored_untracked_payload, "base");
        let restored_untracked_count = nested_run_count(package);
        assert!(restored_untracked_count > untracked_count);

        let source = package.join("src/main.rs");
        let original = fs::read_to_string(&source).unwrap();
        fs::write(&source, format!("{original}\n")).unwrap();
        let (_, dirty_sha, dirty_payload) = cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(dirty_sha, "unverified");
        assert_eq!(dirty_payload, "base");
        let dirty_count = nested_run_count(package);
        assert_eq!(dirty_count, restored_untracked_count + 1);

        git(root, &["add", "apps/nested/src/main.rs"]);
        let (_, staged_sha, _) = cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(staged_sha, "unverified");
        let staged_count = nested_run_count(package);
        assert_eq!(staged_count, dirty_count + 1);

        git(root, &["checkout", "HEAD", "--", "apps/nested/src/main.rs"]);
        let (_, clean_sha, _) = cargo_run_nested_verbose(package, ProvenanceMode::Trusted);
        assert_eq!(clean_sha, first_sha);
        let restored_count = nested_run_count(package);
        assert_eq!(restored_count, staged_count + 1);

        fs::write(&source, format!("{original}\n")).unwrap();
        git(root, &["add", "apps/nested/src/main.rs"]);
        git(root, &["commit", "-m", "advance nested source"]);
        let (_, committed_sha, _) = cargo_run_nested_verbose(package, ProvenanceMode::Trusted);
        assert_ne!(committed_sha, "unverified");
        assert_ne!(committed_sha, first_sha);
        let committed_count = nested_run_count(package);
        assert_eq!(committed_count, restored_count + 1);

        let (_, final_raw_sha, final_raw_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(final_raw_sha, "unverified");
        assert_eq!(final_raw_payload, "base");
        let final_raw_count = nested_run_count(package);
        assert_eq!(final_raw_count, committed_count + 1);
        let (final_output, final_sha, final_payload) =
            cargo_run_nested_verbose(package, ProvenanceMode::Raw);
        assert_eq!(final_sha, "unverified");
        assert_eq!(final_payload, "base");
        assert_eq!(nested_run_count(package), final_raw_count);
        assert!(
            final_output.contains("Fresh nested-provenance-fresh-test"),
            "post-commit unchanged raw/local-CI invocation was not Fresh:\n{}",
            final_output
        );
    }

    #[derive(Clone, Copy)]
    enum ProvenanceMode {
        Direct,
        Raw,
        Trusted,
    }

    fn cargo_run_nested_verbose(
        package: &std::path::Path,
        mode: ProvenanceMode,
    ) -> (String, String, String) {
        let mut command = if matches!(mode, ProvenanceMode::Raw | ProvenanceMode::Trusted) {
            let mut command = Command::new(provenance_wrapper());
            if matches!(mode, ProvenanceMode::Trusted) {
                command.arg("--require-clean");
            }
            command
                .arg("env")
                .arg(format!(
                    "CARGO_TARGET_DIR={}",
                    package.join("target").display()
                ))
                .arg("cargo");
            command
        } else {
            Command::new("cargo")
        };
        let output = command
            .args(["run", "-vv"])
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("PETAL_OFFICIAL_SOURCE_SHA_FULL")
            .env_remove("PETAL_OFFICIAL_SOURCE_STATE")
            .env_remove("PETAL_SOURCE_PROVENANCE_WRAPPED")
            .current_dir(package)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let result = stdout.lines().last().unwrap_or_default().trim();
        let (sha, payload) = result.split_once(':').unwrap();
        (
            String::from_utf8(output.stderr).unwrap(),
            sha.to_string(),
            payload.to_string(),
        )
    }

    fn assert_require_clean_refuses(package: &std::path::Path) {
        let output = Command::new(provenance_wrapper())
            .args(["--require-clean", "cargo", "run", "--quiet"])
            .current_dir(package)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("refusing official release build from a non-clean worktree"));
    }

    fn provenance_wrapper() -> std::path::PathBuf {
        let start = std::env::current_dir().unwrap();
        for ancestor in start.ancestors() {
            let candidate = ancestor.join("scripts/run-with-source-provenance.sh");
            if candidate.is_file() {
                return candidate;
            }
        }
        panic!(
            "could not find scripts/run-with-source-provenance.sh from {:?}",
            start
        );
    }

    fn nested_run_count(package: &std::path::Path) -> u64 {
        let build_root = package.join("target/debug/build");
        fs::read_dir(build_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_to_string(entry.path().join("out/run-count")).ok())
            .filter_map(|value| value.parse::<u64>().ok())
            .max()
            .expect("nested build script run count")
    }

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "petal-build-provenance-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "test@petal.invalid"]);
        git(&root, &["config", "user.name", "Petal Test"]);
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("tracked.txt"), "first\n").unwrap();
        fs::write(root.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"provenance-fresh-test\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let nested = root.join("apps/nested");
        fs::create_dir_all(nested.join("src")).unwrap();
        fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname = \"nested-provenance-fresh-test\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            nested.join("src/main.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/provenance-payload.rs\"));\nfn main() { println!(\"{}:{}\", env!(\"TEST_SOURCE_SHA\"), PROVENANCE_PAYLOAD); }\n",
        )
        .unwrap();
        fs::write(
            nested.join("build.rs"),
            r#"use std::{fs, path::{Path, PathBuf}, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=PETAL_OFFICIAL_SOURCE_SHA_FULL");
    println!("cargo:rerun-if-env-changed=PETAL_OFFICIAL_SOURCE_STATE");
    emit_git_watch_paths();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let count_path = out.join("run-count");
    let count = fs::read_to_string(&count_path)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0) + 1;
    fs::write(&count_path, count.to_string()).unwrap();
    let clean = std::env::var("PETAL_OFFICIAL_SOURCE_SHA_FULL")
        .ok()
        .filter(|value| {
            value.len() == 40
                && value.bytes().all(|byte| {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                })
        })
        .unwrap_or_else(|| "unverified".to_string());
    let payload = fs::read_to_string("untracked-payload.rs")
        .unwrap_or_else(|_| "pub const PROVENANCE_PAYLOAD: &str = \"base\";\n".to_string());
    fs::write(out.join("provenance-payload.rs"), payload).unwrap();
    println!("cargo:rustc-env=TEST_SOURCE_SHA={clean}");
}

fn emit_git_watch_paths() {
    let workdir = Path::new(".");
    let mut paths = Vec::new();
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_path(workdir, name) {
            paths.push(path);
        }
    }
    if let Some(reference) = output(workdir, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path(workdir, &reference) {
            paths.push(path);
        }
    }
    if let Some(root) = output(workdir, &["rev-parse", "--show-toplevel"]) {
        let root = PathBuf::from(root);
        if let Ok(files) = Command::new("git")
            .args(["ls-files", "--full-name", "-z"])
            .current_dir(workdir)
            .output()
        {
            if files.status.success() {
                for name in files.stdout.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
                    if let Ok(name) = std::str::from_utf8(name) {
                        paths.push(root.join(name));
                    }
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_path(workdir: &Path, name: &str) -> Option<PathBuf> {
    output(
        workdir,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )
    .map(PathBuf::from)
    .filter(|path| path.exists())
}

fn output(workdir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}
"#,
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "first"]);
        root
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }
}

fn git_commit() -> String {
    let short = command_output("git", &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    if short == "unknown" {
        return short;
    }

    let dirty = std::process::Command::new("git")
        .args(["diff", "--quiet", "--ignore-submodules", "HEAD"])
        .status()
        .map(|status| !status.success())
        .unwrap_or(false);

    if dirty {
        format!("{short}-dirty")
    } else {
        short
    }
}

#[cfg(windows)]
fn build_date() -> String {
    // Windows does not ship the Unix `date -u` command. Use the inbox
    // Windows PowerShell executable and format explicitly so the result is
    // locale-independent and keeps the existing UTC YYYY-MM-DD contract.
    command_output(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')",
        ],
    )
    .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(not(windows))]
fn build_date() -> String {
    command_output("date", &["-u", "+%Y-%m-%d"]).unwrap_or_else(|| "unknown".to_string())
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    command_output_at(std::path::Path::new("."), command, args)
}

fn command_output_at(workdir: &std::path::Path, command: &str, args: &[&str]) -> Option<String> {
    let bytes = command_output_bytes_at(workdir, command, args)?;
    let value = String::from_utf8(bytes).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn command_output_bytes_at(
    workdir: &std::path::Path,
    command: &str,
    args: &[&str],
) -> Option<Vec<u8>> {
    let output = std::process::Command::new(command)
        .args(args)
        .current_dir(workdir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}
