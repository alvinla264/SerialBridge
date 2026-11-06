use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Sender};
use serialbridge_protocol::{
    encode_line, paths, ErrorBody, ErrorCode, ProtocolError, Request, RequestMessage, ResponseData,
    ServerMessage, MAX_MESSAGE_BYTES,
};

use crate::config::CoreConfig;
use crate::engine::{Engine, WriteStart};

/// Serve the core on its socket until it shuts down.
///
/// One core runs per user: the socket directory is locked, so a second core
/// exits quietly and clients converge on the first. The socket is 0600 and
/// every accepted connection is checked against the core's effective uid.
pub fn run(cfg: CoreConfig) -> Result<(), Box<dyn std::error::Error>> {
    let socket = cfg.socket.clone();
    let directory = socket
        .parent()
        .ok_or_else(|| format!("socket path {} has no directory", socket.display()))?
        .to_path_buf();
    let directory_existed = directory.exists();
    fs::create_dir_all(&directory)?;
    if !directory_existed {
        // Keep a directory we create private to this user. A caller-provided
        // directory is left as it is; connections are checked by uid anyway.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }

    let _lock = match lock_exclusive(&directory.join("core.lock"))? {
        Some(file) => file,
        // Another core owns this socket directory.
        None => return Ok(()),
    };

    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let (shutdown_tx, shutdown_rx) = unbounded();
    let engine = Engine::new(cfg, socket.clone(), shutdown_tx);
    engine.start();
    let uid = paths::effective_uid();

    loop {
        if let Ok(reason) = shutdown_rx.try_recv() {
            engine.shutdown_all(&reason);
            break;
        }
        if engine.is_shutting_down() {
            engine.shutdown_all("shutting down");
            break;
        }
        match listener.accept() {
            Ok((stream, _address)) => {
                stream.set_nonblocking(false)?;
                let engine = Arc::clone(&engine);
                std::thread::Builder::new()
                    .name("serial-core-client".into())
                    .spawn(move || handle_connection(engine, stream, uid))?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }

    drop(listener);
    let _ = fs::remove_file(&socket);
    Ok(())
}

fn lock_exclusive(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    // SAFETY: flock on a valid fd; the File is kept alive by the caller.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(Some(file))
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    getsockopt(stream, PeerCredentials)
        .ok()
        .map(|credentials| credentials.uid())
}

type RequestSender = Sender<Option<Result<RequestMessage, String>>>;

fn handle_connection(engine: Arc<Engine>, stream: UnixStream, uid: u32) {
    if peer_uid(&stream) != Some(uid) {
        // Not the same user: refuse without saying anything useful.
        return;
    }
    let (event_tx, event_rx) = unbounded();
    let client_id = engine.register_client(event_tx);

    // Responses, events and waited-write results all go through the handler
    // thread, so writes to the socket stay ordered and interleaving-free.
    let (response_tx, response_rx) = unbounded::<ServerMessage>();
    let (request_tx, request_rx): (RequestSender, _) = bounded(64);
    let reader_stream = match stream.try_clone() {
        Ok(reader) => reader,
        Err(_) => {
            engine.unregister_client(client_id);
            return;
        }
    };
    if std::thread::Builder::new()
        .name("serial-core-reader".into())
        .spawn(move || read_requests(reader_stream, request_tx))
        .is_err()
    {
        engine.unregister_client(client_id);
        return;
    }

    let mut writer = BufWriter::new(stream);
    loop {
        crossbeam_channel::select! {
            recv(request_rx) -> message => match message {
                Ok(Some(Ok(request))) => {
                    let id = request.id;
                    let message = match request.request {
                        // Enqueue the write on this thread (arrival order) and
                        // only wait for its response lines off-thread, so
                        // later requests are not delayed or reordered.
                        Request::Write(params) => match engine.begin_write(client_id, params) {
                            Ok(WriteStart::Done(result)) => ServerMessage::Response {
                                id,
                                result: ResponseData::Written(result),
                            },
                            Ok(WriteStart::Pending(pending)) => {
                                let worker = Arc::clone(&engine);
                                let sender = response_tx.clone();
                                let spawned = std::thread::Builder::new()
                                    .name("serial-core-write-wait".into())
                                    .spawn(move || {
                                        let message = response(
                                            worker.finish_write(pending).map(ResponseData::Written),
                                            id,
                                        );
                                        let _ = sender.send(message);
                                    });
                                if spawned.is_err() {
                                    ServerMessage::Error {
                                        id,
                                        error: ErrorBody::new(
                                            ErrorCode::Internal,
                                            "could not await the write's response lines",
                                        ),
                                    }
                                } else {
                                    // The worker sends the answer.
                                    continue;
                                }
                            }
                            Err(error) => ServerMessage::Error { id, error },
                        },
                        other => response(engine.handle(client_id, other), id),
                    };
                    if write_message(&mut writer, &message).is_err() {
                        break;
                    }
                }
                // Malformed request or closed input: end this connection.
                _ => break,
            },
            recv(response_rx) -> message => match message {
                Ok(message) => {
                    if write_message(&mut writer, &message).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            recv(event_rx) -> message => match message {
                Ok(message) => {
                    if write_message(&mut writer, &message).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
        }
    }
    // Shut down both halves: the reader clone must see EOF too, or the client
    // would never learn the connection ended.
    let _ = writer.get_ref().shutdown(std::net::Shutdown::Both);
    engine.unregister_client(client_id);
}

fn response(result: Result<ResponseData, ErrorBody>, id: u64) -> ServerMessage {
    match result {
        Ok(result) => ServerMessage::Response { id, result },
        Err(error) => ServerMessage::Error { id, error },
    }
}

fn read_requests(stream: UnixStream, tx: RequestSender) {
    let mut reader = BufReader::new(stream);
    loop {
        let line = match serialbridge_protocol::read_line(&mut reader, MAX_MESSAGE_BYTES) {
            Ok(Some(line)) => line,
            // EOF between messages.
            Ok(None) => break,
            Err(error) => {
                let _ = tx.send(Some(Err(error.to_string())));
                break;
            }
        };
        match serialbridge_protocol::decode_line::<RequestMessage>(&line) {
            Ok(request) => {
                if tx.send(Some(Ok(request))).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Some(Err(error.to_string())));
                break;
            }
        }
    }
    let _ = tx.send(None);
}

fn write_message(writer: &mut BufWriter<UnixStream>, message: &ServerMessage) -> io::Result<()> {
    match encode_line(message) {
        Ok(line) => {
            writer.write_all(&line)?;
            writer.flush()
        }
        Err(ProtocolError::TooLarge { .. }) => {
            // Never drop a connection over one oversized answer: tell the
            // requester to ask for less and keep the session alive.
            let id = match message {
                ServerMessage::Response { id, .. } | ServerMessage::Error { id, .. } => *id,
                ServerMessage::Event { .. } => return Ok(()),
            };
            let fallback = ServerMessage::Error {
                id,
                error: ErrorBody::new(
                    ErrorCode::BadRequest,
                    "response too large; request fewer lines",
                ),
            };
            let line = encode_line(&fallback).map_err(io::Error::other)?;
            writer.write_all(&line)?;
            writer.flush()
        }
        Err(error) => Err(io::Error::other(error)),
    }
}
