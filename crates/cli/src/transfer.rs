//! `qrtx send PATH...` and `qrtx recv [DEST]` over the [`qrtx_proto::files`] format.
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use iroh::endpoint::{RecvStream, SendStream};
use qrtx_proto::{
    files::{Entry, MAX_MANIFEST, Manifest, Reply},
    read_msg, read_msg_max, write_msg, write_msg_max,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Counted, Progress, part_path, status};

/// What `qrtx send` will send: the manifest plus where each file comes from.
#[derive(Debug)]
pub struct Outgoing {
    pub manifest: Manifest,
    /// Same order as the manifest; `None` for directories.
    sources: Vec<Option<PathBuf>>,
    /// Symlinks and special files found inside directories (not sent).
    pub skipped: usize,
}

impl Outgoing {
    pub fn scan(paths: &[PathBuf]) -> Result<Self> {
        let mut out = Outgoing {
            manifest: Manifest::default(),
            sources: Vec::new(),
            skipped: 0,
        };
        for path in paths {
            let meta = std::fs::metadata(path)
                .with_context(|| format!("can't read {}", path.display()))?;
            let name = top_name(path)?;
            if meta.is_file() {
                out.push_file(name, path, &meta);
            } else if meta.is_dir() {
                out.push(Entry::dir(name.clone()), None);
                out.walk(path, &name)?;
            } else {
                bail!("{} is not a file or directory", path.display());
            }
        }
        out.manifest
            .validate()
            .map_err(|e| anyhow::anyhow!("{e} (two of the paths have the same name?)"))?;
        Ok(out)
    }

    pub fn has_dirs(&self) -> bool {
        self.manifest.entries.iter().any(|e| e.dir)
    }

    /// One plain file: the only shape a raw pipe peer can take.
    pub fn single_file(&self) -> Option<&Path> {
        match (self.manifest.entries.as_slice(), self.sources.as_slice()) {
            ([e], [Some(p)]) if !e.dir => Some(p),
            _ => None,
        }
    }

    fn push(&mut self, entry: Entry, source: Option<PathBuf>) {
        self.manifest.entries.push(entry);
        self.sources.push(source);
    }

    fn push_file(&mut self, rel: String, path: &Path, meta: &std::fs::Metadata) {
        let mut entry = Entry::file(rel, meta.len());
        entry.exec = is_exec(meta);
        self.push(entry, Some(path.to_owned()));
    }

    fn walk(&mut self, dir: &Path, rel: &str) -> Result<()> {
        let mut items: Vec<_> = std::fs::read_dir(dir)
            .with_context(|| format!("can't read {}", dir.display()))?
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("can't read {}", dir.display()))?;
        items.sort_by_key(|e| e.file_name());
        for item in items {
            let path = item.path();
            let name = item.file_name().into_string().map_err(|n| {
                anyhow::anyhow!("can't send {}: name is not UTF-8 ({n:?})", path.display())
            })?;
            let child = format!("{rel}/{name}");
            // don't follow symlinks inside directories: no loops, no surprises
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("can't read {}", path.display()))?;
            if meta.is_dir() {
                self.push(Entry::dir(child.clone()), None);
                self.walk(&path, &child)?;
            } else if meta.is_file() {
                self.push_file(child, &path, &meta);
            } else {
                self.skipped += 1;
            }
        }
        Ok(())
    }
}

/// The name a top-level path is sent under (`./photos/` → `photos`, `.` → the cwd's name).
fn top_name(path: &Path) -> Result<String> {
    let name = match path.components().next_back() {
        Some(Component::Normal(n)) => n.to_owned(),
        _ => std::fs::canonicalize(path)?
            .file_name()
            .with_context(|| {
                format!(
                    "can't send {} as a whole; name the files inside it",
                    path.display()
                )
            })?
            .to_owned(),
    };
    name.into_string()
        .map_err(|n| anyhow::anyhow!("can't send {}: name is not UTF-8 ({n:?})", path.display()))
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_: &std::fs::Metadata) -> bool {
    false
}

/// Ask a yes/no question on the terminal (works under `curl … | sh`, where
/// stdin is the script).
pub fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("no terminal to ask for confirmation; pass --yes")?;
    write!(&tty, "qrtx: {question} [y/N] ")?;
    (&tty).flush()?;
    let mut answer = String::new();
    std::io::BufReader::new(&tty).read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Sender side of the framed transfer.
