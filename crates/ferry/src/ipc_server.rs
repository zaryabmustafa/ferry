//! Named-pipe server: one JSON request line -> one JSON response line per connection.
use ferry_core::ipc::{decode, encode, Request, Response, PIPE};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use windows::core::HSTRING;
use windows::Win32::Foundation::{LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

const MAX_REQUEST: u64 = 64 * 1024;
/// Oversize requests are drained (so the client's write completes) up to this much.
const MAX_DRAIN: usize = 16 << 20;
/// Concurrent connection threads; extra connections are dropped immediately.
const MAX_IN_FLIGHT: usize = 8;

/// Serve [`PIPE`]. The pipe is created before this returns, so a startup failure
/// (e.g. another engine already owns the name) is reported to the caller.
pub fn serve(handler: Handler) -> io::Result<()> {
    serve_on(PIPE, handler)
}

fn serve_on(name: &str, handler: Handler) -> io::Result<()> {
    let listener = Listener::bind(name)?;
    std::thread::Builder::new().name("ipc".into()).spawn(move || {
        if let Err(e) = listener.run(handler) {
            eprintln!("ipc server stopped: {e}");
        }
    })?;
    Ok(())
}

struct Listener {
    name: HSTRING,
    sd: PSECURITY_DESCRIPTOR,
    first: Option<File>,
    in_flight: Arc<AtomicUsize>,
}

// The security descriptor is immutable LocalAlloc memory owned by the Listener.
unsafe impl Send for Listener {}

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.sd.0))) };
    }
}

impl Listener {
    fn bind(name: &str) -> io::Result<Self> {
        // DACL: the current user only (not Owner Rights, so an elevated engine
        // still admits the user's unelevated UI).
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;{})", ferry_core::sid::current_user_sid()?));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None)? };
        let mut l = Listener { name: HSTRING::from(name), sd, first: None, in_flight: Arc::new(AtomicUsize::new(0)) };
        // FIRST_PIPE_INSTANCE: fail rather than serve a pipe someone else squatted on.
        l.first = Some(l.create(true)?);
        Ok(l)
    }

    fn create(&self, first: bool) -> io::Result<File> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: false.into(),
        };
        let open_mode = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let h = unsafe {
            CreateNamedPipeW(
                &self.name,
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // Owns the handle: closed on drop.
        Ok(unsafe { File::from_raw_handle(h.0) })
    }

    /// Accept loop. Each connection runs on its own small thread (so a stalled client
    /// cannot block the pipe), capped at MAX_IN_FLIGHT. The next instance is created
    /// before the current one is connected, so clients never see ERROR_PIPE_BUSY.
    fn run(mut self, handler: Handler) -> io::Result<()> {
        let mut cur = self.first.take().expect("bound");
        loop {
            let next = self.create(false)?;
            let connected = match unsafe { ConnectNamedPipe(HANDLE(cur.as_raw_handle()), None) } {
                Ok(()) => true,
                Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
            };
            let conn = std::mem::replace(&mut cur, next);
            if !connected {
                continue;
            }
            if self.in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                unsafe {
                    let _ = DisconnectNamedPipe(HANDLE(conn.as_raw_handle()));
                }
                continue;
            }
            let (handler, guard) = (handler.clone(), InFlight(self.in_flight.clone()));
            let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
                let _guard = guard; // released when the thread ends (or if spawn fails)
                handle(conn, &*handler);
            });
        }
    }
}

