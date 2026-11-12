//! The GUI side of the IPC. A connection is only used after the other end has been confirmed to
//! be the component running as SYSTEM / root, so a process that took the endpoint first cannot
//! pose as it.

use crate::paths::Paths;
use crate::protocol::{PROTOCOL, Request, Response, Trigger, read_frame, write_frame};
use crate::state::State;
use anyhow::{Context as _, Result};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
type Stream = tokio::net::UnixStream;

pub struct Connection {
    stream: Stream,
}

impl Connection {
    pub async fn connect() -> Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(5), open())
            .await
            .context("timed out connecting to the update component")??;
        verify_server(&stream)?;
        Ok(Self { stream })
    }

    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        tokio::time::timeout(Duration::from_secs(60), async {
            write_frame(&mut self.stream, request).await?;
            read_frame(&mut self.stream).await
        })
        .await
        .context("the update component did not answer in time")?
    }

    /// Hands a payload over; the component answers once it has verified it.
    pub async fn stage(&mut self, trigger: Trigger, payload: &[u8], signatures: Vec<String>) -> Result<Response> {
        let request = Request::Stage {
            trigger,
            size: payload.len() as u64,
            signatures,
        };
        tokio::time::timeout(Duration::from_secs(300), async {
            write_frame(&mut self.stream, &request).await?;
            self.stream.write_all(payload).await?;
            self.stream.flush().await?;
            read_frame(&mut self.stream).await
        })
        .await
        .context("the update component did not answer in time")?
    }
}

/// Waits until the component answers, each attempt bounded so a stuck one cannot hang the caller.
/// Blocking; not for use inside an async runtime.
pub fn wait_answering(timeout: Duration) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let attempt = tokio::time::timeout(Duration::from_secs(5), async {
                let mut connection = Connection::connect().await?;
                connection.request(&Request::Hello { protocol: PROTOCOL }).await
            })
            .await;
            if matches!(attempt, Ok(Ok(Response::Hello(_)))) {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the update component does not answer"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

/// The persisted state, readable without the component running.
pub fn read_state() -> Result<State> {
    State::load(&Paths::system().state_file())
}

/// Opens the pipe asking for exactly what its DACL grants users: `GENERIC_WRITE` would also ask
/// for `FILE_CREATE_PIPE_INSTANCE`, which users do not get.
#[cfg(windows)]
async fn open() -> Result<Stream> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_EXISTING,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    let name: Vec<u16> = std::ffi::OsStr::new(crate::paths::PIPE_NAME)
        .encode_wide()
        .chain(Some(0))
        .collect();
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(unsafe { Stream::from_raw_handle(handle) }?);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error).context("the update component is not running");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
async fn open() -> Result<Stream> {
    tokio::net::UnixStream::connect(crate::paths::socket_path())
        .await
        .context("the update component is not running")
}

/// The component creates the pipe owned by SYSTEM. A user cannot give an object that owner, so a
/// pipe a user created under this name while the component was not running is told apart. (A
/// user cannot open the SYSTEM process serving the pipe to look at it directly.)
#[cfg(windows)]
fn verify_server(stream: &Stream) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{IsWellKnownSid, OWNER_SECURITY_INFORMATION, WinLocalSystemSid};

    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            stream.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    anyhow::ensure!(status == ERROR_SUCCESS, "cannot read the owner of the pipe");
    let system = unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0;
    unsafe { LocalFree(descriptor) };
    anyhow::ensure!(system, "the pipe is not the update component's");
    Ok(())
}

#[cfg(unix)]
fn verify_server(stream: &Stream) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let credentials = stream.peer_cred().context("cannot identify the socket server")?;
    anyhow::ensure!(credentials.uid() == 0, "the socket server does not run as root");
    let socket = crate::paths::socket_path();
    let dir = socket.parent().context("socket has no directory")?;
    let meta = std::fs::metadata(dir)?;
    anyhow::ensure!(
        meta.uid() == 0 && meta.mode() & 0o022 == 0,
        "the socket directory is not protected"
    );
    Ok(())
}
