use crate::config;
use crate::config::Selection;
use crate::process;
use crate::runtime;
use crate::state;
use crate::state::Prepared;
use crate::state::State;
use crate::state::Store;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::process::Command;

pub(crate) fn prepared(state: &State, selection: &Selection) -> Result<Prepared> {
    let key = state::key(&selection.project, &selection.entry);
    let prepared = state
        .prepared
        .get(&key)
        .context("model is not prepared; run update-models")?;
    ensure!(
        prepared.root == selection.root && prepared.source == selection.model.huggingface,
        "configured model source or storage root changed; run update-models"
    );
    readable(&prepared.path)?;
    verify_manifest(prepared)?;
    Ok(prepared.clone())
}

pub(crate) fn verify_manifest(prepared: &Prepared) -> Result<()> {
    if prepared.source.file.is_none() {
        ensure!(
            prepared.path == prepared.owned_directory.join("hub"),
            "prepared repository requires a Hugging Face cache; run update-models"
        );
        let repository = prepared.path.join(hub_repository(&prepared.source.repo));
        ensure!(
            fs_err::read_to_string(repository.join("refs/main"))? == prepared.revision,
            "prepared cache revision changed; run update-models"
        );
        let snapshots = repository.join("snapshots");
        let entries = fs_err::read_dir(&snapshots)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            entries.len() == 1
                && entries
                    .first()
                    .is_some_and(|entry| entry.file_name() == prepared.revision.as_str()),
            "prepared cache must contain exactly its recorded revision; run update-models"
        );
        let mut count = 0;
        verify_tree(&prepared.path, &prepared.path, &mut count)?;
        ensure!(
            count == prepared.files.len(),
            "prepared cache file set changed; run update-models"
        );
    }
    for (relative, size) in &prepared.files {
        let path = if relative.is_empty() {
            prepared.path.clone()
        } else {
            prepared.path.join(relative)
        };
        ensure!(
            fs_err::metadata(&path)?.len() == *size,
            "prepared artifact is incomplete; run update-models"
        );
        fs_err::File::open(&path)?;
    }
    Ok(())
}

pub(crate) fn readable(path: &Utf8Path) -> Result<()> {
    let canonical = path
        .canonicalize_utf8()
        .context("prepared artifact is unavailable")?;
    if canonical.is_file() {
        fs_err::File::open(&canonical)?;
    } else {
        ensure!(
            canonical.is_dir(),
            "prepared artifact is not a file or directory"
        );
        let mut count = 0;
        verify_tree(&canonical, &canonical, &mut count)?;
        ensure!(count > 0, "prepared snapshot is empty");
    }
    Ok(())
}

fn verify_tree(root: &Utf8Path, directory: &Utf8Path, count: &mut usize) -> Result<()> {
    for item in fs_err::read_dir(directory)? {
        let path = Utf8PathBuf::from_path_buf(item?.path())
            .map_err(|_redacted_error| anyhow::anyhow!("artifact path is not UTF-8"))?;
        let metadata = fs_err::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            let target = path.canonicalize_utf8()?;
            ensure!(
                target.starts_with(root) && target.is_file(),
                "snapshot link escapes mounted tree or targets a directory"
            );
            fs_err::File::open(&target)?;
            *count += 1;
        } else if metadata.is_dir() {
            verify_tree(root, &path, count)?;
        } else {
            ensure!(metadata.is_file(), "artifact contains a special file");
            fs_err::File::open(&path)?;
            *count += 1;
        }
    }
    Ok(())
}

