#[macro_use]
extern crate lazy_static;

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

use chrono::prelude::*;
use clap::Clap;
use colored::Colorize;
use log::{error, info, trace};
use log::{Level, LevelFilter};
use std::sync::atomic;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use elbus::broker::Broker;

static SERVER_ACTIVE: atomic::AtomicBool = atomic::AtomicBool::new(true);

lazy_static! {
    static ref PID_FILE: Mutex<Option<String>> = Mutex::new(None);
    static ref SOCK_FILES: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let s = format!(
                "{}  {}",
                Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
                record.args()
            );
            println!(
                "{}",
                match record.level() {
                    Level::Trace => s.black().dimmed(),
                    Level::Debug => s.dimmed(),
                    Level::Warn => s.yellow().bold(),
                    Level::Error => s.red(),
                    Level::Info => s.normal(),
                }
            );
        }
    }

    fn flush(&self) {}
}

static LOGGER: SimpleLogger = SimpleLogger;

fn set_verbose_logger(filter: LevelFilter) {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(filter))
        .unwrap();
}

#[derive(Clap)]
struct Opts {
    #[clap(
        short = 'B',
        long = "bind",
        required = true,
        about = "Unix socket path, IP:PORT or fifo:path, can be specified multiple times"
    )]
    path: Vec<String>,
    #[clap(short = 'P', long = "pid-file")]
    pid_file: Option<String>,
    #[clap(short = 'v', about = "Verbose logging")]
    verbose: bool,
    #[clap(short = 'D')]
    daemonize: bool,
    #[clap(long = "log-syslog", about = "Force log to syslog")]
    log_syslog: bool,
    #[clap(short = 'w', default_value = "4")]
    workers: usize,
    #[clap(short = 't', default_value = "1")]
    timeout: f64,
    #[clap(
        long = "buf-size",
        default_value = "16384",
        about = "I/O buffer size, per client"
    )]
    buf_size: usize,
    #[clap(
        long = "queue-size",
        default_value = "8192",
        about = "frame queue size, per client"
    )]
    queue_size: usize,
}

async fn terminate(allow_log: bool) {
    if let Some(f) = PID_FILE.lock().await.as_ref() {
        // do not log anything on C-ref() {
        if allow_log {
            trace!("removing pid file {}", f);
        }
        let _r = std::fs::remove_file(&f);
    }
    for f in SOCK_FILES.lock().await.iter() {
        if allow_log {
            trace!("removing sock file {}", f);
        }
        let _r = std::fs::remove_file(&f);
    }
    if allow_log {
        info!("terminating");
    }
    SERVER_ACTIVE.store(false, atomic::Ordering::SeqCst);
}

macro_rules! handle_term_signal {
    ($kind: expr, $allow_log: expr) => {
        tokio::spawn(async move {
            trace!("starting handler for {:?}", $kind);
            loop {
                match signal($kind) {
                    Ok(mut v) => {
                        v.recv().await;
                    }
                    Err(e) => {
                        error!("Unable to bind to signal {:?}: {}", $kind, e);
                        break;
                    }
                }
                // do not log anything on C-c
                if $allow_log {
                    trace!("got termination signal");
                }
                terminate($allow_log).await
            }
        });
    };
}

fn main() {
    let opts: Opts = Opts::parse();
    if opts.verbose {
        set_verbose_logger(LevelFilter::Trace);
    } else if (!opts.daemonize
        || std::env::var("DISABLE_SYSLOG").unwrap_or_else(|_| "0".to_owned()) == "1")
        && !opts.log_syslog
    {
        set_verbose_logger(LevelFilter::Info);
    } else {
        let formatter = syslog::Formatter3164 {
            facility: syslog::Facility::LOG_USER,
            hostname: None,
            process: "elbusd".into(),
            pid: 0,
        };
        match syslog::unix(formatter) {
            Ok(logger) => {
                log::set_boxed_logger(Box::new(syslog::BasicLogger::new(logger)))
                    .map(|()| log::set_max_level(LevelFilter::Info))
                    .unwrap();
            }
            Err(_) => {
                set_verbose_logger(LevelFilter::Info);
            }
        }
    }
    let timeout = Duration::from_secs_f64(opts.timeout);
    info!(
        "starting elbus server, {} workers, buf size: {}, queue size: {}, timeout: {:?}",
        opts.workers, opts.buf_size, opts.queue_size, timeout
    );
    if opts.daemonize {
        if let Ok(fork::Fork::Child) = fork::daemon(true, false) {
            std::process::exit(0);
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(opts.workers)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        if let Some(pid_file) = opts.pid_file {
            let pid = std::process::id().to_string();
            tokio::fs::write(&pid_file, pid)
                .await
                .expect("Unable to write pid file");
            info!("created pid file {}", pid_file);
            PID_FILE.lock().await.replace(pid_file);
        }
        handle_term_signal!(SignalKind::interrupt(), false);
        handle_term_signal!(SignalKind::terminate(), true);
        let mut broker = Broker::new();
        broker.set_queue_size(opts.queue_size);
        let mut sock_files = SOCK_FILES.lock().await;
        for path in opts.path {
            info!("binding at {}", path);
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            if let Some(_fifo) = path.strip_prefix("fifo:") {
                #[cfg(feature = "broker-api")]
                {
                    broker
                        .spawn_fifo(_fifo, opts.buf_size)
                        .await
                        .expect("unable to start fifo server");
                    sock_files.push(_fifo.to_owned());
                }
            } else if path.ends_with(".sock")
                || path.ends_with(".socket")
                || path.ends_with(".ipc")
                || path.starts_with('/')
            {
                broker
                    .spawn_unix_server(&path, opts.buf_size, timeout)
                    .await
                    .expect("Unable to start unix server");
                sock_files.push(path);
            } else {
                broker
                    .spawn_tcp_server(&path, opts.buf_size, timeout)
                    .await
                    .expect("Unable to start tcp server");
            }
        }
        drop(sock_files);
        info!("elbus broker started");
        let sleep_step = Duration::from_millis(100);
        loop {
            if !SERVER_ACTIVE.load(atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(sleep_step).await;
        }
    });
}
