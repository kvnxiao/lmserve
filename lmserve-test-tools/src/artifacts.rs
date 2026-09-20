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
        ["download", _, filename, ..] => {
            ensure!(
                std::env::var_os("FAKE_DOWNLOAD_FAIL").is_none(),
                "no space left on device: token=must-not-leak"
            );
            let cache = Utf8Path::new(after(args, "--cache-dir")?);
            let revision = after(args, "--revision")?;
            let snapshot = cache.join("models--fixture/snapshots").join(revision);
            let blobs = cache.join("models--fixture/blobs");
            fs_err::create_dir_all(&blobs)?;
            let blob = blobs.join("content");
            fs_err::write(&blob, revision)?;
            fs_err::create_dir_all(&snapshot)?;
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
