use crate::after;
use crate::emit;
use crate::emit_json;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use camino::Utf8Path;
use serde_json::json;

pub(super) fn hf(args: &[&str]) -> Result<()> {
    match args {
        ["models", "info", ..] => emit_json(
            &json!({"sha": std::env::var("FAKE_REVISION").unwrap_or_else(|_| "a".repeat(40))}),
        ),
        ["download", repo, filename, ..] => {
            ensure!(
                std::env::var_os("FAKE_DOWNLOAD_FAIL").is_none(),
                "no space left on device: token=must-not-leak"
            );
            let cache = Utf8Path::new(after(args, "--cache-dir")?);
            let revision = after(args, "--revision")?;
            let repository = cache.join(format!("models--{}", repo.replace('/', "--")));
            let snapshot_revision =
                std::env::var("FAKE_SNAPSHOT_REVISION").unwrap_or_else(|_| revision.to_owned());
            let snapshot = repository.join("snapshots").join(snapshot_revision);
            let blobs = repository.join("blobs");
            fs_err::create_dir_all(&blobs)?;
            let blob = blobs.join("content");
            fs_err::write(&blob, revision)?;
            fs_err::create_dir_all(&snapshot)?;
            if std::env::var_os("FAKE_EMPTY_SNAPSHOT").is_some() {
                return emit(snapshot.as_str());
            }
            let filename = if filename.starts_with("--") {
                "model.bin"
            } else {
                filename
            };
            let artifact = snapshot.join(filename);
            if let Some(parent) = artifact.parent() {
                fs_err::create_dir_all(parent)?;
            }
            fs_err::os::unix::fs::symlink(blob, &artifact)?;
            if filename == "model.bin" {
                fs_err::write(
                    snapshot.join("config.json"),
                    serde_json::to_vec(&json!({
                        "model_type": "bert", "vocab_size": 2, "hidden_size": 4,
                        "num_hidden_layers": 1, "num_attention_heads": 1, "intermediate_size": 8,
                        "lmserve_revision": revision
                    }))?,
                )?;
                fs_err::write(
                    snapshot.join("tokenizer_config.json"),
                    serde_json::to_vec(&json!({
                        "tokenizer_class": "PreTrainedTokenizerFast", "unk_token": "[UNK]"
                    }))?,
                )?;
                fs_err::write(
                    snapshot.join("tokenizer.json"),
                    serde_json::to_vec(&json!({
                        "version": "1.0", "added_tokens": [],
                        "model": {"type": "WordLevel", "vocab": {"[UNK]": 0, revision: 1}, "unk_token": "[UNK]"},
                        "pre_tokenizer": {"type": "Whitespace"}
                    }))?,
                )?;
            }
            emit(if filename == "model.bin" {
                snapshot.as_str()
            } else {
                artifact.as_str()
            })
        }
        _ => bail!("unsupported fake hf invocation"),
    }
}

pub(super) fn cdi(args: &[&str]) -> Result<()> {
    match args {
        ["cdi", "list"] => emit("nvidia.com/gpu=all"),
        ["cdi", "generate", ..] => {
            fs_err::write(
                after(args, "--output")?,
                serde_json::to_vec(&json!({"cdiVersion": "0.6.0", "devices": []}))?,
            )?;
            Ok(())
        }
        _ => bail!("unsupported fake CDI invocation"),
    }
}