struct InFlight(Arc<AtomicUsize>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle(mut pipe: File, handler: &(dyn Fn(Request) -> Response + Send + Sync)) {
    let mut line = Vec::new();
    let read = BufReader::new((&pipe).take(MAX_REQUEST + 1)).read_until(b'\n', &mut line);
    // Never log request bodies: they can carry a password.
    let resp = match read {
        Ok(0) | Err(_) => None, // client went away without a request
        Ok(_) if line.len() as u64 > MAX_REQUEST => {
            // Swallow the rest of the line, or the client stays blocked in its write
            // and our flush below would wait on it forever.
            if line.last() != Some(&b'\n') && !drain_line(&mut pipe) {
                None
            } else {
                Some(Response::Err("request too large".into()))
            }
        }
        Ok(_) => Some(match decode::<Request>(&line) {
            Ok(req) => handler(req),
            Err(e) => Response::Err(format!("bad request: {e}")),
        }),
    };
    if let Some(resp) = resp {
        let _ = pipe.write_all(&encode(&resp));
        let _ = pipe.sync_all(); // FlushFileBuffers: wait until the client has read it
    }
    unsafe {
        let _ = DisconnectNamedPipe(HANDLE(pipe.as_raw_handle()));
    }
}

/// Discard input up to and including the next newline. False if EOF/error/too much first.
fn drain_line(pipe: &mut File) -> bool {
    let mut buf = [0u8; 8192];
    let mut total = 0;
    while total < MAX_DRAIN {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                if buf[..n].contains(&b'\n') {
                    return true;
                }
                total += n;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_core::ipc::{call, Phase, Request, Response, Status, PIPE};
    use ferry_core::location::Location;
    use std::sync::Arc;

    fn test_pipe() -> String {
        PIPE.replace("ferry", &format!("ferry-test-{}", std::process::id()))
    }

    fn start(name: &str) {
        let n = name.to_string();
        let h: Handler = Arc::new(|req| match req {
            Request::Status => Response::Status(Status {
                location: Location::Destination,
                phase: Phase::Idle,
                paused_until: None,
                remote_ok: true,
                bulk_blocked: None,
                trusting_vpn: false,
                cap_bytes: 1,
                carried_bytes: 0,
                rate_bps: 0,
                eta_secs: None,
                trips_behind: None,
                pairs: vec![],
                attention: vec![],
            }),
            _ => Response::Ok,
        });
        serve_on(&n, h).unwrap(); // returns once the pipe exists
    }

    #[test]
    fn pipe_answers_status() {
        let name = test_pipe();
        start(&name);
        match call(&name, &Request::Status).unwrap() {
            Response::Status(s) => assert_eq!(s.location, Location::Destination),
            r => panic!("{r:?}"),
        }
        // the server keeps going after a connection
        assert_eq!(call(&name, &Request::Quit).unwrap(), Response::Ok);
    }

    #[test]
    fn garbage_and_oversize_get_err_not_a_dead_server() {
        use std::io::{BufRead, BufReader, Write};
        let name = format!("{}-b", test_pipe());
        start(&name);
        for payload in [b"not json\n".to_vec(), [vec![b'a'; 100_000], vec![b'\n']].concat()] {
            let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
            let _ = f.write_all(&payload);
            let mut out = String::new();
            BufReader::new(f).read_line(&mut out).unwrap();
            assert!(out.contains("Err"), "{out}");
        }
        assert_eq!(call(&name, &Request::Resume).unwrap(), Response::Ok);
    }

    #[test]
    fn megabyte_request_is_answered_not_deadlocked() {
        use std::io::{BufRead, BufReader, Write};
        let name = format!("{}-c", test_pipe());
        start(&name);
        let (tx, rx) = std::sync::mpsc::channel();
        let n = name.clone();
        std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&n).unwrap();
            let mut payload = vec![b'a'; 1 << 20];
            payload.push(b'\n');
            f.write_all(&payload).unwrap();
            let mut out = String::new();
            BufReader::new(f).read_line(&mut out).unwrap();
            tx.send(out).unwrap();
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("deadlocked");
        assert!(out.contains("request too large"), "{out}");
        assert_eq!(call(&name, &Request::Resume).unwrap(), Response::Ok);
    }

    #[test]
    fn stalled_clients_do_not_stop_the_server_and_are_capped() {
        let name = format!("{}-d", test_pipe());
        start(&name);
        // 8 connections that never send anything fill every slot...
        let idle: Vec<_> = (0..MAX_IN_FLIGHT)
            .map(|_| loop {
                match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                    Ok(f) => break f,
                    Err(e) if e.raw_os_error() == Some(231) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    Err(e) => panic!("{e}"),
                }
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(300));
        // ...so the next one is dropped rather than served, but nothing hangs.
        assert!(call(&name, &Request::Status).is_err());
        drop(idle);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(call(&name, &Request::Resume).unwrap(), Response::Ok);
    }

    #[test]
    fn second_server_on_same_name_fails_at_startup() {
        let name = format!("{}-e", test_pipe());
        start(&name);
        assert!(serve_on(&name, Arc::new(|_| Response::Ok)).is_err());
    }
}
