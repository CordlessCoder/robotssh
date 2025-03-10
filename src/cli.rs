use clap::{Args, Parser};
use std::{ffi::OsString, num::NonZeroU16, path::PathBuf, sync::Arc, time::Duration};

/// A modern rewrite of autossh
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    #[arg(long = "monitor", short = 'm')]
    pub monitor_port: Option<NonZeroU16>,

    #[arg(long = "echo", short = 'e')]
    pub echo_port: Option<NonZeroU16>,

    /// Arguments passed to SSH
    #[arg(last = true)]
    pub ssh_args: Vec<OsString>,

    /// The path to SSH executable
    #[arg(env, long, default_value = "ssh")]
    pub ssh_path: PathBuf,

    /// time to "make it out of the gate"
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "30s")]
    pub gate_time: Duration,

    /// how many times to run (default no limit)
    #[arg(env, long)]
    pub max_start: Option<usize>,

    /// how many times to retry the connection without backing off
    #[arg(env, long, default_value_t = 2)]
    pub backoff_fast_tries: u32,

    /// What should be the absolute maximum time between connection attempts
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "1h")]
    pub backoff_upper_bound: Duration,

    /// how long can the process/daemon live
    #[arg(env, long, value_parser = humantime::parse_duration)]
    pub max_lifetime: Option<Duration>,

    #[command(flatten)]
    pub tester_options: ConnectionTesterOptions,
}

#[derive(Args, Debug, Clone)]
pub struct ConnectionTesterOptions {
    /// Default connection poll time
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "10m")]
    pub poll_time: Duration,

    /// Initial connection poll time
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "10m")]
    pub first_poll_time: Duration,

    #[arg(env, long, default_value = "")]
    pub echo_message: Arc<str>,

    /// how many times to retry the network connection when monitoring
    #[arg(env, long, default_value_t = 3)]
    pub max_conn_tries: usize,

    /// timeout on network data
    #[arg(env, long, value_parser = humantime::parse_duration, default_value = "15s")]
    pub net_timeout: Duration,
}
