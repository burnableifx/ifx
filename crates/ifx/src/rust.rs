//! Rust stack programs compile and run as lightweight Program emitters.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

use anyhow::Context as _;
use fs2::FileExt;
use sha2::{Digest as _, Sha256};

use crate::cli::LoadCtx;
use crate::model::Program;

const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const GC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_VERSION: &str = "shared-target-v2\n";

/// Shared-cache compiler used by `ifxd` to build a stack independently from emitting
/// its configured Program.
#[derive(Clone, Debug)]
pub struct RustCompiler {
    cache_root: PathBuf,
    runtimes: Arc<Mutex<BTreeMap<RuntimeKey, RustRuntime>>>,
    emitters: Arc<Mutex<BTreeMap<EmitterKey, EmitterTarget>>>,
}

impl RustCompiler {
    pub fn from_environment() -> anyhow::Result<Self> {
        Ok(Self {
            cache_root: cache_root_with(|key| std::env::var_os(key))?,
            runtimes: Arc::new(Mutex::new(BTreeMap::new())),
            emitters: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    #[cfg(test)]
    fn new(cache_root: PathBuf) -> Self {
        Self {
            cache_root,
            runtimes: Arc::new(Mutex::new(BTreeMap::new())),
            emitters: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn build(&self, manifest: &Path, current_dir: &Path) -> anyhow::Result<CompiledStack> {
        let manifest = manifest
            .canonicalize()
            .with_context(|| format!("resolving Rust stack manifest {}", manifest.display()))?;
        let early_roots = vec![current_dir.to_path_buf()];
        let early_inputs = early_input_candidates(&manifest, current_dir);
        let early_fingerprint = source_fingerprint(&early_roots, &early_inputs)?;
        let cache = RustCache::new(self.cache_root.clone());
        let guard = cache.prepare()?;
        let runtime = self.runtime(current_dir)?;
        ensure_local_cargo_config(current_dir, &runtime.host)?;
        let emitter_key = EmitterKey {
            manifest: manifest.clone(),
            current_dir: current_dir.to_path_buf(),
        };
        let previous = self
            .emitters
            .lock()
            .map_err(|_| anyhow::anyhow!("Rust emitter cache lock poisoned"))?
            .get(&emitter_key)
            .cloned();
        let previous_fingerprint = previous.as_ref().and_then(|emitter| {
            source_fingerprint(&emitter.source_roots, &emitter.source_inputs).ok()
        });
        let emitter = resolve_emitter(&manifest, current_dir, guard.target_dir())?;
        if let (Some(previous), Some(before)) = (&previous, previous_fingerprint) {
            let after = source_fingerprint(&previous.source_roots, &previous.source_inputs)?;
            if after != before {
                return Err(SourcesChanged.into());
            }
        }
        let fingerprint = source_fingerprint(&emitter.source_roots, &emitter.source_inputs)?;
        let compiled = compile_emitter(&manifest, current_dir, &emitter, guard.target_dir())?;
        let executable = materialize_emitter(&compiled, &manifest, current_dir, &runtime)?;
        let after = source_fingerprint(&emitter.source_roots, &emitter.source_inputs)?;
        let confirmed_runtime = self.runtime(current_dir)?;
        let needs_confirmation = previous_fingerprint.is_none();
        let confirmed_emitter = needs_confirmation
            .then(|| resolve_emitter(&manifest, current_dir, guard.target_dir()))
            .transpose()?;
        let confirmed_fingerprint = confirmed_emitter
            .as_ref()
            .map(|confirmed| source_fingerprint(&confirmed.source_roots, &confirmed.source_inputs))
            .transpose()?;
        let early_after = source_fingerprint(&early_roots, &early_inputs)?;
        if after != fingerprint
            || confirmed_fingerprint.is_some_and(|confirmed| confirmed != after)
            || confirmed_emitter
                .as_ref()
                .is_some_and(|confirmed| confirmed != &emitter)
            || confirmed_runtime != runtime
            || early_after != early_fingerprint
        {
            return Err(SourcesChanged.into());
        }
        self.emitters
            .lock()
            .map_err(|_| anyhow::anyhow!("Rust emitter cache lock poisoned"))?
            .insert(emitter_key, emitter.clone());
        Ok(CompiledStack {
            manifest,
            executable,
            runtime,
            source_roots: emitter.source_roots,
            source_inputs: emitter.source_inputs,
            fingerprint,
            cache_root: self.cache_root.clone(),
        })
    }

    fn runtime(&self, current_dir: &Path) -> anyhow::Result<RustRuntime> {
        let key = RuntimeKey {
            current_dir: current_dir.to_path_buf(),
            selector_fingerprint: source_fingerprint(&[], &rust_toolchain_candidates(current_dir))?,
            rustc: std::env::var_os("RUSTC"),
        };
        let cached = self
            .runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("Rust runtime cache lock poisoned"))?
            .get(&key)
            .cloned();
        if let Some(runtime) = cached {
            return Ok(runtime);
        }
        let runtime = RustRuntime::discover(current_dir)?;
        self.runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("Rust runtime cache lock poisoned"))?
            .insert(key, runtime.clone());
        Ok(runtime)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RuntimeKey {
    current_dir: PathBuf,
    selector_fingerprint: u64,
    rustc: Option<OsString>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EmitterKey {
    manifest: PathBuf,
    current_dir: PathBuf,
}

/// A source generation changed while Cargo was reading or compiling it.
#[derive(Debug)]
pub struct SourcesChanged;

impl std::fmt::Display for SourcesChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Rust stack sources changed while compiling")
    }
}

impl std::error::Error for SourcesChanged {}

fn compile_emitter(
    manifest: &Path,
    current_dir: &Path,
    emitter: &EmitterTarget,
    target_dir: &Path,
) -> anyhow::Result<CompiledEmitter> {
    let suffix = emitter_suffix(manifest);

    let mut command = Command::new("cargo");
    command
        .args(["rustc", "--quiet", "--locked", "--manifest-path"])
        .arg(manifest)
        .arg("--bin")
        .arg(&emitter.name)
        .arg("--message-format=json-render-diagnostics")
        .args(["--", "-C"])
        .arg(format!("extra-filename={suffix}"))
        .current_dir(current_dir)
        .env("CARGO_TARGET_DIR", target_dir);
    if std::env::var_os("CARGO_PROFILE_DEV_DEBUG").is_none() {
        command.env("CARGO_PROFILE_DEV_DEBUG", "0");
    }
    let compiled = command.output().with_context(|| {
        format!(
            "compiling Rust stack {} (is `cargo` on PATH?)",
            manifest.display()
        )
    })?;
    let messages = String::from_utf8_lossy(&compiled.stdout);
    let stderr = String::from_utf8_lossy(&compiled.stderr);
    let diagnostics = cargo_diagnostics(&messages);
    if !compiled.status.success() {
        anyhow::bail!(
            "loading {}: `cargo rustc` exited {}\n{}{}",
            manifest.display(),
            compiled
                .status
                .code()
                .map_or("by signal".to_string(), |code| code.to_string()),
            diagnostics,
            stderr.trim_end()
        );
    }
    if !diagnostics.is_empty() {
        eprint!("{diagnostics}");
    }
    if !stderr.trim().is_empty() {
        eprint!("{stderr}");
    }
    compiled_executable(&messages, manifest, emitter, &suffix)
}

/// One successfully compiled stack emitter. Emission applies configuration without
/// invoking Cargo again.
#[derive(Clone, Debug)]
pub struct CompiledStack {
    manifest: PathBuf,
    executable: CompiledEmitter,
    runtime: RustRuntime,
    source_roots: Vec<PathBuf>,
    source_inputs: Vec<PathBuf>,
    fingerprint: u64,
    cache_root: PathBuf,
}

impl CompiledStack {
    pub fn source_roots(&self) -> &[PathBuf] {
        &self.source_roots
    }

    pub fn source_inputs(&self) -> &[PathBuf] {
        &self.source_inputs
    }

    pub fn source_fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn emit(&self, ctx: &LoadCtx) -> anyhow::Result<Program> {
        // Builds and cache collection are exclusive. Holding the shared lock through
        // process exit also protects the per-stack runtime artifact from replacement.
        let _cache = RustCache::new(self.cache_root.clone()).access_shared()?;
        let config = serde_json::to_string(&ctx.config)?;

        let mut command = Command::new(&self.executable.path);
        self.runtime
            .configure(&mut command, &self.executable.profile)?;
        let emitted = command
            .current_dir(&ctx.dir)
            .env("IFX_STACK", &ctx.stack)
            .env("IFX_CONFIG", config)
            .output()
            .with_context(|| format!("running Rust stack emitter {}", self.executable.display()))?;
        let stderr = String::from_utf8_lossy(&emitted.stderr);
        if !emitted.status.success() {
            anyhow::bail!(
                "loading {}: emitter exited {}\n{}",
                self.manifest.display(),
                emitted
                    .status
                    .code()
                    .map_or("by signal".to_string(), |code| code.to_string()),
                stderr.trim_end()
            );
        }
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        parse_program(&String::from_utf8_lossy(&emitted.stdout), &self.manifest)
    }
}

fn emitter_suffix(manifest: &Path) -> String {
    format!(
        "-ifx-{}",
        hex::encode(Sha256::digest(manifest.as_os_str().as_encoded_bytes()))
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EmitterTarget {
    name: String,
    target_directory: PathBuf,
    source_roots: Vec<PathBuf>,
    source_inputs: Vec<PathBuf>,
}

fn resolve_emitter(
    manifest: &Path,
    current_dir: &Path,
    target_directory: &Path,
) -> anyhow::Result<EmitterTarget> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--quiet",
            "--locked",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(manifest)
        .current_dir(current_dir)
        .env("CARGO_TARGET_DIR", target_directory)
        .output()
        .with_context(|| format!("reading Rust stack metadata {}", manifest.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "reading Rust stack metadata {}: `cargo metadata` exited {}\n{}",
        manifest.display(),
        output
            .status
            .code()
            .map_or("by signal".to_string(), |code| code.to_string()),
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parsing Rust stack metadata {}", manifest.display()))?;
    let package = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            packages.iter().find(|package| {
                package["manifest_path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == manifest)
            })
        })
        .with_context(|| {
            format!(
                "Cargo metadata omitted stack package {}",
                manifest.display()
            )
        })?;
    let bins = package["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter_map(|target| target["name"].as_str())
        .collect::<Vec<_>>();
    let name = match package["default_run"].as_str() {
        Some(default) if bins.contains(&default) => default,
        Some(default) => anyhow::bail!(
            "Rust stack {} sets default-run to unknown binary `{default}`",
            manifest.display()
        ),
        None if bins.len() == 1 => bins[0],
        None if bins.is_empty() => {
            anyhow::bail!("Rust stack {} has no binary emitter", manifest.display())
        }
        None => anyhow::bail!(
            "Rust stack {} has multiple binaries; set package.default-run to the emitter",
            manifest.display()
        ),
    };
    let target_directory = metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .context("Cargo metadata has no target directory")?;
    let mut source_roots = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|package| package["source"].is_null())
        .filter_map(|package| package["manifest_path"].as_str())
        .filter_map(|path| Path::new(path).parent())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    source_roots.push(current_dir.to_path_buf());
    let workspace_root = metadata["workspace_root"].as_str().map(Path::new);
    source_roots.sort();
    source_roots.dedup();
    let source_inputs = cargo_input_files(current_dir, workspace_root);
    Ok(EmitterTarget {
        name: name.to_string(),
        target_directory,
        source_roots,
        source_inputs,
    })
}

fn cargo_input_files(current_dir: &Path, workspace_root: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = cargo_config_candidates(current_dir);
    if let Some(root) = workspace_root {
        for name in ["Cargo.toml", "Cargo.lock"] {
            paths.push(root.join(name));
        }
    }
    paths.extend(rust_toolchain_candidates(current_dir));
    paths.sort();
    paths.dedup();
    paths
}

fn early_input_candidates(manifest: &Path, current_dir: &Path) -> Vec<PathBuf> {
    let mut paths = cargo_config_candidates(current_dir);
    paths.extend(rust_toolchain_candidates(current_dir));
    paths.push(manifest.to_path_buf());
    for ancestor in manifest.parent().into_iter().flat_map(Path::ancestors) {
        paths.push(ancestor.join("Cargo.toml"));
        paths.push(ancestor.join("Cargo.lock"));
    }
    paths.sort();
    paths.dedup();
    paths
}

fn rust_toolchain_candidates(current_dir: &Path) -> Vec<PathBuf> {
    current_dir
        .ancestors()
        .flat_map(|ancestor| {
            ["rust-toolchain.toml", "rust-toolchain"]
                .into_iter()
                .map(move |name| ancestor.join(name))
        })
        .collect()
}

#[derive(Clone, Debug)]
struct CompiledEmitter {
    path: PathBuf,
    profile: PathBuf,
}

impl CompiledEmitter {
    fn display(&self) -> std::path::Display<'_> {
        self.path.display()
    }
}

fn compiled_executable(
    messages: &str,
    manifest: &Path,
    emitter: &EmitterTarget,
    suffix: &str,
) -> anyhow::Result<CompiledEmitter> {
    let artifact = messages
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| {
            message["reason"] == "compiler-artifact"
                && message["manifest_path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == manifest)
                && message["target"]["name"] == emitter.name
                && message["target"]["kind"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .with_context(|| format!("Cargo reported no binary target for {}", manifest.display()))?;
    let target_name = artifact["target"]["name"]
        .as_str()
        .context("Cargo binary target has no name")?;
    let advertised = artifact["executable"]
        .as_str()
        .context("Cargo binary target has no executable path")?;
    let profile = Path::new(advertised)
        .parent()
        .context("Cargo executable path has no profile directory")?
        .to_path_buf();
    anyhow::ensure!(
        profile.parent() == Some(emitter.target_directory.as_path()),
        "Rust stack {} is configured for a non-host Cargo target; remove CARGO_BUILD_TARGET or build.target",
        manifest.display()
    );
    let executable = profile.join("deps").join(format!(
        "{}{suffix}{}",
        target_name.replace('-', "_"),
        std::env::consts::EXE_SUFFIX
    ));
    anyhow::ensure!(
        executable.is_file(),
        "Cargo did not produce the expected isolated emitter {}",
        executable.display()
    );
    Ok(CompiledEmitter {
        path: executable,
        profile,
    })
}

fn materialize_emitter(
    compiled: &CompiledEmitter,
    manifest: &Path,
    current_dir: &Path,
    runtime: &RustRuntime,
) -> anyhow::Result<CompiledEmitter> {
    let key = hex::encode(Sha256::digest(manifest.as_os_str().as_encoded_bytes()));
    let parent = current_dir.join(".ifx/build/rust");
    fs::create_dir_all(&parent)
        .with_context(|| format!("creating Rust runtime directory {}", parent.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let staging = parent.join(format!(".{key}.tmp-{}-{nonce}", std::process::id()));
    let destination = parent.join(&key);
    let backup = parent.join(format!(".{key}.old-{}-{nonce}", std::process::id()));
    let staging_deps = staging.join("deps");
    fs::create_dir_all(&staging_deps)
        .with_context(|| format!("creating Rust runtime staging area {}", staging.display()))?;

    let executable_name = compiled
        .path
        .file_name()
        .context("compiled Rust emitter has no file name")?;
    fs::copy(&compiled.path, staging_deps.join(executable_name)).with_context(|| {
        format!(
            "copying Rust emitter {} into its stable runtime",
            compiled.display()
        )
    })?;
    copy_linked_dynamic_libraries(compiled, runtime, &staging_deps)?;

    if destination.exists() {
        fs::rename(&destination, &backup).with_context(|| {
            format!("preserving previous Rust runtime {}", destination.display())
        })?;
    }
    if let Err(error) = fs::rename(&staging, &destination) {
        if backup.exists() {
            let _ = fs::rename(&backup, &destination);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(error)
            .with_context(|| format!("installing Rust runtime {}", destination.display()));
    }
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("removing replaced Rust runtime {}", backup.display()))?;
    }

    Ok(CompiledEmitter {
        path: destination.join("deps").join(executable_name),
        profile: destination,
    })
}

fn copy_linked_dynamic_libraries(
    compiled: &CompiledEmitter,
    runtime: &RustRuntime,
    destination: &Path,
) -> anyhow::Result<()> {
    if let Some(paths) = linked_runtime_paths(&compiled.path, runtime, &compiled.profile)? {
        for path in paths {
            if !path.starts_with(&compiled.profile) {
                continue;
            }
            let name = path
                .file_name()
                .context("linked Rust runtime library has no file name")?;
            fs::copy(&path, destination.join(name))
                .with_context(|| format!("copying Rust runtime library {}", path.display()))?;
        }
        return Ok(());
    }

    let deps = compiled.profile.join("deps");
    let executable = fs::read(&compiled.path)
        .with_context(|| format!("reading compiled Rust emitter {}", compiled.display()))?;
    let mut pending = linked_library_names(&executable)
        .into_iter()
        .collect::<Vec<_>>();
    let mut copied = std::collections::BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !copied.insert(name.clone()) {
            continue;
        }
        let Some(path) = [compiled.profile.join(&name), deps.join(&name)]
            .into_iter()
            .find(|path| path.is_file())
        else {
            continue;
        };
        let contents = fs::read(&path)
            .with_context(|| format!("reading Rust runtime library {}", path.display()))?;
        fs::copy(&path, destination.join(&name))
            .with_context(|| format!("copying Rust runtime library {}", path.display()))?;
        pending.extend(linked_library_names(&contents));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linked_runtime_paths(
    executable: &Path,
    runtime: &RustRuntime,
    profile: &Path,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let mut command = Command::new("ldd");
    command.arg(executable);
    runtime.configure(&mut command, profile)?;
    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting Rust emitter {}", executable.display()));
        }
    };
    if !output.status.success() {
        return Ok(None);
    }
    let paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            match fields.as_slice() {
                [_, "=>", path, ..] if path.starts_with('/') => Some(PathBuf::from(path)),
                [path, ..] if path.starts_with('/') => Some(PathBuf::from(path)),
                _ => None,
            }
        })
        .collect();
    Ok(Some(paths))
}

#[cfg(not(target_os = "linux"))]
fn linked_runtime_paths(
    _executable: &Path,
    _runtime: &RustRuntime,
    _profile: &Path,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    Ok(None)
}

fn linked_library_names(object: &[u8]) -> std::collections::BTreeSet<String> {
    object
        .split(|byte| !byte.is_ascii_graphic())
        .filter_map(|bytes| std::str::from_utf8(bytes).ok())
        .filter_map(|path| path.rsplit(['/', '\\']).next())
        .filter(|name| is_dynamic_library(std::ffi::OsStr::new(name)))
        .map(str::to_ascii_lowercase)
        .collect()
}

fn is_dynamic_library(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".so")
        || name.contains(".so.")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RustRuntime {
    host: String,
    target_libdir: PathBuf,
}

impl RustRuntime {
    fn discover(current_dir: &Path) -> anyhow::Result<Self> {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let output = Command::new(&rustc)
            .args(["--print", "target-libdir"])
            .current_dir(current_dir)
            .output()
            .with_context(|| {
                format!("querying Rust runtime with {}", Path::new(&rustc).display())
            })?;
        anyhow::ensure!(
            output.status.success(),
            "querying Rust runtime: rustc exited {}\n{}",
            output
                .status
                .code()
                .map_or("by signal".to_string(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
        let target_libdir = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        let host = target_libdir
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .context("rustc target library directory contains no host triple")?
            .to_string();
        Ok(Self {
            host,
            target_libdir,
        })
    }

    fn configure(&self, command: &mut Command, profile: &Path) -> anyhow::Result<()> {
        let variable = dynamic_library_path_variable();
        let mut paths = vec![
            profile.join("deps"),
            profile.to_path_buf(),
            self.target_libdir.clone(),
        ];
        if let Some(existing) = std::env::var_os(variable) {
            paths.extend(std::env::split_paths(&existing));
        }
        command.env(
            variable,
            std::env::join_paths(paths).context("constructing Rust dynamic library path")?,
        );
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn dynamic_library_path_variable() -> &'static str {
    "PATH"
}

#[cfg(target_os = "macos")]
fn dynamic_library_path_variable() -> &'static str {
    "DYLD_FALLBACK_LIBRARY_PATH"
}

#[cfg(target_os = "aix")]
fn dynamic_library_path_variable() -> &'static str {
    "LIBPATH"
}

#[cfg(target_os = "haiku")]
fn dynamic_library_path_variable() -> &'static str {
    "LIBRARY_PATH"
}

#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "aix",
    target_os = "haiku"
)))]
fn dynamic_library_path_variable() -> &'static str {
    "LD_LIBRARY_PATH"
}

fn ensure_local_cargo_config(current_dir: &Path, host: &str) -> anyhow::Result<()> {
    if std::env::var_os("CARGO_BUILD_TARGET").is_some() {
        anyhow::bail!(
            "Rust stacks execute on the IFX client host; remove CARGO_BUILD_TARGET before loading this stack"
        );
    }
    let runner_env = format!(
        "CARGO_TARGET_{}_RUNNER",
        host.to_ascii_uppercase().replace('-', "_")
    );
    if std::env::var_os(&runner_env).is_some() {
        anyhow::bail!(
            "Rust stack emitters do not support Cargo target runners; remove {runner_env} before loading this stack"
        );
    }
    for path in cargo_config_paths(current_dir) {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading Cargo configuration {}", path.display()))?;
        let config: toml::Value = toml::from_str(&text)
            .with_context(|| format!("parsing Cargo configuration {}", path.display()))?;
        if config
            .get("build")
            .and_then(|build| build.get("target"))
            .is_some()
        {
            anyhow::bail!(
                "Rust stacks execute on the IFX client host; remove build.target from {}",
                path.display()
            );
        }
        let Some(targets) = config.get("target").and_then(toml::Value::as_table) else {
            continue;
        };
        let configured_runner = targets.iter().find(|(target, settings)| {
            (*target == host || target.starts_with("cfg(")) && settings.get("runner").is_some()
        });
        if let Some((target, _)) = configured_runner {
            anyhow::bail!(
                "Rust stack emitters do not support Cargo target runner `{target}` from {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn cargo_config_paths(current_dir: &Path) -> Vec<PathBuf> {
    cargo_config_candidates(current_dir)
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

fn cargo_config_candidates(current_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for ancestor in current_dir.ancestors() {
        add_config_paths(&mut paths, &ancestor.join(".cargo"));
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        add_config_paths(&mut paths, &cargo_home);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn add_config_paths(paths: &mut Vec<PathBuf>, directory: &Path) {
    for name in ["config.toml", "config"] {
        paths.push(directory.join(name));
    }
}

/// Fingerprint every required source root and optional Cargo/toolchain input.
/// Missing optional inputs are part of the snapshot so later creation is observable.
pub fn source_fingerprint(roots: &[PathBuf], inputs: &[PathBuf]) -> anyhow::Result<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut visited = std::collections::BTreeSet::new();
    for root in roots {
        fingerprint_path(root, root, &mut hasher, &mut visited)?;
    }
    for input in inputs {
        input.hash(&mut hasher);
        match fs::symlink_metadata(input) {
            Ok(_) => {
                true.hash(&mut hasher);
                fingerprint_path(input, input, &mut hasher, &mut visited)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                false.hash(&mut hasher);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading source input {}", input.display()));
            }
        }
    }
    Ok(hasher.finish())
}

fn fingerprint_path(
    root: &Path,
    path: &Path,
    hasher: &mut impl std::hash::Hasher,
    visited: &mut std::collections::BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading source metadata {}", path.display()))?;
    path.strip_prefix(root).unwrap_or(path).hash(hasher);
    if metadata.file_type().is_symlink() {
        fingerprint_metadata(&metadata, hasher);
        fs::read_link(path)
            .with_context(|| format!("reading source symlink {}", path.display()))?
            .hash(hasher);
        let target = fs::metadata(path)
            .with_context(|| format!("reading source symlink target {}", path.display()))?;
        fingerprint_metadata(&target, hasher);
        #[cfg(not(unix))]
        if target.is_file() {
            fingerprint_contents(path, hasher)?;
        }
        if target.is_dir() {
            let target = path
                .canonicalize()
                .with_context(|| format!("resolving source symlink target {}", path.display()))?;
            target.hash(hasher);
            fingerprint_path(&target, &target, hasher, visited)?;
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        fingerprint_metadata(&metadata, hasher);
        #[cfg(not(unix))]
        if metadata.is_file() {
            fingerprint_contents(path, hasher)?;
        }
        return Ok(());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving source directory {}", path.display()))?;
    if !visited.insert(canonical) {
        return Ok(());
    }
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("reading source directory {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | ".ifx" | "node_modules" | "target")
        ) {
            continue;
        }
        fingerprint_path(root, &entry.path(), hasher, visited)?;
    }
    Ok(())
}

fn fingerprint_metadata(metadata: &fs::Metadata, hasher: &mut impl std::hash::Hasher) {
    metadata.len().hash(hasher);
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .hash(hasher);
    #[cfg(unix)]
    {
        metadata.dev().hash(hasher);
        metadata.ino().hash(hasher);
        metadata.ctime().hash(hasher);
        metadata.ctime_nsec().hash(hasher);
    }
}

#[cfg(not(unix))]
fn fingerprint_contents(path: &Path, hasher: &mut impl std::hash::Hasher) -> anyhow::Result<()> {
    fs::read(path)
        .with_context(|| format!("reading source contents {}", path.display()))?
        .hash(hasher);
    Ok(())
}

fn cargo_diagnostics(messages: &str) -> String {
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|message| message["message"]["rendered"].as_str().map(str::to_owned))
        .collect()
}

fn parse_program(stdout: &str, manifest: &Path) -> anyhow::Result<Program> {
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .with_context(|| format!("loading {}: emitter printed no Program", manifest.display()))?;
    serde_json::from_str(json_line).with_context(|| {
        format!(
            "loading {}: emitter printed invalid Program",
            manifest.display()
        )
    })
}

#[derive(Clone, Copy)]
struct CachePolicy {
    max_bytes: u64,
    max_idle: Duration,
    gc_interval: Duration,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            max_bytes: MAX_CACHE_BYTES,
            max_idle: MAX_IDLE,
            gc_interval: GC_INTERVAL,
        }
    }
}

struct RustCache {
    root: PathBuf,
    policy: CachePolicy,
}

impl RustCache {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            policy: CachePolicy::default(),
        }
    }

    #[cfg(test)]
    fn with_policy(root: PathBuf, policy: CachePolicy) -> Self {
        Self { root, policy }
    }

    fn prepare(&self) -> anyhow::Result<RustCacheGuard> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        self.prepare_at(now)
    }

    fn prepare_at(&self, now: u64) -> anyhow::Result<RustCacheGuard> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating Rust cache {}", self.root.display()))?;
        let lock_path = self.root.join("lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening Rust cache lock {}", lock_path.display()))?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("locking Rust cache {}", self.root.display()))?;

