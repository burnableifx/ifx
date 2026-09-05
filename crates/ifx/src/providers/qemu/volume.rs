//! `qemu.volume`: a durable local disk that can be attached to QEMU instances.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{absolute, required_string, resource_path, write_atomic};
use crate::provider::{
    Actual, Applied, Ctx, Diff, FieldChange, Handler, OperationKind, OperationRisk, Result,
};
use crate::schema::{FieldType, ResourceSchema, field};

pub const TYPE: &str = "qemu.volume";
const GIB: u64 = 1024 * 1024 * 1024;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub struct VolumeHandler;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct VolumeRecord {
    version: u32,
    dir_input: String,
    size_gb: u64,
    format: String,
    source: Option<String>,
    source_path: Option<String>,
    source_mode: String,
    preallocation: String,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileIdentity {
    canonical_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImageInfo {
    format: String,
    virtual_size: u64,
    allocated_size: u64,
    backing_path: Option<String>,
}

fn positive_integer(inputs: &Value, field: &str) -> Result<u64> {
    let value = inputs
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("`{field}` must be a positive integer"))?;
    anyhow::ensure!(value > 0, "`{field}` must be a positive integer");
    Ok(value)
}

fn size_bytes(size_gb: u64) -> Result<u64> {
    size_gb
        .checked_mul(GIB)
        .ok_or_else(|| anyhow::anyhow!("volume size {size_gb} GiB overflows bytes"))
}

fn desired_path(cx: &Ctx<'_>, inputs: &Value) -> Result<PathBuf> {
    let format = inputs
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("qcow2");
    absolute(resource_path(
        required_string(inputs, "dir")?,
        "volumes",
        cx.urn.name(),
        format,
    ))
}

fn record_path(disk: &Path) -> PathBuf {
    let mut name = disk.file_name().unwrap_or_default().to_os_string();
    name.push(".ifx.json");
    disk.with_file_name(name)
}

fn path_to_observe(cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<PathBuf> {
    let expected = desired_path(cx, inputs)?;
    if let Some(id) = id {
        let path = PathBuf::from(id);
        anyhow::ensure!(
            path == expected,
            "QEMU volume identity {} is outside its owned path {}; repair state instead of allowing an arbitrary file mutation",
            path.display(),
            expected.display()
        );
        return Ok(path);
    }
    Ok(expected)
}

#[cfg(unix)]
fn same_file(left: &Path, right: &Path) -> Result<bool> {
    Ok(file_identity(left)? == file_identity(right)?)
}

#[cfg(not(unix))]
fn same_file(left: &Path, right: &Path) -> Result<bool> {
    Ok(std::fs::canonicalize(left)? == std::fs::canonicalize(right)?)
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading file identity for {}", path.display()))?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Result<FileIdentity> {
    Ok(FileIdentity {
        canonical_path: std::fs::canonicalize(path)?,
    })
}

fn matches_identity(path: &Path, expected: &FileIdentity) -> Result<bool> {
    match file_identity(path) {
        Ok(actual) => Ok(&actual == expected),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn remove_if_identity(path: &Path, expected: &FileIdentity) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    anyhow::ensure!(
        matches_identity(path, expected)?,
        "refusing to remove a competing file at {}",
        path.display()
    );
    tokio::fs::remove_file(path)
        .await
        .with_context(|| format!("removing {}", path.display()))
}

fn load_record(path: &Path) -> Result<Option<VolumeRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing QEMU volume record {}", path.display()))
        .map(Some)
}

fn parse_info(value: &Value) -> Result<ImageInfo> {
    let format = value
        .get("format")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("qemu-img info did not report `format`"))?;
    let virtual_size = value
        .get("virtual-size")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("qemu-img info did not report `virtual-size`"))?;
    let allocated_size = value
        .get("actual-size")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            value
                .get("actual_size")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
    let backing_path = value
        .get("full-backing-filename")
        .or_else(|| value.get("backing-filename"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ImageInfo {
        format: format.to_string(),
        virtual_size,
        allocated_size,
        backing_path,
    })
}

fn find_qemu_img() -> Result<String> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("PATH is not set; cannot find `qemu-img`"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join("qemu-img");
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("required command `qemu-img` was not found in PATH; install QEMU image tooling")
}

async fn command_output(program: &str, args: &[String], what: &str) -> Result<Vec<u8>> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running `{program}` for {what}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow::bail!(
        "{what} failed with {}{}",
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

async fn run_command(program: &str, args: &[String], what: &str) -> Result<()> {
    command_output(program, args, what).await.map(|_| ())
}

async fn publish_new_volume(temporary: &Path, destination: &Path) -> Result<()> {
    let identity = file_identity(temporary)?;
    tokio::fs::hard_link(temporary, destination)
        .await
        .with_context(|| {
            format!(
                "publishing QEMU volume {} without replacing an existing file",
                destination.display()
            )
        })?;
    if let Err(error) = remove_if_identity(temporary, &identity).await {
        let _ = remove_if_identity(destination, &identity).await;
        return Err(error).with_context(|| {
            format!(
                "removing staged QEMU volume {} after publication",
                temporary.display()
            )
        });
    }
    Ok(())
}

fn info_args(path: &Path, force_share: bool) -> Vec<String> {
    let mut args = vec!["info".into()];
    if force_share {
        args.push("--force-share".into());
    }
    args.extend(["--output=json".into(), path.to_string_lossy().into_owned()]);
    args
}

async fn inspect_with_mode(qemu_img: &str, path: &Path, force_share: bool) -> Result<ImageInfo> {
    let output = command_output(
        qemu_img,
        &info_args(path, force_share),
        &format!("inspecting QEMU volume {}", path.display()),
    )
    .await?;
    let value: Value = serde_json::from_slice(&output)
        .with_context(|| format!("parsing qemu-img info for {}", path.display()))?;
    parse_info(&value)
}

async fn inspect(qemu_img: &str, path: &Path) -> Result<ImageInfo> {
    inspect_with_mode(qemu_img, path, true).await
}

/// A QEMU-compatible claim on every block permission. QEMU uses read locks at
/// offsets 100..104 to announce held permissions and 200..204 to deny sharing;
/// claiming both sets prevents a VM or qemu-img mutation from opening the inode.
struct QemuImageLock {
    _file: File,
}

impl QemuImageLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening QEMU volume lock target {}", path.display()))?;
        for offset in (100..104).chain(200..204) {
            set_qemu_lock(file.as_raw_fd(), offset, libc::F_RDLCK as libc::c_short).with_context(
                || {
                    format!(
                        "locking QEMU permission byte {offset} for {}",
                        path.display()
                    )
                },
            )?;
        }
        for offset in (100..104).chain(200..204) {
            anyhow::ensure!(
                qemu_lock_available(file.as_raw_fd(), offset)?,
                "QEMU volume {} is in use; stop every attached VM before mutating it",
                path.display()
            );
        }
        Ok(Self { _file: file })
    }
}

#[cfg(target_os = "linux")]
const QEMU_SET_LOCK: libc::c_int = libc::F_OFD_SETLK;
#[cfg(target_os = "linux")]
const QEMU_GET_LOCK: libc::c_int = libc::F_OFD_GETLK;
#[cfg(not(target_os = "linux"))]
const QEMU_SET_LOCK: libc::c_int = libc::F_SETLK;
#[cfg(not(target_os = "linux"))]
const QEMU_GET_LOCK: libc::c_int = libc::F_GETLK;

