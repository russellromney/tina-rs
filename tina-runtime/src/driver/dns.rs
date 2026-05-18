//! DNS resolution lane: a worker thread that runs a pluggable resolver
//! and reports completions back to the driver.

use super::*;

pub(super) enum DnsLane {
    Worker(DnsWorkerLane),
}

pub(super) type DnsResolver = Arc<dyn Fn(&str, u16) -> CallOutput + Send + Sync + 'static>;

pub(super) struct DnsWorkerLane {
    capacity: usize,
    sender: Option<SyncSender<DnsCommand>>,
    completions: Receiver<DnsCompletion>,
    handle: Option<JoinHandle<()>>,
    pending: Vec<DnsPending>,
}

struct DnsPending {
    call_id: CallId,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    timed_out: bool,
}

struct DnsCommand {
    call_id: CallId,
    host: String,
    port: u16,
    cancelled: Arc<AtomicBool>,
}

struct DnsCompletion {
    call_id: CallId,
    result: CallOutput,
}
impl DnsLane {
    pub(super) fn new(capacity: usize) -> Self {
        Self::Worker(DnsWorkerLane::new(capacity, Arc::new(default_dns_resolver)))
    }

    pub(super) fn submit(
        &mut self,
        call_id: CallId,
        host: String,
        port: u16,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit(call_id, host, port, timeout, now),
        }
    }

    pub(super) fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        match self {
            Self::Worker(lane) => lane.advance(now, completed),
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        match self {
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        match self {
            Self::Worker(lane) => lane.cancel_pending(deadline),
        }
    }

    pub(super) fn physical_pending_count(&self) -> usize {
        match self {
            Self::Worker(lane) => lane.physical_pending_count(),
        }
    }
}

impl Drop for DnsLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}
impl DnsWorkerLane {
    pub(super) fn new(capacity: usize, resolver: DnsResolver) -> Self {
        assert!(capacity > 0, "DNS lane capacity must be > 0");
        let (sender, receiver) = sync_channel(capacity);
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        let handle = thread::spawn(move || dns_worker_loop(receiver, completion_sender, resolver));
        Self {
            capacity,
            sender: Some(sender),
            completions,
            handle: Some(handle),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
        }
    }

    pub(super) fn submit(
        &mut self,
        call_id: CallId,
        host: String,
        port: u16,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        if timeout.is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        let Some(sender) = &self.sender else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::DnsClosed),
            });
        };
        if self.unresolved_pending_count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::DnsFull),
            });
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        match sender.try_send(DnsCommand {
            call_id,
            host,
            port,
            cancelled: Arc::clone(&cancelled),
        }) {
            Ok(()) => {
                self.pending.push(DnsPending {
                    call_id,
                    deadline: now + timeout,
                    cancelled,
                    timed_out: false,
                });
                None
            }
            Err(MpscTrySendError::Full(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::DnsFull),
            }),
            Err(MpscTrySendError::Disconnected(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::DnsClosed),
            }),
        }
    }

    pub(super) fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        for pending in &mut self.pending {
            if !pending.timed_out
                && !pending.cancelled.load(Ordering::Acquire)
                && now >= pending.deadline
            {
                pending.timed_out = true;
                pending.cancelled.store(true, Ordering::Release);
                completed.push(DriverCompletion {
                    call_id: pending.call_id,
                    result: CallOutput::Failed(CallError::Timeout),
                });
            }
        }

        loop {
            match self.completions.try_recv() {
                Ok(completion) => self.finish_completion(completion, completed),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sender = None;
                    break;
                }
            }
        }
    }

    fn finish_completion(
        &mut self,
        completion: DnsCompletion,
        completed: &mut Vec<DriverCompletion>,
    ) {
        let Some(index) = self
            .pending
            .iter()
            .position(|entry| entry.call_id == completion.call_id)
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if pending.cancelled.load(Ordering::Acquire) || pending.timed_out {
            return;
        }
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result: completion.result,
        });
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|entry| !entry.cancelled.load(Ordering::Acquire) && !entry.timed_out)
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled.load(Ordering::Acquire))
        else {
            return false;
        };
        pending.cancelled.store(true, Ordering::Release);
        true
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        // Drop the command sender; the worker thread can exit when it
        // returns from the resolver. Drain completions for the budget;
        // remaining `self.pending` after the budget is stuck work the
        // worker has not finished and stays visible in
        // `physical_pending_count`.
        self.sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
            if self.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        sink.clear();
        if self
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
    }

    pub(super) fn unresolved_pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn physical_pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for DnsWorkerLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}
fn dns_worker_loop(
    receiver: Receiver<DnsCommand>,
    completions: SyncSender<DnsCompletion>,
    resolver: DnsResolver,
) {
    while let Ok(command) = receiver.recv() {
        let result = if command.cancelled.load(Ordering::Acquire) {
            CallOutput::Failed(CallError::Timeout)
        } else {
            resolver(&command.host, command.port)
        };
        if completions
            .send(DnsCompletion {
                call_id: command.call_id,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}
fn default_dns_resolver(host: &str, port: u16) -> CallOutput {
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.collect();
            if addrs.is_empty() {
                CallOutput::Failed(CallError::Io)
            } else {
                CallOutput::DnsResolved { addrs }
            }
        }
        Err(_) => CallOutput::Failed(CallError::Io),
    }
}
