use std::ffi::OsStr;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ifx-labs", about = "Run, record, and verify the IFX labs.")]
struct Cli {
    #[arg(long, env = "IFX_BIN")]
    bin_dir: Option<PathBuf>,
    #[arg(long, env = "LAB", default_value = "/tmp/ifx-lab")]
    base: PathBuf,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Show the runnable labs.
    List,
    /// Replay labs quickly and assert every command's exit status.
    Run { labs: Vec<u8> },
    /// Replay labs with narrated pauses suitable for a terminal recording.
    Demo {
        labs: Vec<u8>,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
    /// Record narrated asciinema casts.
    Record {
        labs: Vec<u8>,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
    /// Render committed casts into GIF previews with agg.
    Gif { labs: Vec<u8> },
    /// Validate local Markdown links and the cast/GIF inventory.
    Docs,
    #[command(hide = true)]
    Serve { dir: PathBuf, port: u16 },
}

#[derive(Clone, Copy)]
struct Lab {
    number: u8,
    directory: &'static str,
    title: &'static str,
}

const LABS: &[Lab] = &[
    Lab {
        number: 1,
        directory: "01-first-stack",
        title: "First stack: plan, apply, drift, state",
    },
    Lab {
        number: 2,
        directory: "02-references-and-loops",
        title: "References, loops, components, and targets",
    },
    Lab {
        number: 3,
        directory: "03-lifecycle",
        title: "Lifecycle: triggers, guards, and protection",
    },
    Lab {
        number: 4,
        directory: "04-health-and-daemon",
        title: "Health, drift, and daemon observation",
    },
    Lab {
        number: 5,
        directory: "05-components",
        title: "Reusable Rust components and typed outputs",
    },
    Lab {
        number: 8,
        directory: "08-qemu",
        title: "Chained QEMU VMs with lifecycle-controlled SSH",
    },
    Lab {
        number: 9,
        directory: "09-deployment-explorer",
        title: "Deployment Explorer and live operations",
    },
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ifx-labs: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    match cli.command {
        Action::List => {
            for lab in LABS {
                println!("{}  {}", lab.number, lab.title);
            }
        }
        Action::Run { labs } => {
            let config = Config::new(root, cli.bin_dir, cli.base, Pace::instant())?;
            play(&config, &selected(&labs)?)?;
        }
        Action::Demo { labs, speed } => {
            anyhow::ensure!(speed > 0.0, "speed must be greater than zero");
            let config = Config::new(root, cli.bin_dir, cli.base, Pace::human(speed))?;
            play(&config, &selected(&labs)?)?;
        }
        Action::Record { labs, speed } => record(&root, cli.bin_dir, &cli.base, &labs, speed)?,
        Action::Gif { labs } => render_gifs(&root, &selected(&labs)?)?,
        Action::Docs => check_docs(&root)?,
        Action::Serve { dir, port } => serve(&dir, port)?,
    }
    Ok(())
}

struct Config {
    root: PathBuf,
    bin_dir: PathBuf,
    base: PathBuf,
    pace: Pace,
}

impl Config {
    fn new(
        root: PathBuf,
        bin_dir: Option<PathBuf>,
        base: PathBuf,
        pace: Pace,
    ) -> anyhow::Result<Self> {
        let bin_dir = bin_dir.unwrap_or_else(|| root.join("target/debug"));
        anyhow::ensure!(
            bin_dir.join(binary("ifx")).is_file() && bin_dir.join(binary("ifxd")).is_file(),
            "ifx and ifxd are not in {}; run `cargo build -p ifx-cli -p ifxd -p ifx-labs`",
            bin_dir.display()
        );
        let base = absolute(&base)?;
        anyhow::ensure!(
            base.components().count() > 2,
            "refusing broad lab output directory {}",
            base.display()
        );
        Ok(Self {
            root,
            bin_dir,
            base,
            pace,
        })
    }
}

#[derive(Clone, Copy)]
struct Pace {
    before: Duration,
    after: Duration,
}

impl Pace {
    fn instant() -> Self {
        Self {
            before: Duration::ZERO,
            after: Duration::ZERO,
        }
    }

    fn human(speed: f64) -> Self {
        Self {
            before: Duration::from_secs_f64(1.4 / speed),
            after: Duration::from_secs_f64(2.8 / speed),
        }
    }
}

fn selected(numbers: &[u8]) -> anyhow::Result<Vec<Lab>> {
    if numbers.is_empty() {
        return Ok(LABS.to_vec());
    }
    numbers
        .iter()
        .map(|number| {
            LABS.iter()
                .find(|lab| lab.number == *number)
                .copied()
                .with_context(|| format!("no lab {number}"))
        })
        .collect()
}

fn play(config: &Config, labs: &[Lab]) -> anyhow::Result<()> {
    for lab in labs {
        println!("\n#### Lab {} — {}\n", lab.number, lab.title);
        run_lab(config, *lab)?;
    }
    println!("ifx-labs: all commands exited as expected");
    Ok(())
}

fn run_lab(config: &Config, lab: Lab) -> anyhow::Result<()> {
    let directory = config.root.join("labs").join(lab.directory);
    let output = config.base.join(format!("{:02}", lab.number));
    if lab.number == 8 && contains_pidfile(&output)? {
        say(
            config,
            "Cleanly adopt and destroy resources left by an interrupted QEMU run.",
        );
        let mut session = Session::start(config, &directory)?;
        let _ = session.ifx(&["apply", "-y"], None);
        let _ = session.ifx(&["destroy", "-y"], None);
    }
    remove_scoped(&directory, &directory.join(".ifx"))?;
    remove_scoped(&config.base, &output)?;
    fs::create_dir_all(&config.base)?;

    let mut session = Session::start(config, &directory)?;
    say(
        config,
        "Read the Rust stack: typed builders emit Program IR; ifxd owns compilation and execution.",
    );
    let source_range = if lab.number == 8 { "1,360p" } else { "1,240p" };
    show(config, &format!("sed -n '{source_range}' src/main.rs"));
    session.shell(&format!("sed -n '{source_range}' src/main.rs"), Some(0))?;

    say(
        config,
        "Build immediately. Subsequent commands reuse the daemon's compiled artifact.",
    );
    session.ifx(&["build"], Some(0))?;
    say(
        config,
        "Plan observes real resources; exit 2 means changes are pending.",
    );
    session.ifx(&["plan"], Some(2))?;
    if lab.number == 8 {
        say(
            config,
            "Apply boots router → middle → leaf, seals bootstrap forwards, then configures through the final nested SSH paths.",
        );
    } else {
        say(config, "Apply reconciles the graph in dependency order.");
    }
    session.ifx(&["apply", "-y"], Some(0))?;

    if lab.number == 4 {
        session.spawn_server(&output, 8765)?;
        session.ifx(&["check"], Some(0))?;
    }
    if lab.number == 9 {
        session.spawn_server(&output.join("site"), 8879)?;
        session.ifx(&["check"], Some(0))?;
        session.ifx(
            &[
                "graph",
                "--format",
                "html",
                "--no-refresh",
                "--out",
                output.join("deployment.html").to_string_lossy().as_ref(),
            ],
            Some(0),
        )?;
    }
    if lab.number == 8 {
        say(
            config,
            "Checks prove the host-facing router service and both private links after transitional management is gone.",
        );
        session.ifx(&["check"], Some(0))?;
        say(
            config,
            "Outputs show direct management on the router and routed management on the middle and leaf.",
        );
        session.ifx(&["outputs"], Some(0))?;
    }

    say(
        config,
        "A second plan is empty: the deployment is idempotent.",
    );
    session.ifx(&["plan"], Some(0))?;
    if lab.number == 3 {
        say(
            config,
            "Record an unprotected revision before the state-only destroy.",
        );
        session.ifx(&["apply", "-y", "-c", "protect=false"], Some(0))?;
    }
    say(
        config,
        "Destroy removes managed resources in reverse dependency order.",
    );
    session.ifx(&["destroy", "-y"], Some(0))?;
    drop(session);

    if lab.number == 8 {
        anyhow::ensure!(
            !contains_pidfile(&output)?,
            "QEMU pidfile remained after destroy"
        );
        anyhow::ensure!(
            has_extension(&config.base.join("qemu-cache/images"), "qcow2")?,
            "verified QEMU image was not retained in the shared cache"
        );
    }
    Ok(())
}

struct Session<'a> {
    config: &'a Config,
    directory: PathBuf,
    daemon_url: String,
    daemon: Child,
    children: Vec<Child>,
}

impl<'a> Session<'a> {
    fn start(config: &'a Config, directory: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(directory.join(".ifx"))?;
        let port = free_port()?;
        let daemon_url = format!("http://127.0.0.1:{port}");
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(".ifx/ifxd.log"))?;
        let daemon = Command::new(config.bin_dir.join(binary("ifxd")))
            .args([
                "--db",
                &format!("surrealkv://{}", directory.join(".ifx/db").display()),
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--stack",
                &format!("{}=default", directory.display()),
                "--check-interval",
                "1h",
                "--drift-interval",
                "1h",
            ])
            .current_dir(directory)
            .env("LAB", &config.base)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting ifxd")?;
        let mut session = Self {
            config,
            directory: directory.to_path_buf(),
            daemon_url,
            daemon,
            children: Vec::new(),
        };
        session.wait_ready()?;
        Ok(session)
    }