pub(crate) fn update(store: &Store, selection: &Selection) -> Result<Prepared> {
    let _lock = store.lock("control")?;
    let _execution = store.lock("execution")?;
    let mut state = store.read()?;
    state::reconcile_workers(&mut state);
    ensure!(
        !state
            .operations
            .values()
            .any(|operation| operation.phase.pending()),
        "lifecycle operation is busy"
    );
    let current = runtime::containers()?;
    for key in runtime::active_keys(&state, &current)? {
        let deployment = state
            .deployments
            .get(&key)
            .context("missing active deployment")?;
        ensure!(
            deployment.project != selection.project || deployment.entry != selection.entry,
            "artifact update is blocked by the active model entry"
        );
        ensure!(
            deployment.artifact.root != selection.root
                || deployment.artifact.source.repo != selection.model.huggingface.repo,
            "artifact update is blocked by an active model"
        );
    }
    let revision = resolve_revision(selection)?;
    let directory = owned_root(selection)?;
    let mut identity = serde_json::to_vec(&(
        &selection.model.huggingface.repo,
        &revision,
        &selection.model.huggingface.file,
    ))?;
    if selection.model.huggingface.file.is_none() {
        identity.extend_from_slice(b"hub-cache-v1");
    }
    let identity = state::digest(&identity);
    let target = directory.join(&identity);
    let content = target.join(if selection.model.huggingface.file.is_some() {
        "content"
    } else {
        "hub"
    });
    if !target.exists() {
        download(selection, &revision, &directory, &target)?;
    }
    ensure!(
        !fs_err::symlink_metadata(&target)?.file_type().is_symlink(),
        "managed artifact directory cannot be a symlink"
    );
    ensure!(
        fs_err::read_to_string(target.join("owner"))? == selection.project,
        "artifact ownership is unknown"
    );
    readable(&content)?;
    let prepared = Prepared {
        source: selection.model.huggingface.clone(),
        revision,
        root: selection.root.clone(),
        path: content,
        owned_directory: target,
        files: serde_json::from_slice(&fs_err::read(
            directory.join(&identity).join("manifest.json"),
        )?)?,
    };
    verify_manifest(&prepared)?;
    state.prepared.insert(
        state::key(&selection.project, &selection.entry),
        prepared.clone(),
    );
    store.save(&state)?;
    cleanup(selection, &state, &directory)
        .context("preparation completed but retention cleanup failed; retry update-models")?;
    Ok(prepared)
}

fn resolve_revision(selection: &Selection) -> Result<String> {
    let mut command = Command::new("hf");
    command.args([
        "models",
        "info",
        &selection.model.huggingface.repo,
        "--format",
        "json",
    ]);
    if let Some(revision) = &selection.model.huggingface.revision {
        command.args(["--revision", revision]);
    }
    let output = process::output(&mut command, None, process::PREPARATION_TIMEOUT)
        .context("resolve configured Hugging Face revision")?;
    let info: Value = serde_json::from_slice(&output).context("invalid hf model information")?;
    let revision = config::field(&info, "sha")
        .as_str()
        .context("hf did not return a resolved commit")?;
    ensure!(
        revision.len() == 40 && revision.bytes().all(|c| c.is_ascii_hexdigit()),
        "hf returned an invalid commit identity"
    );
    Ok(revision.to_ascii_lowercase())
}

fn hub_repository(repo: &str) -> String {
    format!("models--{}", repo.replace('/', "--"))
}

fn owned_root(selection: &Selection) -> Result<Utf8PathBuf> {
    if selection.root.join(".lmserve").exists() {
        ensure!(
            !fs_err::symlink_metadata(selection.root.join(".lmserve"))?
                .file_type()
                .is_symlink(),
            "managed cache namespace cannot be a symlink"
        );
    }
    let directory = selection
        .root
        .join(".lmserve")
        .join(state::digest(selection.project.as_bytes()));
    if directory.exists() {
        ensure!(
            !fs_err::symlink_metadata(&directory)?
                .file_type()
                .is_symlink(),
            "managed model directory cannot be a symlink"
        );
        ensure!(
            fs_err::read_to_string(directory.join("owner"))? == selection.project,
            "model root ownership is unknown"
        );
    } else {
        state::private_directory(&directory)?;
        state::atomic_write(&directory.join("owner"), selection.project.as_bytes())?;
    }
    Ok(directory)
}

