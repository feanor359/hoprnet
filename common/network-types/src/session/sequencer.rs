use std::collections::BinaryHeap;
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::prelude::errors::SessionError;
use crate::prelude::FrameId;

#[derive(Copy, Clone, Debug)]
pub struct SequencerConfig {
    pub timeout: Duration,
    pub flush_at: usize,
    pub initial_capacity: usize,
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            flush_at: 0,
            initial_capacity: 1024,
        }
    }
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct Sequencer<T> {
    buffer: BinaryHeap<std::cmp::Reverse<T>>,
    next_id: FrameId,
    last_emitted: Instant,
    tx_waker: Option<Waker>,
    is_closed: bool,
    cfg: SequencerConfig,
}

impl<T: Ord> Sequencer<T> {
    pub fn new(cfg: SequencerConfig) -> Self {
        Self {
            buffer: BinaryHeap::with_capacity(cfg.initial_capacity),
            next_id: 1,
            last_emitted: Instant::now(),
            tx_waker: None,
            is_closed: false,
            cfg,
        }
    }
}

impl<T> futures::Sink<T> for Sequencer<T>
where
    T: Ord + PartialOrd<FrameId>,
{
    type Error = SessionError;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("Sequencer::poll_ready");
        let this = self.project();
        if !*this.is_closed {
            if this.buffer.len() >= this.cfg.initial_capacity {
                this.buffer.reserve(this.cfg.initial_capacity);
            }
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(SessionError::ReassemblerClosed))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.project();
        if !*this.is_closed {
            if item.ge(this.next_id) {
                this.buffer.push(std::cmp::Reverse(item));
                if this.buffer.len() >= this.cfg.flush_at {
                    if let Some(waker) = this.tx_waker.take() {
                        waker.wake();
                    }
                }
            } else {
                tracing::warn!("cannot accept frame older than {}", this.next_id);
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

            if is_next_ready || self.last_emitted.elapsed() >= self.cfg.timeout {
                self.last_emitted = Instant::now();
                self.next_id += 1;

                return if is_next_ready {
                    tracing::trace!("Sequencer::poll_next ready {current_to_emit}");
                    Poll::Ready(self.buffer.pop().map(|r| Ok(r.0)))
                } else {
                    tracing::trace!("Sequencer::poll_next discard {current_to_emit}");
                    Poll::Ready(Some(Err(SessionError::FrameDiscarded(current_to_emit))))
                };
            }

            if self.is_closed && !is_next_ready {
                // If the Sink is closed, but the buffer is not empty,
                // we emit the missing ones as discarded frames, until we
                // catch up with the rest of the buffered frames to flush out.
                self.next_id += 1;
                tracing::trace!("Sequencer::poll_next discard {current_to_emit}");
                return Poll::Ready(Some(Err(SessionError::FrameDiscarded(current_to_emit))));
            }
        }

        // The next Sink operation will wake us up
        self.tx_waker = Some(cx.waker().clone());
        tracing::trace!("Sequencer::poll_next pending");

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
        let cfg = SequencerConfig {
            timeout: Duration::from_secs(2),
            ..Default::default()
        };

        let (seq_sink, seq_stream) = Sequencer::<u32>::new(cfg).split();

        let mut expected = vec![4u32, 1, 5, 7, 8, 6, 2, 3];

        let jh = hopr_async_runtime::prelude::spawn(
            futures::stream::iter(expected.clone())
                .then(|e| futures::future::ok(e).delay(Duration::from_millis(5)))
                .forward(seq_sink),
        );

        let actual: Vec<u32> = seq_stream.try_collect().timeout(Duration::from_secs(5)).await??;

        expected.sort();
        assert_eq!(expected, actual);

        Ok(jh.await?)
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_return_entries_in_order_without_flush() -> anyhow::Result<()> {
        let cfg = SequencerConfig {
            timeout: Duration::from_secs(1),
            flush_at: usize::MAX,
            ..Default::default()
        };

        let (mut seq_sink, seq_stream) = Sequencer::<u32>::new(cfg).split();

        let mut expected = vec![4u32, 1, 5, 7, 8, 6, 2, 3];

        let expected_clone = expected.clone();
        let jh = hopr_async_runtime::prelude::spawn(async move {
            for v in expected_clone {
                seq_sink.feed(v).await?;
            }
            seq_sink.flush().await?;
            seq_sink.close().await
        });

        let actual: Vec<u32> = seq_stream.try_collect().timeout(Duration::from_secs(5)).await??;

        expected.sort();
        assert_eq!(expected, actual);

        Ok(jh.await?)
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_not_allow_emitted_entries() -> anyhow::Result<()> {
        let cfg = SequencerConfig::default();

        let (seq_sink, seq_stream) = Sequencer::<u32>::new(cfg).split();

        pin_mut!(seq_sink);
        pin_mut!(seq_stream);

        seq_sink.send(1u32).await?;
        assert_eq!(Some(1), seq_stream.try_next().await?);

        seq_sink.send(2u32).await?;
        assert_eq!(Some(2), seq_stream.try_next().await?);

        seq_sink.send(2u32).await?;
        seq_sink.send(1u32).await?;

        seq_sink.send(3u32).await?;
        assert_eq!(Some(3), seq_stream.try_next().await?);

        Ok(())
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_discard_entry_on_timeout() -> anyhow::Result<()> {
        let cfg = SequencerConfig {
            timeout: Duration::from_millis(25),
            ..Default::default()
        };

        let (mut seq_sink, seq_stream) = Sequencer::<u32>::new(cfg).split();

        let input = vec![2u32, 1, 4, 5, 8, 7, 9, 11, 10];

        let input_clone = input.clone();
        let jh = hopr_async_runtime::prelude::spawn(async move {
            for v in input_clone {
                seq_sink.feed(v).delay(Duration::from_millis(5)).await?;
            }
            seq_sink.flush().await?;
            seq_sink.close().await
        });

        pin_mut!(seq_stream);

        assert_eq!(Some(1), seq_stream.try_next().await?);
        assert_eq!(Some(2), seq_stream.try_next().await?);

        let now = Instant::now();
        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(3))
        ));
        assert!(now.elapsed() >= cfg.timeout);

        assert_eq!(Some(4), seq_stream.try_next().await?);
        assert_eq!(Some(5), seq_stream.try_next().await?);

        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(6))
        ));

        assert_eq!(Some(7), seq_stream.try_next().await?);
        assert_eq!(Some(8), seq_stream.try_next().await?);
        assert_eq!(Some(9), seq_stream.try_next().await?);
        assert_eq!(Some(10), seq_stream.try_next().await?);
        assert_eq!(Some(11), seq_stream.try_next().await?);

        assert_eq!(None, seq_stream.try_next().await?);

        Ok(jh.await?)
    }

    #[test_log::test(async_std::test)]
    async fn sequencer_should_discard_entry_close() -> anyhow::Result<()> {
        let cfg = SequencerConfig {
            timeout: Duration::from_millis(25),
            ..Default::default()
        };

        let (seq_sink, seq_stream) = Sequencer::<u32>::new(cfg).split();

        let input = vec![2u32, 1, 3, 5, 4, 8, 11];

        hopr_async_runtime::prelude::spawn(futures::stream::iter(input.clone()).map(Ok).forward(seq_sink)).await?;

        pin_mut!(seq_stream);

        assert_eq!(Some(1), seq_stream.try_next().await?);
        assert_eq!(Some(2), seq_stream.try_next().await?);
        assert_eq!(Some(3), seq_stream.try_next().await?);
        assert_eq!(Some(4), seq_stream.try_next().await?);
        assert_eq!(Some(5), seq_stream.try_next().await?);
        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(6))
        ));
        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(7))
        ));
        assert_eq!(Some(8), seq_stream.try_next().await?);
        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(9))
        ));
        assert!(matches!(
            seq_stream.try_next().await,
            Err(SessionError::FrameDiscarded(10))
        ));
        assert_eq!(Some(11), seq_stream.try_next().await?);
        assert_eq!(None, seq_stream.try_next().await?);

        Ok(())
    }
}
