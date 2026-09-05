//! Local QEMU resources: verified cloud-image caches, rootless virtual networks, and
//! virtual machines configured over the same SSH transport as cloud instances.

mod image;
mod instance;
mod network;
mod volume;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use sha2::{Digest as _, Sha256};

pub use crate::generated::qemu::*;
pub use image::ImageHandler;
pub use instance::InstanceHandler;
pub use network::NetworkHandler;
pub use volume::VolumeHandler;

use crate::provider::Registry;

pub fn register(r: &mut Registry) {
    r.register(ImageHandler);
    r.register(NetworkHandler);
    r.register(VolumeHandler);
    r.register(InstanceHandler);
}

pub(super) fn required_string<'a>(
    inputs: &'a serde_json::Value,
    field: &str,
) -> anyhow::Result<&'a str> {
    inputs
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`{field}` must be a string"))
}

fn resource_name(name: &str) -> String {
    let mut readable = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    readable = readable.trim_matches('-').to_string();
    if readable.is_empty() {
        readable.push_str("resource");
    }
    readable.truncate(40);
    let digest = Sha256::digest(name.as_bytes());
    format!("{readable}-{}", hex::encode(&digest[..6]))
}

pub(super) fn resource_dir(dir: &str, kind: &str, name: &str) -> PathBuf {
    Path::new(dir).join(kind).join(resource_name(name))
}

pub(super) fn resource_path(dir: &str, kind: &str, name: &str, extension: &str) -> PathBuf {
    let mut path = resource_dir(dir, kind, name);
    path.set_extension(extension);
    path
}

pub(super) fn absolute(path: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
    let path = path.as_ref();
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(super) fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, data).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("renaming {} to {}", temporary.display(), path.display()))?;
    Ok(())
}
