//! Bounded JSON-lines output. Linux pipes/sockets use nonblocking writes so a slow
//! consumer cannot pin a blocking worker or prevent shutdown. Local output is best-effort.

use crate::observation::Observation;
use anyhow::{Context, Result};
use tokio::{io::AsyncWriteExt as _, sync::mpsc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    mut receiver: mpsc::Receiver<Observation>,
    stop: CancellationToken,
) -> Result<u64> {
    let mut sink = Sink::new()?;
    let mut deadline = None;
    loop {
        let record = if let Some(deadline) = deadline {
            match tokio::time::timeout_at(deadline, receiver.recv()).await {
                Ok(record) => record,
                Err(_) => return Ok(u64::try_from(receiver.len())?),
            }
        } else {
            tokio::select! {
                biased;
                () = stop.cancelled() => {
                    deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
                    continue;
                }
                record = receiver.recv() => record,
            }
        };
        let Some(record) = record else {
            return Ok(0);
        };
        let bytes = line(&record)?;
        if !write_record(&mut sink, &bytes, &stop, &mut deadline).await? {
            return Ok(u64::try_from(receiver.len())? + 1);
        }
    }
}

async fn write_record(
    sink: &mut Sink,
    bytes: &[u8],
    stop: &CancellationToken,
    deadline: &mut Option<tokio::time::Instant>,
) -> Result<bool> {
    let write = sink.write(bytes);
    tokio::pin!(write);
    if deadline.is_none() {
        tokio::select! {
            biased;
            () = stop.cancelled() => {
                *deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
            }
            result = &mut write => { result?; return Ok(true); }
        }
    }
    // Continue the same write future: restarting it could duplicate a partial line.
    match tokio::time::timeout_at(deadline.context("missing output drain deadline")?, write).await {
        Ok(result) => {
            result?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

fn line(record: &Observation) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(target_os = "linux")]
enum Sink {
    Poll {
        descriptor: std::fs::File,
        socket: bool,
    },
    File(tokio::fs::File),
}

#[cfg(target_os = "linux")]
impl Sink {
    fn new() -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let stdout = std::io::stdout();
        let kind = rustix::fs::FileType::from_raw_mode(rustix::fs::fstat(&stdout)?.st_mode);
        if kind == rustix::fs::FileType::Socket {
            let file = std::fs::File::from(rustix::io::dup(&stdout)?);
            return Ok(Self::Poll {
                descriptor: file,
                socket: true,
            });
        }
        // A new open file description avoids changing stdout's process-wide flags.
        let file = std::fs::OpenOptions::new()
            .append(true)
            .custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits())?)
            .open("/proc/self/fd/1")?;
        if kind == rustix::fs::FileType::RegularFile {
            return Ok(Self::File(tokio::fs::File::from_std(file)));
        }
        Ok(Self::Poll {
            descriptor: file,
            socket: false,
        })
    }

    async fn write(&mut self, mut bytes: &[u8]) -> Result<()> {
        let Self::Poll { descriptor, socket } = self else {
            if let Self::File(file) = self {
                file.write_all(bytes).await?;
                file.flush().await?;
            }
            return Ok(());
        };
        while !bytes.is_empty() {
            let written = if *socket {
                rustix::net::send(
                    &*descriptor,
                    bytes,
                    rustix::net::SendFlags::DONTWAIT | rustix::net::SendFlags::NOSIGNAL,
                )
            } else {
                rustix::io::write(&*descriptor, bytes)
            }
            .map_err(std::io::Error::from);
            match written {
                Ok(0) => anyhow::bail!("local output closed during a record"),
                Ok(count) => {
                    bytes = bytes
                        .get(count..)
                        .context("invalid local output write length")?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // Only backpressured sinks poll; at most 100 wakeups per second.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
struct Sink(tokio::io::Stdout);
#[cfg(not(target_os = "linux"))]
impl Sink {
    fn new() -> Result<Self> {
        Ok(Self(tokio::io::stdout()))
    }
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.write_all(bytes).await?;
        Ok(())
    }
}