    fn wait_ready(&mut self) -> anyhow::Result<()> {
        let address = self.daemon_url.trim_start_matches("http://");
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = self.daemon.try_wait()? {
                anyhow::bail!(
                    "ifxd exited {status}; inspect {}",
                    self.directory.join(".ifx/ifxd.log").display()
                );
            }
            if TcpStream::connect_timeout(&address.parse()?, Duration::from_millis(100)).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(40));
        }
        anyhow::bail!("ifxd did not listen at {}", self.daemon_url)
    }

    fn ifx(&mut self, args: &[&str], expected: Option<i32>) -> anyhow::Result<()> {
        show(self.config, &format!("ifx {}", args.join(" ")));
        let status = Command::new(self.config.bin_dir.join(binary("ifx")))
            .args(args)
            .current_dir(&self.directory)
            .env("LAB", &self.config.base)
            .env("IFXD_URL", &self.daemon_url)
            .status()?;
        check_status(&format!("ifx {}", args.join(" ")), status.code(), expected)?;
        thread::sleep(self.config.pace.after);
        Ok(())
    }

    fn shell(&mut self, line: &str, expected: Option<i32>) -> anyhow::Result<()> {
        let status = Command::new("sh")
            .args(["-c", line])
            .current_dir(&self.directory)
            .env("LAB", &self.config.base)
            .env("IFXD_URL", &self.daemon_url)
            .status()?;
        check_status(line, status.code(), expected)
    }

    fn spawn_server(&mut self, directory: &Path, port: u16) -> anyhow::Result<()> {
        say(
            self.config,
            "Start the Rust lab HTTP server, then run health checks.",
        );
        show(
            self.config,
            &format!("ifx-labs serve {} {port} &", directory.display()),
        );
        let child = Command::new(std::env::current_exe()?)
            .args(["serve", directory.to_string_lossy().as_ref()])
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.children.push(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(40));
        }
        anyhow::bail!("HTTP server did not listen on port {port}")
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn say(config: &Config, text: &str) {
    println!("\n# {text}");
    thread::sleep(config.pace.before);
}

