use crate::{
    config::Config,
    nut::{Client, Error, Values},
};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const CONNECT_BUDGET: Duration = Duration::from_secs(5);

pub enum Event {
    Connecting,
    Devices(Vec<String>),
    Data {
        ups: String,
        values: Values,
        at: Instant,
    },
    Failed(Error, u64),
}

struct Stop {
    flag: Arc<AtomicBool>,
    wake: Condvar,
    parked: Mutex<()>,
}

impl Stop {
    fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn wait(&self, duration: Duration) -> bool {
        let guard = self.parked.lock().unwrap();
        let _result = self
            .wake
            .wait_timeout_while(guard, duration, |_| !self.cancelled())
            .unwrap();
        self.cancelled()
    }
}

pub struct Worker(Arc<Stop>);

impl Worker {
    pub fn start(
        cfg: Config,
        password: Zeroizing<String>,
        notify: impl Fn(Event) + Send + 'static,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(Stop {
            flag: Arc::new(AtomicBool::new(false)),
            wake: Condvar::new(),
            parked: Mutex::new(()),
        });
        let signal = stop.clone();
        std::thread::Builder::new()
            .name("nut-monitor".into())
            .spawn(move || {
                let mut backoff = 2;
                while !signal.cancelled() {
                    notify(Event::Connecting);
                    let result = session(&cfg, &password, &signal, |event| {
                        if matches!(event, Event::Data { .. }) {
                            backoff = 2;
                        }
                        if !signal.cancelled() {
                            notify(event);
                        }
                    });
                    if signal.cancelled() {
                        break;
                    }
                    if let Err(error) = result {
                        notify(Event::Failed(error, backoff));
                    }
                    if signal.wait(Duration::from_secs(backoff)) {
                        break;
                    }
                    backoff = (backoff * 2).min(60);
                }
            })?;
        Ok(Self(stop))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _guard = self.0.parked.lock().unwrap();
        self.0.flag.store(true, Ordering::Relaxed);
        self.0.wake.notify_one();
    }
}

type Addresses = Result<Vec<SocketAddr>, Error>;
type Lookup = (String, u16, Instant, SyncSender<Addresses>);

fn resolver() -> &'static SyncSender<Lookup> {
    static RESOLVER: OnceLock<SyncSender<Lookup>> = OnceLock::new();
    RESOLVER.get_or_init(|| {
        let (send, receive): (_, Receiver<Lookup>) = mpsc::sync_channel(1);
        // Windows' system DNS resolver can outlive our deadline. At most one
        // lookup runs and one waits, even when the user repeatedly edits settings.
        let _thread = std::thread::Builder::new()
            .name("nut-dns".into())
            .spawn(move || {
                for (host, port, deadline, reply) in receive {
                    if Instant::now() < deadline {
                        let result = (host.as_str(), port)
                            .to_socket_addrs()
                            .map(|addresses| addresses.take(16).collect())
                            .map_err(|_| Error::Resolve);
                        let _ = reply.send(result);
                    }
                }
            });
        send
    })
}

fn connect(host: &str, port: u16, stop: &Stop) -> Result<TcpStream, Error> {
    let deadline = Instant::now() + CONNECT_BUDGET;
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let (send, receive) = mpsc::sync_channel(1);
        resolver()
            .try_send((host.into(), port, deadline, send))
            .map_err(|_| Error::Resolve)?;
        loop {
            match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => break result?,
                Err(mpsc::RecvTimeoutError::Timeout)
                    if Instant::now() < deadline && !stop.cancelled() => {}
                Err(_) => return Err(Error::Resolve),
            }
        }
    };
    for addr in addresses {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stop.cancelled() {
            break;
        }
        if let Ok(socket) = TcpStream::connect_timeout(&addr, left.min(Duration::from_millis(1500)))
        {
            return Ok(socket);
        }
    }
    Err(Error::Connect)
}

fn session(
    cfg: &Config,
    password: &str,
    stop: &Stop,
    mut notify: impl FnMut(Event),
) -> Result<(), Error> {
    let socket = connect(&cfg.host, cfg.port, stop)?;
    let mut client = Client::new(socket, &cfg.host, cfg.transport, stop.flag.clone())?;
    if !cfg.user.is_empty() {
        client.login(&cfg.user, password)?;
    }
    let devices = client.discover()?;
    let selected = if cfg.ups.is_empty() {
        devices.first().cloned()
    } else {
        devices.iter().find(|name| *name == &cfg.ups).cloned()
    };
    notify(Event::Devices(devices));
    let ups = selected.ok_or(Error::UnknownUps)?;
    loop {
        let values = client.poll(&ups)?;
        notify(Event::Data {
            ups: ups.clone(),
            values,
            at: Instant::now(),
        });
        if stop.wait(Duration::from_secs(cfg.poll)) {
            return Ok(());
        }
    }
}
