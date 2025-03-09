use clap::{Args, Parser};
use color_eyre::{Result, eyre::bail};
use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener, TcpStream},
};
use shared_child::SharedChild;
use std::{
    ffi::OsString,
    io::{ErrorKind, Read, Write},
    net::{SocketAddrV4, ToSocketAddrs},
    num::NonZeroU16,
    path::PathBuf,
    process::{Command, ExitCode, ExitStatus},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PORT_FORWARD_HOST: &str = "127.0.0.1";

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long = "monitor", short = 'm')]
    monitor_port: Option<NonZeroU16>,

    #[arg(long = "echo", short = 'e')]
    echo_port: Option<NonZeroU16>,

    /// Arguments passed to SSH
    #[arg(last = true)]
    ssh_args: Vec<OsString>,

    /// The path to SSH executable
    #[arg(env, long, default_value = "ssh")]
    ssh_path: PathBuf,

    /// time to "make it out of the gate"
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "30s")]
    gate_time: Duration,

    /// how many times to run (default no limit)
    #[arg(env, long)]
    max_start: Option<usize>,

    /// how many times to retry the connection without backing off
    #[arg(env, long, default_value_t = 5)]
    backoff_fast_tries: u32,

    /// how long can the process/daemon live
    #[arg(env, long, value_parser = humantime::parse_duration)]
    max_lifetime: Option<Duration>,

    #[command(flatten)]
    tester_options: ConnectionTesterOptions,
}

fn main() -> Result<ExitCode> {
    color_eyre::install().unwrap();
    let args = dbg!(Cli::parse());

    let parent_start_time = Instant::now();

    // Build SSH command
    let mut ssh = Command::new(args.ssh_path);
    let mut listen_port = None;
    if let Some(monitor_port) = args.monitor_port.map(|p| p.get()) {
        ssh.arg("-L");
        ssh.arg(format!(
            "{monitor_port}:{PORT_FORWARD_HOST}:{echo_port}",
            echo_port = args.echo_port.map(|p| p.get()).unwrap_or(monitor_port)
        ));
        if args.echo_port.is_none() {
            let Some(echo_port) = monitor_port.checked_add(1) else {
                bail!("Calculating the default reply port(--monitor_port + 1) overflowed")
            };
            listen_port = Some(echo_port);
            ssh.arg("-R");
            ssh.arg(format!("{monitor_port}:{PORT_FORWARD_HOST}:{echo_port}",));
        }
    }
    let mut socket = None;
    if let Some(listen_port) = listen_port {
        socket = Some(Arc::new(Mutex::new(
            TcpListener::bind(
                SocketAddrV4::new(PORT_FORWARD_HOST.parse().unwrap(), listen_port).into(),
            )
            .unwrap(),
        )));
    }
    ssh.args(args.ssh_args);

    // Minimum time we have to stay up to avoid backoff behaviour.
    // With default poll_time this is 60 secs.
    let min_uptime_to_reset_backoff =
        (args.tester_options.poll_time / 10).max(Duration::from_secs(10));

    let mut start_count = 0;
    let mut tries = 0;
    loop {
        if args.max_start.is_some_and(|max| start_count > max) {
            // We ran out of restarts
            break;
        }
        let backoff = backoff(
            tries,
            args.backoff_fast_tries,
            args.tester_options.poll_time,
        );
        std::thread::sleep(backoff);

        let ssh_start = Instant::now();
        let ssh = SharedChild::spawn(&mut ssh).unwrap();

        let (kill_tx, kill_rx) = mpsc::sync_channel(2);
        let until_kill = args
            .max_lifetime
            .map(|max_lifetime| max_lifetime.saturating_sub(parent_start_time.elapsed()));

        let connection_tester = args.monitor_port.map(|mon_port| {
            let kill_tx = kill_tx.clone();
            ConnectionTester::spawn(
                args.tester_options.clone(),
                socket.clone(),
                (PORT_FORWARD_HOST, mon_port.get()),
                move || {
                    _ = kill_tx.send(());
                },
            )
        });

        let exit = run_ssh(ssh, until_kill, (kill_tx, kill_rx));
        start_count += 1;
        let mut connection_failed = false;
        if let Some(tester) = connection_tester {
            connection_failed = tester.join();
        }
        if ssh_start.elapsed() >= min_uptime_to_reset_backoff {
            tries = 0
        } else {
            tries += 1;
        };

        let status = match exit {
            // max_lifetime elapsed
            SshExit::Killed if !connection_failed => break,
            SshExit::Killed => {
                // killed due to bad connection
                eprintln!("SSH connection dropped. Restarting");
                continue;
            }
            SshExit::Status(status) if status.success() => {
                // ssh existed with code 0
                break;
            }
            SshExit::Status(status) => status,
        };

        #[cfg(unix)]
        'signal: {
            use std::os::unix::process::ExitStatusExt;
            /// Interactive attention signal.  
            const SIGINT: i32 = 2;
            /// Termination request.  
            const SIGTERM: i32 = 15;
            /// Killed.  
            const SIGKILL: i32 = 9;
            let Some(signal) = status.signal() else {
                break 'signal;
            };
            match signal {
                SIGINT | SIGTERM | SIGKILL => {
                    // Process was signalled to exit
                    eprintln!("SSH was terminated by signal {signal}, parent exiting");
                    std::process::exit(signal);
                }
                _ => {
                    eprintln!("SSH exited due to signal {signal}. Restarting.")
                }
            }
        }
        let Some(code) = status.code() else {
            // Terminated by signal, which was already handled
            continue;
        };
        if start_count == 1 && ssh_start.elapsed() <= args.gate_time {
            // SSH exited too quickly, give up
            eprintln!(
                "SSH exited too fast(less than gate-time, currently set to {gt:?}) with error status {code}. Exiting.",
                gt = args.gate_time
            );
            std::process::exit(code);
        }
        match code {
            255 => {
                // we can get this on an initial connection if the connection itself is ok,
                // but authentication fails. But there's no way to do this nicely: we don't
                // have enough info from the ssh session and we get the same exit status
                // from a dropped connection. Hence the gate_time above.
                eprintln!("SSH exited with error status {code}, restarting");
            }
            // Can exit with 2 with temporary issues setting up tunnels
            2
            // the first time, it could be any of a number of errors; so we exit and let the 
            // user fix. But if been running ok already, then network may be down and then 
            // ssh fails exit(1) on the attempt to reconnect....so we try to restart.
            | 1 if start_count > 1 || args.gate_time.is_zero() => {
                eprintln!("SSH exited with errror status {code}, restarting")
            }
            // Remote command error status
            code => {
                eprintln!("SSH exited with status {code}, exiting");
                std::process::exit(code);
            },
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn backoff(tries: u32, fast_tries: u32, poll_time: Duration) -> Duration {
    if tries < fast_tries {
        return Duration::ZERO;
    }
    let slow_try = tries - fast_tries;
    let backoff = poll_time / 100 * (slow_try * (slow_try * 3));
    backoff.max(poll_time)
}

struct ConnectionTester {
    stop_tx: SyncSender<()>,
    handle: JoinHandle<bool>,
}

#[derive(Args, Debug, Clone)]
struct ConnectionTesterOptions {
    /// Default connection poll time
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "10m")]
    poll_time: Duration,

    /// Initial connection poll time
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "10m")]
    first_poll_time: Duration,

    #[arg(env, long, default_value = "")]
    echo_message: Arc<str>,

    /// how many times to retry the network connection when monitoring
    #[arg(env, long, default_value_t = 3)]
    max_conn_tries: usize,

    /// timeout on network data
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "15s")]
    net_timeout: Duration,
}

