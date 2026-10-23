//! `lof deploy` in a function project (`FDEP-07`, `FDEP-08`): the CLI runs `[function] build` as a process
//! (the manifest's own string, like the `[dev] command`), checks that
//! `[function] component` is a component with the export
//! `wasi:http/incoming-handler@0.2` and uploads it (`POST /deploys`,
//! multipart, field `component`), then waits for the deploy.
//!
//! The binary runs as a child of the test with the config home of a
//! [`Cli`] logged in to the API [`Stub`] (as in `dev.rs`), so the build
//! is a real process. The build command is `sh <root>/build.sh`, a script
//! in the tempdir; a real component is the contract function `fn_rust`
//! (`just functions`), a component that is not a function the module
//! `probe` (`just modules`), both from `WASM_MODULES_DIR`.

use std::path::{Path, PathBuf};

use crate::dev::{Ran, lof};
use crate::support::{Cli, Stub, json};

/// The deploy `POST /deploys` queues.
const QUEUED: &str = r#"{"deploys":[{"project":"shop","environment":"prod","deploy_id":"01J000000000000000000000D2","number":4}]}"#;

/// The deploy, finished.
const SUCCEEDED: &str = r#"{"id":"01J000000000000000000000D2","number":4,"project":"shop","environment":"prod","source":"cli","ref":null,"tag":"fn","status":"succeeded","error":null,"rolled_back_to":null,"triggered_by":"u1","started_at":"2026-10-21T10:00:00Z","finished_at":"2026-10-21T10:00:05Z"}"#;

/// The function environment `shop/prod`.
const ENV: &str = r#"{"id":"01J000000000000000000000E1","project":"shop","name":"prod","kind":"function","branch":"main","pinned_by":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"shared","created_at":"2026-10-21T10:00:00Z"}"#;

/// What the build prints on stdout.
const BUILD_OUT: &str = "compiling function v0.1.0";

/// What the build prints on stderr.
const BUILD_ERR: &str = "error[E0425]: cannot find value `handler` in this scope";

/// A stub that queues the deploy and says it succeeded.
async fn stub() -> Stub {
    Stub::start()
        .await
        .route("POST", "/deploys", vec![json(202, QUEUED)])
        .route(
            "GET",
            "/deploys/01J000000000000000000000D2",
            vec![json(200, SUCCEEDED)],
        )
        .route(
            "GET",
            "/projects/shop/environments/prod",
            vec![json(200, ENV)],
        )
}

/// Where `just functions` and `just modules` put the components.
fn built(name: &str) -> PathBuf {
    let path = liftoff_wasm::testing::components_dir().join(format!("{name}.wasm"));
    assert!(
        path.is_file(),
        "{} is not built: run `just modules functions`",
        path.display()
    );
    path
}

/// A function project at the top of the working tree of `cli` whose
/// build is `sh <root>/build.sh` with `script` and whose component is
/// `out.wasm`.
fn function_project(cli: &Cli, script: &str) {
    let path = cli.root.join("build.sh");
    std::fs::write(&path, script).unwrap();
    cli.write_root(
        "liftoff.toml",
        &format!(
            "[function]\nbuild = \"sh {}\"\ncomponent = \"out.wasm\"\n",
            path.display()
        ),
    );
}

/// A build that leaves the file `ran` next to its script, prints
/// [`BUILD_OUT`] and [`BUILD_ERR`] and exits 3.
fn failing_build() -> String {
    format!("touch \"$(dirname \"$0\")/ran\"\necho '{BUILD_OUT}'\necho '{BUILD_ERR}' >&2\nexit 3\n")
}

/// A build that copies `source` to `out.wasm` in the directory it runs in.
fn copying(source: &Path) -> String {
    format!("cp '{}' out.wasm\n", source.display())
}

/// `lof --project shop deploy` in a function project logged in to a
/// fresh [`stub`] whose build is `script`.
async fn deploy_with(script: &str) -> (Ran, Stub) {
    let stub = stub().await;
    let cli = Cli::logged_in(&stub.url);
    function_project(&cli, script);
    let ran = lof(&cli, &["--project", "shop", "deploy"]).await;
    (ran, stub)
}

/// Whether the CLI uploaded anything.
fn uploaded(stub: &Stub) -> bool {
    !stub.seen_at("POST", "/deploys").is_empty()
}

mod failing_build {
    use super::*;

    // TC: FDEP-07
    #[tokio::test]
    async fn exits_one_when_build_fails() {
        let (ran, _stub) = deploy_with(&failing_build()).await;
        assert_eq!(ran.code, Some(1), "{ran:?}");
    }