fn lock_request(offset: i64, lock_type: libc::c_short) -> libc::flock {
    // SAFETY: every field is initialized below before the value crosses the FFI
    // boundary. Zero is the required value for unused platform-specific fields.
    let mut request: libc::flock = unsafe { std::mem::zeroed() };
    request.l_type = lock_type;
    request.l_whence = libc::SEEK_SET as libc::c_short;
    request.l_start = offset as libc::off_t;
    request.l_len = 1;
    request
}

fn set_qemu_lock(fd: std::os::fd::RawFd, offset: i64, lock_type: libc::c_short) -> Result<()> {
    let request = lock_request(offset, lock_type);
    // SAFETY: `fd` is open for the lock lifetime and `request` points to a valid
    // `flock` value for this immediate fcntl call.
    let status = unsafe { libc::fcntl(fd, QEMU_SET_LOCK, &request) };
    if status == -1 {
        return Err(std::io::Error::last_os_error()).context("setting QEMU image lock");
    }
    Ok(())
}

fn qemu_lock_available(fd: std::os::fd::RawFd, offset: i64) -> Result<bool> {
    let mut request = lock_request(offset, libc::F_WRLCK as libc::c_short);
    // SAFETY: `fd` is open and `request` is a valid mutable `flock` for this
    // immediate fcntl query.
    let status = unsafe { libc::fcntl(fd, QEMU_GET_LOCK, &mut request) };
    if status == -1 {
        return Err(std::io::Error::last_os_error()).context("checking QEMU image lock");
    }
    Ok(request.l_type == libc::F_UNLCK as libc::c_short)
}

fn preallocation_option(preallocation: &str) -> Option<String> {
    (preallocation != "off").then(|| format!("preallocation={preallocation}"))
}

fn create_commands(
    record: &VolumeRecord,
    temporary: &Path,
    source_info: Option<&ImageInfo>,
) -> Result<Vec<Vec<String>>> {
    let bytes = size_bytes(record.size_gb)?;
    let mut commands = Vec::new();
    match (&record.source_path, record.source_mode.as_str()) {
        (None, _) => {
            let mut args = vec![
                "create".into(),
                "-q".into(),
                "-f".into(),
                record.format.clone(),
            ];
            if let Some(option) = preallocation_option(&record.preallocation) {
                args.extend(["-o".into(), option]);
            }
            args.extend([temporary.to_string_lossy().into_owned(), bytes.to_string()]);
            commands.push(args);
        }
        (Some(source), "overlay") => {
            let source_info =
                source_info.ok_or_else(|| anyhow::anyhow!("overlay source was not inspected"))?;
            let mut args = vec![
                "create".into(),
                "-q".into(),
                "-f".into(),
                "qcow2".into(),
                "-F".into(),
                source_info.format.clone(),
                "-b".into(),
                source.clone(),
            ];
            if record.preallocation != "off" {
                args.extend([
                    "-o".into(),
                    format!("extended_l2=on,preallocation={}", record.preallocation),
                ]);
            }
            args.push(temporary.to_string_lossy().into_owned());
            commands.push(args);
            if bytes > source_info.virtual_size {
                commands.push(vec![
                    "resize".into(),
                    "-q".into(),
                    temporary.to_string_lossy().into_owned(),
                    bytes.to_string(),
                ]);
            }
        }
        (Some(source), "copy") => {
            let source_info =
                source_info.ok_or_else(|| anyhow::anyhow!("copy source was not inspected"))?;
            let mut args = vec![
                "convert".into(),
                "-q".into(),
                "-O".into(),
                record.format.clone(),
            ];
            if let Some(option) = preallocation_option(&record.preallocation) {
                args.extend(["-o".into(), option]);
            }
            args.extend([source.clone(), temporary.to_string_lossy().into_owned()]);
            commands.push(args);
            if bytes > source_info.virtual_size {
                commands.push(vec![
                    "resize".into(),
                    "-q".into(),
                    temporary.to_string_lossy().into_owned(),
                    bytes.to_string(),
                ]);
            }
        }
        (Some(_), other) => anyhow::bail!("unsupported QEMU volume source mode `{other}`"),
    }
    Ok(commands)
}

struct StagedVolume {
    directory: PathBuf,
    disk: PathBuf,
    record: PathBuf,
    info: ImageInfo,
}

async fn create_work_dir(parent: &Path, purpose: &str) -> Result<PathBuf> {
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    for _ in 0..1_024 {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".ifx-{purpose}-{}-{sequence}", std::process::id()));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    anyhow::bail!(
        "could not allocate a unique QEMU volume {purpose} directory under {}",
        parent.display()
    )
}

async fn remove_staged_volume(staged: &StagedVolume) -> Result<()> {
    for path in [&staged.disk, &staged.record] {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
        }
    }
    match tokio::fs::remove_dir(&staged.directory).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("removing {}", staged.directory.display()))
        }
    }
}

async fn cleanup_after_commit(staged: &StagedVolume) {
    if let Err(error) = remove_staged_volume(staged).await {
        tracing::warn!(
            path = %staged.directory.display(),
            %error,
            "could not remove committed QEMU volume staging directory"
        );
    }
}

async fn stage_volume(
    qemu_img: &str,
    record: &VolumeRecord,
    destination: &Path,
) -> Result<StagedVolume> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", destination.display()))?;
    let directory = create_work_dir(parent, "stage").await?;
    let disk = directory.join("volume.partial");
    let record_path = directory.join("volume.ifx.json");

    let result: Result<(ImageInfo, Vec<u8>)> = async {
        let source_info = if let Some(source) = &record.source_path {
            let source = Path::new(source);
            anyhow::ensure!(
                source.is_file(),
                "QEMU volume source does not exist: {}",
                source.display()
            );
            anyhow::ensure!(
                record.source_mode != "overlay" || source != destination,
                "QEMU volume {} cannot use itself as a backing image",
                destination.display()
            );
            let info = inspect(qemu_img, source).await?;
            anyhow::ensure!(
                size_bytes(record.size_gb)? >= info.virtual_size,
                "QEMU volume size {} GiB is smaller than source virtual size {} bytes",
                record.size_gb,
                info.virtual_size
            );
            Some(info)
        } else {
            None
        };
        for args in create_commands(record, &disk, source_info.as_ref())? {
            run_command(qemu_img, &args, "staging QEMU volume").await?;
        }
        let info = inspect(qemu_img, &disk).await?;
        anyhow::ensure!(
            info.format == record.format,
            "staged QEMU volume format is {}, expected {}",
            info.format,
            record.format
        );
        anyhow::ensure!(
            info.virtual_size == size_bytes(record.size_gb)?,
            "staged QEMU volume size is {} bytes, expected {} bytes",
            info.virtual_size,
            size_bytes(record.size_gb)?
        );
        let expected_backing = (record.source_mode == "overlay")
            .then_some(record.source_path.as_deref())
            .flatten();
        anyhow::ensure!(
            info.backing_path.as_deref() == expected_backing,
            "staged QEMU volume backing path is {:?}, expected {:?}",
            info.backing_path,
            expected_backing
        );
        Ok((info, serde_json::to_vec_pretty(record)?))
    }
    .await;

    match result {
        Ok((info, record_bytes)) => {
            if let Err(error) = tokio::fs::write(&record_path, record_bytes).await {
                let staged = StagedVolume {
                    directory,
                    disk,
                    record: record_path,
                    info,
                };
                let _ = remove_staged_volume(&staged).await;
                return Err(error).context("writing staged QEMU volume record");
            }
            Ok(StagedVolume {
                directory,
                disk,
                record: record_path,
                info,
            })
        }
        Err(error) => {
            let staged = StagedVolume {
                directory,
                disk,
                record: record_path,
                info: ImageInfo {
                    format: String::new(),
                    virtual_size: 0,
                    allocated_size: 0,
                    backing_path: None,
                },
            };
            let _ = remove_staged_volume(&staged).await;
            Err(error)
        }
    }
}

