// SPDX-License-Identifier: Apache-2.0

//! Interrupt subscription retained across registration and execution turns.

#[cfg(unix)]
pub(crate) struct WorkerInterrupt {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(windows)]
pub(crate) struct WorkerInterrupt {
    interrupt: tokio::signal::windows::CtrlC,
}

impl WorkerInterrupt {
    pub(crate) fn new() -> std::io::Result<Self> {
        // Subscribe before any registration/drive await. A ctrl_c future
        // created per select loses signals while that turn's body runs.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    pub(crate) async fn until_interrupt<T>(
        &mut self,
        operation: impl Future<Output = T>,
    ) -> Option<T> {
        tokio::select! {
            biased;
            () = self.wait() => None,
            result = operation => Some(result),
        }
    }

    pub(crate) async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
        }
        #[cfg(windows)]
        self.interrupt.recv().await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::WorkerInterrupt;
    use std::process::Command;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[test]
    fn interrupt_survives_an_awaited_drive_turn() {
        let result = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--ignored",
                "--exact",
                "shutdown_signal::tests::interrupt_child",
                "--nocapture",
            ])
            .env("WWC_INTERRUPT_TEST_CHILD", "1")
            .output()
            .expect("spawn isolated signal test");
        assert!(
            result.status.success(),
            "isolated interrupt test failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[tokio::test]
    #[ignore = "sends SIGINT to its own isolated process; run by the parent test"]
    async fn interrupt_child() {
        if std::env::var("WWC_INTERRUPT_TEST_CHILD").as_deref() != Ok("1") {
            return;
        }
        let mut interrupt = WorkerInterrupt::new().expect("subscribe to interrupts");
        // Prime the registration select, then let a drive turn win. Its body
        // awaits external work while the interrupt branch is no longer polled.
        tokio::select! {
            biased;
            () = interrupt.wait() => panic!("unexpected early interrupt"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {},
        }
        send_interrupt();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(1), interrupt.wait())
            .await
            .expect("interrupt during the drive body must remain pending");

        // Subscription is eager: even a signal before the first registration
        // poll must be remembered rather than depending on select ordering.
        let mut before_registration =
            WorkerInterrupt::new().expect("subscribe before registration");
        send_interrupt();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(1), before_registration.wait())
            .await
            .expect("interrupt before registration must remain pending");

        let mut terminate = WorkerInterrupt::new().expect("subscribe to termination");
        assert!(
            Command::new("kill")
                .args(["-TERM", &std::process::id().to_string()])
                .status()
                .expect("send SIGTERM to isolated child")
                .success()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(1), terminate.wait())
            .await
            .expect("termination during registration must remain pending");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut active = WorkerInterrupt::new().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            send_interrupt();
            let mut byte = [0];
            let read = tokio::time::timeout(Duration::from_millis(500), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                read, 0,
                "interrupt must close the real in-flight connection"
            );
        });
        let started = std::time::Instant::now();
        let result = active
            .until_interrupt(async {
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                let mut byte = [0];
                socket.read(&mut byte).await.unwrap()
            })
            .await;
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_millis(500));
        peer.await.unwrap();
    }

    fn send_interrupt() {
        assert!(
            Command::new("kill")
                .args(["-INT", &std::process::id().to_string()])
                .status()
                .expect("send SIGINT to isolated child")
                .success()
        );
    }
}