    // TC: FDEP-07 (the build ran, and nothing went up after it failed)
    #[tokio::test]
    async fn uploads_nothing_when_build_fails() {
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url);
        function_project(&cli, &failing_build());
        let ran = lof(&cli, &["--project", "shop", "deploy"]).await;
        assert!(
            cli.root.join("ran").is_file() && !uploaded(&stub),
            "{ran:?} {:?}",
            stub.trail()
        );
    }

    // TC: FDEP-07 (the command's output is shown)
    #[tokio::test]
    async fn shows_build_output_on_stderr_when_build_fails() {
        let (ran, _stub) = deploy_with(&failing_build()).await;
        assert!(
            ran.err.contains(BUILD_OUT) && ran.err.contains(BUILD_ERR),
            "{ran:?}"
        );
    }

    // TC: FDEP-07 (the CLI owns stdout: what it runs logs to stderr)
    #[tokio::test]
    async fn keeps_build_output_off_stdout_when_build_fails() {
        let (ran, _stub) = deploy_with(&failing_build()).await;
        assert!(
            ran.code == Some(1) && !ran.out.contains(BUILD_OUT),
            "{ran:?}"
        );
    }
}

mod check {
    use super::*;

    // TC: FDEP-08 (a component without `incoming-handler`: a module)
    #[tokio::test]
    async fn exits_one_without_upload_when_component_lacks_incoming_handler() {
        let (ran, stub) = deploy_with(&copying(&built("probe"))).await;
        assert!(
            ran.code == Some(1) && !uploaded(&stub),
            "{ran:?} {:?}",
            stub.trail()
        );
    }

    // TC: FDEP-08 («это не компонент wasi:http»)
    #[tokio::test]
    async fn says_not_a_wasi_http_component_when_component_lacks_incoming_handler() {
        let (ran, _stub) = deploy_with(&copying(&built("probe"))).await;
        assert!(ran.err.contains("not a wasi:http component"), "{ran:?}");
    }

    // TC: FDEP-08 (a file that is no WebAssembly at all)
    #[tokio::test]
    async fn exits_one_without_upload_when_component_is_not_wasm() {
        let (ran, stub) = deploy_with("printf 'not wasm' > out.wasm\n").await;
        assert!(
            ran.code == Some(1)
                && !uploaded(&stub)
                && ran.err.contains("not a wasi:http component"),
            "{ran:?} {:?}",
            stub.trail()
        );
    }
}

mod upload {
    use super::*;

    // TC: FDEP-08 (a wasi:http component passes the check and goes up as the multipart field `component`)
    #[tokio::test]
    async fn uploads_built_component_as_field_component_when_it_is_a_function() {
        let component = built("fn_rust");
        let (ran, stub) = deploy_with(&copying(&component)).await;
        let sent = stub.seen_at("POST", "/deploys");
        let first = sent.first();
        assert_eq!(
            (
                first.and_then(|seen| seen.param("project")),
                first.and_then(|seen| seen.param("environment")),
                first.and_then(|seen| seen.part("component")),
            ),
            (
                Some("shop".to_owned()),
                Some("prod".to_owned()),
                Some(std::fs::read(&component).unwrap()),
            ),
            "{ran:?}"
        );
    }

    // TC: FDEP-08 (`[function] component` names a path of the working
    // tree: the build runs at its top, wherever `lof` starts)
    #[tokio::test]
    async fn uploads_component_built_at_top_of_tree_when_started_in_subdirectory() {
        let component = built("fn_rust");
        let stub = stub().await;
        let cli = Cli::logged_in(&stub.url).inside("src");
        function_project(&cli, &copying(&component));
        let ran = lof(&cli, &["--project", "shop", "deploy"]).await;
        assert_eq!(
            stub.seen_at("POST", "/deploys")
                .first()
                .and_then(|seen| seen.part("component")),
            Some(std::fs::read(&component).unwrap()),
            "{ran:?}"
        );
    }

    // TC: FDEP-03 (`--wait` is the default for functions)
    #[tokio::test]
    async fn waits_for_deploy_without_wait_flag_when_function_deployed() {
        let (ran, stub) = deploy_with(&copying(&built("fn_rust"))).await;
        assert!(
            ran.code == Some(0)
                && !stub
                    .seen_at("GET", "/deploys/01J000000000000000000000D2")
                    .is_empty()
                && ran.out.ends_with("deploy #4 succeeded\n"),
            "{ran:?} {:?}",
            stub.trail()
        );
    }

    // TC: FDEP-08 (the upload's progress goes to stderr; stdout
    // is the CLI's answer)
    #[tokio::test]
    async fn shows_upload_progress_on_stderr_when_function_deployed() {
        let (ran, _stub) = deploy_with(&copying(&built("fn_rust"))).await;
        assert_eq!(
            (
                ran.err.contains("uploading out.wasm ("),
                ran.err.contains("uploaded out.wasm\n"),
                ran.out.contains("upload"),
            ),
            (true, true, false),
            "{ran:?}"
        );
    }
}

mod build_group {
    use super::*;

    // TC: FDEP-07 (the build leads its own process group, as the
    // `[dev] command`: what it leaves running in the background, holding
    // its output, ends with it instead of keeping the run open)
    #[tokio::test]
    async fn uploads_when_build_leaves_process_running_in_background() {
        let script = format!("{}sleep 600 &\n", copying(&built("fn_rust")));
        let (ran, stub) = deploy_with(&script).await;
        assert_eq!((ran.code, uploaded(&stub)), (Some(0), true), "{ran:?}");
    }
}