pub async fn send_files(
    send: &mut SendStream,
    recv: &mut RecvStream,
    out: &Outgoing,
) -> Result<()> {
    let m = &out.manifest;
    write_msg_max(send, m, MAX_MANIFEST).await?;
    match read_msg::<Reply>(recv).await? {
        Some(Reply::Accept) => {}
        Some(Reply::Reject { message }) => bail!("the receiver refused: {message}"),
        Some(other) => bail!("unexpected reply {other:?}"),
        None => bail!("the receiver hung up"),
    }
    let total = m.total_bytes();
    let progress = Progress::start("sending", "sent", m.label(), Some(total));
    for (entry, source) in m.entries.iter().zip(&out.sources) {
        let Some(source) = source else { continue };
        let file = tokio::fs::File::open(source)
            .await
            .with_context(|| format!("can't read {}", source.display()))?;
        let mut input = Counted::new(file.take(entry.size), progress.counter());
        let n = tokio::io::copy(&mut input, send).await?;
        ensure!(n == entry.size, "{} shrank while sending", source.display());
    }
    send.finish()?;
    match read_msg::<Reply>(recv).await? {
        Some(Reply::Done { bytes }) if bytes == total => {}
        Some(Reply::Reject { message }) => bail!("the receiver failed: {message}"),
        other => bail!("the receiver didn't confirm the transfer ({other:?})"),
    }
    progress.finish();
    Ok(())
}

/// Where every entry of an incoming manifest will land.
fn plan(m: &Manifest, dest: Option<&Path>, force: bool) -> Result<Vec<PathBuf>> {
    m.validate()?;
    let single_file = matches!(m.entries.as_slice(), [e] if !e.dir);
    let targets: Vec<PathBuf> = match dest {
        // `qrtx recv out.bin`: a single file can be renamed
        Some(d) if single_file && !d.is_dir() && !d.to_string_lossy().ends_with('/') => {
            vec![d.to_owned()]
        }
        _ => {
            let root = dest.unwrap_or(Path::new("."));
            m.entries
                .iter()
                .map(|e| e.path.split('/').fold(root.to_owned(), |p, c| p.join(c)))
                .collect()
        }
    };
    for (e, t) in m.entries.iter().zip(&targets) {
        match (e.dir, t.symlink_metadata()) {
            (true, Ok(meta)) if !meta.is_dir() => {
                bail!("{} exists and is not a directory", t.display())
            }
            (false, Ok(meta)) if meta.is_dir() => {
                bail!("{} exists and is a directory", t.display())
            }
            (false, Ok(_)) if !force => {
                bail!("{} already exists (use --force to overwrite)", t.display())
            }
            _ => {}
        }
    }
    Ok(targets)
}

/// Receiver side of the framed transfer.
pub async fn recv_files(
    send: &mut SendStream,
    recv: &mut RecvStream,
    dest: Option<&Path>,
    force: bool,
) -> Result<()> {
    let m: Manifest = read_msg_max(recv, MAX_MANIFEST)
        .await?
        .context("the sender hung up")?;
    let targets = match plan(&m, dest, force) {
        Ok(t) => t,
        Err(err) => {
            write_msg(
                send,
                &Reply::Reject {
                    message: format!("{err:#}"),
                },
            )
            .await
            .ok();
            send.finish().ok();
            return Err(err);
        }
    };
    write_msg(send, &Reply::Accept).await?;
    let total = m.total_bytes();
    // a renamed single file is reported under its new name
    let label = match targets.as_slice() {
        [t] if !m.entries[0].dir => crate::file_name(t),
        _ => m.label(),
    };
    let progress = Progress::start("receiving", "received", label, Some(total));
    for (entry, target) in m.entries.iter().zip(&targets) {
        if entry.dir {
            tokio::fs::create_dir_all(target)
                .await
                .with_context(|| format!("can't create {}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("can't create {}", parent.display()))?;
        }
        let part = part_path(target);
        let res = async {
            let mut file = tokio::fs::File::create(&part)
                .await
                .with_context(|| format!("can't write {}", part.display()))?;
            let mut output = Counted::new(&mut file, progress.counter());
            let n = tokio::io::copy(&mut (&mut *recv).take(entry.size), &mut output).await?;
            ensure!(
                n == entry.size,
                "the transfer ended early (in {})",
                entry.path
            );
            file.flush().await?;
            set_exec(&file, entry.exec).await;
            tokio::fs::rename(&part, target)
                .await
                .with_context(|| format!("can't save {}", target.display()))
        }
        .await;
        if let Err(err) = res {
            tokio::fs::remove_file(&part).await.ok();
            return Err(err);
        }
    }
    let mut extra = [0u8; 1];
    ensure!(
        recv.read(&mut extra).await?.is_none(),
        "the sender sent more than it announced"
    );
    write_msg(send, &Reply::Done { bytes: total }).await?;
    send.finish()?;
    // make sure the sender actually gets our confirmation
    send.stopped().await.ok();
    progress.finish();
    if m.entries.len() > 1 {
        status(format_args!(
            "saved under {}",
            dest.unwrap_or(Path::new(".")).display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
async fn set_exec(file: &tokio::fs::File, exec: bool) {
    use std::os::unix::fs::PermissionsExt;
    if !exec {
        return;
    }
    if let Ok(meta) = file.metadata().await {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | ((perms.mode() & 0o444) >> 2));
        file.set_permissions(perms).await.ok();
    }
}

#[cfg(not(unix))]
async fn set_exec(_: &tokio::fs::File, _: bool) {}