struct VolumeBackup {
    directory: PathBuf,
    disk: PathBuf,
    record: PathBuf,
    record_bytes: Vec<u8>,
    disk_identity: FileIdentity,
    record_identity: FileIdentity,
}

async fn backup_volume(path: &Path) -> Result<VolumeBackup> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    let owned_record = record_path(path);
    let record_bytes = tokio::fs::read(&owned_record)
        .await
        .with_context(|| format!("reading {} before replacement", owned_record.display()))?;
    let directory = create_work_dir(parent, "backup").await?;
    let disk = directory.join("volume.previous");
    let record = directory.join("volume.previous.ifx.json");
    let result: Result<()> = async {
        tokio::fs::hard_link(path, &disk).await.with_context(|| {
            format!(
                "backing up QEMU volume {} before replacement; its filesystem must support hard links",
                path.display()
            )
        })?;
        tokio::fs::hard_link(&owned_record, &record)
            .await
            .with_context(|| format!("backing up {} before replacement", owned_record.display()))?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&disk).await;
        let _ = tokio::fs::remove_file(&record).await;
        let _ = tokio::fs::remove_dir(&directory).await;
        return Err(error);
    }
    Ok(VolumeBackup {
        directory,
        disk_identity: file_identity(&disk)?,
        record_identity: file_identity(&record)?,
        disk,
        record,
        record_bytes,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransactionPhase {
    Prepared,
    OldRemoved,
    DiskPublished,
    RecordPublished,
    PhysicalCommit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VolumeTransaction {
    version: u32,
    phase: TransactionPhase,
    old_path: PathBuf,
    desired_path: PathBuf,
    staged_directory: PathBuf,
    staged_disk: PathBuf,
    staged_record: PathBuf,
    staged_disk_identity: FileIdentity,
    staged_record_identity: FileIdentity,
    backup_directory: PathBuf,
    backup_disk: PathBuf,
    backup_record: PathBuf,
    old_disk_identity: FileIdentity,
    old_record_identity: FileIdentity,
    old_record: VolumeRecord,
    desired_record: VolumeRecord,
}

fn transaction_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".ifx-replace.json");
    path.with_file_name(name)
}

fn transaction_paths(transaction: &VolumeTransaction) -> Vec<PathBuf> {
    let old = transaction_path(&transaction.old_path);
    let desired = transaction_path(&transaction.desired_path);
    if old == desired {
        vec![old]
    } else {
        vec![old, desired]
    }
}

fn sync_file(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("opening {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", path.display()))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("opening directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory {}", path.display()))
}

fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    for _ in 0..1_024 {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.tmp-{}-{sequence}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating transaction temporary file"),
        };
        let result: Result<()> = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
                .with_context(|| format!("publishing transaction manifest {}", path.display()))?;
            sync_directory(parent)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        return result;
    }
    anyhow::bail!(
        "could not allocate a transaction temporary file under {}",
        parent.display()
    )
}

fn write_transaction(transaction: &VolumeTransaction) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(transaction)?;
    for path in transaction_paths(transaction) {
        write_durable(&path, &bytes)?;
    }
    Ok(())
}

fn load_transaction(path: &Path) -> Result<Option<VolumeTransaction>> {
    let manifest = transaction_path(path);
    let bytes = match std::fs::read(&manifest) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", manifest.display()));
        }
    };
    let transaction: VolumeTransaction = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", manifest.display()))?;
    anyhow::ensure!(
        transaction.version == 1,
        "unsupported QEMU volume transaction version {}",
        transaction.version
    );
    Ok(Some(transaction))
}

async fn remove_directory_if_empty(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn collect_error(errors: &mut Vec<String>, what: &str, result: Result<()>) {
    if let Err(error) = result {
        errors.push(format!("{what}: {error:#}"));
    }
}

async fn remove_new_file(path: &Path, new_identity: &FileIdentity) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if matches_identity(path, new_identity)? {
        return remove_if_identity(path, new_identity).await;
    }
    // The pathname now belongs to somebody else (or still names the old inode).
    // It is not part of this transaction and must be preserved.
    Ok(())
}

async fn restore_backup(backup: &Path, destination: &Path, expected: &FileIdentity) -> Result<()> {
    if destination.exists() {
        anyhow::ensure!(
            matches_identity(destination, expected)?,
            "refusing to overwrite a competing file at {} while rolling back; the previous volume remains at {}",
            destination.display(),
            backup.display()
        );
        return Ok(());
    }
    anyhow::ensure!(
        matches_identity(backup, expected)?,
        "replacement backup {} is missing or has changed",
        backup.display()
    );
    tokio::fs::hard_link(backup, destination)
        .await
        .with_context(|| {
            format!(
                "restoring {} from {}",
                destination.display(),
                backup.display()
            )
        })
}

async fn rollback_transaction(transaction: &VolumeTransaction) -> Result<()> {
    let mut errors = Vec::new();
    let previous_lock_path = if transaction.backup_disk.exists() {
        &transaction.backup_disk
    } else {
        &transaction.old_path
    };
    anyhow::ensure!(
        matches_identity(previous_lock_path, &transaction.old_disk_identity)?,
        "previous QEMU volume and its rollback backup are missing"
    );
    let _previous_lock = QemuImageLock::acquire(previous_lock_path)?;
    let _replacement_lock = if transaction.desired_path.exists()
        && matches_identity(&transaction.desired_path, &transaction.staged_disk_identity)?
    {
        Some(QemuImageLock::acquire(&transaction.desired_path)?)
    } else {
        None
    };
    collect_error(
        &mut errors,
        "removing replacement record",
        remove_new_file(
            &record_path(&transaction.desired_path),
            &transaction.staged_record_identity,
        )
        .await,
    );
    collect_error(
        &mut errors,
        "removing replacement disk",
        remove_new_file(&transaction.desired_path, &transaction.staged_disk_identity).await,
    );
    collect_error(
        &mut errors,
        "restoring previous disk",
        restore_backup(
            &transaction.backup_disk,
            &transaction.old_path,
            &transaction.old_disk_identity,
        )
        .await,
    );
    let old_record_path = record_path(&transaction.old_path);
    let record_restore = if transaction.backup_record.exists() {
        restore_backup(
            &transaction.backup_record,
            &old_record_path,
            &transaction.old_record_identity,
        )
        .await
    } else if old_record_path.exists() {
        anyhow::ensure!(
            matches_identity(&old_record_path, &transaction.old_record_identity)?,
            "previous volume record was replaced at {}",
            old_record_path.display()
        );
        Ok(())
    } else {
        anyhow::bail!("previous volume record and its rollback backup are both missing")
    };
    collect_error(&mut errors, "restoring previous record", record_restore);

    if errors.is_empty() {
        for (what, path, identity) in [
            (
                "removing staged disk",
                &transaction.staged_disk,
                &transaction.staged_disk_identity,
            ),
            (
                "removing staged record",
                &transaction.staged_record,
                &transaction.staged_record_identity,
            ),
            (
                "removing disk backup",
                &transaction.backup_disk,
                &transaction.old_disk_identity,
            ),
            (
                "removing record backup",
                &transaction.backup_record,
                &transaction.old_record_identity,
            ),
        ] {
            collect_error(&mut errors, what, remove_if_identity(path, identity).await);
        }
        collect_error(
            &mut errors,
            "removing staging directory",
            remove_directory_if_empty(&transaction.staged_directory).await,
        );
        collect_error(
            &mut errors,
            "removing backup directory",
            remove_directory_if_empty(&transaction.backup_directory).await,
        );
    }
    if errors.is_empty() {
        for manifest in transaction_paths(transaction) {
            if let Err(error) = tokio::fs::remove_file(&manifest).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(format!("removing {}: {error}", manifest.display()));
            }
        }
    }
    anyhow::ensure!(
        errors.is_empty(),
        "QEMU volume rollback is incomplete: {}",
        errors.join("; ")
    );
    Ok(())
}