fn download(
    selection: &Selection,
    revision: &str,
    directory: &Utf8Path,
    target: &Utf8Path,
) -> Result<()> {
    let stage = tempfile::Builder::new()
        .prefix(".preparing-")
        .tempdir_in(directory)?;
    let stage_path = Utf8Path::from_path(stage.path()).context("temporary path is not UTF-8")?;
    let cache = stage_path.join("download");
    let mut command = Command::new("hf");
    command.args(["download", &selection.model.huggingface.repo]);
    if let Some(file) = &selection.model.huggingface.file {
        command.arg(file);
    }
    command.args([
        "--revision",
        revision,
        "--cache-dir",
        cache.as_str(),
        "--quiet",
    ]);
    let output = process::output(&mut command, None, process::PREPARATION_TIMEOUT)
        .context("model download failed; previous prepared revision is preserved")?;
    let downloaded = Utf8PathBuf::from(
        String::from_utf8(output)
            .context("hf returned a non-UTF-8 path")?
            .trim(),
    );
    let downloaded = downloaded
        .canonicalize_utf8()
        .context("hf returned an unreadable artifact")?;
    let cache = cache.canonicalize_utf8()?;
    ensure!(
        downloaded.starts_with(&cache),
        "hf returned content outside the managed download directory"
    );
    let published = stage_path.join("published");
    state::private_directory(&published)?;
    let content;
    if selection.model.huggingface.file.is_some() {
        content = published.join("content");
        ensure!(
            downloaded.is_file(),
            "single-file download returned a directory"
        );
        fs_err::copy(&downloaded, &content)?;
    } else {
        content = published.join("hub");
        ensure!(downloaded.is_dir(), "snapshot download returned a file");
        let repository = hub_repository(&selection.model.huggingface.repo);
        let snapshot = Utf8Path::new(&repository).join("snapshots").join(revision);
        ensure!(
            downloaded == cache.join(&snapshot),
            "hf returned a different repository or revision"
        );
        let snapshot = content.join(snapshot);
        copy_snapshot(&downloaded, &snapshot, &cache)?;
        readable(&snapshot)?;
        let refs = content.join(repository).join("refs");
        state::private_directory(&refs)?;
        state::atomic_write(&refs.join("main"), revision.as_bytes())?;
    }
    readable(&content)?;
    let mut files = BTreeMap::new();
    manifest(&content, &content, &mut files)?;
    state::atomic_write(
        &published.join("manifest.json"),
        &serde_json::to_vec(&files)?,
    )?;
    state::atomic_write(&published.join("owner"), selection.project.as_bytes())?;
    state::atomic_write(
        &published.join("source.json"),
        &serde_json::to_vec(&(&selection.model.huggingface.repo, revision))?,
    )?;
    fs_err::rename(&published, target)?;
    fs_err::File::open(directory)?.sync_all()?;
    Ok(())
}

fn manifest(root: &Utf8Path, path: &Utf8Path, files: &mut BTreeMap<String, u64>) -> Result<()> {
    if path.is_file() {
        fs_err::File::open(path)?.sync_all()?;
        files.insert(
            path.strip_prefix(root)?.to_string(),
            fs_err::metadata(path)?.len(),
        );
    } else {
        for item in fs_err::read_dir(path)? {
            let item = Utf8PathBuf::from_path_buf(item?.path())
                .map_err(|_non_utf8| anyhow::anyhow!("artifact path is not UTF-8"))?;
            manifest(root, &item, files)?;
        }
        fs_err::File::open(path)?.sync_all()?;
    }
    Ok(())
}

fn copy_snapshot(source: &Utf8Path, target: &Utf8Path, cache: &Utf8Path) -> Result<()> {
    state::private_directory(target)?;
    for item in fs_err::read_dir(source)? {
        let item = item?;
        let path = Utf8PathBuf::from_path_buf(item.path())
            .map_err(|_redacted_error| anyhow::anyhow!("artifact path is not UTF-8"))?;
        let destination = target.join(path.file_name().context("artifact has no file name")?);
        let canonical = path.canonicalize_utf8()?;
        ensure!(
            canonical.starts_with(cache),
            "download contains a link outside its cache"
        );
        if fs_err::symlink_metadata(&path)?.is_dir() {
            copy_snapshot(&path, &destination, cache)?;
        } else {
            ensure!(
                canonical.is_file(),
                "download contains a directory symlink or special file"
            );
            fs_err::copy(canonical, destination)?;
        }
    }
    Ok(())
}

