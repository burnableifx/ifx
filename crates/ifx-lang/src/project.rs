//! Explicit package scope and offline source loading. Git is only used by `fetch`.
use crate::{
    language::{self, Analysis, Imports},
    syntax::{Diagnostic, MAX_SOURCE, Span},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("project I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("project filesystem: {0}")]
    Fs(#[from] rustix::io::Errno),
    #[error("invalid TOML; inspect the manifest/lockfile syntax and supported fields")]
    Toml(#[from] toml::de::Error),
    #[error("cannot encode lockfile: {0}")]
    Encode(#[from] toml::ser::Error),
}
type Result<T> = std::result::Result<T, Error>;
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: Package,
    #[serde(default)]
    pub modules: BTreeMap<String, String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub entry: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub path: Option<String>,
    pub git: Option<String>,
    pub rev: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    version: u32,
    #[serde(default)]
    packages: BTreeMap<String, Locked>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Locked {
    git: String,
    rev: String,
    files: BTreeMap<String, String>,
}
fn identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(s, "crate" | "self" | "super" | "linode" | "memory")
}
fn relative(s: &str) -> bool {
    !s.contains('\\')
        && s.len() <= 512
        && s.split('/')
            .all(|p| !p.is_empty() && !p.starts_with('.') && !p.chars().any(char::is_control))
}
fn source_path(s: &str) -> bool {
    relative(s) && s.ends_with(".ifx")
}
pub fn parse_manifest(text: &str) -> Result<Manifest> {
    if text.len() > 32 * 1024 {
        return Err(invalid("manifest exceeds 32 KiB"));
    }
    let manifest: Manifest = toml::from_str(text)?;
    if !identifier(&manifest.package.name) || manifest.package.version.is_empty() {
        return Err(invalid(
            "package requires an identifier name and nonempty version",
        ));
    }
    if manifest
        .package
        .entry
        .as_ref()
        .is_some_and(|p| !source_path(p))
    {
        return Err(invalid(
            "package.entry must be a package-relative .ifx path",
        ));
    }
    if manifest.modules.len() > 31 || manifest.dependencies.len() > 8 {
        return Err(invalid("project exceeds 31 exports or 8 dependencies"));
    }
    for (name, path) in &manifest.modules {
        if !name.split("::").all(identifier) || !source_path(path) {
            return Err(invalid(format!(
                "invalid module export `{name}`: use identifiers and package-relative .ifx paths"
            )));
        }
    }
    for (name, dep) in &manifest.dependencies {
        if !identifier(name) {
            return Err(invalid(format!("invalid dependency name `{name}`")));
        }
        match (&dep.path, &dep.git, &dep.rev) {
            (Some(path), None, None) if relative(path) => {}
            (None, Some(git), Some(rev)) => {
                validate_remote(git, rev)?;
            }
            _ => {
                return Err(invalid(format!(
                    "dependency `{name}` requires either a package-relative path or git + full rev"
                )));
            }
        }
    }
    Ok(manifest)
}
fn validate_remote(git: &str, rev: &str) -> Result<()> {
    let url = git
        .strip_prefix("https://")
        .ok_or_else(|| invalid("Git dependencies currently require HTTPS URLs"))?;
    let (host, path) = url
        .split_once('/')
        .ok_or_else(|| invalid("Git URL requires host/repository"))?;
    if host.is_empty()
        || path.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        || git
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '?' | '#' | '\\'))
        || rev.len() != 40
        || !rev
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(invalid(
            "Git dependency requires a credential-free HTTPS URL and 40 lowercase hex commit SHA",
        ));
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn cache_key(git: &str, rev: &str) -> String {
    digest(format!("{git}\n{rev}").as_bytes())
}

/// All components are opened relative to a pinned directory, with no symlink traversal.
pub fn read_at(root: &File, path: &str) -> Result<String> {
    let mut file = root.try_clone()?;
    let parts: Vec<_> = path.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || matches!(*p, "." | "..") || p.contains('\\'))
    {
        return Err(invalid("invalid confined file path"));
    }
    for (index, part) in parts.iter().enumerate() {
        let mut flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        if index + 1 < parts.len() {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        file = File::from(rustix::fs::openat(
            &file,
            *part,
            flags,
            rustix::fs::Mode::empty(),
        )?);
    }
    if !file.metadata()?.is_file() {
        return Err(invalid("source must be a regular file"));
    }
    let mut text = String::new();
    file.take((MAX_SOURCE + 1) as u64)
        .read_to_string(&mut text)?;
    if text.len() > MAX_SOURCE {
        return Err(invalid("file exceeds 256 KiB"));
    }
    Ok(text)
}
pub fn open_root(path: &Path) -> Result<File> {
    Ok(File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?))
}
#[derive(Default)]
pub struct Snapshot {
    pub entry: Option<String>,
    pub sources: BTreeMap<String, String>,
    pub imports: Imports,
    pub paths: BTreeMap<String, PathBuf>,
}
impl Snapshot {
    pub fn analyze(&self, entry: &str) -> Analysis {
        language::analyze_project(entry, &self.sources, &self.imports)
    }
}
/// Load a bounded package snapshot; overlays are already-open editor buffers keyed by absolute path.
pub fn load(root_path: &Path, overlays: &BTreeMap<PathBuf, String>) -> Result<Snapshot> {
    let root_path = root_path.canonicalize()?;
    let root = open_root(&root_path)?;
    load_root(&root_path, root, overlays)
}
fn load_root(
    root_path: &Path,
    root: File,
    overlays: &BTreeMap<PathBuf, String>,
) -> Result<Snapshot> {
    let manifest = parse_manifest(&read_at(&root, "Ifx.toml")?)?;
    let mut snapshot = Snapshot {
        entry: manifest
            .package
            .entry
            .as_ref()
            .map(|p| format!("crate/{p}")),
        ..Snapshot::default()
    };
    let mut packages = BTreeMap::from([("crate".to_string(), (String::new(), manifest))]);
    let root_manifest = &packages["crate"].1;
    let lock = if root_manifest.dependencies.values().any(|d| d.git.is_some()) {
        let text = read_at(&root, "Ifx.lock").map_err(|_| {
            invalid("Git dependencies unavailable: run `ifx-lang fetch --manifest-path Ifx.toml`")
        })?;
        let lock: Lock = toml::from_str(&text)?;
        if lock.version != 1 {
            return Err(invalid("unsupported Ifx.lock version"));
        }
        lock
    } else {
        Lock::default()
    };
    let mut children = Vec::new();
    let mut verified = BTreeMap::new();
    for (alias, dep) in &root_manifest.dependencies {
        let prefix = if let Some(path) = &dep.path {
            format!("{path}/")
        } else {
            let git = dep
                .git
                .as_deref()
                .ok_or_else(|| invalid("missing git URL"))?;
            let rev = dep
                .rev
                .as_deref()
                .ok_or_else(|| invalid("missing Git revision"))?;
            let locked = lock
                .packages
                .get(alias)
                .ok_or_else(|| invalid(format!("missing lock for `{alias}`; run fetch")))?;
            if locked.git != git || locked.rev != rev {
                return Err(invalid(format!("stale lock for `{alias}`; run fetch")));
            }
            format!(".ifx/deps/{}/", cache_key(git, rev))
        };
        let text = read_at(&root, &format!("{prefix}Ifx.toml"))?;
        let child = parse_manifest(&text)?;
        if !child.dependencies.is_empty() {
            return Err(invalid(
                "MVP dependency packages must be self-contained; nested dependencies are not supported yet",
            ));
        }
        if dep.git.is_some() {
            let locked = &lock.packages[alias];
            let expected: std::collections::BTreeSet<_> = child
                .modules
                .values()
                .cloned()
                .chain(std::iter::once("Ifx.toml".into()))
                .collect();
            if expected != locked.files.keys().cloned().collect() {
                return Err(invalid("cached exports do not match lockfile"));
            }
            for (path, hash) in &locked.files {
                if path != "Ifx.toml" && !source_path(path) {
                    return Err(invalid("invalid locked module path"));
                }
                let contents = if path == "Ifx.toml" {
                    text.clone()
                } else {
                    read_at(&root, &format!("{prefix}{path}"))?
                };
                if digest(contents.as_bytes()) != *hash {
                    return Err(invalid(format!(
                        "cached dependency `{alias}` failed integrity check; run fetch"
                    )));
                }
                verified.insert(format!("{prefix}{path}"), contents);
                if verified.values().map(String::len).sum::<usize>() > 1024 * 1024 {
                    return Err(invalid("verified dependency snapshot exceeds 1 MiB"));
                }
            }
        }
        children.push((alias.clone(), (prefix, child)));
    }
    packages.extend(children);
    for (alias, (prefix, package)) in &packages {
        let mut visible: BTreeMap<String, String> = package
            .modules
            .iter()
            .map(|(name, path)| (format!("crate::{name}"), format!("{alias}/{path}")))
            .collect();
        if alias == "crate" {
            for (dep, (_, other)) in packages.iter().filter(|(key, _)| key.as_str() != "crate") {
                visible.extend(
                    other
                        .modules
                        .iter()
                        .map(|(name, path)| (format!("{dep}::{name}"), format!("{dep}/{path}"))),
                );
            }
        }
        let files: std::collections::BTreeSet<_> = package
            .modules
            .values()
            .cloned()
            .chain(
                (alias == "crate")
                    .then(|| package.package.entry.clone())
                    .flatten(),
            )
            .collect();
        for path in files {
            let id = format!("{alias}/{path}");
            let disk = root_path.join(format!("{prefix}{path}"));
            // Always validate the disk entry before applying an unsaved overlay.
            let disk_text = if let Some(text) = verified.get(&format!("{prefix}{path}")) {
                text.clone()
            } else {
                read_at(&root, &format!("{prefix}{path}"))?
            };
            let text = if alias == "crate" || package_is_local(&packages["crate"].1, alias) {
                overlays.get(&disk).cloned().unwrap_or(disk_text)
            } else {
                disk_text
            };
            snapshot.paths.insert(id.clone(), disk);
            snapshot.sources.insert(id.clone(), text);
            snapshot.imports.insert(id, visible.clone());
            if snapshot.sources.len() > 32
                || snapshot.sources.values().map(String::len).sum::<usize>() > 1024 * 1024
            {
                return Err(invalid("project exceeds 32 sources or 1 MiB"));
            }
        }
    }
    Ok(snapshot)
}
fn package_is_local(root: &Manifest, alias: &str) -> bool {
    root.dependencies
        .get(alias)
        .is_some_and(|d| d.path.is_some())
}
pub fn diagnostic(error: Error) -> Analysis {
    Analysis {
        diagnostics: vec![Diagnostic::new(Span::default(), error.to_string())],
        ..Analysis::default()
    }
}

/// Explicit network operation. Dependencies contain data only: no checkout, hooks or build scripts.
pub fn fetch(root_path: &Path) -> Result<()> {
    fetch_packages(root_path, |git, rev| {
        eprintln!("fetching {git} at {rev}");
        let temp = tempfile::tempdir()?;
        git_command(temp.path(), &["init", "--bare", "--template=", "."])?;
        git_command(
            temp.path(),
            &[
                "-c",
                "protocol.https.allow=always",
                "fetch",
                "--depth=1",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                git,
                rev,
            ],
        )?;
        let actual = git_command(temp.path(), &["rev-parse", "FETCH_HEAD^{commit}"])?;
        if actual.trim() != rev {
            return Err(invalid("Git returned a different commit"));
        }
        git_sources(temp.path(), rev)
    })
}
fn fetch_packages(
    root_path: &Path,
    mut resolve: impl FnMut(&str, &str) -> Result<BTreeMap<String, String>>,
) -> Result<()> {
    let root = open_root(root_path)?;
    let manifest = parse_manifest(&read_at(&root, "Ifx.toml")?)?;
    let mut lock = Lock {
        version: 1,
        ..Lock::default()
    };
    for (alias, dep) in manifest.dependencies {
        let Some(git) = dep.git else {
            continue;
        };
        let rev = dep.rev.ok_or_else(|| invalid("missing Git revision"))?;
        let files = resolve(&git, &rev)?;
        let cache = directory_at(&root, ".ifx")?;
        let cache = directory_at(&cache, "deps")?;
        let cache = directory_at(&cache, &cache_key(&git, &rev))?;
        for (path, text) in &files {
            write_at(&cache, path, text.as_bytes())?;
        }
        lock.packages.insert(
            alias,
            Locked {
                git,
                rev,
                files: files
                    .iter()
                    .map(|(p, t)| (p.clone(), digest(t.as_bytes())))
                    .collect(),
            },
        );
    }
    write_at(&root, "Ifx.lock", toml::to_string_pretty(&lock)?.as_bytes())?;
    Ok(())
}
fn directory_at(root: &File, name: &str) -> Result<File> {
    match rustix::fs::mkdirat(root, name, rustix::fs::Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(File::from(rustix::fs::openat(
        root,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?))
}
fn write_at(root: &File, path: &str, bytes: &[u8]) -> Result<()> {
    let mut dir = root.try_clone()?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            dir = directory_at(&dir, part)?;
            continue;
        }
        // Atomic replacement through the directory descriptor, including when an old leaf is a symlink.
        let temporary = format!(".ifx-write-{}", std::process::id());
        let file = rustix::fs::openat(
            &dir,
            &temporary,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        let result = (|| -> Result<()> {
            let mut file = File::from(file);
            file.write_all(bytes)?;
            file.sync_all()?;
            rustix::fs::renameat(&dir, &temporary, &dir, part)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&dir, &temporary, rustix::fs::AtFlags::empty());
        }
        result?;
    }
    Ok(())
}
fn git_sources(repo: &Path, rev: &str) -> Result<BTreeMap<String, String>> {
    let tree = git_command(repo, &["ls-tree", "-r", "-z", rev])?;
    let mut objects = BTreeMap::new();
    for record in tree.split('\0').filter(|s| !s.is_empty()) {
        let Some((meta, path)) = record.split_once('\t') else {
            return Err(invalid("invalid Git tree"));
        };
        let parts: Vec<_> = meta.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(invalid("invalid Git object metadata"));
        }
        objects.insert(path, (parts[0], parts[1], parts[2]));
    }
    let read = |path: &str| -> Result<String> {
        let Some(("100644", "blob", hash)) = objects.get(path).copied() else {
            return Err(invalid(format!(
                "export `{path}` must be a regular non-executable Git file"
            )));
        };
        git_command(repo, &["cat-file", "blob", hash])
    };
    let text = read("Ifx.toml")?;
    let manifest = parse_manifest(&text)?;
    if !manifest.dependencies.is_empty() {
        return Err(invalid("MVP dependency packages must be self-contained"));
    }
    let mut files = BTreeMap::from([("Ifx.toml".into(), text)]);
    for path in manifest.modules.values() {
        let text = read(path)?;
        files.insert(path.clone(), text);
        if files.values().map(String::len).sum::<usize>() > 1024 * 1024 {
            return Err(invalid("Git package exceeds 1 MiB"));
        }
    }
    Ok(files)
}
fn git_command(repo: &Path, args: &[&str]) -> Result<String> {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    // prlimit sets inherited limits before Git can spawn helpers. Configuration, helpers,
    // URL rewrites and redirects are disabled; only the fetch call enables HTTPS.
    let mut command = Command::new("/usr/bin/prlimit");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "--fsize=67108864",
            "--as=536870912",
            "--cpu=30",
            "--nofile=64",
            "--",
            "/usr/bin/git",
        ])
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "credential.helper=",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.maxRequests=1",
            "-c",
            "fetch.unpackLimit=1",
        ])
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    run_command(&mut command, repo, std::time::Duration::from_secs(60))
}
struct Running(std::process::Child);
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.wait();
    }
}
fn run_command(
    command: &mut std::process::Command,
    repo: &Path,
    deadline: std::time::Duration,
) -> Result<String> {
    use std::time::{Duration, Instant};
    let mut running = Running(command.spawn()?);
    let stdout = running
        .0
        .stdout
        .take()
        .ok_or_else(|| invalid("missing Git output pipe"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((MAX_SOURCE + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let start = Instant::now();
    let mut output = None;
    loop {
        if let Ok(result) = receiver.try_recv() {
            let bytes = result?;
            if bytes.len() > MAX_SOURCE {
                return Err(invalid("Git output exceeds 256 KiB"));
            }
            output = Some(bytes);
        }
        check_storage(repo)?;
        let status = running.0.try_wait()?;
        if let (Some(status), Some(bytes)) = (status, output.as_ref()) {
            if !status.success() {
                return Err(invalid(
                    "Git command failed (check HTTPS access, commit and fetch resource limits)",
                ));
            }
            return String::from_utf8(bytes.clone())
                .map_err(|_| invalid("Git package must use UTF-8 paths and sources"));
        }
        if start.elapsed() >= deadline {
            return Err(invalid("Git command exceeded its deadline"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn check_storage(repo: &Path) -> Result<()> {
    let mut pending = vec![repo.to_path_buf()];
    let mut count = 0;
    let mut size = 0u64;
    while let Some(path) = pending.pop() {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let meta = match entry.path().symlink_metadata() {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            count += 1;
            size = size.saturating_add(meta.len());
            if count > 1024 || size > 64 * 1024 * 1024 {
                return Err(invalid(
                    "Git repository exceeds 1024 entries or 64 MiB storage budget",
                ));
            }
            if meta.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Discover and load a project through the initialized workspace descriptor. No filesystem
/// probe follows a document-directory symlink, including during nearest-manifest discovery.
pub fn load_in_workspace(
    start: &Path,
    boundary: &Path,
    overlays: &BTreeMap<PathBuf, String>,
) -> Result<Option<Snapshot>> {
    let base = open_root(boundary)?;
    for path in start
        .ancestors()
        .take(32)
        .take_while(|p| p.starts_with(boundary))
    {
        let relative = path
            .strip_prefix(boundary)
            .map_err(|_| invalid("project outside workspace"))?;
        let mut directory = base.try_clone()?;
        for part in relative.components() {
            let std::path::Component::Normal(part) = part else {
                return Err(invalid("invalid workspace path"));
            };
            directory = File::from(rustix::fs::openat(
                &directory,
                part,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?);
        }
        match rustix::fs::statat(
            &directory,
            "Ifx.toml",
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => return load_root(path, directory, overlays).map(Some),
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

/// Find the closest manifest, optionally bounded by an explicitly initialized editor workspace.
pub fn find_root(start: &Path, boundary: Option<&Path>) -> Option<PathBuf> {
    start
        .ancestors()
        .take(32)
        .take_while(|p| boundary.is_none_or(|root| p.starts_with(root)))
        .find(|p| std::fs::symlink_metadata(p.join("Ifx.toml")).is_ok())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Component tier. Real local Git objects exercise the fetch/extraction boundary; no network.
    fn repository() -> (tempfile::TempDir, String) {
        let repo = tempfile::tempdir().unwrap();
        git_command(repo.path(), &["init", "--template=", "."]).unwrap();
        std::fs::write(
            repo.path().join("Ifx.toml"),
            "[package]\nname = 'shared'\nversion = '1'\n[modules]\nweb = 'web.ifx'\n",
        )
        .unwrap();
        std::fs::write(repo.path().join("web.ifx"), "output value: Int = 23;").unwrap();
        std::fs::write(
            repo.path().join("unexported.ifx"),
            "unsupported secret-shaped source",
        )
        .unwrap();
        git_command(repo.path(), &["add", "--all"]).unwrap();
        git_command(
            repo.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        let rev = git_command(repo.path(), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        (repo, rev)
    }
    fn consumer(rev: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Ifx.toml"), format!("[package]\nname = 'app'\nversion = '1'\nentry = 'main.ifx'\n[dependencies]\nshared = {{ git = 'https://example.invalid/modules', rev = '{rev}' }}\n")).unwrap();
        std::fs::write(
            root.path().join("main.ifx"),
            "use shared::web; module app = web(\"app\"); output value: Int = app.value;",
        )
        .unwrap();
        root
    }
    #[test]
    fn pinned_git_objects_round_trip_through_lock_cache_and_offline_compilation() {
        let (repo, rev) = repository();
        let root = consumer(&rev);
        let files = git_sources(repo.path(), &rev).unwrap();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["Ifx.toml", "web.ifx"],
            "only manifest exports may enter the cache"
        );
        fetch_packages(root.path(), |_, revision| {
            git_sources(repo.path(), revision)
        })
        .unwrap();
        let locked = std::fs::read(root.path().join("Ifx.lock")).unwrap();
        let snapshot = load(root.path(), &BTreeMap::new()).unwrap();
        let compiled = snapshot
            .analyze(snapshot.entry.as_ref().unwrap())
            .compilation
            .unwrap();
        assert_eq!(compiled.outputs["value"], 23);
        let cache_file = &snapshot.paths["shared/web.ifx"];
        std::fs::write(cache_file, "output value: Int = 99;").unwrap();
        assert!(
            load(root.path(), &BTreeMap::new())
                .err()
                .unwrap()
                .to_string()
                .contains("integrity")
        );
        fetch_packages(root.path(), |_, revision| {
            git_sources(repo.path(), revision)
        })
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("Ifx.lock")).unwrap(),
            locked,
            "same commit must yield the same lock bytes"
        );
        let failed = fetch_packages(root.path(), |_, _| Err(invalid("unavailable repository")));
        assert!(failed.is_err());
        assert_eq!(
            std::fs::read(root.path().join("Ifx.lock")).unwrap(),
            locked,
            "failed fetch must preserve the previous lock"
        );
        let manifest = std::fs::read_to_string(root.path().join("Ifx.toml")).unwrap();
        std::fs::write(
            root.path().join("Ifx.toml"),
            manifest.replace(&rev, &"0".repeat(40)),
        )
        .unwrap();
        assert!(
            load(root.path(), &BTreeMap::new())
                .err()
                .unwrap()
                .to_string()
                .contains("stale lock")
        );
    }
    #[test]
    fn git_exports_reject_symlinks_and_cache_writes_cannot_follow_directory_links() {
        let (repo, rev) = repository();
        let root = consumer(&rev);
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".ifx")).unwrap();
        assert!(
            fetch_packages(root.path(), |_, revision| git_sources(
                repo.path(),
                revision
            ))
            .is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_file(repo.path().join("web.ifx")).unwrap();
        std::os::unix::fs::symlink("unexported.ifx", repo.path().join("web.ifx")).unwrap();
        git_command(repo.path(), &["add", "--all"]).unwrap();
        git_command(
            repo.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "symlink",
            ],
        )
        .unwrap();
        let rev = git_command(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        assert!(
            git_sources(repo.path(), rev.trim())
                .unwrap_err()
                .to_string()
                .contains("regular non-executable")
        );
    }
    #[test]
    fn git_runner_enforces_storage_and_output_budgets() {
        let root = tempfile::tempdir().unwrap();
        let big = File::create(root.path().join("oversized")).unwrap();
        big.set_len(64 * 1024 * 1024 + 1).unwrap();
        assert!(
            git_command(root.path(), &["--version"])
                .unwrap_err()
                .to_string()
                .contains("storage budget")
        );
        big.set_len(0).unwrap();
        std::fs::write(root.path().join("oversized"), "x".repeat(MAX_SOURCE + 1)).unwrap();
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/cat");
        command
            .arg(root.path().join("oversized"))
            .stdout(std::process::Stdio::piped())
            .process_group(0);
        assert!(
            run_command(&mut command, root.path(), std::time::Duration::from_secs(1))
                .unwrap_err()
                .to_string()
                .contains("output exceeds")
        );
    }
    #[test]
    fn deadline_kills_helpers_even_after_the_parent_exits_with_a_held_output_pipe() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        // FIFO handshake proves the helper exists before its parent exits; no timing sleep.
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "mkfifo ready blocked; /bin/sh -c 'echo $$ > helper.pid; echo ready > ready; exec /bin/cat blocked' & read ready < ready; exit 0"])
            .current_dir(root.path()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).process_group(0);
        let error = run_command(
            &mut command,
            root.path(),
            std::time::Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.to_string().contains("deadline"));
        let pid = std::fs::read_to_string(root.path().join("helper.pid")).unwrap();
        // kill(2) queues SIGKILL synchronously; scheduling/reaping is asynchronous.
        // The signal must either remain pending or have already stopped the helper.
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", pid.trim())) {
            let stopped = status.lines().any(|line| {
                line.starts_with("State:")
                    && (line.contains("Z (zombie)") || line.contains("X (dead)"))
            });
            let pending_kill = status
                .lines()
                .filter(|line| line.starts_with("SigPnd:") || line.starts_with("ShdPnd:"))
                .filter_map(|line| u64::from_str_radix(line.split_whitespace().nth(1)?, 16).ok())
                .any(|bits| bits & (1 << 8) != 0);
            assert!(
                stopped || pending_kill,
                "helper must have received SIGKILL after the deadline: {status}"
            );
        }
    }
}
