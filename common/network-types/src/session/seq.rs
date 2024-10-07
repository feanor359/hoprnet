use std::collections::BinaryHeap;
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::prelude::errors::SessionError;
use crate::prelude::FrameId;

#[derive(Debug)]
#[pin_project::pin_project]
pub struct Sequencer<T> {
    buffer: BinaryHeap<std::cmp::Reverse<T>>,
    next_id: FrameId,
    last_emitted: Instant,
    timeout: Duration,
    tx_waker: Option<Waker>,
    is_closed: bool,
    flush_at: usize,
    initial_capacity: usize,
}

impl<T: Ord> Sequencer<T> {
    pub fn with_capacity(timeout: Duration, flush_at: usize, initial_capacity: usize) -> Self {
        Self {
            buffer: BinaryHeap::with_capacity(initial_capacity),
            next_id: 1_u32,
            is_closed: false,
            last_emitted: Instant::now(),
            tx_waker: None,
            flush_at,
            timeout,
            initial_capacity,
        }
    }

    pub fn new(timeout: Duration, flush_at: usize) -> Self {
        Self::with_capacity(timeout, flush_at, 1024)
    }
}

impl<T: Ord> futures::Sink<T> for Sequencer<T> {
    type Error = SessionError;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("Sequencer::poll_ready");
        let this = self.project();
        if !*this.is_closed {
            if this.buffer.len() >= *this.initial_capacity {
                this.buffer.reserve(*this.initial_capacity);
            }
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(SessionError::ReassemblerClosed))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.project();
        if !*this.is_closed {
            this.buffer.push(std::cmp::Reverse(item));
            if this.buffer.len() >= *this.flush_at {
                if let Some(waker) = this.tx_waker.take() {
                    waker.wake();
                }
            }
            Ok(())
        } else {
            Err(SessionError::ReassemblerClosed)
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("Sequencer::poll_flush");
        let this = self.project();
        if !*this.is_closed {
            if let Some(waker) = this.tx_waker.take() {
                waker.wake();
            }
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(SessionError::ReassemblerClosed))
        }
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("Sequencer::poll_close");
        let this = self.project();
        if !*this.is_closed {
            *this.is_closed = true;
            if let Some(waker) = this.tx_waker.take() {
                waker.wake();
            }
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(SessionError::ReassemblerClosed))
        }
    }
}

impl<T> futures::Stream for Sequencer<T>
where
    T: Ord + PartialEq<FrameId>,
{
    type Item = Result<T, SessionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        tracing::trace!("Sequencer::poll_next");
        if self.is_closed && self.buffer.is_empty() {
            tracing::trace!("Sequencer::poll_next done");
            return Poll::Ready(None);
        }

        if let Some(current_item) = self.buffer.peek().map(|e| &e.0) {
            let current_to_emit = self.next_id;
            let is_next_ready = current_item.eq(&current_to_emit);

            if is_next_ready || self.last_emitted.elapsed() >= self.timeout {
                self.last_emitted = Instant::now();
                self.next_id += 1;

                return if is_next_ready {
                    tracing::trace!("Sequencer::poll_next ready");
                    Poll::Ready(self.buffer.pop().map(|r| Ok(r.0)))
                } else {
                    tracing::trace!("Sequencer::poll_next discard");
                    Poll::Ready(Some(Err(SessionError::FrameDiscarded(current_to_emit))))
                };
            }
        }

        if self.is_closed {
            // If the Sink is closed, but the buffer is not empty,
            // we need to schedule automatic wake-up after the timeout
            // since the last emitted frame has elapsed.
            let next_wake_up = self.timeout - self.last_emitted.elapsed();
            let waker = cx.waker().clone();
            hopr_async_runtime::prelude::spawn(async move {
                hopr_async_runtime::prelude::sleep(next_wake_up).await;
                waker.wake();
            });
            tracing::trace!("Sequencer::poll_next pending on close");
        } else {
            // Otherwise, the next Sink operation will wake us up
            self.tx_waker = Some(cx.waker().clone());
            tracing::trace!("Sequencer::poll_next pending");
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::prelude::FutureExt;
    use futures::{pin_mut, SinkExt, StreamExt, TryStreamExt};

    #[test_log::test(async_std::test)]
    async fn sequencer_should_return_entries_in_order() -> anyhow::Result<()> {
        let (seq_sink, seq_stream) = Sequencer::<u32>::new(Duration::from_secs(2), 0).split();

        let mut expected = vec![4u32, 1, 5, 7, 8, 6, 2, 3];

        async_std::task::spawn(
            futures::stream::iter(expected.clone())
                .then(|e| futures::future::ok(e).delay(Duration::from_millis(5)))
                .forward(seq_sink),
        );

        let actual: Vec<u32> = seq_stream.try_collect().timeout(Duration::from_secs(5)).await??;

        expected.sort();
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_return_entries_in_order_without_flush() -> anyhow::Result<()> {
        let (mut seq_sink, seq_stream) = Sequencer::<u32>::new(Duration::from_secs(1), usize::MAX).split();

        let mut expected = vec![4u32, 1, 5, 7, 8, 6, 2, 3];

        let expected_clone = expected.clone();
        async_std::task::spawn(async move {
            for v in expected_clone {
                seq_sink.feed(v).await?;
            }
            seq_sink.flush().await?;
            seq_sink.close().await
        });

        let actual: Vec<u32> = seq_stream.try_collect().timeout(Duration::from_secs(5)).await??;

        expected.sort();
        assert_eq!(expected, actual);
        Ok(())
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_skip_entry_on_timeout() -> anyhow::Result<()> {
        let timeout = Duration::from_millis(50);
        let (seq_sink, seq_stream) = Sequencer::<u32>::new(timeout, 0).split();

        let input = vec![2u32, 1, 4];

        async_std::task::spawn(
            futures::stream::iter(input.clone())
                .then(futures::future::ok)
                .forward(seq_sink)
                .delay(Duration::from_millis(10)),
        );

        pin_mut!(seq_stream);

        assert_eq!(Some(1), seq_stream.try_next().await?);
        assert_eq!(Some(2), seq_stream.try_next().await?);

        let now = Instant::now();
        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(3))
        ));
        assert!(now.elapsed() >= timeout);

        assert_eq!(Some(4), seq_stream.try_next().await?);
        assert_eq!(None, seq_stream.try_next().await?);

        Ok(())
    }
}
