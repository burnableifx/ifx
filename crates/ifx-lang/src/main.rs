use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use ifx_lang::{
    language::{analyze_workspace, module_path},
    lsp, project,
    simulator::Simulator,
    syntax::{self, MAX_SOURCE, StmtKind},
};
use std::{collections::BTreeMap, io::Read, path::PathBuf};

#[derive(Parser)]
#[command(
    about = "Experimental IFX DSL: offline checks, graph compilation, in-memory simulation and LSP"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// TOML values for edition 0.2 main parameters.
    #[arg(long, global = true)]
    inputs: Option<PathBuf>,
}
#[derive(Subcommand)]
enum Command {
    Check {
        file: PathBuf,
    },
    /// Emit an envelope containing the existing Program graph AND deferred configurations.
    Compile {
        file: PathBuf,
    },
    /// Pure in-memory execution. Provider output fixtures are optional explicit JSON.
    Simulate {
        file: PathBuf,
        #[arg(long,default_value_t=2,value_parser=clap::value_parser!(u16).range(1..=100))]
        applies: u16,
        #[arg(long)]
        outputs: Option<PathBuf>,
        #[arg(long)]
        replace_at: Option<u16>,
    },
    Format {
        file: PathBuf,
    },
    /// Fetch explicitly declared, pinned Git module packages and write Ifx.lock.
    Fetch {
        #[arg(long, default_value = "Ifx.toml")]
        manifest_path: PathBuf,
    },
    Lsp,
}
fn read(path: &PathBuf) -> anyhow::Result<String> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .with_context(|| format!("opening {}", path.display()))?;
    read_file(std::fs::File::from(descriptor))
}
fn read_file(file: std::fs::File) -> anyhow::Result<String> {
    if !file.metadata()?.is_file() {
        bail!("source must be a regular file");
    }
    let mut source = String::new();
    file.take((MAX_SOURCE + 1) as u64)
        .read_to_string(&mut source)?;
    if source.len() > MAX_SOURCE {
        bail!("file exceeds 256 KiB limit");
    }
    Ok(source)
}
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let file = match &cli.command {
        Command::Check { file }
        | Command::Compile { file }
        | Command::Simulate { file, .. }
        | Command::Format { file } => file,
        Command::Lsp => return Ok(lsp::serve()?),
        Command::Fetch { manifest_path } => {
            anyhow::ensure!(
                manifest_path.file_name().is_some_and(|n| n == "Ifx.toml"),
                "manifest must be named Ifx.toml"
            );
            let root = manifest_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            project::fetch(root)?;
            println!("dependencies fetched; commit Ifx.toml and Ifx.lock, ignore .ifx/");
            return Ok(());
        }
    };
    let manifest = file.file_name().is_some_and(|n| n == "Ifx.toml");
    let mut source = if manifest { String::new() } else { read(file)? };
    if matches!(cli.command, Command::Format { .. }) {
        anyhow::ensure!(!manifest, "format expects an .ifx source, not Ifx.toml");
        print!("{}", lsp::format_source(&source));
        return Ok(());
    }
    let root = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .canonicalize()?;
    let entry = file
        .file_name()
        .and_then(|s| s.to_str())
        .context("entry filename must be UTF-8")?;
    let project_root = if manifest {
        Some(root.clone())
    } else {
        project::find_root(&root, None)
    };
    let inputs: BTreeMap<String, serde_json::Value> = if let Some(path) = &cli.inputs {
        let table: toml::Table = toml::from_str(&read(path)?).map_err(|e: toml::de::Error| {
            anyhow::anyhow!(
                "invalid input TOML at byte {}: {}",
                e.span().map_or(0, |s| s.start),
                e.message()
            )
        })?;
        serde_json::from_value(serde_json::to_value(table)?)?
    } else {
        BTreeMap::new()
    };
    let analysis = if let Some(root) = project_root {
        let snapshot = project::load(&root, &BTreeMap::new())?;
        if manifest && snapshot.entry.is_none() && matches!(cli.command, Command::Check { .. }) {
            anyhow::ensure!(inputs.is_empty(), "library checks do not take root inputs");
            let id = snapshot
                .sources
                .keys()
                .next()
                .context("library has no declared source modules")?;
            let a = snapshot.check(id);
            for d in &a.diagnostics {
                eprintln!("{}", d.message);
            }
            anyhow::ensure!(a.diagnostics.is_empty(), "language check failed");
            println!("checked library definitions");
            return Ok(());
        }
        let selected = if manifest {
            let entry = snapshot
                .entry
                .as_ref()
                .context("library package has no entry; select an exported .ifx file")?;
            snapshot
                .paths
                .get(entry)
                .context("missing project entry")?
                .clone()
        } else {
            file.canonicalize()?
        };
        let id = snapshot
            .paths
            .iter()
            .find(|(_, p)| **p == selected)
            .map(|(id, _)| id)
            .context("file is not declared in Ifx.toml")?;
        source = snapshot.sources[id].clone();
        anyhow::ensure!(
            inputs.is_empty() || snapshot.edition == ifx_lang::authoring::EDITION,
            "--inputs requires edition 0.2"
        );
        snapshot.analyze_with_inputs(id, &inputs)
    } else {
        anyhow::ensure!(
            inputs.is_empty(),
            "--inputs requires an edition 0.2 project"
        );
        let mut sources = BTreeMap::from([(entry.to_string(), source.clone())]);
        load_modules(&root, entry, &mut sources)?;
        analyze_workspace(entry, &sources)
    };
    if !analysis.diagnostics.is_empty() {
        for d in analysis.diagnostics {
            let p = lsp::position(&source, d.span.start);
            eprintln!(
                "{}:{}:{}: {}",
                file.display(),
                p["line"].as_u64().unwrap_or(0) + 1,
                p["character"].as_u64().unwrap_or(0) + 1,
                d.message
            );
        }
        bail!("language check failed");
    }
    if analysis.compilation.is_none() && matches!(cli.command, Command::Check { .. }) {
        println!("checked library definitions");
        return Ok(());
    }
    let compilation = analysis
        .compilation
        .context("compilation requires an entry file with fn main")?;
    match cli.command {
        Command::Check { .. } => println!(
            "checked {} resources, {} configurations",
            compilation.program.resources.len(),
            compilation.configurations.len()
        ),
        Command::Compile { .. } => println!("{}", serde_json::to_string_pretty(&compilation)?),
        Command::Simulate {
            applies,
            outputs,
            replace_at,
            ..
        } => {
            let fixtures: BTreeMap<String, serde_json::Value> = if let Some(path) = outputs {
                serde_json::from_str(&read(&path)?)?
            } else {
                BTreeMap::new()
            };
            let mut sim = Simulator::default();
            for n in 1..=applies {
                if replace_at == Some(n) {
                    for host in sim.hosts.values_mut() {
                        host.replace();
                    }
                }
                if let Err(d) = sim.apply(&compilation, &fixtures) {
                    bail!("simulation stopped at byte {}: {}", d.span.start, d.message);
                }
            }
            println!("{}", serde_json::to_string_pretty(&sim)?);
        }
        _ => {}
    }
    Ok(())
}

fn load_modules(
    root: &std::path::Path,
    entry: &str,
    sources: &mut BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let parsed = syntax::parse(&sources[entry]);
    for statement in parsed.statements {
        let StmtKind::Import { path, .. } = statement.kind else {
            continue;
        };
        let path = module_path(entry, &path)
            .context("import must be a relative .ifx path without hidden or parent components")?;
        if sources.contains_key(&path) {
            continue;
        }
        if sources.len() >= 32 {
            bail!("module count exceeds 32");
        }
        let text = project::read_at(&project::open_root(root)?, &path)?;
        sources.insert(path.clone(), text);
        load_modules(root, &path, sources)?;
    }
    Ok(())
}
