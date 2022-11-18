use crate::{Msg, MSG_BUS};
use anyhow::{anyhow, bail, Context, Result};
use cargo_metadata::{Artifact, Message};
use log::LevelFilter;
use serde::Deserialize;
use simplelog::{ColorChoice, ConfigBuilder, TermLogger, TerminalMode};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
};

pub fn setup_logging(verbose: u8) {
    let log_level = match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    let config = ConfigBuilder::default()
        .set_time_level(LevelFilter::Off)
        .build();
    TermLogger::init(log_level, config, TerminalMode::Stderr, ColorChoice::Auto)
        .expect("Failed to start logger");
    log::info!("Log level set to: {log_level}");
}

pub fn rm_dir_content<P: AsRef<Path>>(dir: P) -> Result<()> {
    let dir = dir.as_ref();
    log::info!("Cleaning contents of {dir:?}");

    if !dir.exists() {
        log::debug!("Not cleaning {dir:?} because it does not exist");
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if entry.file_type()?.is_dir() {
            rm_dir_content(&path)?;
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub fn rm_dir(dir: &str) -> Result<()> {
    let path = Path::new(&dir);

    if !path.exists() {
        log::debug!("Not cleaning {dir} because it does not exist");
        return Ok(());
    }

    log::info!("Cleaning dir '{dir}'");
    fs::remove_dir_all(path).context(format!("remove dir {dir}"))?;
    Ok(())
}

pub fn rm_file<S: AsRef<str>>(file: S) -> Result<()> {
    let path = Path::new(file.as_ref());
    if path.exists() {
        fs::remove_file(path).context(format!("remove file {}", file.as_ref()))?;
    }
    Ok(())
}

pub fn mkdirs<S: ToString>(dir: S) -> Result<String> {
    let dir = dir.to_string();
    fs::create_dir_all(&dir).context(format!("create dir {dir}"))?;
    Ok(dir)
}

pub fn write(file: &str, text: &str) -> Result<()> {
    log::trace!("Content of {file}:\n{text}");
    fs::write(&file, text).context(format!("write {file}"))
}

pub fn os_arch() -> Result<(&'static str, &'static str)> {
    let target_os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        bail!("unsupported OS")
    };

    let target_arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        bail!("unsupported target architecture")
    };
    Ok((target_os, target_arch))
}

pub trait StrAdditions {
    fn with(&self, append: &str) -> String;
}

impl StrAdditions for str {
    fn with(&self, append: &str) -> String {
        let mut s = self.to_string();
        s.push_str(append);
        s
    }
}

impl StrAdditions for String {
    fn with(&self, append: &str) -> String {
        let mut s = self.clone();
        s.push_str(append);
        s
    }
}

pub trait PathBufAdditions {
    /// drops the last path component
    fn without_last(self) -> Self;
    /// drops the last path component
    fn with<P: AsRef<Path>>(self, append: P) -> Self;
}

impl PathBufAdditions for PathBuf {
    fn without_last(mut self) -> Self {
        self.pop();
        self
    }
    fn with<P: AsRef<Path>>(mut self, append: P) -> Self {
        self.push(append);
        self
    }
}

pub trait CommandAdditions {
    /// Sets up the command so that stdout is redirected and parsed by cargo_metadata.
    /// It returns a handle and a child process. Waiting on the handle returns
    /// a vector of cargo_metadata Artifacts.
    fn spawn_cargo_parsed(&mut self) -> Result<(JoinHandle<Vec<Artifact>>, Child)>;
}

impl CommandAdditions for Command {
    fn spawn_cargo_parsed(&mut self) -> Result<(JoinHandle<Vec<Artifact>>, Child)> {
        let mut process = self
            .stdout(Stdio::piped())
            .arg("--message-format=json-render-diagnostics")
            .spawn()?;

        let mut stdout = BufReader::new(process.stdout.take().unwrap());

        let handle = tokio::spawn(async move {
            let mut line = String::new();
            let mut artifacts: Vec<Artifact> = Vec::new();
            loop {
                match stdout.read_line(&mut line).await {
                    Ok(_) => {
                        let mut deserializer = serde_json::Deserializer::from_str(&line);
                        deserializer.disable_recursion_limit();
                        match Message::deserialize(&mut deserializer) {
                            Ok(Message::BuildFinished(v)) => {
                                if !v.success {
                                    log::warn!("Build failed")
                                }
                                break;
                            }
                            Ok(Message::BuildScriptExecuted(_script)) => {}
                            Ok(Message::CompilerArtifact(art)) => artifacts.push(art),
                            Ok(Message::CompilerMessage(msg)) => log::info!("MESSAGE {msg:?}"),
                            Ok(Message::TextLine(txt)) => log::info!("TEXT {txt:?}"),
                            Err(e) => {
                                log::error!("cargo stdout: {e}");
                                break;
                            }
                            Ok(_) => log::info!("UNPARSEABLE: {line}"),
                        };
                        line.clear();
                    }
                    Err(e) => {
                        log::error!("cargo stdout: {e}");
                        break;
                    }
                }
            }
            artifacts
        });
        Ok((handle, process))
    }
}

pub fn oneshot_when<S: ToString>(msgs: &'static [Msg], to: S) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel::<()>();

    let mut interrupt = MSG_BUS.subscribe();

    let to = to.to_string();
    tokio::spawn(async move {
        loop {
            match interrupt.recv().await {
                Ok(Msg::ShutDown) => break,
                Ok(msg) if msgs.contains(&msg) => {
                    if let Err(_) = tx.send(()) {
                        log::debug!("Could not send {msg:?} to {to}");
                    }
                    return;
                }
                Err(e) => {
                    log::debug!("Error recieving from MSG_BUS: {e}");
                    return;
                }
                Ok(_) => {}
            }
        }
    });

    rx
}

pub async fn run_interruptible<S: AsRef<str>>(name: S, mut process: Child) -> Result<()> {
    let stop_rx = oneshot_when(
        &[Msg::SrcChanged, Msg::ShutDown],
        format!("cargo {}", name.as_ref()),
    );
    tokio::select! {
        res = process.wait() => match res?.success() {
                true => return Ok(()),
                false => return Err(anyhow!("{} failed", name.as_ref())),
        },
        _ = stop_rx => {
            process.kill().await.map(|_| true).expect("Could not kill process");
            log::debug!("{} stopped", name.as_ref());
            Ok(())
        }
    }
}