fn cleanup(selection: &Selection, state: &State, directory: &Utf8Path) -> Result<()> {
    let containers = runtime::containers()?;
    let active = runtime::active_keys(state, &containers)?;
    let retained: BTreeSet<_> = state
        .prepared
        .values()
        .map(|prepared| &prepared.owned_directory)
        .chain(
            active
                .iter()
                .filter_map(|key| state.deployments.get(key))
                .map(|deployment| &deployment.artifact.owned_directory),
        )
        .collect();
    let mut candidates = Vec::new();
    for item in fs_err::read_dir(directory)? {
        let path = Utf8PathBuf::from_path_buf(item?.path())
            .map_err(|_redacted_error| anyhow::anyhow!("cache path is not UTF-8"))?;
        if !fs_err::symlink_metadata(&path)?.is_dir() || retained.contains(&path) {
            continue;
        }
        if !path.join("owner").exists() {
            continue;
        }
        candidates.push(path);
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut references = BTreeSet::new();
    for (entry, source) in &selection.sources {
        let source = source
            .as_ref()
            .context("cannot establish another model's references; retaining superseded content")?;
        references.insert((source.repo.clone(), source.revision.clone()));
        if entry != &selection.entry
            && !state
                .prepared
                .contains_key(&state::key(&selection.project, entry))
        {
            ensure!(
                source
                    .revision
                    .as_ref()
                    .is_some_and(|revision| revision.len() == 40
                        && revision.bytes().all(|c| c.is_ascii_hexdigit()))
                    || source.repo != selection.model.huggingface.repo,
                "another unprepared entry has an unresolved reference; preserving superseded content"
            );
        }
    }
    for path in candidates {
        ensure!(
            fs_err::read_to_string(path.join("owner"))? == selection.project,
            "unknown cache ownership; preserving content"
        );
        let (repo, revision): (String, String) =
            serde_json::from_slice(&fs_err::read(path.join("source.json"))?)?;
        if references.contains(&(repo, Some(revision))) {
            continue;
        }
        fs_err::remove_dir_all(&path)?;
    }
    Ok(())
}

pub(crate) fn update_images(
    store: &Store,
    selection: &Selection,
    provider: &str,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    let mut failed = Vec::new();
    for name in &selection.services {
        let service = config::field(config::field(&selection.document, "services"), name);
        let reference = runtime::image_reference(&selection.project, name, service);
        let identity = state::digest(&serde_json::to_vec(&(
            &reference,
            config::field(service, "build"),
        ))?);
        if !seen.insert(identity) {
            continue;
        }
        let action = if config::field(service, "build").is_null() {
            "pull"
        } else {
            "build"
        };
        crate::app::report(&format!("{name}: preparing image ({action})"));
        let build = config::field(service, "build");
        let remote = config::field(build, "context")
            .as_str()
            .is_some_and(|context| context.contains("://") || context.starts_with("git@"));
        let dockerfile = config::field(build, "dockerfile")
            .as_str()
            .filter(|_| remote);
        let forwarded = dockerfile.map(|file| format!("'--file={}'", file.replace('\'', "'\\''")));
        let mut args = Vec::new();
        if let Some(forwarded) = &forwarded {
            args.extend(["--podman-build-args", forwarded.as_str()]);
        }
        args.extend([action, name]);
        let result = process::compose(
            provider,
            &selection.directory,
            &selection.document,
            &[],
            &args,
            process::PREPARATION_TIMEOUT,
            None,
        )
        .and_then(|_| runtime::image_identity(&reference));
        match result {
            Ok(image) => {
                let inspected = process::podman(&["image", "inspect", &image])?;
                let source_revision = inspected
                    .as_array()
                    .and_then(|items| items.first())
                    .and_then(|item| {
                        item.pointer("/Config/Labels/org.opencontainers.image.revision")
                    })
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let _lock = crate::lifecycle::wait_control(store)?;
                let mut state = store.read()?;
                state.images.insert(
                    state::key(&selection.project, name),
                    state::ImagePreparation {
                        reference,
                        identity: image.clone(),
                        source_revision,
                    },
                );
                store.save(&state)?;
                crate::app::report(&format!("{name}: prepared {image}"));
            }
            Err(error) => failed.push(format!("{name}: {error:#}")),
        }
    }
    if !failed.is_empty() {
        bail!("image preparation failed: {}", failed.join("; "));
    }
    Ok(())
}
