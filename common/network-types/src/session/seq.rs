use std::collections::BinaryHeap;
use std::pin::{pin, Pin};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::prelude::errors::SessionError;
use crate::prelude::FrameId;

#[pin_project::pin_project]
pub struct Sequencer<T> {
    buffer: BinaryHeap<std::cmp::Reverse<T>>,
    next_id: FrameId,
    last_emitted: Instant,
    timeout: Duration,
    tx_waker: Option<Arc<Mutex<Waker>>>,
    closing: bool,
    buffer_len: usize,
    initial_capacity: usize,
}

impl<T: Ord> Sequencer<T> {
    pub fn new(timeout: Duration, buffer_len: usize, initial_capacity: usize) -> Self {
        Self {
            buffer: BinaryHeap::with_capacity(initial_capacity),
            next_id: 1_u32,
            closing: false,
            last_emitted: Instant::now(),
            tx_waker: None,
            buffer_len,
            timeout,
            initial_capacity
        }
    }
}

impl<T: Ord> futures::Sink<T> for Sequencer<T> {
    type Error = SessionError;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        Poll::Ready(if !*this.closing {
            if this.buffer.len() >= *this.initial_capacity {
                this.buffer.reserve(*this.initial_capacity);
            }
            Ok(())
        } else {
            Err(SessionError::ReassemblerClosed)
        })
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.project();
        this.buffer.push(std::cmp::Reverse(item));
        if this.buffer.len() >= *this.buffer_len {
            if let Some(waker) = this.tx_waker {
                waker.lock().unwrap().wake_by_ref();
            }
        }
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Some(waker) = self.project().tx_waker {
            waker.lock().unwrap().wake_by_ref();
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        *this.closing = true;

        if let Some(waker) = this.tx_waker.clone() {
            let next_wake_up = *this.timeout - this.last_emitted.elapsed();
            hopr_async_runtime::prelude::spawn(async move {
                hopr_async_runtime::prelude::sleep(next_wake_up * 2).await;
                waker.lock().unwrap().wake_by_ref();
            });
        }
        Poll::Ready(Ok(()))
    }
}

impl<T> futures::Stream for Sequencer<T>
where T: Ord + PartialEq<FrameId> {
    type Item = Result<T, SessionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        tracing::debug!("Sequencer::poll_next");
        if self.closing && self.buffer.is_empty() {
            tracing::debug!("Sequencer::poll_next done");
            return Poll::Ready(None);
        }

        if let Some(current_item) = self.buffer.peek().map(|e| &e.0) {
            let current_to_emit = self.next_id;
            let is_next_ready = current_item.eq(&current_to_emit);

            if is_next_ready || self.last_emitted.elapsed() >= self.timeout {
                self.last_emitted = Instant::now();
                self.next_id += 1;

                return if is_next_ready {
                    tracing::debug!("Sequencer::poll_next ready");
                    Poll::Ready(self.buffer.pop().map(|r| Ok(r.0)))
                } else {
                    tracing::debug!("Sequencer::poll_next discard");
                    Poll::Ready(Some(Err(SessionError::FrameDiscarded(current_to_emit))))
                }
            }
        }

        if let Some(waker) = &self.tx_waker {
            let mut waker = waker.lock().unwrap();
            if !waker.will_wake(cx.waker()) {
                *waker = cx.waker().clone();
            }
        } else {
            let waker = Arc::new(Mutex::new(cx.waker().clone()));
            self.tx_waker = Some(waker.clone());
        }
        tracing::debug!("Sequencer::poll_next pending");
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::prelude::FutureExt;
    use futures::{pin_mut, StreamExt, TryStreamExt};

    #[test_log::test(async_std::test)]
    async fn sequencer_should_return_entries_in_order() -> anyhow::Result<()> {
        let (seq_sink, seq_stream) = Sequencer::<u32>::new(Duration::from_secs(2), 0, 1024).split();

        let mut expected = vec![4u32,1,5,7,8,6,2,3];

        async_std::task::spawn(futures::stream::iter(expected.clone())
            .then(|e| futures::future::ok(e).delay(Duration::from_millis(5)))
            .forward(seq_sink));

        let actual: Vec<u32> = seq_stream.try_collect().timeout(Duration::from_secs(5)).await??;

        expected.sort();
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_skip_entry_on_timeout() -> anyhow::Result<()> {
        let timeout = Duration::from_millis(50);
        let (seq_sink, seq_stream) = Sequencer::<u32>::new(timeout, 0, 1024).split();

        let input = vec![1u32, 2, 4];

        async_std::task::spawn(futures::stream::iter(input.clone())
            .then(futures::future::ok)
            .forward(seq_sink).delay(Duration::from_millis(10))
        );

        pin_mut!(seq_stream);

        assert_eq!(Some(1), seq_stream.try_next()/*.timeout(Duration::from_millis(100))*/.await?);
        assert_eq!(Some(2), seq_stream.try_next()/*.timeout(Duration::from_millis(100))*/.await?);

        let now = Instant::now();
        assert!(matches!(seq_stream.try_next()/*.timeout(Duration::from_millis(100))*/.await, Err(SessionError::FrameDiscarded(3))));
        assert!(now.elapsed() >= timeout);

        assert_eq!(Some(4), seq_stream.try_next()/*.timeout(Duration::from_millis(100))*/.await?);
        assert_eq!(None, seq_stream.try_next()/*.timeout(Duration::from_millis(100))*/.await?);

        Ok(())
    }
}