fn show(config: &Config, command: &str) {
    println!("$ {command}");
    thread::sleep(config.pace.before / 3);
}

fn check_status(command: &str, actual: Option<i32>, expected: Option<i32>) -> anyhow::Result<()> {
    if let Some(expected) = expected {
        anyhow::ensure!(
            actual == Some(expected),
            "`{command}` exited {}, expected {expected}",
            actual.map_or_else(|| "by signal".to_string(), |code| code.to_string())
        );
    }
    Ok(())
}

fn record(
    root: &Path,
    bin_dir: Option<PathBuf>,
    base: &Path,
    numbers: &[u8],
    speed: f64,
) -> anyhow::Result<()> {
    let labs = selected(numbers)?;
    let executable = std::env::current_exe()?;
    for lab in labs {
        let target = root.join(format!("labs/casts/lab-{}.cast", lab.number));
        let mut command = format!("{} --base {}", shell_word(&executable), shell_word(base));
        if let Some(bin_dir) = &bin_dir {
            command.push_str(&format!(" --bin-dir {}", shell_word(bin_dir)));
        }
        command.push_str(&format!(" demo --speed {speed} {}", lab.number));
        let status = Command::new("asciinema")
            .args([
                "rec",
                "--overwrite",
                "--quiet",
                "--cols",
                "100",
                "--rows",
                "32",
                "--title",
                &format!("ifx — {}", lab.title),
                "--command",
                &command,
            ])
            .arg(&target)
            .status()
            .context("running asciinema")?;
        anyhow::ensure!(status.success(), "asciinema failed for lab {}", lab.number);
    }
    Ok(())
}

fn render_gifs(root: &Path, labs: &[Lab]) -> anyhow::Result<()> {
    for lab in labs {
        let source = root.join(format!("labs/casts/lab-{}.cast", lab.number));
        let target = root.join(format!("labs/media/lab-{}.gif", lab.number));
        let status = Command::new("agg")
            .args([
                "--quiet",
                "--font-size",
                "12",
                "--font-aa",
                "off",
                "--idle-time-limit",
                "6",
                "--fps-cap",
                "10",
                "--last-frame-duration",
                "2",
            ])
            .arg(&source)
            .arg(&target)
            .status()
            .context("running agg")?;
        anyhow::ensure!(status.success(), "agg failed for lab {}", lab.number);
    }
    Ok(())
}