        migrate_cache(&self.root)?;
        let target = self.root.join("target");
        let last_gc = read_stamp(&self.root.join("last-gc"));
        let gc_due = last_gc
            .is_none_or(|stamp| now.saturating_sub(stamp) >= self.policy.gc_interval.as_secs());
        if gc_due {
            collect_target(&self.root, &target, now, self.policy)?;
            write_stamp(&self.root.join("last-gc"), now)?;
        }

        fs::create_dir_all(&target)
            .with_context(|| format!("creating Rust target cache {}", target.display()))?;
        write_stamp(&self.root.join("last-used"), now)?;

        Ok(RustCacheGuard {
            _lock: lock,
            target,
        })
    }

    fn access_shared(&self) -> anyhow::Result<File> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating Rust cache {}", self.root.display()))?;
        let lock_path = self.root.join("lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening Rust cache lock {}", lock_path.display()))?;
        FileExt::lock_shared(&lock)
            .with_context(|| format!("locking Rust cache {} for execution", self.root.display()))?;
        Ok(lock)
    }
}

fn collect_target(root: &Path, target: &Path, now: u64, policy: CachePolicy) -> anyhow::Result<()> {
    let last_used = read_stamp(&root.join("last-used")).unwrap_or(0);
    let idle = now.saturating_sub(last_used) >= policy.max_idle.as_secs();
    let oversized = directory_size(target)? > policy.max_bytes;
    if idle || oversized {
        clear_cache_directory(root, target)?;
    }
    Ok(())
}

