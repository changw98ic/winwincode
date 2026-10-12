// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport,
};

#[cfg(test)]
#[path = "transport_diagnostics.test.rs"]
mod diagnostics_regression_tests;

#[derive(Debug)]
pub struct ExchangeIo {
    cancelled: AtomicBool,
    body: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
    started: Instant,
    connect: Duration,
    idle: Duration,
}
impl ExchangeIo {
    pub fn new(connect: Duration, idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            body: AtomicBool::new(false),
            socket: Mutex::new(None),
            started: Instant::now(),
            connect,
            idle,
        })
    }
    pub fn body_started(&self) {
        self.body.store(true, Ordering::Release);
    }
    pub fn connection_established(&self) -> bool {
        self.socket.lock().map_or(true, |socket| socket.is_some())
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Ok(socket) = self.socket.lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
    fn timeout(&self, next: NextTimeout) -> Result<Duration, ureq::Error> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(interrupted());
        }
        if next.after.is_zero() {
            return Err(ureq::Error::Timeout(next.reason));
        }
        let duration = if self.body.load(Ordering::Acquire) {
            self.idle
        } else {
            (self.connect + self.idle)
                .checked_sub(self.started.elapsed())
                .filter(|v| !v.is_zero())
                .ok_or_else(timed_out)?
        };
        Ok(next
            .not_zero()
            .map_or(duration, |value| duration.min(*value)))
    }
    fn socket_error(&self, error: std::io::Error) -> ureq::Error {
        if self.is_cancelled() {
            interrupted()
        } else if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            // These sockets are blocking with a finite read/write timeout. Linux
            // reports an expired socket timeout as EAGAIN rather than ETIMEDOUT.
            timed_out()
        } else {
            error.into()
        }
    }
}
fn interrupted() -> ureq::Error {
    std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "provider exchange cancelled",
    )
    .into()
}
fn timed_out() -> ureq::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "provider stage made no progress",
    )
    .into()
}

pub fn transient_transport(error: &ureq::Error) -> bool {
    crate::classify_ureq(error, false, crate::Phase::ResponseHeaders).retryable()
}

pub fn wait_for_connection(error: &ureq::Error, io: &ExchangeIo) -> bool {
    !io.connection_established()
        && matches!(
            crate::classify_ureq(error, true, crate::Phase::Connect).kind,
            crate::ErrorKind::ConnectionUnavailable | crate::ErrorKind::Timeout
        )
}

#[derive(Debug)]
pub struct ExchangeConnector(pub Arc<ExchangeIo>);
impl<In: Transport> Connector<In> for ExchangeConnector {
    type Out = Either<In, ExchangeTransport>;
    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        if let Some(transport) = chained {
            return Ok(Some(Either::A(transport)));
        }
        let deadline = Instant::now() + self.0.connect;
        let mut last = std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "provider connection unavailable",
        );
        for address in &details.addrs {
            if self.0.cancelled.load(Ordering::Acquire) {
                return Err(interrupted());
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|v| !v.is_zero())
                .ok_or_else(timed_out)?;
            match connect_cancellable(*address, remaining, &self.0) {
                Ok(socket) => {
                    socket.set_nodelay(true)?;
                    let mut retained = self.0.socket.lock().map_err(|_| interrupted())?;
                    *retained = Some(socket.try_clone()?);
                    if self.0.cancelled.load(Ordering::Acquire) {
                        let _ = socket.shutdown(Shutdown::Both);
                        return Err(interrupted());
                    }
                    return Ok(Some(Either::B(ExchangeTransport {
                        socket,
                        buffers: LazyBuffers::new(
                            details.config.input_buffer_size(),
                            details.config.output_buffer_size(),
                        ),
                        io: Arc::clone(&self.0),
                    })));
                }
                Err(error) => last = error,
            }
        }
        Err(last.into())
    }
}
#[derive(Debug)]
pub struct ExchangeTransport {
    socket: TcpStream,
    buffers: LazyBuffers,
    io: Arc<ExchangeIo>,
}
impl Transport for ExchangeTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, next: NextTimeout) -> Result<(), ureq::Error> {
        self.socket
            .set_write_timeout(Some(self.io.timeout(next)?))?;
        self.socket
            .write_all(&self.buffers.output()[..amount])
            .map_err(|error| self.io.socket_error(error))?;
        Ok(())
    }
    fn await_input(&mut self, next: NextTimeout) -> Result<bool, ureq::Error> {
        self.socket.set_read_timeout(Some(self.io.timeout(next)?))?;
        let count = self
            .socket
            .read(self.buffers.input_append_buf())
            .map_err(|error| self.io.socket_error(error))?;
        if self.io.cancelled.load(Ordering::Acquire) {
            return Err(interrupted());
        }
        self.buffers.input_appended(count);
        Ok(count > 0)
    }
    fn is_open(&mut self) -> bool {
        false
    }
}

#[derive(Debug, Default)]
pub struct ExchangeCancellation {
    cancelled: AtomicBool,
    sockets: Mutex<Vec<std::sync::Weak<ExchangeIo>>>,
    changed: tokio::sync::Notify,
}
impl ExchangeCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn attach(&self, io: &Arc<ExchangeIo>) {
        if let Ok(mut sockets) = self.sockets.lock() {
            sockets.retain(|socket| socket.strong_count() > 0);
            sockets.push(Arc::downgrade(io));
            if self.is_cancelled() {
                io.cancel();
            }
        } else {
            io.cancel();
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Ok(sockets) = self.sockets.lock() {
            for io in sockets.iter().filter_map(std::sync::Weak::upgrade) {
                io.cancel();
            }
        }
        self.changed.notify_waiters();
    }
    pub async fn cancelled(&self) {
        let notified = self.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }
}

fn connect_cancellable(
    address: std::net::SocketAddr,
    timeout: Duration,
    io: &ExchangeIo,
) -> std::io::Result<TcpStream> {
    static REACTOR: std::sync::OnceLock<Result<tokio::runtime::Runtime, std::io::Error>> =
        std::sync::OnceLock::new();
    let runtime = REACTOR.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
    });
    let runtime = runtime
        .as_ref()
        .map_err(|_| std::io::Error::other("network reactor unavailable"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let task = runtime.spawn(async move {
        let result = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))
            .and_then(std::convert::identity)
            .and_then(tokio::net::TcpStream::into_std);
        let _ = sender.send(result);
    });
    loop {
        if io.is_cancelled() {
            task.abort();
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        match receiver.recv_timeout(Duration::from_millis(crate::defaults().authority_check_ms)) {
            Ok(result) => {
                let socket = result?;
                socket.set_nonblocking(false)?;
                return Ok(socket);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("network reactor stopped"));
            }
        }
    }
}

#[derive(Debug)]
pub struct ExchangeResolver(pub Arc<ExchangeIo>);
impl ureq::unversioned::resolver::Resolver for ExchangeResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        if self.0.is_cancelled() {
            return Err(interrupted());
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let uri = uri.clone();
        let config = config.clone();
        std::thread::spawn(move || {
            let result = ureq::unversioned::resolver::DefaultResolver::default()
                .resolve(&uri, &config, timeout);
            let _ = sender.send(result);
        });
        loop {
            if self.0.is_cancelled() {
                return Err(interrupted());
            }
            let remaining = self
                .0
                .connect
                .checked_sub(self.0.started.elapsed())
                .ok_or_else(timed_out)?;
            match receiver.recv_timeout(
                remaining.min(Duration::from_millis(crate::defaults().authority_check_ms)),
            ) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(interrupted()),
            }
        }
    }
}
