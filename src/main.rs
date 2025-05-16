use clap::{CommandFactory, Parser};
use color_eyre::{Result, eyre::bail};
use conntester::ConnectionTester;
use mio::net::TcpListener;
use shared_child::SharedChild;
use std::{
    net::SocketAddrV4,
    num::NonZeroU16,
    process::{Command, ExitStatus},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self},
    time::{Duration, Instant},
};

mod cli;
mod conntester;

const PORT_FORWARD_HOST: &str = "127.0.0.1";

fn main() -> Result<()> {
    color_eyre::install().unwrap();

    let mut args = cli::Cli::parse();
    if let Some(shell) = args.completions {
        clap_complete::generate(
            shell,
            &mut cli::Cli::command(),
            "robotssh",
            &mut std::io::stdout(),
        );
        return Ok(());
    }
    if args.ssh_args.is_empty() {
        clap::Command::print_long_help(&mut cli::Cli::command()).unwrap();
        return Ok(());
    }

    args.tester_options.net_timeout = args
        .tester_options
        .net_timeout
        .min(args.tester_options.poll_time / 2);

    let parent_start_time = Instant::now();

    // Build SSH command
    let mut ssh = Command::new(args.ssh_path);
    let mut listen_port = None;
    if let Some(monitor_port) = args.monitor_port.map(NonZeroU16::get) {
        ssh.arg("-L");
        ssh.arg(format!(
            "{monitor_port}:{PORT_FORWARD_HOST}:{echo_port}",
            echo_port = args
                .echo_server_port
                .map(|p| p.get())
                .unwrap_or(monitor_port)
        ));
        if args.echo_server_port.is_none() {
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
        )
        .min(args.backoff_upper_bound);
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
                    _ = kill_tx.try_send(());
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
    Ok(())
}

fn backoff(tries: u32, fast_tries: u32, poll_time: Duration) -> Duration {
    if tries < fast_tries {
        return Duration::ZERO;
    }
    let slow_try = tries - fast_tries;
    let backoff = poll_time / 100 * (slow_try * (slow_try * 3));
    backoff.max(poll_time)
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
