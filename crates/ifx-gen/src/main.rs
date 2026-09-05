//! `cargo run -p ifx-gen` — regenerate `crates/ifx-program/src/generated/*.rs` from the resource
//! schemas of the built-in registry. `--check` fails if the checked-in files are stale.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use clap::Parser;
use ifx::schema::ResourceSchema;

mod rust;

#[derive(Parser)]
#[command(about = "Generate ifx front-end code from resource schemas")]
struct Cli {
    /// Repository root (default: derived from this crate's location).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Do not write; exit 1 if any generated file would change.
    #[arg(long)]
    check: bool,
    /// Refresh `providers/linode/catalog.json` from the public Linode API first.
    #[arg(long)]
    fetch_catalog: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = cli
        .root
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .canonicalize()?;
    if cli.fetch_catalog {
        fetch_catalog(&root.join("crates/ifx/src/providers/linode/catalog.json"))?;
        eprintln!("catalog refreshed; rebuild and rerun ifx-gen to regenerate the enums");
        return Ok(());
    }
    let schemas = ifx::Registry::builtin().schemas();
    let out_dir = root.join("crates/ifx-program/src/generated");

    let mut stale = Vec::new();
    for (provider, group) in by_provider(&schemas) {
        let path = out_dir.join(format!("{provider}.rs"));
        let code = rustfmt(&rust::provider_module(provider, &group))?;
        check_or_write(&root, &path, code, cli.check, &mut stale)?;
    }
    if !stale.is_empty() {
        for p in &stale {
            eprintln!("stale: {}", p.display());
        }
        bail!("generated code is out of date; run `cargo run -p ifx-gen`");
    }
    Ok(())
}

fn check_or_write(
    root: &Path,
    path: &Path,
    contents: String,
    check: bool,
    stale: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current == contents {
        return Ok(());
    }
    if check {
        stale.push(path.to_path_buf());
    } else {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "wrote {}",
            path.strip_prefix(root).unwrap_or(path).display()
        );
    }
    Ok(())
}

/// Regions, plan types and public non-deprecated images, ids + labels only.
fn fetch_catalog(path: &Path) -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    struct Page {
        data: Vec<serde_json::Value>,
        pages: u32,
    }
    let rt = tokio::runtime::Runtime::new()?;
    let client = reqwest::Client::new();
    let get_all = |endpoint: &'static str| {
        let client = client.clone();
        async move {
            let mut out = Vec::new();
            for page in 1.. {
                let url = format!("https://api.linode.com/v4/{endpoint}?page={page}&page_size=500");
                let p: Page = client
                    .get(&url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                out.extend(p.data);
                if page >= p.pages {
                    break;
                }
            }
            anyhow::Ok(out)
        }
    };
    let entries = |items: Vec<serde_json::Value>,
                   keep: &dyn Fn(&serde_json::Value) -> bool|
     -> Vec<serde_json::Value> {
        items
            .into_iter()
            .filter(|i| keep(i))
            .map(|i| serde_json::json!({"id": i["id"], "label": i["label"]}))
            .collect()
    };
    let (regions, types, images) = rt.block_on(async {
        anyhow::Ok((
            get_all("regions").await?,
            get_all("linode/types").await?,
            get_all("images").await?,
        ))
    })?;
    let catalog = serde_json::json!({
        "regions": entries(regions, &|_| true),
        "types": entries(types, &|_| true),
        "images": entries(images, &|i| i["is_public"] == true && i["deprecated"] != true),
    });
    std::fs::write(path, serde_json::to_string_pretty(&catalog)? + "\n")?;
    Ok(())
}

fn by_provider(schemas: &[ResourceSchema]) -> Vec<(&str, Vec<&ResourceSchema>)> {
    let mut out: Vec<(&str, Vec<&ResourceSchema>)> = Vec::new();
    for s in schemas {
        let (provider, _) = ifx::stubs::split_type(&s.type_name);
        match out.iter_mut().find(|(p, _)| *p == provider) {
            Some((_, v)) => v.push(s),
            None => out.push((provider, vec![s])),
        }
    }
    out.sort_by(|a, b| a.0.cmp(b.0));
    for (_, v) in &mut out {
        v.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    }
    out
}

fn rustfmt(code: &str) -> anyhow::Result<String> {
    use std::io::Write as _;
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running rustfmt")?;
    child.stdin.take().unwrap().write_all(code.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "rustfmt failed: {}\n--- input ---\n{code}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}
