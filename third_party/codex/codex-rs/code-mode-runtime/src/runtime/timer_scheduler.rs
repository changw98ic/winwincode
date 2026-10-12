use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::RuntimeCommand;

enum TimerCommand {
    Schedule { id: u64, deadline: Option<Instant> },
    Cancel(u64),
    Stop,
}

pub(super) struct TimerScheduler {
    commands: mpsc::Sender<TimerCommand>,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TimerScheduler {
    pub(super) fn new(runtime_commands: mpsc::Sender<RuntimeCommand>) -> Self {
        let (commands, receiver) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let thread = thread::spawn(move || {
            let mut deadlines = BTreeSet::<(Instant, u64)>::new();
            let mut timers = HashMap::new();
            loop {
                if thread_stopped.load(Ordering::Acquire) {
                    break;
                }
                let command = match deadlines.first() {
                    Some((deadline, _)) => {
                        receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match command {
                    Ok(TimerCommand::Schedule { id, deadline }) => {
                        timers.insert(id, deadline);
                        if let Some(deadline) = deadline {
                            deadlines.insert((deadline, id));
                        }
                    }
                    Ok(TimerCommand::Cancel(id)) => {
                        if let Some(Some(deadline)) = timers.remove(&id) {
                            deadlines.remove(&(deadline, id));
                        }
                    }
                    Ok(TimerCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        while let Some((deadline, id)) = deadlines.first().copied()
                            && deadline <= Instant::now()
                            && !thread_stopped.load(Ordering::Acquire)
                        {
                            deadlines.pop_first();
                            timers.remove(&id);
                            if runtime_commands
                                .send(RuntimeCommand::TimeoutFired { id })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        });
        Self {
            commands,
            stopped,
            thread: Some(thread),
        }
    }

    pub(super) fn schedule(&self, id: u64, delay: Duration) -> Result<(), String> {
        self.commands
            .send(TimerCommand::Schedule {
                id,
                deadline: Instant::now().checked_add(delay),
            })
            .map_err(|_| "code mode timer scheduler ended unexpectedly".to_string())
    }

    pub(super) fn cancel(&self, id: u64) {
        let _ = self.commands.send(TimerCommand::Cancel(id));
    }
}

impl Drop for TimerScheduler {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.commands.send(TimerCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