async fn finalize_transaction(transaction: &VolumeTransaction) -> Result<()> {
    anyhow::ensure!(
        matches_identity(&transaction.desired_path, &transaction.staged_disk_identity)?,
        "replacement destination {} no longer matches the committed disk",
        transaction.desired_path.display()
    );
    anyhow::ensure!(
        matches_identity(
            &record_path(&transaction.desired_path),
            &transaction.staged_record_identity
        )?,
        "replacement record for {} no longer matches the committed record",
        transaction.desired_path.display()
    );
    let mut errors = Vec::new();
    for (what, path, identity) in [
        (
            "removing staged disk",
            &transaction.staged_disk,
            &transaction.staged_disk_identity,
        ),
        (
            "removing staged record",
            &transaction.staged_record,
            &transaction.staged_record_identity,
        ),
        (
            "removing disk backup",
            &transaction.backup_disk,
            &transaction.old_disk_identity,
        ),
        (
            "removing record backup",
            &transaction.backup_record,
            &transaction.old_record_identity,
        ),
    ] {
        collect_error(&mut errors, what, remove_if_identity(path, identity).await);
    }
    collect_error(
        &mut errors,
        "removing staging directory",
        remove_directory_if_empty(&transaction.staged_directory).await,
    );
    collect_error(
        &mut errors,
        "removing backup directory",
        remove_directory_if_empty(&transaction.backup_directory).await,
    );
    if errors.is_empty() {
        for manifest in transaction_paths(transaction) {
            if let Err(error) = tokio::fs::remove_file(&manifest).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(format!("removing {}: {error}", manifest.display()));
            }
        }
    }
    anyhow::ensure!(
        errors.is_empty(),
        "QEMU volume finalization is incomplete: {}",
        errors.join("; ")
    );
    Ok(())
}

async fn prepare_transaction(
    staged: &StagedVolume,
    desired: &Path,
    old: &Path,
    desired_record: &VolumeRecord,
) -> Result<VolumeTransaction> {
    for manifest in [transaction_path(old), transaction_path(desired)] {
        anyhow::ensure!(
            !manifest.exists(),
            "unfinished QEMU volume transaction exists at {}; retry or cancel its owning run",
            manifest.display()
        );
    }
    let backup = backup_volume(old).await?;
    sync_file(&staged.disk)?;
    sync_file(&staged.record)?;
    sync_directory(&staged.directory)?;
    sync_directory(&backup.directory)?;
    let old_record: VolumeRecord = serde_json::from_slice(&backup.record_bytes)
        .context("parsing previous QEMU volume record before replacement")?;
    let transaction = VolumeTransaction {
        version: 1,
        phase: TransactionPhase::Prepared,
        old_path: old.to_path_buf(),
        desired_path: desired.to_path_buf(),
        staged_directory: staged.directory.clone(),
        staged_disk: staged.disk.clone(),
        staged_record: staged.record.clone(),
        staged_disk_identity: file_identity(&staged.disk)?,
        staged_record_identity: file_identity(&staged.record)?,
        backup_directory: backup.directory,
        backup_disk: backup.disk,
        backup_record: backup.record,
        old_disk_identity: backup.disk_identity,
        old_record_identity: backup.record_identity,
        old_record,
        desired_record: desired_record.clone(),
    };
    write_transaction(&transaction)?;
    Ok(transaction)
}

