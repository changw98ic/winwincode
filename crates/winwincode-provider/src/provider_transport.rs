// SPDX-License-Identifier: Apache-2.0

//! Per-exchange socket authority: finite opening stages, progress idle timeout, and real cancellation.
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
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};

#[derive(Debug)]
pub(crate) struct ExchangeIo {
    cancelled: AtomicBool,
    body: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
    started: Instant,
    connect: Duration,
    idle: Duration,
}
impl ExchangeIo {
    pub(crate) fn new(connect: Duration, idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            body: AtomicBool::new(false),
            socket: Mutex::new(None),
            started: Instant::now(),
            connect,
            idle,
        })
    }
    pub(crate) fn body_started(&self) {
        self.body.store(true, Ordering::Release);
    }
    pub(crate) fn cancel(&self) {
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

#[derive(Debug)]
pub(crate) struct ExchangeConnector(pub(crate) Arc<ExchangeIo>);
impl Connector for ExchangeConnector {
    type Out = ExchangeTransport;
    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        _: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
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
            match TcpStream::connect_timeout(address, remaining) {
                Ok(socket) => {
                    socket.set_nodelay(true)?;
                    let mut retained = self.0.socket.lock().map_err(|_| interrupted())?;
                    *retained = Some(socket.try_clone()?);
                    if self.0.cancelled.load(Ordering::Acquire) {
                        let _ = socket.shutdown(Shutdown::Both);
                        return Err(interrupted());
                    }
                    return Ok(Some(ExchangeTransport {
                        socket,
                        buffers: LazyBuffers::new(
                            details.config.input_buffer_size(),
                            details.config.output_buffer_size(),
                        ),
                        io: Arc::clone(&self.0),
                    }));
                }
                Err(error) => last = error,
            }
        }
        Err(last.into())
    }
}
#[derive(Debug)]
pub(crate) struct ExchangeTransport {
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
        self.socket.write_all(&self.buffers.output()[..amount])?;
        Ok(())
    }
    fn await_input(&mut self, next: NextTimeout) -> Result<bool, ureq::Error> {
        self.socket.set_read_timeout(Some(self.io.timeout(next)?))?;
        let count = self.socket.read(self.buffers.input_append_buf())?;
        if self.io.cancelled.load(Ordering::Acquire) {
            return Err(interrupted());
        }
        self.buffers.input_appended(count);
        Ok(count > 0)
    }
    // Each Agent belongs to one exchange; a completed connection is never pooled for another.
    fn is_open(&mut self) -> bool {
        false
    }
}

/// Shared cancellation from before semantic preprocessing through the final model stream.
#[derive(Debug, Default)]
pub(crate) struct ExchangeCancellation {
    cancelled: AtomicBool,
    sockets: Mutex<Vec<std::sync::Weak<ExchangeIo>>>,
    changed: tokio::sync::Notify,
}
impl ExchangeCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn attach(&self, io: &Arc<ExchangeIo>) {
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
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Ok(sockets) = self.sockets.lock() {
            for io in sockets.iter().filter_map(std::sync::Weak::upgrade) {
                io.cancel();
            }
        }
        self.changed.notify_waiters();
    }
    pub(crate) async fn cancelled(&self) {
        let notified = self.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }
}