impl ConnectionTester {
    fn spawn(
        opts: ConnectionTesterOptions,
        listener: Option<Arc<Mutex<TcpListener>>>,
        target: impl ToSocketAddrs + Send + 'static + Clone,
        on_death: impl FnOnce() + 'static + Send,
    ) -> Self {
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            let random_id: [u8; 16] = rand::random();
            let name = b"robotssh";
            let payload: Vec<u8> = name
                .iter()
                .copied()
                .chain(random_id)
                .chain(opts.echo_message.bytes())
                .collect();
            let mut listener = listener.as_ref().map(|l| l.lock().unwrap());

            if stop_rx.recv_timeout(opts.first_poll_time).is_ok() {
                return false;
            };

            let mut tries = 0;
            loop {
                if !Self::run_test(
                    listener.as_deref_mut(),
                    target.clone(),
                    opts.net_timeout,
                    &payload,
                ) {
                    tries += 1;
                } else {
                    tries = 0;
                };
                if tries > opts.max_conn_tries {
                    break;
                }
                if stop_rx.recv_timeout(opts.poll_time).is_ok() {
                    return false;
                };
            }
            on_death();
            true
        });
        ConnectionTester { handle, stop_tx }
    }
    fn run_test(
        listener: Option<&mut TcpListener>,
        target: impl ToSocketAddrs + Send,
        timeout: Duration,
        payload: &[u8],
    ) -> bool {
        const WRITER: Token = Token(0);
        const CONN: Token = Token(1);
        const READER: Token = Token(2);
        let mut to_be_sent = payload;
        let mut to_be_recieved = payload;
        let target = target.to_socket_addrs().unwrap().next().unwrap();
        let Ok(mut writer) = TcpStream::connect(target) else {
            return false;
        };
        let mut reader = None;
        let mut events = Events::with_capacity(128);
        let mut poll = Poll::new().unwrap();
        if let Some(listener) = listener {
            poll.registry()
                .register(&mut writer, WRITER, Interest::WRITABLE)
                .unwrap();
            poll.registry()
                .register(listener, CONN, Interest::READABLE)
                .unwrap();
            loop {
                // Poll Mio for events, blocking until we get an event.
                _ = poll.poll(&mut events, Some(timeout));

                if events.is_empty() {
                    // timed out
                    return false;
                }
                // Process each event.
                for event in events.iter() {
                    // We can use the token we previously provided to `register` to
                    // determine for which socket the event is.
                    match event.token() {
                        CONN if event.is_readable() => {
                            let Ok((mut connection, _)) = listener.accept() else {
                                return false;
                            };
                            poll.registry().deregister(listener).unwrap();
                            poll.registry()
                                .register(&mut connection, READER, Interest::READABLE)
                                .unwrap();
                            reader = Some(connection)
                        }
                        WRITER if event.is_writable() => {
                            let Ok(written) = writer.write(to_be_sent) else {
                                return false;
                            };
                            to_be_sent = &to_be_sent[written..];
                            if to_be_sent.is_empty() {
                                poll.registry().deregister(&mut writer).unwrap();
                            }
                        }
                        READER if event.is_readable() => {
                            let reader = reader.as_mut().unwrap();
                            let mut buf = [0; 128];
                            let read = match reader.read(&mut buf) {
                                Err(err)
                                    if matches!(
                                        err.kind(),
                                        ErrorKind::Interrupted | ErrorKind::WouldBlock
                                    ) =>
                                {
                                    continue;
                                }
                                Ok(0) | Err(_) => return false,
                                Ok(read) => read,
                            };
                            let Some((expected, left)) = to_be_recieved.split_at_checked(read)
                            else {
                                return false;
                            };
                            let read = &buf[..read];
                            if expected != read {
                                return false;
                            }
                            to_be_recieved = left;
                            if to_be_recieved.is_empty() {
                                poll.registry().deregister(reader).unwrap();
                                return true;
                            }
                        }
                        // We don't expect any events with tokens other than those we provided.
                        _ => unreachable!(),
                    }
                }
            }
        } else {
            poll.registry()
                .register(&mut writer, WRITER, Interest::WRITABLE | Interest::READABLE)
                .unwrap();
            loop {
                let start = Instant::now();
                // Poll Mio for events, blocking until we get an event.
                _ = poll.poll(&mut events, Some(timeout.saturating_sub(start.elapsed())));

                if events.is_empty() {
                    // timed out
                    return false;
                }

                // Process each event.
                for event in events.iter() {
                    if event.is_writable() && !to_be_sent.is_empty() {
                        let wrote = match writer.write(to_be_sent) {
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    ErrorKind::Interrupted | ErrorKind::WouldBlock
                                ) =>
                            {
                                continue;
                            }
                            Ok(0) | Err(_) => return false,
                            Ok(wrote) => wrote,
                        };
                        to_be_sent = &to_be_sent[wrote..];
                    }
                    if event.is_readable() {
                        let mut buf = [0; 128];
                        let read = match writer.read(&mut buf) {
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    ErrorKind::Interrupted | ErrorKind::WouldBlock
                                ) =>
                            {
                                continue;
                            }
                            Ok(0) | Err(_) => return false,
                            Ok(read) => read,
                        };
                        let Some((expected, left)) = to_be_recieved.split_at_checked(read) else {
                            return false;
                        };
                        let read = &buf[..read];
                        if expected != read {
                            return false;
                        }
                        to_be_recieved = left;
                        if to_be_recieved.is_empty() {
                            poll.registry().deregister(&mut writer).unwrap();
                            return true;
                        }
                    }
                }
            }
        }
    }
    // Joins the thread, returning true if ConnectionTester called the on_death callback
    fn join(self) -> bool {
        _ = self.stop_tx.try_send(());
        self.handle.join().unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
enum SshExit {
    Status(ExitStatus),
    Killed,
}

fn run_ssh(
    ssh: SharedChild,
    max_lifetime: Option<Duration>,
    kill_channel: (SyncSender<()>, Receiver<()>),
) -> SshExit {
    let start = Instant::now();
    let exited = AtomicBool::new(false);
    thread::scope(|s| {
        let ssh = &ssh;
        let start = &start;
        let exited = &exited;
        let killer = s.spawn(move || {
            if let Some(max) = max_lifetime {
                let until_kill = max.saturating_sub(start.elapsed());
                _ = kill_channel.1.recv_timeout(until_kill);
            } else {
                _ = kill_channel.1.recv();
            };
            if exited.load(Ordering::Acquire) {
                return false;
            }
            ssh.kill().unwrap();
            true
        });
        let status = ssh.wait().unwrap();
        exited.store(true, Ordering::Release);
        _ = kill_channel.0.try_send(());
        if killer.join().unwrap() {
            return SshExit::Killed;
        }
        SshExit::Status(status)
    })
}