async fn commit_replacement(
    staged: &StagedVolume,
    desired: &Path,
    old: &Path,
    desired_record: &VolumeRecord,
) -> Result<()> {
    let mut transaction = prepare_transaction(staged, desired, old, desired_record).await?;
    let same_path = desired == old;
    let result: Result<()> = async {
        if same_path {
            remove_if_identity(old, &transaction.old_disk_identity).await?;
            remove_if_identity(&record_path(old), &transaction.old_record_identity).await?;
            transaction.phase = TransactionPhase::OldRemoved;
            write_transaction(&transaction)?;
        }
        publish_new_volume(&staged.disk, desired).await?;
        transaction.phase = TransactionPhase::DiskPublished;
        write_transaction(&transaction)?;
        publish_new_volume(&staged.record, &record_path(desired)).await?;
        transaction.phase = TransactionPhase::RecordPublished;
        write_transaction(&transaction)?;
        if !same_path {
            remove_if_identity(old, &transaction.old_disk_identity).await?;
            remove_if_identity(&record_path(old), &transaction.old_record_identity).await?;
        }
        transaction.phase = TransactionPhase::PhysicalCommit;
        write_transaction(&transaction)?;
        Ok(())
    }
    .await;

    if let Err(error) = result {
        if let Err(rollback) = rollback_transaction(&transaction).await {
            return Err(error).context(format!(
                "QEMU volume replacement also failed to roll back: {rollback:#}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn record_from_inputs(inputs: &Value) -> Result<VolumeRecord> {
    let format = inputs
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("qcow2");
    anyhow::ensure!(
        matches!(format, "qcow2" | "raw"),
        "unsupported QEMU volume format `{format}`"
    );
    let source_mode = inputs
        .get("source_mode")
        .and_then(Value::as_str)
        .unwrap_or("overlay");
    anyhow::ensure!(
        matches!(source_mode, "overlay" | "copy"),
        "unsupported QEMU volume source mode `{source_mode}`"
    );
    let preallocation = inputs
        .get("preallocation")
        .and_then(Value::as_str)
        .unwrap_or("off");
    anyhow::ensure!(
        matches!(preallocation, "off" | "metadata" | "falloc" | "full"),
        "unsupported QEMU volume preallocation `{preallocation}`"
    );
    let source = inputs.get("source").and_then(Value::as_str);
    anyhow::ensure!(
        source.is_none() || source_mode != "overlay" || format == "qcow2",
        "QEMU backing overlays require `format=\"qcow2\"`; use `source_mode=\"copy\"` for raw volumes"
    );
    anyhow::ensure!(
        format != "raw" || preallocation != "metadata",
        "raw QEMU volumes do not support `preallocation=\"metadata\"`; use `off`, `falloc`, or `full`"
    );
    let source_path = source
        .map(|path| {
            std::fs::canonicalize(path)
                .map_err(anyhow::Error::from)
                .or_else(|_| absolute(path))
        })
        .transpose()?
        .map(|path| path.to_string_lossy().into_owned());
    Ok(VolumeRecord {
        version: 1,
        dir_input: required_string(inputs, "dir")?.to_string(),
        size_gb: positive_integer(inputs, "size_gb")?,
        format: format.to_string(),
        source: source.map(str::to_string),
        source_path,
        source_mode: source_mode.to_string(),
        preallocation: preallocation.to_string(),
    })
}

fn props(record: &VolumeRecord, info: &ImageInfo) -> Value {
    let expected_backing = (record.source_mode == "overlay")
        .then_some(record.source_path.as_deref())
        .flatten();
    json!({
        "dir": record.dir_input,
        "size_gb": record.size_gb,
        "format": info.format,
        "source": record.source,
        "source_mode": record.source_mode,
        "preallocation": record.preallocation,
        "_virtual_size": info.virtual_size,
        "_backing_matches": info.backing_path.as_deref() == expected_backing,
        "_backing_path": info.backing_path,
    })
}

fn outputs(path: &Path, info: &ImageInfo) -> Value {
    json!({
        "path": path.to_string_lossy(),
        "format": info.format,
        "virtual_size_bytes": info.virtual_size,
        "allocated_size_bytes": info.allocated_size,
        "backing_path": info.backing_path,
    })
}

fn validate_transaction(
    cx: &Ctx<'_>,
    transaction: &VolumeTransaction,
    old_id: Option<&str>,
    old_inputs: &Value,
    id: Option<&str>,
    inputs: &Value,
) -> Result<()> {
    let old_path = path_to_observe(cx, old_id, old_inputs)?;
    let desired_path = path_to_observe(cx, id, inputs)?;
    anyhow::ensure!(
        transaction.old_path == old_path
            && transaction.desired_path == desired_path
            && transaction.old_record == record_from_inputs(old_inputs)?
            && transaction.desired_record == record_from_inputs(inputs)?,
        "QEMU volume transaction does not match the persisted replacement intent"
    );
    Ok(())
}

async fn finalize_committed_transaction(path: &Path, inputs: &Value, allowed: bool) -> Result<()> {
    if !allowed {
        return Ok(());
    }
    let Some(transaction) = load_transaction(path)? else {
        return Ok(());
    };
    if path == transaction.desired_path
        && record_from_inputs(inputs)? == transaction.desired_record
        && matches_identity(path, &transaction.staged_disk_identity)?
    {
        finalize_transaction(&transaction).await?;
    }
    Ok(())
}

async fn observe(cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<Option<Actual>> {
    observe_with_cleanup(cx, id, inputs, true).await
}

async fn observe_with_cleanup(
    cx: &Ctx<'_>,
    id: Option<&str>,
    inputs: &Value,
    allow_cleanup: bool,
) -> Result<Option<Actual>> {
    let path = path_to_observe(cx, id, inputs)?;
    finalize_committed_transaction(&path, inputs, allow_cleanup).await?;
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => anyhow::bail!("QEMU volume path is not a file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    anyhow::ensure!(metadata.is_file(), "QEMU volume is not a file");
    let record_path = record_path(&path);
    let record = load_record(&record_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "QEMU volume {} exists without its IFX record {}; move it aside or restore the record before adoption",
            path.display(),
            record_path.display()
        )
    })?;
    anyhow::ensure!(
        record.version == 1,
        "unsupported QEMU volume record version {}",
        record.version
    );
    let qemu_img = find_qemu_img()?;
    let info = inspect(&qemu_img, &path).await?;
    Ok(Some(Actual {
        id: Some(path.to_string_lossy().into_owned()),
        props: props(&record, &info),
        outputs: outputs(&path, &info),
    }))
}

#[async_trait]
impl Handler for VolumeHandler {
    fn schema(&self) -> ResourceSchema {
        ResourceSchema::new(
            TYPE,
            "A durable QEMU disk created blank, as a backing overlay, or as an independent image copy.",
        )
        .input(
            field("dir", FieldType::String)
                .required()
                .replace()
                .doc("Persistent QEMU working directory containing owned volumes."),
        )
        .input(
            field("size_gb", FieldType::Int)
                .required()
                .doc("Virtual size in GiB. Growth is in place; shrink replaces the volume after approval."),
        )
        .input(
            field("format", FieldType::enumeration(["qcow2", "raw"]))
                .default("qcow2")
                .replace()
                .doc("On-disk image format."),
        )
        .input(
            field("source", FieldType::String)
                .replace()
                .doc("Optional source image path, normally `qemu.image.path`."),
        )
        .input(
            field(
                "source_mode",
                FieldType::enumeration(["overlay", "copy"]),
            )
            .default("overlay")
            .replace()
            .doc("Use the source as a qcow2 backing file, or create an independent copy."),
        )
        .input(
            field(
                "preallocation",
                FieldType::enumeration(["off", "metadata", "falloc", "full"]),
            )
            .default("off")
            .replace()
            .doc("qemu-img allocation policy used only when the volume is created."),
        )
        .output(field("path", FieldType::String).doc("Absolute owned volume path."))
        .output(field("format", FieldType::enumeration(["qcow2", "raw"])).doc(
            "Observed image format for typed instance attachments.",
        ))
        .output(
            field("virtual_size_bytes", FieldType::Int).doc("Observed virtual capacity in bytes."),
        )
        .output(
            field("allocated_size_bytes", FieldType::Int)
                .doc("Host bytes currently allocated according to qemu-img."),
        )
        .output(
            field("backing_path", FieldType::String)
                .doc("Observed backing image path for overlay volumes, otherwise null."),
        )
    }

    async fn read(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<Option<Actual>> {
        observe(cx, id, inputs).await
    }

    async fn read_for_recovery(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<Option<Actual>> {
        observe_with_cleanup(cx, id, inputs, false).await
    }

    fn diff(&self, desired: &Value, actual: &Actual) -> Result<Diff> {
        let schema = self.schema();
        let replace: Vec<_> = schema.replace_fields().collect();
        let mut diff = Diff::generic(desired, &actual.props, &replace);
        diff.changes.retain(|change| change.field != "size_gb");
        let desired_gb = positive_integer(desired, "size_gb")?;
        let desired_bytes = size_bytes(desired_gb)?;
        let actual_bytes = actual
            .props
            .get("_virtual_size")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("observed QEMU volume has no virtual size"))?;
        if desired_bytes != actual_bytes {
            diff.changes.push(FieldChange {
                field: "size_gb".into(),
                from: Some(json!(actual_bytes)),
                to: Some(json!(desired_gb)),
                forces_replace: desired_bytes < actual_bytes,
            });
        }
        if actual.props.get("_backing_matches") == Some(&Value::Bool(false))
            && !diff.changes.iter().any(|change| change.field == "source")
        {
            diff.changes.push(FieldChange {
                field: "source".into(),
                from: actual.props.get("_backing_path").cloned(),
                to: desired.get("source").cloned(),
                forces_replace: true,
            });
        }
        Ok(diff)
    }

    fn risks(
        &self,
        operation: OperationKind,
        _desired: &Value,
        actual: Option<&Actual>,
    ) -> Result<Vec<OperationRisk>> {
        if actual.is_none() || !matches!(operation, OperationKind::Replace | OperationKind::Delete)
        {
            return Ok(Vec::new());
        }
        Ok(vec![OperationRisk {
            name: "volume-data-loss".into(),
            reason:
                "replacing or deleting this volume permanently removes its current disk contents"
                    .into(),
        }])
    }

    async fn create(&self, cx: &Ctx<'_>, inputs: &Value) -> Result<Applied> {
        let qemu_img = find_qemu_img()?;
        let record = record_from_inputs(inputs)?;
        let path = desired_path(cx, inputs)?;
        anyhow::ensure!(
            !path.exists(),
            "refusing to overwrite existing QEMU volume {}; move it aside or restore its IFX state",
            path.display()
        );
        let staged = stage_volume(&qemu_img, &record, &path).await?;
        let staged_disk_identity = file_identity(&staged.disk)?;
        let result: Result<()> = async {
            publish_new_volume(&staged.disk, &path).await?;
            if let Err(error) = publish_new_volume(&staged.record, &record_path(&path)).await {
                let _ = remove_if_identity(&path, &staged_disk_identity).await;
                return Err(error);
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = remove_staged_volume(&staged).await;
        }
        result?;
        cleanup_after_commit(&staged).await;
        Ok(Applied {
            id: Some(path.to_string_lossy().into_owned()),
            outputs: outputs(&path, &staged.info),
        })
    }

    async fn replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &Value,
        inputs: &Value,
        _actual: &Actual,
    ) -> Result<Applied> {
        let qemu_img = find_qemu_img()?;
        let old_path = path_to_observe(cx, id, old_inputs)?;
        anyhow::ensure!(
            old_path.is_file(),
            "QEMU volume to replace does not exist: {}",
            old_path.display()
        );

        let record = record_from_inputs(inputs)?;
        let path = desired_path(cx, inputs)?;
        anyhow::ensure!(
            load_transaction(&old_path)?.is_none()
                && (path == old_path || load_transaction(&path)?.is_none()),
            "unfinished QEMU volume transaction exists; retry or cancel its owning run"
        );
        if let Some(source) = record.source_path.as_deref().map(Path::new)
            && source.is_file()
        {
            anyhow::ensure!(
                !same_file(source, &old_path)?,
                "replacement QEMU volume cannot use the volume being replaced as its source or backing image"
            );
        }
        let _old_lock = QemuImageLock::acquire(&old_path)?;
        let staged = stage_volume(&qemu_img, &record, &path).await?;
        let _staged_lock = QemuImageLock::acquire(&staged.disk)?;
        commit_replacement(&staged, &path, &old_path, &record).await?;
        Ok(Applied {
            id: Some(path.to_string_lossy().into_owned()),
            outputs: outputs(&path, &staged.info),
        })
    }

    async fn recover_replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &Value,
        inputs: &Value,
        _old_actual: Option<&Actual>,
        _desired_actual: Option<&Actual>,
    ) -> Result<Applied> {
        let old_path = path_to_observe(cx, id, old_inputs)?;
        if let Some(transaction) = load_transaction(&old_path)? {
            validate_transaction(cx, &transaction, id, old_inputs, None, inputs)?;
            rollback_transaction(&transaction).await?;
        }
        let old_actual = observe_with_cleanup(cx, id, old_inputs, false).await?;
        if let Some(actual) = old_actual {
            if self.diff(inputs, &actual)?.is_empty() {
                return Ok(Applied {
                    id: actual.id,
                    outputs: actual.outputs,
                });
            }
            return self.replace(cx, id, old_inputs, inputs, &actual).await;
        }
        if let Some(actual) = observe_with_cleanup(cx, None, inputs, false).await? {
            anyhow::ensure!(
                self.diff(inputs, &actual)?.is_empty(),
                "replacement recovery found the desired identity with mismatched properties"
            );
            return Ok(Applied {
                id: actual.id,
                outputs: actual.outputs,
            });
        }
        self.create(cx, inputs).await
    }

    async fn abort_replace(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        old_inputs: &Value,
        inputs: &Value,
    ) -> Result<()> {
        let old_path = path_to_observe(cx, id, old_inputs)?;
        if let Some(transaction) = load_transaction(&old_path)? {
            validate_transaction(cx, &transaction, id, old_inputs, None, inputs)?;
            return rollback_transaction(&transaction).await;
        }
        let actual = observe_with_cleanup(cx, id, old_inputs, false)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the previous QEMU volume no longer exists"))?;
        anyhow::ensure!(
            self.diff(old_inputs, &actual)?.is_empty(),
            "the previous QEMU volume no longer matches its applied state"
        );
        Ok(())
    }

    async fn finalize_replace(
        &self,
        cx: &Ctx<'_>,
        old_id: Option<&str>,
        old_inputs: &Value,
        id: Option<&str>,
        inputs: &Value,
    ) -> Result<()> {
        let old_path = path_to_observe(cx, old_id, old_inputs)?;
        let desired_path = path_to_observe(cx, id, inputs)?;
        let transaction = match load_transaction(&old_path)? {
            Some(transaction) => Some(transaction),
            None if desired_path != old_path => load_transaction(&desired_path)?,
            None => None,
        };
        let Some(transaction) = transaction else {
            return Ok(());
        };
        validate_transaction(cx, &transaction, old_id, old_inputs, id, inputs)?;
        finalize_transaction(&transaction).await
    }

    async fn update(
        &self,
        cx: &Ctx<'_>,
        id: Option<&str>,
        inputs: &Value,
        actual: &Actual,
    ) -> Result<Applied> {
        let qemu_img = find_qemu_img()?;
        let path = path_to_observe(cx, id, inputs)?;
        let info = inspect(&qemu_img, &path).await?;
        let desired = record_from_inputs(inputs)?;
        let desired_bytes = size_bytes(desired.size_gb)?;
        anyhow::ensure!(
            desired_bytes >= info.virtual_size,
            "refusing in-place QEMU volume shrink from {} to {} bytes; the plan must replace the volume with exact data-loss approval",
            info.virtual_size,
            desired_bytes
        );
        if desired_bytes > info.virtual_size {
            run_command(
                &qemu_img,
                &[
                    "resize".into(),
                    "-q".into(),
                    path.to_string_lossy().into_owned(),
                    desired_bytes.to_string(),
                ],
                "growing QEMU volume",
            )
            .await?;
        }
        write_atomic(&record_path(&path), &serde_json::to_vec_pretty(&desired)?)?;
        let info = inspect(&qemu_img, &path).await?;
        Ok(Applied {
            id: actual
                .id
                .clone()
                .or_else(|| Some(path.to_string_lossy().into_owned())),
            outputs: outputs(&path, &info),
        })
    }

    async fn delete(&self, cx: &Ctx<'_>, id: Option<&str>, inputs: &Value) -> Result<()> {
        let path = path_to_observe(cx, id, inputs)?;
        finalize_committed_transaction(&path, inputs, id.is_some()).await?;
        let owned_record_path = record_path(&path);
        if !path.exists() {
            let Some(owned_record) = load_record(&owned_record_path)? else {
                return Ok(());
            };
            anyhow::ensure!(
                owned_record == record_from_inputs(inputs)?,
                "refusing to remove residual QEMU volume record {} because it no longer matches state",
                owned_record_path.display()
            );
            let record_identity = file_identity(&owned_record_path)?;
            remove_if_identity(&owned_record_path, &record_identity).await?;
            return Ok(());
        }
        let owned_record = load_record(&owned_record_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "refusing to delete QEMU volume {} without its IFX ownership record",
                path.display()
            )
        })?;
        anyhow::ensure!(
            owned_record == record_from_inputs(inputs)?,
            "refusing to delete QEMU volume {} because its ownership record no longer matches state",
            path.display()
        );
        let disk_identity = file_identity(&path)?;
        let record_identity = file_identity(&owned_record_path)?;
        let _lock = QemuImageLock::acquire(&path)?;
        remove_if_identity(&path, &disk_identity).await?;
        remove_if_identity(&owned_record_path, &record_identity).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Urn;
    use crate::transport::TransportPool;

    fn context<'a>(urn: &'a Urn, transports: &'a TransportPool) -> Ctx<'a> {
        Ctx {
            urn,
            transports,
            triggered: false,
        }
    }

    fn record(source: Option<&str>, source_mode: &str) -> VolumeRecord {
        VolumeRecord {
            version: 1,
            dir_input: "/var/lib/ifx".into(),
            size_gb: 10,
            format: "qcow2".into(),
            source: source.map(str::to_string),
            source_path: source.map(str::to_string),
            source_mode: source_mode.into(),
            preallocation: "off".into(),
        }
    }

    #[test]
    fn qemu_img_info_uses_full_backing_path_and_exact_sizes() {
        let info = parse_info(&json!({
            "format": "qcow2",
            "virtual-size": 10 * GIB,
            "actual-size": 1_048_576,
            "backing-filename": "../base.qcow2",
            "full-backing-filename": "/var/lib/ifx/images/base.qcow2"
        }))
        .unwrap();
        assert_eq!(info.format, "qcow2");
        assert_eq!(info.virtual_size, 10 * GIB);
        assert_eq!(info.allocated_size, 1_048_576);
        assert_eq!(
            info.backing_path.as_deref(),
            Some("/var/lib/ifx/images/base.qcow2")
        );
    }

    #[test]
    fn live_observation_forces_shared_qemu_img_access() {
        let path = Path::new("/volumes/running.qcow2");
        assert_eq!(
            info_args(path, true),
            [
                "info",
                "--force-share",
                "--output=json",
                "/volumes/running.qcow2"
            ]
        );
        assert!(
            !info_args(path, false)
                .iter()
                .any(|arg| arg == "--force-share")
        );
    }

    #[test]
    fn qemu_permission_lock_excludes_other_open_file_descriptions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("volume.qcow2");
        std::fs::write(&path, b"disk").unwrap();

        let first = QemuImageLock::acquire(&path).unwrap();
        let error = QemuImageLock::acquire(&path).err().unwrap();
        assert!(error.to_string().contains("in use"));
        drop(first);
        QemuImageLock::acquire(&path).unwrap();
    }

    #[test]
    fn hard_linked_source_is_recognized_as_the_same_volume() {
        let directory = tempfile::tempdir().unwrap();
        let volume = directory.path().join("volume.qcow2");
        let alias = directory.path().join("alias.qcow2");
        std::fs::write(&volume, b"disk").unwrap();
        std::fs::hard_link(&volume, &alias).unwrap();
        assert!(same_file(&volume, &alias).unwrap());
    }

    #[test]
    fn creation_commands_keep_overlay_and_copy_semantics_distinct() {
        let source = ImageInfo {
            format: "qcow2".into(),
            virtual_size: 2 * GIB,
            allocated_size: 0,
            backing_path: None,
        };
        let mut overlay_record = record(Some("/images/base.qcow2"), "overlay");
        overlay_record.preallocation = "metadata".into();
        let overlay = create_commands(
            &overlay_record,
            Path::new("/volumes/data.partial"),
            Some(&source),
        )
        .unwrap();
        assert!(
            overlay[0]
                .windows(2)
                .any(|pair| pair == ["-b", "/images/base.qcow2"])
        );
        assert!(
            overlay[0]
                .iter()
                .any(|argument| argument == "extended_l2=on,preallocation=metadata"),
            "backed qcow2 preallocation requires extended L2 entries"
        );
        assert_eq!(overlay[1][0], "resize");

        let copy = create_commands(
            &record(Some("/images/base.qcow2"), "copy"),
            Path::new("/volumes/data.partial"),
            Some(&source),
        )
        .unwrap();
        assert_eq!(copy[0][0], "convert");
        assert!(!copy[0].iter().any(|argument| argument == "-b"));
        assert_eq!(copy[1][0], "resize");
    }

    #[test]
    fn raw_volume_rejects_unsupported_metadata_preallocation() {
        let error = record_from_inputs(&json!({
            "dir": "/var/lib/ifx",
            "size_gb": 10,
            "format": "raw",
            "preallocation": "metadata"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("do not support"));
    }

    #[test]
    fn growth_updates_in_place_but_shrink_replaces() {
        let handler = VolumeHandler;
        let actual = Actual {
            id: Some("/volumes/data.qcow2".into()),
            props: json!({
                "dir": "/var/lib/ifx",
                "size_gb": 10,
                "format": "qcow2",
                "source": null,
                "source_mode": "overlay",
                "preallocation": "off",
                "_virtual_size": 10 * GIB,
                "_backing_matches": true,
                "_backing_path": null
            }),
            outputs: Value::Null,
        };
        let grow = handler
            .diff(
                &json!({
                    "dir": "/var/lib/ifx", "size_gb": 20, "format": "qcow2",
                    "source_mode": "overlay", "preallocation": "off"
                }),
                &actual,
            )
            .unwrap();
        assert!(!grow.requires_replace(), "growth must remain an update");
        assert_eq!(grow.changes.len(), 1);

        let shrink = handler
            .diff(
                &json!({
                    "dir": "/var/lib/ifx", "size_gb": 5, "format": "qcow2",
                    "source_mode": "overlay", "preallocation": "off"
                }),
                &actual,
            )
            .unwrap();
        assert!(shrink.requires_replace(), "shrink must never run in place");
    }

    #[test]
    fn existing_volume_replacement_and_delete_require_data_loss_approval() {
        let handler = VolumeHandler;
        let actual = Actual::default();
        for operation in [OperationKind::Replace, OperationKind::Delete] {
            let risks = handler
                .risks(operation, &Value::Null, Some(&actual))
                .unwrap();
            assert_eq!(risks.len(), 1);
            assert_eq!(risks[0].name, "volume-data-loss");
        }
        assert!(
            handler
                .risks(OperationKind::Create, &Value::Null, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn volume_path_is_stable_and_separate_from_instance_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let urn = Urn::new(TYPE, "database/data");
        let transports = TransportPool::default();
        let cx = context(&urn, &transports);
        let path = desired_path(&cx, &json!({"dir": dir.path(), "format": "qcow2"})).unwrap();
        assert!(path.starts_with(dir.path().join("volumes")));
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("qcow2")
        );
    }

    #[tokio::test]
    async fn publication_never_replaces_a_competing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let temporary = dir.path().join("staged.qcow2");
        let destination = dir.path().join("volume.qcow2");
        tokio::fs::write(&temporary, b"new volume").await.unwrap();
        tokio::fs::write(&destination, b"existing volume")
            .await
            .unwrap();

        let error = publish_new_volume(&temporary, &destination)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("without replacing"), "{error:#}");
        assert_eq!(
            tokio::fs::read(&destination).await.unwrap(),
            b"existing volume"
        );
        assert_eq!(tokio::fs::read(&temporary).await.unwrap(), b"new volume");
    }

    fn staged_volume(directory: &Path, disk: &[u8], record: &[u8]) -> StagedVolume {
        std::fs::create_dir(directory).unwrap();
        let disk_path = directory.join("volume.partial");
        let record_path = directory.join("volume.ifx.json");
        std::fs::write(&disk_path, disk).unwrap();
        std::fs::write(&record_path, record).unwrap();
        StagedVolume {
            directory: directory.to_path_buf(),
            disk: disk_path,
            record: record_path,
            info: ImageInfo {
                format: "qcow2".into(),
                virtual_size: GIB,
                allocated_size: disk.len() as u64,
                backing_path: None,
            },
        }
    }

    #[tokio::test]
    async fn replacement_rolls_back_when_both_outputs_cannot_be_published() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.qcow2");
        let desired = dir.path().join("new.qcow2");
        std::fs::write(&old, b"old disk").unwrap();
        let old_record = serde_json::to_vec(&record(None, "overlay")).unwrap();
        std::fs::write(record_path(&old), &old_record).unwrap();
        std::fs::write(record_path(&desired), b"competing record").unwrap();
        let staged = staged_volume(&dir.path().join("stage"), b"new disk", b"new record");

        let desired_record = record(None, "overlay");
        let error = commit_replacement(&staged, &desired, &old, &desired_record)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("without replacing"), "{error:#}");
        assert_eq!(std::fs::read(&old).unwrap(), b"old disk");
        assert_eq!(std::fs::read(record_path(&old)).unwrap(), old_record);
        assert!(!desired.exists());
        assert_eq!(
            std::fs::read(record_path(&desired)).unwrap(),
            b"competing record"
        );
    }

    #[tokio::test]
    async fn replacement_commits_new_contents_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.qcow2");
        std::fs::write(&path, b"old disk").unwrap();
        std::fs::write(
            record_path(&path),
            serde_json::to_vec(&record(None, "overlay")).unwrap(),
        )
        .unwrap();
        let desired_record = record(None, "overlay");
        let desired_record_bytes = serde_json::to_vec(&desired_record).unwrap();
        let staged = staged_volume(
            &dir.path().join("stage"),
            b"new disk",
            &desired_record_bytes,
        );
        commit_replacement(&staged, &path, &path, &desired_record)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new disk");
        assert_eq!(
            std::fs::read(record_path(&path)).unwrap(),
            desired_record_bytes
        );
        let transaction = load_transaction(&path).unwrap().unwrap();
        assert!(transaction.backup_disk.exists());
        finalize_transaction(&transaction).await.unwrap();
        assert!(!staged.directory.exists());
        assert!(load_transaction(&path).unwrap().is_none());
    }

    #[tokio::test]
    async fn durable_manifest_recovers_a_crash_after_old_unlink() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.qcow2");
        let old_record = record(None, "overlay");
        let old_record_bytes = serde_json::to_vec(&old_record).unwrap();
        std::fs::write(&path, b"old disk").unwrap();
        std::fs::write(record_path(&path), &old_record_bytes).unwrap();
        let desired_record = record(None, "copy");
        let desired_record_bytes = serde_json::to_vec(&desired_record).unwrap();
        let staged = staged_volume(
            &directory.path().join("stage"),
            b"new disk",
            &desired_record_bytes,
        );
        let mut transaction = prepare_transaction(&staged, &path, &path, &desired_record)
            .await
            .unwrap();
        remove_if_identity(&path, &transaction.old_disk_identity)
            .await
            .unwrap();
        remove_if_identity(&record_path(&path), &transaction.old_record_identity)
            .await
            .unwrap();
        transaction.phase = TransactionPhase::OldRemoved;
        write_transaction(&transaction).unwrap();

        let recovered = load_transaction(&path).unwrap().unwrap();
        rollback_transaction(&recovered).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"old disk");
        assert_eq!(std::fs::read(record_path(&path)).unwrap(), old_record_bytes);
        assert!(load_transaction(&path).unwrap().is_none());
        assert!(!staged.directory.exists());
    }

    #[tokio::test]
    async fn physical_commit_remains_rollback_capable_until_finalization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.qcow2");
        let old_record = record(None, "overlay");
        let old_record_bytes = serde_json::to_vec(&old_record).unwrap();
        std::fs::write(&path, b"old disk").unwrap();
        std::fs::write(record_path(&path), &old_record_bytes).unwrap();
        let desired_record = record(None, "copy");
        let desired_record_bytes = serde_json::to_vec(&desired_record).unwrap();
        let staged = staged_volume(
            &directory.path().join("stage"),
            b"new disk",
            &desired_record_bytes,
        );

        commit_replacement(&staged, &path, &path, &desired_record)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new disk");
        let transaction = load_transaction(&path).unwrap().unwrap();
        rollback_transaction(&transaction).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"old disk");
        assert_eq!(std::fs::read(record_path(&path)).unwrap(), old_record_bytes);
        assert!(load_transaction(&path).unwrap().is_none());
    }

    #[tokio::test]
    async fn recovery_probe_cannot_finalize_before_successor_state_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.qcow2");
        let inputs = json!({
            "dir": directory.path(),
            "size_gb": 10,
            "format": "qcow2",
            "source_mode": "copy",
            "preallocation": "off",
        });
        let desired_record = record_from_inputs(&inputs).unwrap();
        let old_record = desired_record.clone();
        std::fs::write(&path, b"old disk").unwrap();
        std::fs::write(record_path(&path), serde_json::to_vec(&old_record).unwrap()).unwrap();
        let desired_record_bytes = serde_json::to_vec(&desired_record).unwrap();
        let staged = staged_volume(
            &directory.path().join("stage"),
            b"new disk",
            &desired_record_bytes,
        );
        commit_replacement(&staged, &path, &path, &desired_record)
            .await
            .unwrap();

        finalize_committed_transaction(&path, &inputs, false)
            .await
            .unwrap();
        let transaction = load_transaction(&path).unwrap().unwrap();
        assert!(transaction.backup_disk.exists());
        rollback_transaction(&transaction).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old disk");
    }

    #[tokio::test]
    async fn delete_retry_removes_record_left_after_disk_unlink() {
        let directory = tempfile::tempdir().unwrap();
        let urn = Urn::new(TYPE, "delete-crash");
        let transports = TransportPool::default();
        let cx = context(&urn, &transports);
        let inputs = json!({
            "dir": directory.path(),
            "size_gb": 1,
            "format": "qcow2",
            "source_mode": "overlay",
            "preallocation": "off",
        });
        let path = desired_path(&cx, &inputs).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let owned_record_path = record_path(&path);
        std::fs::write(
            &owned_record_path,
            serde_json::to_vec(&record_from_inputs(&inputs).unwrap()).unwrap(),
        )
        .unwrap();

        VolumeHandler
            .delete(&cx, Some(path.to_str().unwrap()), &inputs)
            .await
            .unwrap();
        assert!(!owned_record_path.exists());
    }
}
