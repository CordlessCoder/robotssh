use crate::cli::ConnectionTesterOptions;
use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener, TcpStream},
};
use std::{
    io::{ErrorKind, Read, Write},
    net::ToSocketAddrs,
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct ConnectionTester {
    stop_tx: SyncSender<()>,
    handle: JoinHandle<bool>,
}

#[derive(Debug)]
enum TestResult {
    Success,
    AcceptFail,
    WriteFail,
    ReadFail,
    PollFail,
    Timeout(usize),
}

impl ConnectionTester {
    pub fn spawn(
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
                let start = Instant::now();
                use TestResult::*;
                match Self::run_test(
                    listener.as_deref_mut(),
                    target.clone(),
                    opts.net_timeout,
                    &payload,
                ) {
                    PollFail | AcceptFail | ReadFail | WriteFail | Timeout(0) => break,
                    Timeout(_) => {
                        tries += 1;
                    }
                    Success => {}
                };
                if tries >= opts.max_conn_tries {
                    break;
                }
                if stop_rx
                    .recv_timeout(opts.poll_time.saturating_sub(start.elapsed()))
                    .is_ok()
                {
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
    ) -> TestResult {
        const WRITER: Token = Token(0);
        const CONN: Token = Token(1);
        const READER: Token = Token(2);
        let mut to_be_sent = payload;
        let mut to_be_recieved = payload;
        let target = target.to_socket_addrs().unwrap().next().unwrap();
        let Ok(mut writer) = TcpStream::connect(target) else {
            return TestResult::AcceptFail;
        };
        let mut reader = None;
        let mut events = Events::with_capacity(128);
        let mut poll = Poll::new().unwrap();
        let mut start = Instant::now();
        if let Some(listener) = listener {
            _ = poll
                .registry()
                .register(&mut writer, WRITER, Interest::WRITABLE);
            _ = poll.registry().register(listener, CONN, Interest::READABLE);
            loop {
                if poll
                    .poll(
                        &mut events,
                        Some(
                            timeout
                                .saturating_sub(start.elapsed())
                                .min(Duration::from_millis(20)),
                        ),
                    )
                    .is_err()
                {
                    return TestResult::PollFail;
                };

                if !events.is_empty() {
                    start = Instant::now();
                }
                if events.is_empty() && start.elapsed() >= timeout {
                    // timed out
                    return TestResult::Timeout(
                        payload.len() * 2 - to_be_recieved.len() - to_be_sent.len(),
                    );
                }
                // Process each event.
                for event in events.iter() {
                    // We can use the token we previously provided to `register` to
                    // determine for which socket the event is.
                    match event.token() {
                        CONN if event.is_readable() => {
                            let Ok((mut connection, _)) = listener.accept() else {
                                return TestResult::AcceptFail;
                            };
                            poll.registry().deregister(listener).unwrap();
                            _ = poll.registry().register(
                                &mut connection,
                                READER,
                                Interest::READABLE,
                            );
                            reader = Some(connection)
                        }
                        WRITER if event.is_writable() => {
                            let wrote = match writer.write(to_be_sent) {
                                Err(err)
                                    if matches!(
                                        err.kind(),
                                        ErrorKind::Interrupted | ErrorKind::WouldBlock
                                    ) =>
                                {
                                    continue;
                                }
                                Ok(0) | Err(_) => return TestResult::WriteFail,
                                Ok(wrote) => wrote,
                            };
                            to_be_sent = &to_be_sent[wrote..];
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
                                Ok(0) | Err(_) => return TestResult::ReadFail,
                                Ok(read) => read,
                            };
                            let Some((expected, left)) = to_be_recieved.split_at_checked(read)
                            else {
                                return TestResult::ReadFail;
                            };
                            let read = &buf[..read];
                            if expected != read {
                                return TestResult::ReadFail;
                            }
                            to_be_recieved = left;
                            if to_be_recieved.is_empty() {
                                poll.registry().deregister(reader).unwrap();
                                return TestResult::Success;
                            }
                        }
                        // We don't expect any events with tokens other than those we provided.
                        _ => unreachable!(),
                    }
                }
            }
        } else {
            _ = poll.registry().register(
                &mut writer,
                WRITER,
                Interest::WRITABLE | Interest::READABLE,
            );
            loop {
                if poll
                    .poll(
                        &mut events,
                        Some(
                            timeout
                                .saturating_sub(start.elapsed())
                                .min(Duration::from_millis(20)),
                        ),
                    )
                    .is_err()
                {
                    return TestResult::PollFail;
                };
                if !events.is_empty() {
                    start = Instant::now();
                }

                if events.is_empty() && start.elapsed() >= timeout {
                    // timed out
                    return TestResult::Timeout(
                        payload.len() * 2 - to_be_recieved.len() - to_be_sent.len(),
                    );
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
                            Ok(0) | Err(_) => return TestResult::WriteFail,
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
                            Ok(0) | Err(_) => return TestResult::ReadFail,
                            Ok(read) => read,
                        };
                        let Some((expected, left)) = to_be_recieved.split_at_checked(read) else {
                            return TestResult::ReadFail;
                        };
                        let read = &buf[..read];
                        if expected != read {
                            return TestResult::ReadFail;
                        }
                        to_be_recieved = left;
                        if to_be_recieved.is_empty() {
                            poll.registry().deregister(&mut writer).unwrap();
                            return TestResult::Success;
                        }
                    }
                }
            }
        }
    }
    // Joins the thread, returning true if ConnectionTester called the on_death callback
    pub fn join(self) -> bool {
        _ = self.stop_tx.try_send(());
        self.handle.join().unwrap_or(false)
    }
}
