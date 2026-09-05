//! `qemu.image`: download and verify a cloud image into a local cache.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use super::required_string;
use crate::provider::{Actual, Applied, Ctx, Handler, Result};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "qemu.image";

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageHandler;

fn image_path(_cx: &Ctx<'_>, inputs: &Value) -> Result<PathBuf> {
    let dir = required_string(inputs, "dir")?;
    let url = required_string(inputs, "url")?;
    let checksum = expected_sha256(inputs)?;
    let candidate = url
        .split('?')
        .next()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or_default();
    let safe = !matches!(candidate, "" | "." | "..")
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    let filename = if safe {
        format!("{checksum}-{candidate}")
    } else {
        format!("{checksum}.qcow2")
    };
    Ok(Path::new(dir).join("images").join(filename))
}

fn expected_sha256(inputs: &Value) -> Result<String> {
    let value = required_string(inputs, "sha256")?;
    anyhow::ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "`sha256` must be exactly 64 hexadecimal characters"
    );
    Ok(value.to_ascii_lowercase())
}

async fn sha256_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut digest = Sha256::new();
        std::io::copy(&mut file, &mut digest)
            .with_context(|| format!("hashing {}", path.display()))?;
        Ok::<_, anyhow::Error>(hex::encode(digest.finalize()))
    })
    .await
    .context("image checksum task failed")?
}

fn outputs(path: &Path, size: u64) -> Value {
    json!({
        "path": path.to_string_lossy(),
        "size": size,
    })
}

async fn observe(cx: &Ctx<'_>, inputs: &Value) -> Result<Option<Actual>> {
    let path = image_path(cx, inputs)?;
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => anyhow::bail!("image cache path is not a file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let checksum = sha256_file(&path).await?;
    Ok(Some(Actual {
        id: Some(path.to_string_lossy().into_owned()),
        props: json!({
            // The source URL is not embedded in qcow2, so carry it from the inputs used
            // to locate this cache entry. The checksum itself is directly observed.
            "url": inputs.get("url").cloned().unwrap_or(Value::Null),
            "sha256": checksum,
            "dir": inputs.get("dir").cloned().unwrap_or(Value::Null),
        }),
        outputs: outputs(&path, metadata.len()),
    }))
}

async fn download(cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
    let url = required_string(inputs, "url")?;
    let expected = expected_sha256(inputs)?;
    let path = image_path(cx, inputs)?;
    let parent = path.parent().expect("resource_path has a parent");
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating image cache {}", parent.display()))?;
    let temporary = path.with_extension(format!("partial-{}", std::process::id()));
    let _ = tokio::fs::remove_file(&temporary).await;

    let result: Result<u64> = async {
        let mut response = reqwest::Client::builder()
            .user_agent(concat!("ifx/", env!("CARGO_PKG_VERSION")))
            .build()?
            .get(url)
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?
            .error_for_status()
            .with_context(|| format!("downloading {url}"))?;
        let mut destination = tokio::fs::File::create(&temporary)
            .await
            .with_context(|| format!("creating {}", temporary.display()))?;
        let mut size = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("downloading {url}"))?
        {
            destination
                .write_all(&chunk)
                .await
                .with_context(|| format!("writing {}", temporary.display()))?;
            size += chunk.len() as u64;
        }
        destination
            .flush()
            .await
            .with_context(|| format!("flushing {}", temporary.display()))?;
        drop(destination);

        let actual = sha256_file(&temporary).await?;
        anyhow::ensure!(
            actual == expected,
            "downloaded image failed SHA-256 verification: expected {expected}, got {actual}"
        );
        tokio::fs::rename(&temporary, &path)
            .await
            .with_context(|| format!("moving verified image to {}", path.display()))?;
        Ok(size)
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    let size = result?;
    Ok(Applied {
        id: Some(path.to_string_lossy().into_owned()),
        outputs: outputs(&path, size),
    })
}

#[async_trait]
impl Handler for ImageHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A downloaded, SHA-256-verified cloud image cached for local QEMU instances. The cache survives destroy and is adopted on the next apply.",
        )
        .input(
            field("url", FieldType::String)
                .required()
                .replace()
                .doc("HTTPS URL of a qcow2 cloud image; changing it replaces the cache entry."),
        )
        .input(
            field("sha256", FieldType::String)
                .required()
                .doc("Expected lowercase or uppercase SHA-256 digest of the downloaded image."),
        )
        .input(
            field("dir", FieldType::String)
                .required()
                .replace()
                .doc("Persistent QEMU working directory containing the image cache."),
        )
        .output(
            field("path", FieldType::String).doc("Absolute or stack-relative cached image path."),
        )
        .output(field("size", FieldType::Int).doc("Cached image size in bytes."))
    }

    async fn read(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        observe(cx, inputs).await
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        download(cx, inputs).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        _id: Option<&str>,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        download(cx, inputs).await
    }

    async fn delete(&self, _cx: &Ctx<'_>, _id: Option<&str>, _inputs: &Value) -> Result<()> {
        // This resource models a download cache, not an owned VM disk. Keeping verified
        // bases makes destroy/recreate loops cheap; a later stack adopts the file after
        // re-hashing it. Users can remove the cache directory explicitly when desired.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Urn;
    use crate::transport::TransportPool;

    fn cx<'a>(urn: &'a Urn, transports: &'a TransportPool) -> Ctx<'a> {
        Ctx {
            urn,
            transports,
            triggered: false,
        }
    }

    #[tokio::test]
    async fn cache_path_is_content_addressed_and_checksum_is_observed() {
        let dir = tempfile::tempdir().unwrap();
        let urn = Urn::new(TYPE, "debian/13");
        let transports = TransportPool::default();
        let context = cx(&urn, &transports);
        let inputs = json!({
            "url": "https://example.test/debian.qcow2",
            "sha256": hex::encode(Sha256::digest(b"cloud image")),
            "dir": dir.path(),
        });
        let path = image_path(&context, &inputs).unwrap();
        assert!(
            path.ends_with(format!(
                "images/{}-debian.qcow2",
                inputs["sha256"].as_str().unwrap()
            )),
            "{}",
            path.display()
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"cloud image").unwrap();

        let actual = observe(&context, &inputs).await.unwrap().unwrap();
        assert_eq!(actual.props["sha256"], inputs["sha256"]);
        assert_eq!(actual.outputs["size"], 11);
    }

    #[test]
    fn changing_checksum_never_reuses_an_overlay_backing_path() {
        let urn = Urn::new(TYPE, "debian/13");
        let transports = TransportPool::default();
        let context = cx(&urn, &transports);
        let first = image_path(
            &context,
            &json!({
                "url": "https://example.test/debian.qcow2",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "dir": "/var/lib/ifx",
            }),
        )
        .unwrap();
        let second = image_path(
            &context,
            &json!({
                "url": "https://example.test/debian.qcow2",
                "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "dir": "/var/lib/ifx",
            }),
        )
        .unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn rejects_malformed_digest() {
        assert!(expected_sha256(&json!({"sha256": "abcd"})).is_err());
    }
}