struct RustCacheGuard {
    _lock: File,
    target: PathBuf,
}

impl RustCacheGuard {
    fn target_dir(&self) -> &Path {
        &self.target
    }
}

fn cache_root_with(get: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    if let Some(path) = get("IFX_CACHE_DIR") {
        return Ok(PathBuf::from(path).join("rust"));
    }
    if let Some(path) = get("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("ifx/rust"));
    }
    if let Some(path) = get("LOCALAPPDATA") {
        return Ok(PathBuf::from(path).join("ifx/rust"));
    }
    if let Some(path) = get("HOME") {
        return Ok(PathBuf::from(path).join(".cache/ifx/rust"));
    }
    anyhow::bail!("cannot locate the Rust cache; set IFX_CACHE_DIR or XDG_CACHE_HOME")
}

fn read_stamp(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_stamp(path: &Path, value: u64) -> anyhow::Result<()> {
    fs::write(path, format!("{value}\n"))
        .with_context(|| format!("writing Rust cache stamp {}", path.display()))
}

fn directory_size(path: &Path) -> anyhow::Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    fs::read_dir(path)?.try_fold(0_u64, |total, entry| {
        let size = directory_size(&entry?.path())?;
        total
            .checked_add(size)
            .context("Rust cache size exceeds u64")
    })
}