fn check_docs(root: &Path) -> anyhow::Result<()> {
    let mut documents = Vec::new();
    collect_files(root, "md", &mut documents)?;
    let mut errors = Vec::new();
    let all_markdown = documents
        .iter()
        .map(fs::read_to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    for document in &documents {
        let text = fs::read_to_string(document)?;
        for target in markdown_targets(&text) {
            if !is_local_link(&target) {
                continue;
            }
            let path = target.split('#').next().unwrap_or_default();
            if !path.is_empty() && !document.parent().unwrap_or(root).join(path).exists() {
                errors.push(format!(
                    "{}: missing link target {target}",
                    document.strip_prefix(root).unwrap_or(document).display()
                ));
            }
        }
    }
    let casts = named_files(&root.join("labs/casts"), "cast")?;
    let gifs = named_files(&root.join("labs/media"), "gif")?;
    if casts != gifs {
        errors.push(format!(
            "cast/GIF inventory differs: casts={casts:?}, gifs={gifs:?}"
        ));
    }
    for name in &casts {
        let cast = root.join("labs/casts").join(format!("{name}.cast"));
        let header = fs::read_to_string(&cast)?
            .lines()
            .next()
            .context("empty asciicast")?
            .to_string();
        anyhow::ensure!(
            serde_json::from_str::<serde_json::Value>(&header)?["version"] == 2,
            "{} is not asciicast v2",
            cast.display()
        );
        if !all_markdown.contains(&format!("{name}.cast")) {
            errors.push(format!("{} is not linked from Markdown", cast.display()));
        }
        let gif = root.join("labs/media").join(format!("{name}.gif"));
        let bytes = fs::read(&gif)?;
        if !matches!(bytes.get(..6), Some(b"GIF87a" | b"GIF89a")) {
            errors.push(format!("{} has an invalid GIF header", gif.display()));
        }
        if bytes.len() > 2 * 1024 * 1024 {
            errors.push(format!("{} exceeds 2 MiB", gif.display()));
        }
    }
    anyhow::ensure!(
        errors.is_empty(),
        "documentation checks failed:\n  - {}",
        errors.join("\n  - ")
    );
    println!(
        "documentation checks passed: {} Markdown files, {} casts",
        documents.len(),
        casts.len()
    );
    Ok(())
}

fn serve(root: &Path, port: u16) -> anyhow::Result<()> {
    let root = root.canonicalize()?;
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = serve_one(&root, stream);
            }
            Err(error) => {
                eprintln!("ifx-labs serve: {error}");
            }
        }
    }
    Ok(())
}

fn serve_one(root: &Path, mut stream: TcpStream) -> anyhow::Result<()> {
    let mut request = [0_u8; 4096];
    let read = stream.read(&mut request)?;
    let line = String::from_utf8_lossy(&request[..read]);
    let path = line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.split('/').any(|part| matches!(part, ".." | ".")) {
        write_response(&mut stream, 400, b"bad path")?;
        return Ok(());
    }
    match fs::read(root.join(path)) {
        Ok(body) => write_response(&mut stream, 200, &body)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_response(&mut stream, 404, b"not found")?
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn collect_files(root: &Path, extension: &str, output: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir()
            && !matches!(
                name.to_str(),
                Some(".git" | ".ifx" | ".venv" | "node_modules" | "target")
            )
        {
            collect_files(&path, extension, output)?;
        } else if path.extension() == Some(OsStr::new(extension)) {
            output.push(path);
        }
    }
    Ok(())
}

fn markdown_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find(')') else {
            break;
        };
        targets.push(rest[..end].trim().trim_matches(['<', '>']).to_string());
        rest = &rest[end + 1..];
    }
    targets
}

fn is_local_link(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with(['#', '/'])
        && !["http://", "https://", "mailto:"]
            .iter()
            .any(|prefix| target.starts_with(prefix))
}

fn named_files(directory: &Path, extension: &str) -> anyhow::Result<Vec<String>> {
    let mut names = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new(extension)))
        .filter_map(|path| path.file_stem().and_then(OsStr::to_str).map(str::to_string))
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

fn remove_scoped(root: &Path, target: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        target.parent().is_some() && target.starts_with(root) && target != root,
        "refusing to remove unscoped path {}",
        target.display()
    );
    match fs::remove_dir_all(target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn contains_pidfile(root: &Path) -> anyhow::Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    let mut files = Vec::new();
    collect_files(root, "pid", &mut files)?;
    Ok(files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == OsStr::new("qemu.pid"))
    }))
}

fn has_extension(root: &Path, extension: &str) -> anyhow::Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    let mut files = Vec::new();
    collect_files(root, extension, &mut files)?;
    Ok(!files.is_empty())
}

fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn shell_word(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn binary(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}
