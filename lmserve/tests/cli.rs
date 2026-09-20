//! Exercise the CLI with isolated fake tools and local HTTP readiness
//! responses.

#[cfg(test)]
mod tests {
    use assert_cmd::assert::OutputAssertExt;
    use serde_json::Value;
    use serde_json::json;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::path::Path;
    use std::process::Command;
    use std::process::Output;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::TempDir;

    fn fake_tool() -> &'static Path {
        static BINARY: OnceLock<std::path::PathBuf> = OnceLock::new();
        BINARY.get_or_init(|| {
            let binary = escargot::CargoBuild::new()
                .bin("lmserve-test-tools")
                .manifest_path(
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("../lmserve-test-tools/Cargo.toml"),
                )
                .current_release()
                .current_target()
                .arg("--locked")
                .arg("--offline")
                .run()
                .expect("build native fake tools");
            binary.path().to_path_buf()
        })
    }

    struct Fixture {
        directory: TempDir,
        config: Value,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("create fixture directory");
            let root = directory.path();
            let bin = root.join("bin");
            fs_err::create_dir(&bin).expect("create fake tool directory");
            for name in ["podman", "podman-compose", "hf", "nvidia-ctk"] {
                let path = bin.join(name);
                fs_err::os::unix::fs::symlink(fake_tool(), path)
                    .expect("link native fake executable");
            }
            let mut fixture = Self {
                directory,
                config: json!({
                    "name": "fixture", "x-lmserve": {"version": 1}, "services": {
                        "first": model("first-image", "first-repo"),
                        "second": model("second-image", "second-repo"),
                        "webui": {"image": "webui-image", "volumes": ["webui-data:/data"]}
                    }, "volumes": {"webui-data": {}}
                }),
            };
            fixture.config["x-lmserve"]["model-directory"] =
                json!(fixture.directory.path().join("models with spaces"));
            fixture.write();
            fixture
        }

        fn write(&self) {
            fs_err::write(
                self.directory.path().join("compose.yaml"),
                serde_json::to_vec(&self.config).expect("serialize fixture"),
            )
            .expect("write fixture configuration");
        }

        fn command(&self, args: &[&str]) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_lmserve"));
            command
                .env_clear()
                .env("PATH", self.directory.path().join("bin"))
                .env("HOME", self.directory.path())
                .env("LMSERVE_TEST_ROOT", self.directory.path())
                .env("XDG_STATE_HOME", self.directory.path().join("state"))
                .env("XDG_CACHE_HOME", self.directory.path().join("cache"))
                .env("XDG_CONFIG_HOME", self.directory.path().join("config"))
                .current_dir(self.directory.path())
                .args(args);
            command
        }

        fn run(&self, args: &[&str]) -> Output {
            self.command(args).output().expect("execute fixture CLI")
        }

        fn success(&self, args: &[&str]) -> Output {
            self.command(args).assert().success().get_output().clone()
        }

        fn state(&self) -> Value {
            serde_json::from_slice(
                &fs_err::read(self.directory.path().join("state/lmserve/state.json"))
                    .expect("read durable state"),
            )
            .expect("parse durable state")
        }

        fn runtime(&self) -> Value {
            serde_json::from_slice(
                &fs_err::read(self.directory.path().join("runtime.json"))
                    .expect("read fake runtime"),
            )
            .expect("parse fake runtime")
        }

        fn calls(&self) -> Vec<Vec<String>> {
            fs_err::read_to_string(self.directory.path().join("calls.jsonl"))
                .expect("read tool invocations")
                .lines()
                .map(|line| serde_json::from_str(line).expect("parse invocation"))
                .collect()
        }

        fn prepare(&self) {
            self.success(&["update-images", "--all"]);
            self.success(&["update-models", "--all"]);
        }

        fn wait(&self, output: &Output) -> Value {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let id = stdout
                .split_whitespace()
                .nth(1)
                .expect("accepted output contains operation ID")
                .trim_end_matches(';');
            wait_operation(self, id)
        }
    }

    fn wait_operation(fixture: &Fixture, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let operation = fixture.state()["operations"][id].clone();
            if ["ready", "stopped", "failed", "cancelled", "interrupted"]
                .contains(&operation["phase"].as_str().expect("phase is a string"))
            {
                return operation;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not complete: {operation}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn model(image: &str, repo: &str) -> Value {
        json!({"image": image, "volumes": [],
            "x-lmserve": {"huggingface": {"repo": format!("fixture/{repo}")}, "companions": ["webui"],
                "readiness": {"url": "http://127.0.0.1:9/health", "timeout": "2s"}}})
    }

    struct HealthServer {
        url: String,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl HealthServer {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test readiness server");
            listener
                .set_nonblocking(true)
                .expect("set server nonblocking");
            let url = format!(
                "http://{}/health",
                listener.local_addr().expect("read server address")
            );
            let stop = Arc::new(AtomicBool::new(false));
            let closed = stop.clone();
            let worker = thread::spawn(move || serve(&listener, &closed));
            Self {
                url,
                stop,
                worker: Some(worker),
            }
        }

        fn configure(&self, fixture: &mut Fixture) {
            for entry in ["first", "second"] {
                fixture.config["services"][entry]["x-lmserve"]["readiness"]["url"] =
                    json!(self.url);
            }
            fixture.write();
        }
    }

    fn serve(listener: &TcpListener, closed: &AtomicBool) {
        while !closed.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .expect("bound request read");
                    let mut request = [0; 1024];
                    let _bytes = stream.read(&mut request).expect("read readiness request");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .expect("send readiness response");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("readiness server failed: {error}"),
            }
        }
    }

    impl Drop for HealthServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                worker.join().expect("join readiness server");
            }
        }
    }

    #[test]
    fn validation_and_planning_do_not_mutate_managed_state() {
        let fixture = Fixture::new();
        fixture.success(&["validate"]);
        fixture.success(&["plan", "start", "first"]);
        assert!(!fixture.directory.path().join("state").exists());
        assert!(!fixture.directory.path().join("models with spaces").exists());
        assert!(fixture.calls().iter().all(|args| {
            !args
                .iter()
                .any(|arg| ["up", "pull", "build", "download", "stop"].contains(&arg.as_str()))
        }));
    }

    #[test]
    fn invalid_inactive_metadata_does_not_block_targeted_validation() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["second"]["x-lmserve"] = json!({"unsupported": true});
        fixture.write();
        fixture.success(&["validate", "first"]);
        assert!(!fixture.run(&["validate"]).status.success());
    }

    #[test]
    fn validation_rejects_invalid_metadata_and_deployment_contracts() {
        let cases = [
            (
                "unknown field",
                "/services/first/x-lmserve/unknown",
                json!(1),
            ),
            ("schema version", "/x-lmserve/version", json!(2)),
            (
                "parent traversal",
                "/services/first/x-lmserve/huggingface/file",
                json!("../secret"),
            ),
            (
                "absolute artifact",
                "/services/first/x-lmserve/huggingface/file",
                json!("/secret"),
            ),
            (
                "zero timeout",
                "/services/first/x-lmserve/readiness/timeout",
                json!("0s"),
            ),
            (
                "negative timeout",
                "/services/first/x-lmserve/readiness/timeout",
                json!("-1s"),
            ),
            (
                "bad URL",
                "/services/first/x-lmserve/readiness/url",
                json!("file:///etc/passwd"),
            ),
            (
                "model companion",
                "/services/first/x-lmserve/companions",
                json!(["second"]),
            ),
            (
                "missing companion",
                "/services/first/x-lmserve/companions",
                json!(["absent"]),
            ),
            (
                "automatic restart",
                "/services/first/restart",
                json!("always"),
            ),
            ("duplicate model", "/services/first/scale", json!(2)),
        ];
        for (name, pointer, value) in cases {
            let mut fixture = Fixture::new();
            let (parent, key) = pointer
                .rsplit_once('/')
                .expect("fixture pointer contains a field");
            fixture
                .config
                .pointer_mut(parent)
                .expect("fixture parent exists")
                .as_object_mut()
                .expect("fixture parent is a mapping")
                .insert(key.to_owned(), value);
            fixture.write();
            assert!(
                !fixture.run(&["validate", "first"]).status.success(),
                "accepted {name}"
            );
        }
    }

    #[test]
    fn model_path_rejects_additional_writable_mounts() {
        for extra in [
            json!("${LMSERVE_MODEL_FIRST_PATH}:/writable:rw"),
            json!({"type": "bind", "source": "${LMSERVE_MODEL_FIRST_PATH}", "target": "/writable", "read_only": false}),
        ] {
            let mut fixture = Fixture::new();
            fixture.config["services"]["first"]["x-lmserve"]["huggingface"]["file"] =
                json!("model.gguf");
            fixture.config["services"]["first"]["volumes"] =
                json!(["${LMSERVE_MODEL_FIRST_PATH}:/model:ro"]);
            fixture.config["services"]["first"]["volumes"]
                .as_array_mut()
                .expect("mounts")
                .push(extra);
            fixture.write();
            assert!(!fixture.run(&["validate", "first"]).status.success());
        }
    }

    #[test]
    fn missing_external_resources_preserve_active_model() {
        for (kind, mount) in [
            ("volumes", json!(["external-data:/data"])),
            ("networks", json!(["external-data"])),
            ("secrets", json!(["external-data"])),
        ] {
            let mut fixture = Fixture::new();
            let server = HealthServer::new();
            server.configure(&mut fixture);
            fixture.prepare();
            fixture.wait(&fixture.success(&["start", "first"]));
            let before = fixture.runtime();
            if kind == "volumes" {
                fixture.config["services"]["second"][kind]
                    .as_array_mut()
                    .expect("mounts")
                    .push(json!("external-data:/data"));
            } else {
                fixture.config["services"]["second"][kind] = mount;
            }
            fixture.config[kind]["external-data"] =
                json!({"external": true, "name": "actual-resource"});
            fixture.write();
            let output = fixture
                .command(&["switch", "second"])
                .env("FAKE_MISSING_EXTERNAL", "actual-resource")
                .output()
                .expect("switch with missing external resource");
            assert!(!output.status.success());
            assert_eq!(fixture.runtime(), before);
            assert!(
                !fixture
                    .calls()
                    .iter()
                    .any(|args| args.get(1).is_some_and(|arg| arg == "stop"))
            );
            fixture.wait(&fixture.success(&["stop", "first"]));
        }
    }

    #[test]
    fn external_resource_removed_after_acceptance_preserves_active_model() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let before = fixture.runtime();
        fixture.config["services"]["second"]["networks"] = json!(["external-data"]);
        fixture.config["networks"]["external-data"] =
            json!({"external": true, "name": "actual-resource"});
        fixture.write();
        let output = fixture
            .command(&["switch", "second"])
            .env("FAKE_EXTERNAL_DISAPPEARS", "actual-resource")
            .output()
            .expect("switch before external resource disappears");
        assert!(output.status.success());
        assert_eq!(fixture.wait(&output)["phase"], "failed");
        assert_eq!(fixture.runtime(), before);
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|args| args.get(1).is_some_and(|arg| arg == "stop"))
        );
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn unsupported_podman_version_preserves_active_model() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let before = fixture.runtime();
        let output = fixture
            .command(&["switch", "second"])
            .env("FAKE_PODMAN_VERSION", "4.5.1")
            .output()
            .expect("switch on unsupported Podman");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Podman 4.6"));
        assert_eq!(fixture.runtime(), before);
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn dependencies_reject_cycles_and_indirect_models() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["webui"]["depends_on"] = json!(["first"]);
        fixture.config["services"]["first"]["depends_on"] = json!(["webui"]);
        fixture.write();
        assert!(!fixture.run(&["validate", "first"]).status.success());
        fixture.config["services"]["webui"]["depends_on"] = json!(["second"]);
        fixture.write();
        assert!(!fixture.run(&["validate", "first"]).status.success());
    }

    #[test]
    fn reserved_variables_are_rejected_in_environment_and_dotenv() {
        let fixture = Fixture::new();
        let output = fixture
            .command(&["validate", "first"])
            .env("LMSERVE_MODEL_FIRST_PATH", "/wrong")
            .output()
            .expect("run conflicting environment");
        assert!(!output.status.success());
        fs_err::write(
            fixture.directory.path().join(".env"),
            "LMSERVE_MODEL_FIRST_PATH=/wrong\n",
        )
        .expect("write conflicting dotenv");
        assert!(!fixture.run(&["validate", "first"]).status.success());
    }

    #[test]
    fn provider_version_accepts_minimum_and_newer_releases() {
        let fixture = Fixture::new();
        for version in ["1.5.0", "1.5.1", "1.6.0", "1.10.0", "2.0.0"] {
            fixture
                .command(&["validate"])
                .env("FAKE_PROVIDER_VERSION", version)
                .assert()
                .success();
        }
    }

    #[test]
    fn provider_version_rejects_older_and_malformed_releases() {
        let fixture = Fixture::new();
        for version in [
            "0.0.1",
            "0.99.0",
            "1.4.9",
            "",
            "unknown",
            "1.5",
            "1.5.0.1",
            "1.5.0rc1",
            "1.6.0-dev",
            "1.6.x",
            "-1.6.0",
            "+1.6.0",
            "1.6.0 extra",
            "18446744073709551616.0.0",
        ] {
            let output = fixture
                .command(&["validate"])
                .env("FAKE_PROVIDER_VERSION", version)
                .output()
                .expect("run unsupported provider");
            assert!(!output.status.success(), "accepted version {version:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported provider"));
        }
    }

    #[test]
    #[ignore = "requires Hugging Face and Transformers; run just test-hf"]
    fn huggingface_cache_contract() {
        let python = std::env::var("LMSERVE_TEST_PYTHON").expect("run just test-hf");
        let fixture = Fixture::new();
        fixture.success(&["update-models", "first"]);
        let state = fixture.state();
        let cache = Path::new(
            state["prepared"]["fixture/first"]["path"]
                .as_str()
                .expect("cache path"),
        );
        let mut pending = vec![cache.to_owned()];
        let mut original_permissions = Vec::new();
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    fs_err::read_dir(&path)
                        .expect("read cache directory")
                        .map(|item| item.expect("read cache entry").path()),
                );
            }
            let mut permissions = fs_err::metadata(&path)
                .expect("read cache permissions")
                .permissions();
            original_permissions.push((path.clone(), permissions.clone()));
            permissions.set_readonly(true);
            fs_err::set_permissions(path, permissions).expect("make cache read-only");
        }
        let output = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/hf_cache.py"))
            .env_clear()
            .env("HOME", fixture.directory.path())
            .env("HF_HUB_CACHE", cache)
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_HOME", fixture.directory.path().join("hf-home"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output();
        for (path, permissions) in original_permissions {
            fs_err::set_permissions(path, permissions).expect("restore cache permissions");
        }
        output
            .expect("execute HF cache contract")
            .assert()
            .success();
    }

    #[test]
    fn repository_cache_rejects_mount_and_environment_overrides() {
        for mount in [
            json!("./other:/lmserve/huggingface/hub:ro"),
            json!("./other:/lmserve:ro"),
            json!({"type": "volume", "source": "other", "target": "/lmserve/huggingface/hub/models--fixture--first-repo"}),
            json!("./other:/lmserve/huggingface/./hub:ro"),
            json!("./other:/lmserve/else/../huggingface/hub:ro"),
            json!("${LMSERVE_MODEL_FIRST_PATH}:/model:ro"),
        ] {
            let mut fixture = Fixture::new();
            fixture.config["services"]["first"]["volumes"] = json!([mount]);
            fixture.write();
            assert!(
                !fixture.run(&["validate", "first"]).status.success(),
                "accepted {mount}"
            );
        }
        for (key, value) in [
            ("HF_HUB_CACHE", "/elsewhere"),
            ("HF_HUB_OFFLINE", "0"),
            ("TRANSFORMERS_OFFLINE", "0"),
            ("HUGGINGFACE_HUB_CACHE", "/elsewhere"),
            ("TRANSFORMERS_CACHE", "/elsewhere"),
            ("PYTORCH_TRANSFORMERS_CACHE", "/elsewhere"),
            ("PYTORCH_PRETRAINED_BERT_CACHE", "/elsewhere"),
        ] {
            for environment in [json!({key: value}), json!([format!("{key}={value}")])] {
                let mut fixture = Fixture::new();
                fixture.config["services"]["first"]["environment"] = environment.clone();
                fixture.write();
                assert!(
                    !fixture.run(&["validate", "first"]).status.success(),
                    "accepted {environment}"
                );
            }
            let mut fixture = Fixture::new();
            fs_err::write(
                fixture.directory.path().join("model.env"),
                format!("{key}={value}\n"),
            )
            .expect("write conflicting environment");
            fixture.config["services"]["first"]["env_file"] = json!(["model.env"]);
            fixture.write();
            assert!(
                !fixture.run(&["validate", "first"]).status.success(),
                "accepted {key} in env_file"
            );
        }
    }

    #[test]
    fn repository_cache_rejects_overlapping_tmpfs_configs_and_secrets() {
        for (kind, value) in [
            ("tmpfs", json!("/lmserve/huggingface/hub:rw")),
            ("tmpfs", json!(["/lmserve:size=4096"])),
            (
                "configs",
                json!([{"source": "tuning", "target": "/lmserve/huggingface/hub/config.json"}]),
            ),
            (
                "secrets",
                json!([{"source": "token", "target": "/lmserve/huggingface/hub/token"}]),
            ),
        ] {
            let mut fixture = Fixture::new();
            fixture.config["services"]["first"][kind] = value.clone();
            fixture.write();
            assert!(
                !fixture.run(&["validate", "first"]).status.success(),
                "accepted {kind}: {value}"
            );
        }
    }

    #[test]
    fn preparing_another_revision_keeps_each_cache_ref_independent() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["second"]["x-lmserve"]["huggingface"]["repo"] =
            json!("fixture/first-repo");
        fixture.write();
        fixture.success(&["update-models", "first"]);
        fixture
            .command(&["update-models", "second"])
            .env("FAKE_REVISION", "b".repeat(40))
            .assert()
            .success();
        let state = fixture.state();
        for (entry, revision) in [
            ("fixture/first", "a".repeat(40)),
            ("fixture/second", "b".repeat(40)),
        ] {
            let cache = Path::new(
                state["prepared"][entry]["path"]
                    .as_str()
                    .expect("cache path"),
            );
            assert_eq!(
                fs_err::read_to_string(cache.join("models--fixture--first-repo/refs/main"))
                    .expect("read local ref"),
                revision
            );
        }
    }

    #[test]
    fn damaged_cache_fails_before_stopping_the_active_model() {
        for damage in [
            "ref",
            "missing",
            "extra_revision",
            "extra_file",
            "escaping_link",
        ] {
            let mut fixture = Fixture::new();
            let server = HealthServer::new();
            server.configure(&mut fixture);
            fixture.prepare();
            fixture.wait(&fixture.success(&["start", "first"]));
            let before = fixture.runtime();
            let state = fixture.state();
            let cache = Path::new(
                state["prepared"]["fixture/second"]["path"]
                    .as_str()
                    .expect("cache path"),
            );
            let repository = cache.join("models--fixture--second-repo");
            let weights = repository
                .join("snapshots")
                .join("a".repeat(40))
                .join("model.bin");
            match damage {
                "ref" => {
                    fs_err::write(repository.join("refs/main"), "b".repeat(40)).expect("alter ref");
                }
                "missing" => fs_err::remove_file(weights).expect("remove weights"),
                "extra_revision" => {
                    fs_err::create_dir(repository.join("snapshots").join("b".repeat(40)))
                        .expect("add revision");
                }
                "extra_file" => fs_err::write(cache.join("extra"), "unexpected").expect("add file"),
                "escaping_link" => {
                    let outside = fixture.directory.path().join("outside");
                    fs_err::write(&outside, "a".repeat(40)).expect("write external content");
                    fs_err::remove_file(&weights).expect("remove weights");
                    fs_err::os::unix::fs::symlink(outside, weights)
                        .expect("replace with escaping link");
                }
                _ => panic!("unknown damage"),
            }
            assert!(
                !fixture.run(&["switch", "second"]).status.success(),
                "accepted {damage}"
            );
            assert_eq!(fixture.runtime(), before, "stopped model after {damage}");
            fixture.wait(&fixture.success(&["stop", "first"]));
        }
    }

    #[test]
    fn configured_branch_prepares_a_frozen_local_main_without_ref_paths() {
        for revision in [
            "release/candidate",
            "../branch",
            "a tag",
            "b".repeat(40).as_str(),
        ] {
            let mut fixture = Fixture::new();
            fixture.config["services"]["first"]["x-lmserve"]["huggingface"]["revision"] =
                json!(revision);
            fixture.write();
            fixture.success(&["update-models", "first"]);
            let state = fixture.state();
            let cache = Path::new(
                state["prepared"]["fixture/first"]["path"]
                    .as_str()
                    .expect("cache path"),
            );
            assert_eq!(
                fs_err::read_to_string(cache.join("models--fixture--first-repo/refs/main"))
                    .expect("read ref"),
                "a".repeat(40)
            );
            let calls = fixture.calls();
            assert!(
                calls
                    .iter()
                    .any(|args| args.get(1).is_some_and(|arg| arg == "models")
                        && args.windows(2).any(|pair| pair == ["--revision", revision]))
            );
            assert!(calls.iter().any(|args| {
                args.get(1).is_some_and(|arg| arg == "download")
                    && args
                        .windows(2)
                        .any(|pair| pair == ["--revision", "a".repeat(40).as_str()])
            }));
        }
    }

    #[test]
    fn repository_start_uses_prepared_cache_after_remote_branch_moves() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.config["services"]["first"]["volumes"] = json!([]);
        fixture.config["services"]["first"]["environment"] = json!({});
        fixture.config["services"]["first"]["command"] = json!([
            "--model",
            "fixture/first-repo",
            "--served-model-name",
            "local-model"
        ]);
        fixture.write();
        fixture.success(&["update-images", "first"]);
        fixture.success(&["update-models", "first"]);
        let before = fixture.state()["prepared"]["fixture/first"].clone();
        let hf_calls = fixture
            .calls()
            .into_iter()
            .filter(|args| args[0] == "hf")
            .count();
        let accepted = fixture
            .command(&["start", "first"])
            .env("FAKE_REVISION", "b".repeat(40))
            .env("FAKE_DOWNLOAD_FAIL", "1")
            .assert()
            .success()
            .get_output()
            .clone();
        assert_eq!(fixture.wait(&accepted)["phase"], "ready");
        let definition: Value = serde_json::from_slice(
            &fs_err::read(fixture.directory.path().join("definition-first.json"))
                .expect("read model definition"),
        )
        .expect("parse model definition");
        assert_eq!(
            definition["environment"]["HF_HUB_CACHE"],
            "/lmserve/huggingface/hub"
        );
        assert_eq!(definition["environment"]["HF_HUB_OFFLINE"], "1");
        assert_eq!(definition["environment"]["TRANSFORMERS_OFFLINE"], "1");
        assert_eq!(
            definition["command"],
            fixture.config["services"]["first"]["command"]
        );
        assert_eq!(
            definition["volumes"],
            json!([{
                "type": "bind", "source": before["path"],
                "target": "/lmserve/huggingface/hub", "read_only": true
            }])
        );
        let cache = Path::new(before["path"].as_str().expect("cache path"));
        assert_eq!(
            fs_err::read_to_string(cache.join("models--fixture--first-repo/refs/main"))
                .expect("read frozen ref"),
            "a".repeat(40)
        );
        assert_eq!(
            fs_err::read_to_string(cache.join(format!(
                "models--fixture--first-repo/snapshots/{}/model.bin",
                "a".repeat(40)
            )))
            .expect("read prepared weights"),
            "a".repeat(40)
        );
        assert_eq!(
            fixture
                .calls()
                .into_iter()
                .filter(|args| args[0] == "hf")
                .count(),
            hf_calls
        );
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn snapshot_links_are_materialized_and_failed_updates_preserve_content() {
        let fixture = Fixture::new();
        fixture.success(&["update-models", "first"]);
        let before = fixture.state()["prepared"]["fixture/first"].clone();
        let path = Path::new(before["path"].as_str().expect("prepared path"))
            .join("models--fixture--first-repo/snapshots")
            .join("a".repeat(40));
        assert_eq!(
            fs_err::read_to_string(path.join("model.bin")).expect("read mounted artifact"),
            "a".repeat(40)
        );
        assert!(
            !fs_err::symlink_metadata(path.join("model.bin"))
                .expect("inspect artifact")
                .is_symlink()
        );
        let output = fixture
            .command(&["update-models", "first"])
            .env("FAKE_REVISION", "b".repeat(40))
            .env("FAKE_DOWNLOAD_FAIL", "1")
            .output()
            .expect("run failed download");
        assert!(!output.status.success());
        assert_eq!(fixture.state()["prepared"]["fixture/first"], before);
        assert!(path.join("model.bin").exists());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-leak"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("must-not-leak"));
    }

    #[test]
    fn invalid_downloads_preserve_the_previous_cache() {
        for (key, value) in [
            ("FAKE_EMPTY_SNAPSHOT", "1".to_owned()),
            ("FAKE_SNAPSHOT_REVISION", "c".repeat(40)),
        ] {
            let fixture = Fixture::new();
            fixture.success(&["update-models", "first"]);
            let before = fixture.state()["prepared"]["fixture/first"].clone();
            fixture
                .command(&["update-models", "first"])
                .env("FAKE_REVISION", "b".repeat(40))
                .env(key, value)
                .assert()
                .failure();
            assert_eq!(fixture.state()["prepared"]["fixture/first"], before);
            assert!(Path::new(before["path"].as_str().expect("previous cache")).exists());
        }
    }

    #[test]
    fn unchanged_updates_skip_download_and_replacement_removes_old_revision() {
        let fixture = Fixture::new();
        fixture.success(&["update-models", "first"]);
        let previous = fixture.state()["prepared"]["fixture/first"]["owned_directory"]
            .as_str()
            .expect("owned path")
            .to_owned();
        fixture.success(&["update-models", "first"]);
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|args| args.first().is_some_and(|arg| arg == "hf")
                    && args.get(1).is_some_and(|arg| arg == "download"))
                .count(),
            1
        );
        let output = fixture
            .command(&["update-models", "first"])
            .env("FAKE_REVISION", "b".repeat(40))
            .output()
            .expect("run replacement");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!Path::new(&previous).exists());
    }

    #[test]
    fn startup_requires_explicit_preparation() {
        let fixture = Fixture::new();
        assert!(!fixture.run(&["start", "first"]).status.success());
        assert!(fixture.calls().iter().all(|args| {
            !args
                .iter()
                .any(|arg| ["up", "pull", "build", "download"].contains(&arg.as_str()))
        }));
    }

    #[test]
    fn readiness_timeout_stops_group_and_preserves_logs() {
        let fixture = Fixture::new();
        fixture.prepare();
        let accepted = fixture.success(&["start", "first"]);
        let operation = fixture.wait(&accepted);
        assert_eq!(operation["phase"], "failed");
        assert!(
            operation["error"]
                .as_str()
                .expect("failure text")
                .contains("deadline")
        );
        assert!(
            fixture.runtime()["containers"]
                .as_array()
                .expect("container list")
                .iter()
                .all(|container| container["State"]["Running"] == false)
        );
        fixture.success(&["logs", "first"]);
    }

    #[test]
    fn switches_retain_companions_and_stale_stops_preserve_new_group() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        assert_eq!(
            fixture.wait(&fixture.success(&["start", "first"]))["phase"],
            "ready"
        );
        let companion =
            fixture.state()["deployments"]["fixture/first"]["components"]["webui"]["id"].clone();
        assert!(!fixture.run(&["start", "second"]).status.success());
        assert!(!fixture.run(&["update-models", "first"]).status.success());
        assert_eq!(
            fixture.wait(&fixture.success(&["switch", "second"]))["phase"],
            "ready"
        );
        assert_eq!(
            fixture.state()["deployments"]["fixture/second"]["components"]["webui"]["id"],
            companion
        );
        assert_eq!(
            fixture.wait(&fixture.success(&["stop", "first"]))["phase"],
            "stopped"
        );
        fixture.success(&["health", "second"]);
        assert_eq!(
            fixture.runtime()["containers"]
                .as_array()
                .expect("container list")
                .iter()
                .filter(|container| container["State"]["Running"] == true)
                .count(),
            2
        );
        assert_eq!(
            fixture.wait(&fixture.success(&["stop", "second"]))["phase"],
            "stopped"
        );
    }

    #[test]
    fn optional_companion_failure_preserves_ready_model() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        let output = fixture
            .command(&["start", "first"])
            .env("FAKE_FAIL_SERVICE", "webui")
            .output()
            .expect("start with failed companion");
        assert!(output.status.success());
        assert_eq!(fixture.wait(&output)["phase"], "ready");
        fixture.success(&["health", "first"]);
        assert!(
            fixture.state()["deployments"]["fixture/first"]["components"]["webui"]["error"]
                .is_string()
        );
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn recorded_shutdown_works_without_configuration() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        fs_err::remove_file(fixture.directory.path().join("compose.yaml"))
            .expect("remove serving configuration");
        fixture.success(&["status", "first"]);
        fixture.success(&["logs", "first"]);
        assert_eq!(
            fixture.wait(&fixture.success(&["stop", "first"]))["phase"],
            "stopped"
        );
    }

    #[test]
    fn simultaneous_starts_admit_one_model() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        let first = fixture
            .command(&["start", "first"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn first start");
        let second = fixture
            .command(&["start", "second"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn second start");
        let results = [
            first.wait_with_output().expect("wait for first submission"),
            second
                .wait_with_output()
                .expect("wait for second submission"),
        ];
        assert_eq!(
            results
                .iter()
                .filter(|output| output.status.success())
                .count(),
            1
        );
        let accepted = results
            .iter()
            .find(|output| output.status.success())
            .expect("one accepted operation");
        assert_eq!(fixture.wait(accepted)["phase"], "ready");
        let running = fixture.runtime();
        assert_eq!(
            running["containers"]
                .as_array()
                .expect("containers")
                .iter()
                .filter(|container| container["State"]["Running"] == true
                    && container["Config"]["Labels"]["io.lmserve.model"] == "true")
                .count(),
            1
        );
    }

    fn wait_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture signal not received: {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn write_runtime(fixture: &Fixture, value: &Value) {
        fs_err::write(
            fixture.directory.path().join("runtime.json"),
            serde_json::to_vec(value).expect("serialize runtime"),
        )
        .expect("write runtime");
    }

    #[test]
    fn stop_cancels_blocked_provider_without_launching_companions() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["first"]["x-lmserve"]["readiness"]["timeout"] = json!("30s");
        fixture.write();
        fixture.prepare();
        let accepted = fixture
            .command(&["start", "first"])
            .env("FAKE_BLOCK_SERVICE", "first")
            .output()
            .expect("submit blocked startup");
        assert!(accepted.status.success());
        wait_file(&fixture.directory.path().join("started-first"));
        let stopped = fixture.success(&["stop", "first"]);
        assert_eq!(fixture.wait(&accepted)["phase"], "cancelled");
        assert_eq!(fixture.wait(&stopped)["phase"], "stopped");
        assert!(!fixture.directory.path().join("started-webui").exists());
        assert!(
            fixture.runtime()["containers"]
                .as_array()
                .expect("containers")
                .iter()
                .all(|container| container["State"]["Running"] == false)
        );
    }

    #[test]
    fn interrupted_worker_preserves_provider_lease_and_allows_recorded_stop() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["first"]["x-lmserve"]["readiness"]["timeout"] = json!("30s");
        fixture.write();
        fixture.prepare();
        let accepted = fixture
            .command(&["start", "first"])
            .env("FAKE_BLOCK_SERVICE", "first")
            .output()
            .expect("submit blocked startup");
        assert!(accepted.status.success());
        wait_file(&fixture.directory.path().join("started-first"));
        let state = fixture.state();
        let operation = state["operations"]
            .as_object()
            .expect("operations")
            .values()
            .next()
            .expect("accepted operation");
        let pid = i32::try_from(operation["worker"]["pid"].as_u64().expect("worker PID"))
            .expect("PID fits i32");
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("interrupt owned fixture worker");
        let status = fixture.success(&["status", "first"]);
        assert!(String::from_utf8_lossy(&status.stdout).contains("interrupted"));
        assert!(!fixture.run(&["switch", "second"]).status.success());
        fs_err::write(fixture.directory.path().join("release"), "release")
            .expect("release orphaned fixture provider");
        assert_eq!(
            fixture.wait(&fixture.success(&["stop", "first"]))["phase"],
            "stopped"
        );
    }

    #[test]
    fn slow_optional_companion_does_not_expire_a_ready_model() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        let accepted = fixture
            .command(&["start", "first"])
            .env("FAKE_BLOCK_SERVICE", "webui")
            .output()
            .expect("submit slow companion");
        assert!(accepted.status.success());
        assert_eq!(fixture.wait(&accepted)["phase"], "ready");
        fixture.success(&["health", "first"]);
        assert!(
            fixture.state()["deployments"]["fixture/first"]["components"]["webui"]["error"]
                .is_string()
        );
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn repeated_start_reports_existing_session_despite_configuration_removal() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let before = fixture.state();
        fs_err::remove_file(fixture.directory.path().join("compose.yaml")).expect("remove config");
        let output = fixture.success(&["start", "first"]);
        assert!(String::from_utf8_lossy(&output.stdout).contains("existing serving session"));
        assert_eq!(fixture.state(), before);
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn model_exit_keeps_companion_available_for_next_session() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let companion =
            fixture.state()["deployments"]["fixture/first"]["components"]["webui"]["id"].clone();
        let mut runtime = fixture.runtime();
        for container in runtime["containers"].as_array_mut().expect("containers") {
            if container["Config"]["Labels"]["io.lmserve.model"] == "true" {
                container["State"]["Running"] = json!(false);
            }
        }
        write_runtime(&fixture, &runtime);
        assert_eq!(
            fixture.wait(&fixture.success(&["start", "second"]))["phase"],
            "ready"
        );
        assert_eq!(
            fixture.state()["deployments"]["fixture/second"]["components"]["webui"]["id"],
            companion
        );
        fixture.wait(&fixture.success(&["stop", "second"]));
        assert!(
            fixture.runtime()["containers"]
                .as_array()
                .expect("containers")
                .iter()
                .all(|container| container["State"]["Running"] == false)
        );
    }

    #[test]
    fn changed_shared_companion_and_missing_bind_fail_before_switch() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let before = fixture.runtime();
        fixture.config["services"]["webui"]["environment"] = json!({"CHANGED": "1"});
        fixture.write();
        assert!(!fixture.run(&["switch", "second"]).status.success());
        assert_eq!(fixture.runtime(), before);
        fixture.config["services"]["webui"]
            .as_object_mut()
            .expect("webui mapping")
            .remove("environment");
        fixture.config["services"]["second"]["volumes"]
            .as_array_mut()
            .expect("mount list")
            .push(json!("./missing:/required:ro"));
        fixture.write();
        assert!(!fixture.run(&["switch", "second"]).status.success());
        assert_eq!(fixture.runtime(), before);
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn completed_required_dependency_is_delegated_to_provider() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.config["services"]["init"] = json!({"image": "init-image"});
        fixture.config["services"]["first"]["depends_on"] =
            json!({"init": {"condition": "service_completed_successfully"}});
        fixture.write();
        fixture.prepare();
        let accepted = fixture
            .command(&["start", "first"])
            .env("FAKE_COMPLETED_SERVICE", "init")
            .output()
            .expect("submit model with one-shot dependency");
        assert!(accepted.status.success());
        assert_eq!(fixture.wait(&accepted)["phase"], "ready");
        assert!(
            fixture
                .calls()
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "up"))
                .all(|args| !args.iter().any(|arg| arg == "--no-deps"))
        );
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn interpolated_pinned_reference_protects_another_entries_artifact() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["second"]["x-lmserve"]["huggingface"] =
            json!({"repo": "${SHARED_REPO}", "revision": "${PINNED_COMMIT}"});
        fixture.write();
        let run = |revision: &str| {
            fixture
                .command(&["update-models", "first"])
                .env("SHARED_REPO", "fixture/first-repo")
                .env("PINNED_COMMIT", "a".repeat(40))
                .env("FAKE_REVISION", revision)
                .output()
                .expect("prepare interpolated references")
        };
        assert!(run(&"a".repeat(40)).status.success());
        let previous = fixture.state()["prepared"]["fixture/first"]["owned_directory"]
            .as_str()
            .expect("owned directory")
            .to_owned();
        assert!(run(&"b".repeat(40)).status.success());
        assert!(Path::new(&previous).exists());
    }

    #[test]
    fn single_file_preparation_keeps_its_content_identity() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["first"]["x-lmserve"]["huggingface"]["file"] =
            json!("model.gguf");
        fixture.config["services"]["first"]["volumes"] =
            json!(["${LMSERVE_MODEL_FIRST_PATH}:/model:ro"]);
        fixture.write();
        fixture.success(&["update-models", "first"]);
        let state = fixture.state();
        let owned = Path::new(
            state["prepared"]["fixture/first"]["owned_directory"]
                .as_str()
                .expect("owned directory"),
        );
        assert_eq!(
            owned.file_name().expect("content identity"),
            "3d9323983e9ae310893cde3ec10b8b33cdef6b46002fc410016a025be2c88c92"
        );
        fixture
            .command(&["update-models", "first"])
            .env("FAKE_DOWNLOAD_FAIL", "1")
            .assert()
            .success();
        assert_eq!(fixture.state()["prepared"], state["prepared"]);
    }

    #[test]
    fn single_file_artifact_and_source_build_use_explicit_targets() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["first"]["x-lmserve"]["huggingface"]["file"] =
            json!("weights/model file.gguf");
        fixture.config["services"]["first"]["volumes"] =
            json!(["${LMSERVE_MODEL_FIRST_PATH}:/model:ro"]);
        fixture.config["services"]["first"]["build"] = json!({"context": "https://example.invalid/engine.git#pinned", "dockerfile": "Dockerfile"});
        fixture.write();
        fixture.success(&["update-models", "first"]);
        let prepared = fixture.state()["prepared"]["fixture/first"].clone();
        let path = Path::new(prepared["path"].as_str().expect("artifact path"));
        assert!(path.is_file());
        assert!(
            !fs_err::symlink_metadata(path)
                .expect("artifact metadata")
                .is_symlink()
        );
        fixture.success(&["update-images", "first"]);
        assert!(
            fixture
                .calls()
                .iter()
                .any(|args| args.iter().any(|arg| arg == "build")
                    && args.last().is_some_and(|arg| arg == "first"))
        );
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|args| args.iter().any(|arg| arg == "pull")
                    && args.last().is_some_and(|arg| arg == "first"))
        );
    }

    #[test]
    fn unowned_name_collision_does_not_adopt_or_stop_container() {
        let fixture = Fixture::new();
        fixture.prepare();
        let mut runtime = fixture.runtime();
        runtime["containers"] = json!([{"Id": "unowned", "Name": "fixture_first_1", "Config": {"Labels": {}}, "State": {"Running": true}, "NetworkSettings": {"Ports": {}}}]);
        write_runtime(&fixture, &runtime);
        assert!(!fixture.run(&["start", "first"]).status.success());
        assert_eq!(fixture.runtime(), runtime);
    }

    #[test]
    fn captured_environment_file_survives_edits_after_acceptance() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.config["services"]["webui"]["env_file"] = json!("webui.env");
        fixture.write();
        fs_err::write(
            fixture.directory.path().join("webui.env"),
            "CONFIG_VALUE=accepted\n",
        )
        .expect("write original environment");
        fixture.prepare();
        let accepted = fixture
            .command(&["start", "first"])
            .env("FAKE_BLOCK_SERVICE", "first")
            .output()
            .expect("submit captured configuration");
        assert!(accepted.status.success());
        wait_file(&fixture.directory.path().join("started-first"));
        fs_err::write(
            fixture.directory.path().join("webui.env"),
            "CONFIG_VALUE=changed\n",
        )
        .expect("edit source environment");
        fs_err::write(fixture.directory.path().join("release"), "release")
            .expect("release model startup");
        assert_eq!(fixture.wait(&accepted)["phase"], "ready");
        let definition: Value = serde_json::from_slice(
            &fs_err::read(fixture.directory.path().join("definition-webui.json"))
                .expect("read launched configuration"),
        )
        .expect("parse launched configuration");
        let captured = definition["env_file"][0]
            .as_str()
            .expect("captured environment path");
        assert_eq!(
            fs_err::read_to_string(captured).expect("read captured environment"),
            "CONFIG_VALUE=accepted\n"
        );
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn failed_old_shutdown_prevents_new_model_launch() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let output = fixture
            .command(&["switch", "second"])
            .env("FAKE_STOP_FAIL", "1")
            .output()
            .expect("submit failed shutdown");
        assert!(output.status.success());
        assert_eq!(fixture.wait(&output)["phase"], "failed");
        assert!(!fixture.directory.path().join("started-second").exists());
        fixture.success(&["health", "first"]);
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn failed_switch_stops_retained_companion_without_rollback() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let output = fixture
            .command(&["switch", "second"])
            .env("FAKE_FAIL_SERVICE", "second")
            .output()
            .expect("submit failed switch");
        assert!(output.status.success());
        assert_eq!(fixture.wait(&output)["phase"], "failed");
        assert!(
            fixture.runtime()["containers"]
                .as_array()
                .expect("containers")
                .iter()
                .all(|container| container["State"]["Running"] == false)
        );
        fixture.success(&["logs", "first"]);
    }

    #[test]
    fn model_update_batch_continues_after_active_entry_is_blocked() {
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.prepare();
        fixture.wait(&fixture.success(&["start", "first"]));
        let output = fixture
            .command(&["update-models", "--all"])
            .env("FAKE_REVISION", "b".repeat(40))
            .output()
            .expect("update batch");
        assert!(!output.status.success());
        let state = fixture.state();
        assert_eq!(
            state["prepared"]["fixture/first"]["revision"],
            "a".repeat(40)
        );
        assert_eq!(
            state["prepared"]["fixture/second"]["revision"],
            "b".repeat(40)
        );
        fixture.success(&["health", "first"]);
        fixture.wait(&fixture.success(&["stop", "first"]));
    }

    #[test]
    fn storage_root_changes_require_preparation_and_preserve_old_data() {
        let mut fixture = Fixture::new();
        fixture.prepare();
        let old_path = fixture.state()["prepared"]["fixture/first"]["path"]
            .as_str()
            .expect("old artifact path")
            .to_owned();
        fixture.config["x-lmserve"]["model-directory"] =
            json!(fixture.directory.path().join("new root"));
        fixture.write();
        assert!(!fixture.run(&["start", "first"]).status.success());
        fixture.success(&["update-models", "first"]);
        assert!(Path::new(&old_path).exists());
        assert_ne!(
            fixture.state()["prepared"]["fixture/first"]["path"],
            old_path
        );
    }

    #[test]
    fn shared_sources_download_once_and_keep_each_entry_reference() {
        let mut fixture = Fixture::new();
        fixture.config["services"]["second"]["x-lmserve"]["huggingface"]["repo"] =
            json!("fixture/first-repo");
        fixture.write();
        fixture.success(&["update-models", "--all"]);
        let state = fixture.state();
        assert_eq!(
            state["prepared"]["fixture/first"]["path"],
            state["prepared"]["fixture/second"]["path"]
        );
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|args| args.get(1).is_some_and(|arg| arg == "download"))
                .count(),
            1
        );
    }

    #[test]
    fn occupied_host_port_fails_before_runtime_mutation() {
        let mut fixture = Fixture::new();
        let occupied = TcpListener::bind("127.0.0.1:0").expect("reserve conflict fixture port");
        fixture.config["services"]["first"]["ports"] = json!([format!(
            "{}:8000",
            occupied.local_addr().expect("port address")
        )]);
        fixture.write();
        fixture.prepare();
        let before = fixture.runtime();
        let output = fixture.run(&["start", "first"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("host port conflict"));
        assert_eq!(fixture.runtime(), before);
    }

    #[test]
    #[ignore = "requires externally installed podman-compose >= 1.5.0; Podman remains fake"]
    fn provider_contract() {
        let selected =
            std::env::var_os("LMSERVE_TEST_PROVIDER").unwrap_or_else(|| "podman-compose".into());
        let selected = Path::new(&selected);
        let provider = if selected.components().count() > 1 {
            selected.to_path_buf()
        } else {
            std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
                .map(|directory| directory.join(selected))
                .find(|path| path.is_file())
                .expect("install podman-compose >= 1.5.0 or set LMSERVE_TEST_PROVIDER to its absolute path")
        };
        let provider = fs_err::canonicalize(provider).expect("resolve installed provider path");
        let provider = provider.to_str().expect("provider path is UTF-8");
        let mut fixture = Fixture::new();
        let server = HealthServer::new();
        server.configure(&mut fixture);
        fixture.config["services"]["first"]["devices"] = json!(["nvidia.com/gpu=all"]);
        fixture.config["services"]["first"]["ports"] = json!(["127.0.0.1:0:8000"]);
        fixture.config["services"]["first"]["stop_grace_period"] = json!("5s");
        fixture.config["services"]["first"]["networks"] =
            json!({"default": {"aliases": ["model-api"]}});
        fixture.config["services"]["second"]["build"] = json!({"context": "https://example.invalid/engine.git#fixed-ref", "dockerfile": "custom file"});
        fixture.config["services"]["init"] = json!({"image": "init-image"});
        fixture.config["services"]["first"]["depends_on"] =
            json!({"init": {"condition": "service_healthy"}});
        fixture.write();
        fixture.success(&["--provider", provider, "validate", "first"]);
        fixture.success(&["--provider", provider, "update-models", "first"]);
        fixture.success(&["--provider", provider, "update-images", "first"]);
        fixture.success(&["--provider", provider, "update-images", "second"]);
        let operation = fixture.wait(&fixture.success(&["--provider", provider, "start", "first"]));
        assert_eq!(operation["phase"], "ready", "{operation}");
        let calls = fixture.calls();
        let creates: Vec<_> = calls
            .iter()
            .filter(|args| args.get(1).is_some_and(|arg| arg == "create"))
            .collect();
        assert_eq!(creates.len(), 3);
        assert!(
            creates
                .iter()
                .any(|args| args.iter().any(|arg| arg == "nvidia.com/gpu=all"))
        );
        assert!(
            creates
                .iter()
                .any(|args| args.iter().any(|arg| arg == "127.0.0.1:0:8000"))
        );
        assert!(
            creates
                .iter()
                .any(|args| args.iter().any(|arg| arg.contains("alias=model-api")))
        );
        assert!(calls.iter().any(|args| {
            args.iter().any(|arg| arg == "--file=custom file")
                && args
                    .last()
                    .is_some_and(|arg| arg == "https://example.invalid/engine.git#fixed-ref")
        }));
        assert!(creates.iter().any(|args| {
            args.iter()
                .any(|arg| arg.contains("/lmserve/huggingface/hub:ro"))
        }));
        for value in [
            "HF_HUB_CACHE=/lmserve/huggingface/hub",
            "HF_HUB_OFFLINE=1",
            "TRANSFORMERS_OFFLINE=1",
        ] {
            assert!(
                creates
                    .iter()
                    .any(|args| args.iter().any(|arg| arg == value)),
                "missing {value}"
            );
        }
        assert!(
            creates.iter().any(|args| {
                args.iter().any(|arg| {
                    arg.contains("fixture_webui-data:/data")
                        || arg == "type=volume,source=fixture_webui-data,destination=/data"
                })
            }),
            "{creates:?}"
        );
        assert!(
            calls
                .iter()
                .any(|args| args.iter().any(|arg| arg == "--condition=healthy"))
        );
        assert_eq!(
            fixture.wait(&fixture.success(&["stop", "first"]))["phase"],
            "stopped"
        );
        assert!(
            fixture
                .calls()
                .iter()
                .any(|args| args.windows(2).any(|pair| pair == ["--time", "5"]))
        );
    }

    #[test]
    fn cdi_publishes_complete_output_at_explicit_path() {
        let fixture = Fixture::new();
        fixture.success(&["cdi", "--output", "nvidia.yaml"]);
        let document: Value = serde_json::from_slice(
            &fs_err::read(fixture.directory.path().join("nvidia.yaml"))
                .expect("read generated CDI"),
        )
        .expect("parse CDI");
        assert_eq!(document["cdiVersion"], "0.6.0");
    }
}