fn migrate_cache(root: &Path) -> anyhow::Result<()> {
    let version = root.join("cache-version");
    if matches!(fs::read_to_string(&version), Ok(contents) if contents == CACHE_VERSION) {
        return Ok(());
    }

    clear_cache_directory(root, &root.join("target"))?;
    clear_cache_directory(root, &root.join("projects"))?;
    remove_cache_file(root, &root.join("last-used"))?;
    remove_cache_file(root, &root.join("last-gc"))?;
    fs::write(&version, CACHE_VERSION)
        .with_context(|| format!("writing Rust cache version {}", version.display()))
}

fn clear_cache_directory(root: &Path, directory: &Path) -> anyhow::Result<()> {
    let valid_name = directory
        .file_name()
        .is_some_and(|name| name == "target" || name == "projects");
    anyhow::ensure!(
        directory.parent() == Some(root) && valid_name,
        "refusing to clear invalid Rust cache directory {}",
        directory.display()
    );
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("garbage-collecting Rust cache {}", directory.display())),
    }
}

fn remove_cache_file(root: &Path, path: &Path) -> anyhow::Result<()> {
    let valid_name = path
        .file_name()
        .is_some_and(|name| name == "last-used" || name == "last-gc");
    anyhow::ensure!(
        path.parent() == Some(root) && valid_name,
        "refusing to remove invalid Rust cache file {}",
        path.display()
    );
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("removing Rust cache file {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_bytes: u64) -> CachePolicy {
        CachePolicy {
            max_bytes,
            max_idle: Duration::from_secs(100),
            gc_interval: Duration::from_secs(10),
        }
    }

    #[test]
    fn cache_root_prefers_explicit_then_platform_locations() {
        let explicit = cache_root_with(|key| match key {
            "IFX_CACHE_DIR" => Some(OsString::from("/cache")),
            "XDG_CACHE_HOME" => Some(OsString::from("/xdg")),
            _ => None,
        })
        .expect("explicit cache root should resolve");
        assert_eq!(explicit, Path::new("/cache/rust"));

        let xdg = cache_root_with(|key| match key {
            "XDG_CACHE_HOME" => Some(OsString::from("/xdg")),
            _ => None,
        })
        .expect("XDG cache root should resolve");
        assert_eq!(xdg, Path::new("/xdg/ifx/rust"));
    }

    #[test]
    fn idle_cache_is_collected_before_reuse() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let cache = RustCache::with_policy(temp.path().join("rust"), policy(u64::MAX));
        let target = cache.root.join("target");
        fs::create_dir_all(&target).expect("target should be created");
        fs::write(cache.root.join("cache-version"), CACHE_VERSION)
            .expect("cache version should be written");
        fs::write(target.join("stale"), b"stale").expect("fixture should be written");
        fs::write(cache.root.join("keep"), b"keep").expect("fixture should be written");
        write_stamp(&cache.root.join("last-used"), 10).expect("stamp should be written");
        write_stamp(&cache.root.join("last-gc"), 10).expect("stamp should be written");

        let guard = cache
            .prepare_at(110)
            .expect("cache preparation should succeed");

        assert!(!guard.target_dir().join("stale").exists());
        assert!(cache.root.join("keep").exists());
        assert_eq!(read_stamp(&cache.root.join("last-used")), Some(110));
    }

    #[test]
    fn oversized_shared_cache_is_rebuilt_when_scan_is_due() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let cache = RustCache::with_policy(temp.path().join("rust"), policy(9));
        let target = cache.root.join("target");
        fs::create_dir_all(&target).expect("target should be created");
        fs::write(cache.root.join("cache-version"), CACHE_VERSION)
            .expect("cache version should be written");
        fs::write(target.join("large"), b"1234567890").expect("fixture should be written");
        write_stamp(&cache.root.join("last-used"), 95).expect("stamp should be written");
        write_stamp(&cache.root.join("last-gc"), 95).expect("stamp should be written");

        let guard = cache
            .prepare_at(100)
            .expect("cache preparation should succeed");
        assert!(guard.target_dir().join("large").exists());
        drop(guard);

        let guard = cache
            .prepare_at(105)
            .expect("cache preparation should succeed");
        assert!(!guard.target_dir().join("large").exists());
        assert!(guard.target_dir().is_dir());
    }

    #[test]
    fn old_cache_layout_is_removed_during_migration() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let cache = RustCache::new(temp.path().join("cache"));
        fs::create_dir_all(cache.root.join("target")).expect("target should be created");
        fs::create_dir_all(cache.root.join("projects/old")).expect("project should be created");
        fs::write(cache.root.join("target/old"), b"old").expect("fixture should be written");
        fs::write(cache.root.join("projects/old/file"), b"old").expect("fixture should be written");

        let guard = cache
            .prepare_at(100)
            .expect("cache migration should succeed");

        assert!(!guard.target_dir().join("old").exists());
        assert!(!cache.root.join("projects").exists());
        assert_eq!(
            fs::read_to_string(cache.root.join("cache-version"))
                .expect("cache version should exist"),
            CACHE_VERSION
        );
    }

    #[test]
    fn same_named_packages_share_dependencies_without_aliasing_emitters() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let compiler = RustCompiler::new(temp.path().join("cache"));
        let first = write_test_crate(temp.path(), "first", "first");
        let second = write_test_crate(temp.path(), "second", "second");

        let first_program = compile_and_emit(&compiler, &first).expect("first package should run");
        let second_program =
            compile_and_emit(&compiler, &second).expect("second package should run");
        let first_again =
            compile_and_emit(&compiler, &first).expect("first package should still run");
        let profile = compiler.cache_root.join("target/debug/deps");
        let first_executable = profile.join(format!(
            "same_name{}{}",
            emitter_suffix(&first),
            std::env::consts::EXE_SUFFIX
        ));
        let second_executable = profile.join(format!(
            "same_name{}{}",
            emitter_suffix(&second),
            std::env::consts::EXE_SUFFIX
        ));

        assert!(first_executable.is_file());
        assert!(second_executable.is_file());
        assert_ne!(first_executable, second_executable);
        assert_eq!(first_program.resources[0].urn.name(), "first");
        assert_eq!(second_program.resources[0].urn.name(), "second");
        assert_eq!(first_again.resources[0].urn.name(), "first");
    }

    #[test]
    fn default_run_selects_one_emitter_from_multiple_binaries() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let compiler = RustCompiler::new(temp.path().join("cache"));
        let manifest = write_multi_bin_test_crate(temp.path());

        let program =
            compile_and_emit(&compiler, &manifest).expect("default-run emitter should execute");

        assert_eq!(program.resources[0].urn.name(), "selected");
    }

    #[test]
    fn dynamic_emitters_receive_the_rust_runtime_library_path() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let compiler = RustCompiler::new(temp.path().join("cache"));
        let manifest = write_test_crate(temp.path(), "dynamic", "dynamic");
        let cargo = manifest
            .parent()
            .expect("manifest should have a parent")
            .join(".cargo");
        fs::create_dir(&cargo).expect("Cargo configuration directory should exist");
        fs::write(
            cargo.join("config.toml"),
            "[build]\nrustflags = [\"-C\", \"prefer-dynamic\"]\n",
        )
        .expect("Cargo configuration should be written");

        let program = compile_and_emit(&compiler, &manifest)
            .expect("a dynamically linked emitter should execute");

        assert_eq!(program.resources[0].urn.name(), "dynamic");
    }

    #[test]
    fn running_emitters_hold_the_shared_cache_lease() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let compiler = RustCompiler::new(temp.path().join("cache"));
        let manifest = write_test_crate(temp.path(), "blocking", "blocking");
        fs::write(
            manifest.parent().unwrap().join("src/main.rs"),
            r##"fn main() {
    let barrier = std::env::var("IFX_STACK").unwrap();
    std::fs::write(format!("{barrier}.ready"), b"ready").unwrap();
    while !std::path::Path::new(&format!("{barrier}.go")).exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    println!("{}", r#"{"resources":[]}"#);
}
"##,
        )
        .expect("blocking emitter should be written");
        let artifact = compiler
            .build(&manifest, manifest.parent().unwrap())
            .expect("blocking emitter should compile");
        assert!(
            artifact.executable.path.starts_with(
                manifest
                    .parent()
                    .expect("manifest should have a parent")
                    .join(".ifx/build")
            )
        );
        let barrier = temp.path().join("emitter-barrier");
        let mut context = test_context(&manifest);
        context.stack = barrier.display().to_string();
        let emitter = std::thread::spawn(move || artifact.emit(&context));
        wait_for_path(&barrier.with_extension("ready"));

        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let rebuild_compiler = compiler.clone();
        let rebuild_manifest = manifest.clone();
        let rebuild = std::thread::spawn(move || {
            let result = rebuild_compiler.build(
                &rebuild_manifest,
                rebuild_manifest
                    .parent()
                    .expect("manifest should have a parent"),
            );
            finished_tx.send(result).unwrap();
        });
        assert!(
            finished_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "an exclusive rebuild must wait for the running emitter"
        );
        fs::write(barrier.with_extension("go"), b"go").expect("barrier should open");
        emitter
            .join()
            .expect("emitter thread should not panic")
            .expect("emitter should finish");
        finished_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("rebuild should resume after emission")
            .expect("rebuild should succeed");
        rebuild.join().expect("rebuild thread should not panic");
    }

    #[test]
    fn source_edits_during_compilation_reject_the_mixed_generation() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let compiler = RustCompiler::new(temp.path().join("cache"));
        let manifest = write_test_crate(temp.path(), "changing", "old");
        let crate_dir = manifest.parent().expect("manifest should have a parent");
        let barrier = temp.path().join("compile-barrier");
        let mut cargo_toml = fs::read_to_string(&manifest).expect("manifest should exist");
        cargo_toml.push_str("build = \"build.rs\"\n");
        fs::write(&manifest, cargo_toml).expect("build script should be configured");
        fs::write(
            crate_dir.join("build.rs"),
            format!(
                r#"fn main() {{
    let barrier = {:?};
    std::fs::write(format!("{{barrier}}.ready"), b"ready").unwrap();
    while !std::path::Path::new(&format!("{{barrier}}.go")).exists() {{
        std::thread::sleep(std::time::Duration::from_millis(5));
    }}
}}
"#,
                barrier.display().to_string()
            ),
        )
        .expect("blocking build script should be written");

        let build_compiler = compiler.clone();
        let build_manifest = manifest.clone();
        let building = std::thread::spawn(move || {
            build_compiler.build(&build_manifest, build_manifest.parent().unwrap())
        });
        wait_for_path(&barrier.with_extension("ready"));
        fs::write(crate_dir.join("src/main.rs"), test_program_source("new"))
            .expect("source generation should change");
        fs::write(barrier.with_extension("go"), b"go").expect("barrier should open");

        let error = building
            .join()
            .expect("build thread should not panic")
            .expect_err("mixed source generations must be rejected");
        assert!(
            error.downcast_ref::<SourcesChanged>().is_some(),
            "{error:#}"
        );
        let program = compile_and_emit(&compiler, &manifest)
            .expect("the stable source generation should compile");
        assert_eq!(program.resources[0].urn.name(), "new");
    }

    #[test]
    fn cargo_workspace_configuration_and_toolchain_files_are_watched() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let workspace = temp.path().join("workspace");
        let member = workspace.join("member");
        fs::create_dir_all(member.join(".cargo")).expect("fixture directories should exist");
        for path in [
            workspace.join("Cargo.toml"),
            workspace.join("Cargo.lock"),
            workspace.join("rust-toolchain.toml"),
            member.join(".cargo/config.toml"),
        ] {
            fs::write(&path, "fixture").expect("watched input should be written");
        }

        let inputs = cargo_input_files(&member, Some(&workspace));

        for expected in [
            workspace.join("Cargo.toml"),
            workspace.join("Cargo.lock"),
            workspace.join("rust-toolchain.toml"),
            member.join(".cargo/config.toml"),
        ] {
            assert!(inputs.contains(&expected), "missing {}", expected.display());
        }
        let early = early_input_candidates(&member.join("Cargo.toml"), &member);
        assert!(early.contains(&workspace.join("Cargo.toml")));
        assert!(early.contains(&workspace.join("Cargo.lock")));
    }

    #[cfg(unix)]
    #[test]
    fn source_fingerprint_traverses_symlinked_directories() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let root = temp.path().join("root");
        let target = temp.path().join("target");
        fs::create_dir_all(&root).expect("source root should exist");
        fs::create_dir_all(&target).expect("linked source should exist");
        fs::write(target.join("module.rs"), "old\n").expect("linked source should be written");
        std::os::unix::fs::symlink(&target, root.join("linked"))
            .expect("source directory symlink should be created");
        let roots = vec![root];
        let before = source_fingerprint(&roots, &[]).expect("source should be fingerprinted");

        fs::write(target.join("module.rs"), "new\n").expect("linked source should change");
        let after = source_fingerprint(&roots, &[]).expect("source should be fingerprinted");

        assert_ne!(before, after);
    }

    #[test]
    fn configured_target_runners_are_rejected_actionably() {
        let temp = tempfile::tempdir().expect("temporary directory should exist");
        let cargo = temp.path().join(".cargo");
        fs::create_dir(&cargo).expect("Cargo configuration directory should exist");
        fs::write(
            cargo.join("config.toml"),
            "[target.'cfg(unix)']\nrunner = \"remote-runner\"\n",
        )
        .expect("Cargo configuration should be written");

        let error = ensure_local_cargo_config(temp.path(), "x86_64-unknown-linux-gnu")
            .expect_err("target runners should be rejected");

        assert!(
            error
                .to_string()
                .contains("do not support Cargo target runner")
        );
    }

    #[test]
    fn program_parser_uses_the_last_json_line() {
        let program = parse_program("diagnostic\n{\"resources\":[]}\n", Path::new("Cargo.toml"))
            .expect("Program should parse");
        assert!(program.resources.is_empty());
    }

    fn write_test_crate(root: &Path, directory: &str, resource: &str) -> PathBuf {
        let crate_dir = root.join(directory);
        fs::create_dir_all(crate_dir.join("src")).expect("source directory should exist");
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"same-name\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest should be written");
        fs::write(
            crate_dir.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"same-name\"\nversion = \"0.1.0\"\n",
        )
        .expect("lockfile should be written");
        fs::write(crate_dir.join("src/lib.rs"), "").expect("library source should be written");
        fs::write(crate_dir.join("src/main.rs"), test_program_source(resource))
            .expect("source should be written");
        crate_dir.join("Cargo.toml")
    }

    fn write_multi_bin_test_crate(root: &Path) -> PathBuf {
        let crate_dir = root.join("multiple");
        fs::create_dir_all(crate_dir.join("src")).expect("source directory should exist");
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"multiple\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             default-run = \"selected\"\nautobins = false\n\n\
             [[bin]]\nname = \"selected\"\npath = \"src/selected.rs\"\n\n\
             [[bin]]\nname = \"other\"\npath = \"src/other.rs\"\n",
        )
        .expect("manifest should be written");
        fs::write(
            crate_dir.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"multiple\"\nversion = \"0.1.0\"\n",
        )
        .expect("lockfile should be written");
        fs::write(
            crate_dir.join("src/selected.rs"),
            test_program_source("selected"),
        )
        .expect("selected emitter should be written");
        fs::write(crate_dir.join("src/other.rs"), test_program_source("other"))
            .expect("other emitter should be written");
        crate_dir.join("Cargo.toml")
    }

    fn test_program_source(resource: &str) -> String {
        format!(
            r##"fn main() {{ println!("{{}}", r#"{{"resources":[{{"urn":"test:{resource}","inputs":{{}},"depends_on":[],"triggers":[],"protect":false}}]}}"#); }}
"##
        )
    }

    fn test_context(manifest: &Path) -> LoadCtx {
        LoadCtx {
            dir: manifest
                .parent()
                .expect("manifest should have a parent")
                .to_path_buf(),
            file: Some(manifest.to_path_buf()),
            stack: "test".to_string(),
            config: Default::default(),
            project: "test".to_string(),
        }
    }

    fn compile_and_emit(compiler: &RustCompiler, manifest: &Path) -> anyhow::Result<Program> {
        let context = test_context(manifest);
        compiler.build(manifest, &context.dir)?.emit(&context)
    }

    fn wait_for_path(path: &Path) {
        let started = std::time::Instant::now();
        while !path.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
