use crate::*;

/// A type used to wait upon multiple blocking operations at once.
///
/// A [`Selector`] implements [`select`](https://en.wikipedia.org/wiki/Select_(Unix))-like behaviour,
/// allowing a thread to wait upon the result of more than one operation at once.
///
/// # Examples
/// ```
/// let (tx0, rx0) = flume::unbounded();
/// let (tx1, rx1) = flume::unbounded();
///
/// std::thread::spawn(move || {
///     tx0.send(true).unwrap();
///     tx1.send(42).unwrap();
/// });
///
/// flume::Selector::new()
///     .recv(&rx0, |b| println!("Received {:?}", b))
///     .recv(&rx1, |n| println!("Received {:?}", n))
///     .wait();
/// ```
pub struct Selector<'a, T: 'a> {
    selections: Vec<(
        Box<dyn FnMut() -> Option<T> + 'a>, // Poll
        Box<dyn FnMut() + 'a>, // Drop
    )>,
    signal: Arc<SelectorSignal>,
}

impl<'a, T: 'a> Selector<'a, T> {
    /// Create a new selector.
    pub fn new() -> Self {
        Self {
            selections: Vec::new(),
            signal: Arc::new(SelectorSignal {
                wait_lock: Mutex::new(None),
                trigger: Condvar::new(),
                //listeners: AtomicUsize::new(0)
            }),
        }
    }

    /// Add a send operation to the selector.
    ///
    /// Once added, the selector can be used to run the provided handler function on completion of
    /// this operation.
    pub fn send<U>(mut self, sender: &'a Sender<U>, msg: U, mut f: impl FnMut(Result<(), SendError<U>>) -> T + 'a) -> Self {
        let token = self.selections.len();
        let selector_id = sender.shared.connect_send_selector(self.signal.clone(), token);
        let mut msg = Some(msg);
        self.selections.push((
            Box::new(move || {
                if let Some(m) = msg.take() {
                    match sender.try_send(m) {
                        Ok(()) => {
                            msg = None;
                            Some((&mut f)(Ok(())))
                        },
                        Err(TrySendError::Disconnected(m)) => {
                            msg = None;
                            Some((&mut f)(Err(SendError(m))))
                        },
                        Err(TrySendError::Full(m)) => {
                            msg = Some(m);
                            None
                        },
                    }
                } else {
                    None
                }
            }),
            Box::new(move || sender.shared.disconnect_send_selector(selector_id)),
        ));
        self
    }

    /// Add a receive operation to the selector.
    ///
    /// Once added, the selector can be used to run the provided handler function on completion of
    /// this operation.
    pub fn recv<U>(mut self, receiver: &'a Receiver<U>, mut f: impl FnMut(Result<U, RecvError>) -> T + 'a) -> Self {
        let token = self.selections.len();
        receiver.shared.connect_recv_selector(self.signal.clone(), token);
        self.selections.push((
            Box::new(move || match receiver.try_recv() {
                Ok(msg) => Some((&mut f)(Ok(msg))),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some((&mut f)(Err(RecvError::Disconnected))),
            }),
            Box::new(move || receiver.shared.disconnect_recv_selector()),
        ));
        self
    }

    fn wait_inner(&mut self) -> T {
        // Speculatively poll
        if let Some(msg) = self.poll() {
            return msg;
        }

        loop {
            let mut guard = self.signal.wait_lock.lock().unwrap();
            // Reset token
            *guard = None;
            // TODO: use signal.listeners
            //self.signal.listeners.fetch_add(1, Ordering::Acquire);
            let token = *self.signal.trigger.wait(guard).unwrap();
            //self.signal.listeners.fetch_sub(1, Ordering::Acquire);

            // Attempt to receive a message
            if let Some(msg) = match token {
                None => self.poll(), // Unknown event
                Some(token) => (&mut self.selections[token].0)()
            } {
                break msg;
            }
        }
    }

    /// Poll each event associated with this [`Selector`] to see whether any have completed. If
    /// more than one event has completed, a random event handler will be run and its return value
    /// returned. If none of the events have completed a `None` is returned.
    pub fn poll(&mut self) -> Option<T> {
        self
            .selections
            .iter_mut()
            .find_map(|(poll, _)| poll())
    }

    /// Wait until one of the events associated with this [`Selector`] has completed. If more than
    /// one event has completed, a random event handler will be run and its return value produced.
    pub fn wait(&mut self) -> T {
        self.wait_inner()
    }

    /// Create an iterator over incoming events on this [`Selector`].
    pub fn iter<'b: 'a>(&'b mut self) -> SelectorIter<'b, T> {
        SelectorIter { selector: self }
    }

    /// Create an iterator over pending events on this [`Selector`]. This iterator will only
    /// produce values while there are events immediately available to handle.
    pub fn try_iter<'b: 'a>(&'b mut self) -> SelectorTryIter<'b, T> {
        SelectorTryIter { selector: self }
    }
}

/// An iterator over the events received by a [`Selector`].
pub struct SelectorIter<'a, T: 'a> {
    selector: &'a mut Selector<'a, T>,
}

impl<'a, T> Iterator for SelectorIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.selector.wait_inner())
    }
}

/// An iterator over the pending events received by a [`Selector`].
pub struct SelectorTryIter<'a, T: 'a> {
    selector: &'a mut Selector<'a, T>,
}

impl<'a, T> Iterator for SelectorTryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.selector.poll()
    }
}

impl<'a, T> Drop for Selector<'a, T> {
    fn drop(&mut self) {
        for (_, drop) in self.selections.iter_mut() {
            drop();
        }
    }
}