//! Detached host transport. The original daemon owns the journal and PTYs;
//! reconnect only authenticates to its socket and never starts a replacement.
use super::{
    FileJournal, Host, Lifecycle, OperationStatus,
    native::{NativeBackend, Output, Screen},
    protocol::{Action, Binding, Effect, Id, Intent, Writer},
    wire,
};
use ansa_linux::{fs, local, managed};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read},
    net::Shutdown,
    os::{
        fd::{AsFd, OwnedFd},
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

const ARG: &str = "--internal-terminal-host-v1";
const EVIDENCE: &str = "evidence";
const ACCESS: &str = "attachment-v1.json";
const ENDPOINT: &str = "terminal-endpoint-v1.json";
const SOCKET: &str = "host.sock";
const MAX_CLIENTS: usize = 4;
const TICK: Duration = Duration::from_millis(5);
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
type NativeHost = Host<FileJournal, NativeBackend>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Access {
    binding: Binding,
    agent: String,
    human: String,
}

impl Access {
    fn key(&self, writer: Writer) -> &str {
        match writer {
            Writer::Agent => &self.agent,
            Writer::Human => &self.human,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Startup {
    binding: Binding,
    registry: PathBuf,
    disconnected_seconds: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CommandFrame {
    Attach {
        binding: Binding,
        writer: Writer,
        key: String,
        can_write: bool,
    },
    Execute {
        intent: Intent,
        input: Vec<u8>,
    },
    Operation {
        operation: Id,
    },
    List,
    Capture {
        terminal: Id,
        epoch: u64,
        after: u64,
    },
    Read {
        terminal: Id,
        epoch: u64,
        after: u64,
        limit: usize,
    },
    ReadAck {
        ticket: u64,
    },
    Shutdown,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Attached {
        connection: u64,
        epochs: Vec<(Id, u64)>,
    },
    Completed {
        status: OperationStatus,
    },
    Operation {
        status: Option<OperationStatus>,
    },
    Terminals {
        terminals: Vec<TerminalInfo>,
    },
    Screen {
        screen: Screen,
        ticket: u64,
    },
    Output {
        output: Output,
        ticket: u64,
    },
    ReadAccepted,
    Closed {
        clean: bool,
    },
    Error {
        code: String,
    },
}

/// Testable process owner and endpoint. Finishing the owner or dropping a
/// connection never replaces or restarts a daemon with the same binding.
pub struct Daemon {
    endpoint: Endpoint,
    completion: Receiver<io::Result<bool>>,
    completed: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    directory: PathBuf,
    binding: Binding,
    inode: (u64, u64),
    uid: u32,
    pid: u32,
    process: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalInfo {
    pub id: Id,
    pub label: String,
    pub lifecycle: Lifecycle,
    pub writer: Writer,
    pub epoch: u64,
    pub visible_after: u64,
}

impl Endpoint {
    /// Store the original process identity alongside its owning conversation.
    /// The immutable anchor lives outside the daemon's replaceable directory.
    pub fn remember(&self, conversation: &Path) -> io::Result<()> {
        if self.directory.parent() != Some(conversation) {
            return Err(io::Error::other("terminal_endpoint_directory"));
        }
        let owned = fs::private_directory(conversation)?;
        local::create(
            &owned,
            ENDPOINT,
            &serde_json::to_vec(self).map_err(io::Error::other)?,
        )
    }

    pub fn recall(conversation: &Path, binding: &Binding) -> io::Result<Self> {
        let owned = fs::private_directory(conversation)?;
        let record = local::read(&owned, ENDPOINT, 8192)?
            .ok_or_else(|| io::Error::other("terminal_endpoint_missing"))?;
        let endpoint: Self = serde_json::from_slice(&record).map_err(io::Error::other)?;
        if &endpoint.binding != binding || endpoint.directory.parent() != Some(conversation) {
            return Err(io::Error::other("terminal_endpoint_binding"));
        }
        Ok(endpoint)
    }
}

impl Daemon {
    pub fn launch(
        executable: &Path,
        directory: &Path,
        registry: &Path,
        binding: Binding,
        disconnected_seconds: u32,
    ) -> io::Result<Self> {
        if !(1..=super::protocol::MAX_LIFETIME_SECONDS).contains(&disconnected_seconds) {
            return Err(io::Error::other("terminal_disconnect_deadline"));
        }
        let (mut control, remote) = UnixStream::pair()?;
        control.set_read_timeout(Some(RPC_TIMEOUT))?;
        control.set_write_timeout(Some(RPC_TIMEOUT))?;
        let remote: OwnedFd = remote.into();
        let mut command = Command::new(executable);
        command
            .arg(ARG)
            .arg(directory)
            .stdin(Stdio::from(remote))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = managed::spawn(command, None, None)?;
        let started = wire::write(
            &mut control,
            0,
            Startup {
                binding: binding.clone(),
                registry: registry.to_path_buf(),
                disconnected_seconds,
            },
        )
        .and_then(|_| wire::read::<Endpoint>(&mut control, 0));
        let Ok(endpoint) = started else {
            // No PTY operations are possible before the named listener starts.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("terminal_host_start"));
        };
        if endpoint.directory != directory
            || endpoint.binding != binding
            || endpoint.pid != child.id()
        {
            return Err(io::Error::other("terminal_host_identity"));
        }
        let (finished, completion) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("ansa-terminal-host-reaper".into())
            .spawn(move || {
                let _ = finished.send(child.wait().map(|status| status.success()));
            })?;
        Ok(Self {
            endpoint,
            completion,
            completed: None,
        })
    }

    pub fn attach(&self, writer: Writer, can_write: bool) -> io::Result<Client> {
        Client::attach(&self.endpoint, writer, can_write)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn pid(&self) -> u32 {
        self.endpoint.pid
    }

    pub fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        if let Some(completed) = self.completed {
            return Ok(completed);
        }
        match self.completion.recv_timeout(timeout) {
            Ok(status) => {
                let completed = status?;
                self.completed = Some(completed);
                Ok(completed)
            }
            Err(RecvTimeoutError::Timeout) => Err(io::ErrorKind::TimedOut.into()),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::other("terminal_host_reaper")),
        }
    }
}

pub struct Client {
    stream: UnixStream,
    sequence: u64,
    connection: u64,
    usable: bool,
    epochs: Vec<(Id, u64)>,
}

impl Client {
    pub fn attach(endpoint: &Endpoint, writer: Writer, can_write: bool) -> io::Result<Self> {
        let owned = fs::private_directory(&endpoint.directory)?;
        let meta = owned.metadata()?;
        if (meta.dev(), meta.ino()) != endpoint.inode || meta.uid() != endpoint.uid {
            return Err(io::Error::other("terminal_store_replaced"));
        }
        let content = local::read(&owned, ACCESS, 8192)?
            .ok_or_else(|| io::Error::other("terminal_attachment_missing"))?;
        let access: Access = serde_json::from_slice(&content).map_err(io::Error::other)?;
        if access.binding != endpoint.binding {
            return Err(io::Error::other("terminal_binding"));
        }
        let stream = UnixStream::connect(endpoint.directory.join(SOCKET))?;
        let credentials = ansa_linux::process::peer_credentials(&stream)?;
        if credentials != (endpoint.uid, endpoint.pid)
            || process_identity(endpoint.pid)? != endpoint.process
        {
            return Err(io::Error::other("terminal_host_identity"));
        }
        stream.set_read_timeout(Some(RPC_TIMEOUT))?;
        stream.set_write_timeout(Some(RPC_TIMEOUT))?;
        let mut client = Self {
            stream,
            sequence: 0,
            connection: 0,
            usable: true,
            epochs: Vec::new(),
        };
        let Reply::Attached { connection, epochs } = client.call(CommandFrame::Attach {
            binding: endpoint.binding.clone(),
            writer,
            key: access.key(writer).to_owned(),
            can_write,
        })?
        else {
            return Err(io::Error::other("terminal_attach"));
        };
        client.connection = connection;
        client.epochs = epochs;
        Ok(client)
    }

    pub fn connection(&self) -> u64 {
        self.connection
    }

    pub fn attached_epoch(&self, terminal: &Id) -> Option<u64> {
        self.epochs
            .iter()
            .find(|(id, _)| id == terminal)
            .map(|(_, epoch)| *epoch)
    }

    fn call(&mut self, frame: CommandFrame) -> io::Result<Reply> {
        if !self.usable {
            return Err(io::Error::other("terminal_reattach_required"));
        }
        let reply = wire::write(&mut self.stream, self.sequence, frame)
            .and_then(|_| wire::read(&mut self.stream, self.sequence));
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                self.usable = false;
                let _ = self.stream.shutdown(Shutdown::Both);
                return Err(error);
            }
        };
        self.sequence += 1;
        match reply {
            Reply::Error { code } => Err(io::Error::other(code)),
            value => Ok(value),
        }
    }

    pub fn execute(&mut self, intent: Intent, input: Vec<u8>) -> io::Result<OperationStatus> {
        match self.call(CommandFrame::Execute { intent, input })? {
            Reply::Completed { status } => Ok(status),
            _ => Err(io::Error::other("terminal_protocol_reply")),
        }
    }

    pub fn operation(&mut self, operation: Id) -> io::Result<Option<OperationStatus>> {
        match self.call(CommandFrame::Operation { operation })? {
            Reply::Operation { status } => Ok(status),
            _ => Err(io::Error::other("terminal_protocol_reply")),
        }
    }

    pub fn terminals(&mut self) -> io::Result<Vec<TerminalInfo>> {
        match self.call(CommandFrame::List)? {
            Reply::Terminals { terminals } => Ok(terminals),
            _ => Err(io::Error::other("terminal_protocol_reply")),
        }
    }

    pub fn capture(&mut self, terminal: Id, epoch: u64, after: u64) -> io::Result<Screen> {
        match self.call(CommandFrame::Capture {
            terminal,
            epoch,
            after,
        })? {
            Reply::Screen { screen, ticket } => {
                self.ack(ticket)?;
                Ok(screen)
            }
            _ => Err(io::Error::other("terminal_protocol_reply")),
        }
    }

    pub fn read(
        &mut self,
        terminal: Id,
        epoch: u64,
        after: u64,
        limit: usize,
    ) -> io::Result<Output> {
        match self.call(CommandFrame::Read {
            terminal,
            epoch,
            after,
            limit,
        })? {
            Reply::Output { output, ticket } => {
                self.ack(ticket)?;
                Ok(output)
            }
            _ => Err(io::Error::other("terminal_protocol_reply")),
        }
    }

    pub fn shutdown(&mut self) -> io::Result<bool> {
        match self.call(CommandFrame::Shutdown)? {
            Reply::Closed { clean } => Ok(clean),
            _ => Err(io::Error::other("terminal_protocol_reply")),
        }
    }

    fn ack(&mut self, ticket: u64) -> io::Result<()> {
        match self.call(CommandFrame::ReadAck { ticket })? {
            Reply::ReadAccepted => Ok(()),
            _ => Err(io::Error::other("terminal_read_ack")),
        }
    }
}

#[derive(Clone, Copy)]
struct Attached {
    writer: Writer,
    can_write: bool,
}

struct Peer {
    stream: UnixStream,
    reader: wire::Reader,
    sequence: u64,
    connection: u64,
    attached: Option<Attached>,
    handshake_deadline: Instant,
    read: Option<PendingRead>,
}

struct PendingRead {
    terminal: Id,
    ticket: u64,
    deadline: Instant,
}

pub fn private_entry(args: &[String]) -> Option<io::Result<()>> {
    (args.get(1).map(String::as_str) == Some(ARG)).then(|| {
        if args.len() != 3 {
            return Err(io::Error::other("terminal_host_args"));
        }
        serve(Path::new(&args[2]))
    })
}

fn random_key() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("terminal_attachment_random"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn serve(path: &Path) -> io::Result<()> {
    let mut bootstrap = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    bootstrap.peer_addr()?;
    bootstrap.set_read_timeout(Some(RPC_TIMEOUT))?;
    let Startup {
        binding,
        registry,
        disconnected_seconds,
    } = wire::read(&mut bootstrap, 0)?;
    if !(1..=super::protocol::MAX_LIFETIME_SECONDS).contains(&disconnected_seconds)
        || path.starts_with(&binding.workspace)
        || !binding.validate()
    {
        return Err(io::Error::other("terminal_binding"));
    }
    let directory = fs::create_private_leaf(path)?;
    let metadata = directory.metadata()?;
    let uid = metadata.uid();
    let endpoint = Endpoint {
        directory: path.to_path_buf(),
        binding: binding.clone(),
        inode: (metadata.dev(), metadata.ino()),
        uid,
        pid: std::process::id(),
        process: process_identity(std::process::id())?,
    };
    let mut host = Host::create_file(
        &path.join(EVIDENCE),
        binding.clone(),
        NativeBackend::new(std::env::current_exe()?, registry),
    )
    .map_err(io::Error::other)?;
    let access = Access {
        binding,
        agent: random_key()?,
        human: random_key()?,
    };
    local::create(
        &directory,
        ACCESS,
        &serde_json::to_vec(&access).map_err(io::Error::other)?,
    )?;
    let listener = UnixListener::bind(path.join(SOCKET))?;
    std::fs::set_permissions(path.join(SOCKET), std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    wire::write(&mut bootstrap, 0, endpoint)?;
    drop(bootstrap);
    let disconnect = Duration::from_secs(u64::from(disconnected_seconds));
    let host_deadline =
        Instant::now() + Duration::from_secs(u64::from(super::protocol::MAX_LIFETIME_SECONDS));
    let mut detached_at = Instant::now();
    let mut peers = Vec::<Peer>::new();
    let mut connection = 0;
    let mut shutdown = None;
    while shutdown.is_none() {
        if peers.iter().any(|peer| peer.attached.is_some()) {
            detached_at = Instant::now();
        }
        if detached_at.elapsed() >= disconnect || Instant::now() >= host_deadline {
            break;
        }
        if let Ok((stream, _)) = listener.accept()
            && peers.len() < MAX_CLIENTS
            && authorized_peer(&stream, uid)
        {
            stream.set_nonblocking(true)?;
            connection += 1;
            peers.push(Peer {
                stream,
                reader: wire::Reader::default(),
                sequence: 0,
                connection,
                attached: None,
                handshake_deadline: Instant::now() + RPC_TIMEOUT,
                read: None,
            });
        }
        let mut index = 0;
        while index < peers.len() {
            let now = Instant::now();
            if (peers[index].attached.is_none() && now >= peers[index].handshake_deadline)
                || peers[index]
                    .read
                    .as_ref()
                    .is_some_and(|read| now >= read.deadline)
            {
                peers.swap_remove(index);
                continue;
            }
            let frame = {
                let peer = &mut peers[index];
                peer.reader
                    .poll::<CommandFrame>(&mut peer.stream, peer.sequence)
            };
            let reply = match frame {
                Ok(Some(CommandFrame::Attach {
                    binding,
                    writer,
                    key,
                    can_write,
                })) => {
                    let already = peers.iter().any(|peer| {
                        peer.attached
                            .is_some_and(|role| role.writer == writer && role.can_write)
                    });
                    let peer = &mut peers[index];
                    if peer.attached.is_some()
                        || binding != access.binding
                        || key != access.key(writer)
                        || (already && can_write)
                    {
                        peers.swap_remove(index);
                        continue;
                    }
                    if can_write
                        && (peers.iter().any(|peer| peer.read.is_some())
                            || !renew(&mut host, writer))
                    {
                        peers.swap_remove(index);
                        continue;
                    }
                    let peer = &mut peers[index];
                    peer.attached = Some(Attached { writer, can_write });
                    Reply::Attached {
                        connection: peer.connection,
                        epochs: host
                            .terminals()
                            .iter()
                            .filter(|(_, terminal)| {
                                terminal.writer == writer && terminal.lifecycle == Lifecycle::Live
                            })
                            .map(|(id, terminal)| (id.clone(), terminal.epoch))
                            .collect(),
                    }
                }
                Ok(Some(CommandFrame::ReadAck { ticket })) => {
                    if peers[index]
                        .read
                        .take()
                        .is_some_and(|read| read.ticket == ticket)
                    {
                        Reply::ReadAccepted
                    } else {
                        peers.swap_remove(index);
                        continue;
                    }
                }
                Ok(Some(frame)) => {
                    let Some(role) = peers[index].attached else {
                        peers.swap_remove(index);
                        continue;
                    };
                    if peers[index].read.is_some() {
                        peers.swap_remove(index);
                        continue;
                    }
                    let conflicts = match &frame {
                        CommandFrame::Execute { intent, .. }
                            if matches!(
                                intent.action,
                                Action::Takeover | Action::Handback { .. }
                            ) =>
                        {
                            peers.iter().any(|peer| {
                                peer.read
                                    .as_ref()
                                    .is_some_and(|read| read.terminal == intent.terminal)
                            })
                        }
                        _ => false,
                    };
                    if conflicts {
                        Reply::Error {
                            code: "terminal_read_in_flight".into(),
                        }
                    } else {
                        let observed = match &frame {
                            CommandFrame::Capture { terminal, .. }
                            | CommandFrame::Read { terminal, .. } => Some(terminal.clone()),
                            _ => None,
                        };
                        let ticket = peers[index].sequence;
                        let reply = dispatch(&mut host, role, frame, ticket, &mut shutdown);
                        if let Some(terminal) = observed
                            && matches!(reply, Reply::Screen { .. } | Reply::Output { .. })
                        {
                            peers[index].read = Some(PendingRead {
                                terminal,
                                ticket,
                                deadline: Instant::now() + RPC_TIMEOUT,
                            });
                        }
                        reply
                    }
                }
                Ok(None) => {
                    index += 1;
                    continue;
                }
                Err(_) => {
                    peers.swap_remove(index);
                    continue;
                }
            };
            let peer = &mut peers[index];
            if wire::write(&mut peer.stream, peer.sequence, reply).is_err() {
                peers.swap_remove(index);
            } else {
                peer.sequence += 1;
                index += 1;
            }
            if shutdown.is_some() {
                break;
            }
        }
        thread::sleep(TICK);
    }
    if shutdown.unwrap_or_else(|| close_and_seal(&mut host)) {
        Ok(())
    } else {
        Err(io::Error::other("terminal_cleanup_unknown"))
    }
}

fn authorized_peer(stream: &UnixStream, uid: u32) -> bool {
    ansa_linux::process::peer_credentials(stream).is_ok_and(|credential| credential.0 == uid)
}

fn process_identity(pid: u32) -> io::Result<String> {
    let mut stat = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))?
        .take(4097)
        .read_to_string(&mut stat)?;
    let mut boot = String::new();
    std::fs::File::open("/proc/sys/kernel/random/boot_id")?
        .take(128)
        .read_to_string(&mut boot)?;
    let start = stat
        .rsplit_once(") ")
        .and_then(|(_, suffix)| suffix.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok());
    let Some(start) = start.filter(|_| stat.len() <= 4096 && boot.trim().len() == 36) else {
        return Err(io::Error::other("terminal_process_identity"));
    };
    Ok(format!("{}:{start}", boot.trim()))
}

fn dispatch(
    host: &mut NativeHost,
    role: Attached,
    frame: CommandFrame,
    ticket: u64,
    shutdown: &mut Option<bool>,
) -> Reply {
    let reply = match frame {
        CommandFrame::Execute { intent, input }
            if role.can_write && !matches!(intent.action, Action::Renew) =>
        {
            host.execute(role.writer, intent, &input)
                .map(|status| Reply::Completed { status })
        }
        CommandFrame::Operation { operation } => Ok(Reply::Operation {
            status: host.operation(&operation),
        }),
        CommandFrame::List => Ok(Reply::Terminals {
            terminals: host
                .terminals()
                .iter()
                .map(|(id, terminal)| TerminalInfo {
                    id: id.clone(),
                    label: terminal.launch.label.clone(),
                    lifecycle: terminal.lifecycle,
                    writer: terminal.writer,
                    epoch: terminal.epoch,
                    visible_after: terminal.visible_after,
                })
                .collect(),
        }),
        CommandFrame::Capture {
            terminal,
            epoch,
            after,
        } => host
            .capture(role.writer, &terminal, epoch, after)
            .map(|screen| Reply::Screen { screen, ticket }),
        CommandFrame::Read {
            terminal,
            epoch,
            after,
            limit,
        } => host
            .read_output(role.writer, &terminal, epoch, after, limit)
            .map(|output| Reply::Output { output, ticket }),
        CommandFrame::Shutdown if role.writer == Writer::Human && role.can_write => {
            let clean = close_and_seal(host);
            *shutdown = Some(clean);
            Ok(Reply::Closed { clean })
        }
        _ => Err("terminal_attachment_access"),
    };
    reply.unwrap_or_else(|code| Reply::Error { code: code.into() })
}

fn renew(host: &mut NativeHost, writer: Writer) -> bool {
    let open: Vec<_> = host
        .terminals()
        .iter()
        .filter(|(_, terminal)| terminal.writer == writer && terminal.lifecycle == Lifecycle::Live)
        .map(|(id, terminal)| (id.clone(), terminal.epoch))
        .collect();
    open.into_iter().all(|(terminal, epoch)| {
        let Some(operation) = random_key()
            .ok()
            .and_then(|key| Id::try_from(format!("renew_{key}")).ok())
        else {
            return false;
        };
        let invocation_digest = (writer == Writer::Agent)
            .then(|| super::protocol::sha256(operation.as_str().as_bytes()));
        host.execute(
            writer,
            Intent {
                operation,
                terminal,
                binding: host.binding().clone(),
                writer,
                epoch,
                invocation_digest,
                action: Action::Renew,
            },
            &[],
        ) == Ok(OperationStatus::Durable(Effect::Applied))
    })
}

fn close_and_seal(host: &mut NativeHost) -> bool {
    close_owned(host) && host.seal_clean().is_ok()
}

pub(super) fn close_owned(host: &mut NativeHost) -> bool {
    let open: Vec<_> = host
        .terminals()
        .iter()
        .filter(|(_, value)| value.lifecycle != Lifecycle::Closed)
        .map(|(id, value)| (id.clone(), value.epoch))
        .collect();
    let mut clean = true;
    for (terminal, epoch) in open {
        let operation = random_key()
            .ok()
            .and_then(|key| Id::try_from(format!("daemon_close_{key}")).ok());
        let Some(operation) = operation else {
            clean = false;
            continue;
        };
        let intent = Intent {
            operation,
            terminal,
            binding: host.binding().clone(),
            writer: Writer::Human,
            epoch,
            invocation_digest: None,
            action: Action::Close,
        };
        clean &= host.execute(Writer::Human, intent, &[])
            == Ok(OperationStatus::Durable(Effect::Applied));
    }
    if !clean {
        host.emergency_stop();
    }
    clean
}
