use crate::prelude::errors::SessionError;
use crate::prelude::{Frame, FrameId, Segment};
use crate::session::frame::SeqNum;
use crate::session::sequencer::Sequencer;
use futures::StreamExt;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct FrameBuilder {
    segments: BTreeSet<Segment>,
    last_recv: Instant,
}

impl From<Segment> for FrameBuilder {
    fn from(value: Segment) -> Self {
        Self {
            segments: BTreeSet::from_iter([value]),
            last_recv: Instant::now(),
        }
    }
}

impl FrameBuilder {
    pub fn add_segment(&mut self, segment: Segment) {
        self.segments.insert(segment);
        self.last_recv = Instant::now();
    }

    pub fn frame_id(&self) -> FrameId {
        self.segments.first().unwrap().frame_id
    }

    pub fn remaining(&self) -> SeqNum {
        self.segments.first().unwrap().seq_len - self.segments.len() as SeqNum
    }

    pub fn build(self) -> Result<Frame, SessionError> {
        let frame_id = self.frame_id();

        if self.remaining() > 0 {
            return Err(SessionError::IncompleteFrame(frame_id));
        }

        Ok(Frame {
            frame_id,
            data: self
                .segments
                .into_iter()
                .flat_map(|s| s.data.into_vec())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        })
    }
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct Reassembler {
    frames: HashMap<FrameId, FrameBuilder>,
    complete_frames: VecDeque<Result<Frame, SessionError>>,
    tx_waker: Option<Waker>,
    is_closed: bool,
    max_age: Duration,
}

impl Reassembler {
    pub fn new(max_age: Duration) -> Self {
        Self {
            frames: Default::default(),
            complete_frames: VecDeque::with_capacity(1024),
            tx_waker: None,
            is_closed: false,
            max_age,
        }
    }

    fn expire_frames(&mut self) {
        self.frames.retain(|id, b| {
            if b.last_recv.elapsed() >= self.max_age || self.is_closed {
                self.complete_frames.push_back(Err(SessionError::FrameDiscarded(*id)));
                tracing::trace!("frame {id} discarded");
                false
            } else {
                true
            }
        });
    }
}

impl futures::Sink<Segment> for Reassembler {
    type Error = SessionError;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("Reassembler::poll_ready");
        let this = self.project();
        if !*this.is_closed {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(SessionError::ReassemblerClosed))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Segment) -> Result<(), Self::Error> {
        let this = self.project();
        if !*this.is_closed {
            match this.frames.entry(item.frame_id) {
                Entry::Occupied(mut e) => {
                    let builder = e.get_mut();
                    builder.add_segment(item);
                    if builder.remaining() == 0 {
                        tracing::trace!("frame {} is complete", e.key());
                        this.complete_frames.push_back(e.remove().build());
                        if let Some(waker) = this.tx_waker.take() {
                            waker.wake();
                        }
                    }
                }
                Entry::Vacant(e) => {
                    let builder = FrameBuilder::from(item);
                    if builder.remaining() == 0 {
                        tracing::trace!("single segment frame {} is complete", builder.frame_id());
                        this.complete_frames.push_back(builder.build());
                        if let Some(waker) = this.tx_waker.take() {
                            waker.wake();
                        }
                    } else {
                        e.insert(builder);
                    }
                }
            };

            Ok(())
        } else {
            Err(SessionError::ReassemblerClosed)
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("Reassembler::poll_flush");
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
        tracing::trace!("Reassembler::poll_close");
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

impl futures::Stream for Reassembler {
    type Item = Result<Frame, SessionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        tracing::trace!("Reassembler::poll_next");
        self.expire_frames();

        if self.is_closed && self.complete_frames.is_empty() {
            tracing::trace!("Reassembler::poll_next done");
            return Poll::Ready(None);
        }

        if let Some(complete) = self.complete_frames.pop_front() {
            Poll::Ready(Some(
                complete
                    .inspect(|f| tracing::trace!("Reassembler::poll_next ready {}", f.frame_id))
                    .inspect_err(|e| tracing::trace!("Reassembler::poll_next ready error ({e})")),
            ))
        } else if !self.is_closed {
            self.tx_waker = Some(cx.waker().clone());
            tracing::trace!("Reassembler::poll_next pending");
            Poll::Pending
        } else {
            tracing::trace!("Reassembler::poll_next done");
            Poll::Ready(None)
        }
    }
}

pub fn frame_reconstructor(
    frame_timeout: Duration,
) -> (
    impl futures::Sink<Segment, Error = SessionError>,
    impl futures::Stream<Item = Result<Frame, SessionError>>,
) {
    let (sink, rs_stream) = Reassembler::new(frame_timeout).split();
    let (seq_sink, stream) = Sequencer::new(frame_timeout, 0).split();

    hopr_async_runtime::prelude::spawn(
        rs_stream
            .filter_map(|maybe_frame| async {
                maybe_frame
                    .inspect_err(|e| tracing::error!("failed to reassemble frame: {e}"))
                    .ok()
                    .map(Ok)
            })
            .forward(seq_sink),
    );
    (sink, stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::prelude::FutureExt;
    use futures::{StreamExt, TryStreamExt};
    use hex_literal::hex;
    use rand::prelude::SliceRandom;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    const RNG_SEED: [u8; 32] = hex!("d8a471f1c20490a3442b96fdde9d1807428096e1601b0cef0eea7e6d44a24c01");

    #[test_log::test(async_std::test)]
    pub async fn reassembler_should_reassemble_frames() -> anyhow::Result<()> {
        let expected = (1u32..=10)
            .map(|frame_id| Frame {
                frame_id,
                data: hopr_crypto_random::random_bytes::<100>().into(),
            })
            .collect::<Vec<_>>();

        let (r_sink, r_stream) = Reassembler::new(Duration::from_secs(5)).split();

        let mut segments = expected
            .iter()
            .cloned()
            .flat_map(|f| f.segment(22).unwrap())
            .collect::<Vec<_>>();

        let mut rng = StdRng::from_seed(RNG_SEED);
        segments.shuffle(&mut rng);

        hopr_async_runtime::prelude::spawn(futures::stream::iter(segments).map(Ok).forward(r_sink));

        let mut actual = r_stream
            .try_collect::<Vec<_>>()
            .timeout(Duration::from_secs(5))
            .await??;

        assert_eq!(actual.len(), expected.len());

        actual.sort();
        assert_eq!(actual, expected);

        Ok(())
    }

    #[test_log::test(async_std::test)]
    pub async fn reassembler_should_discard_incomplete_frames_on_expiration() -> anyhow::Result<()> {
        let expected = (1u32..=10)
            .map(|frame_id| Frame {
                frame_id,
                data: hopr_crypto_random::random_bytes::<100>().into(),
            })
            .collect::<Vec<_>>();

        let (r_sink, r_stream) = Reassembler::new(Duration::from_millis(45)).split();

        let mut segments = expected
            .iter()
            .cloned()
            .flat_map(|f| f.segment(22).unwrap())
            .filter(|s| s.frame_id != 2 || s.seq_idx != 1)
            .collect::<Vec<_>>();

        let mut rng = StdRng::from_seed(RNG_SEED);
        segments.shuffle(&mut rng);

        let seg_count = segments.len();
        hopr_async_runtime::prelude::spawn(
            futures::stream::iter(segments)
                .enumerate()
                .then(move |(i, s)| {
                    async move {
                        // Delay the very last segment,
                        // so the incomplete frame gets a chance to expire
                        if i == seg_count - 1 {
                            futures::future::ok(s).delay(Duration::from_millis(55)).await
                        } else {
                            Ok(s)
                        }
                    }
                })
                .forward(r_sink),
        );

        let mut actual = r_stream.collect::<Vec<_>>().timeout(Duration::from_secs(5)).await?;

        assert_eq!(actual.len(), expected.len());

        actual.sort_by(|a, b| match (a, b) {
            (Ok(a), Ok(b)) => a.frame_id.cmp(&b.frame_id),
            (Err(SessionError::FrameDiscarded(a)), Ok(b)) => a.cmp(&b.frame_id),
            (Ok(a), Err(SessionError::FrameDiscarded(b))) => a.frame_id.cmp(b),
            (Err(SessionError::FrameDiscarded(a)), Err(SessionError::FrameDiscarded(b))) => a.cmp(b),
            _ => panic!("unexpected result"),
        });

        for i in 0..expected.len() {
            if i != 1 {
                assert!(matches!(&actual[i], Ok(f) if *f == expected[i]));
            } else {
                assert!(matches!(actual[i], Err(SessionError::FrameDiscarded(2))));
            }
        }

        Ok(())
    }

    #[test_log::test(async_std::test)]
    pub async fn reassembler_should_discard_incomplete_frames_on_close() -> anyhow::Result<()> {
        let expected = (1u32..=10)
            .map(|frame_id| Frame {
                frame_id,
                data: hopr_crypto_random::random_bytes::<100>().into(),
            })
            .collect::<Vec<_>>();

        let (r_sink, r_stream) = Reassembler::new(Duration::from_millis(100)).split();

        let mut segments = expected
            .iter()
            .cloned()
            .flat_map(|f| f.segment(22).unwrap())
            .filter(|s| s.frame_id != 5 || s.seq_idx != 2)
            .collect::<Vec<_>>();

        let mut rng = StdRng::from_seed(RNG_SEED);
        segments.shuffle(&mut rng);

        hopr_async_runtime::prelude::spawn(futures::stream::iter(segments).map(Ok).forward(r_sink));

        let mut actual = r_stream.collect::<Vec<_>>().timeout(Duration::from_secs(5)).await?;

        assert_eq!(actual.len(), expected.len());

        actual.sort_by(|a, b| match (a, b) {
            (Ok(a), Ok(b)) => a.frame_id.cmp(&b.frame_id),
            (Err(SessionError::FrameDiscarded(a)), Ok(b)) => a.cmp(&b.frame_id),
            (Ok(a), Err(SessionError::FrameDiscarded(b))) => a.frame_id.cmp(b),
            (Err(SessionError::FrameDiscarded(a)), Err(SessionError::FrameDiscarded(b))) => a.cmp(b),
            _ => panic!("unexpected result"),
        });

        for i in 0..expected.len() {
            if i != 4 {
                assert!(matches!(&actual[i], Ok(f) if *f == expected[i]));
            } else {
                assert!(matches!(actual[i], Err(SessionError::FrameDiscarded(5))));
            }
        }

        Ok(())
    }

    #[async_std::test]
    pub async fn reassembler_and_sequencer_test() -> anyhow::Result<()> {
        let expected = (1u32..=10)
            .map(|frame_id| Frame {
                frame_id,
                data: hopr_crypto_random::random_bytes::<100>().into(),
            })
            .collect::<Vec<_>>();

        let (r_sink, seq_stream) = frame_reconstructor(Duration::from_secs(5));

        let mut segments = expected
            .iter()
            .cloned()
            .flat_map(|f| f.segment(22).unwrap())
            .collect::<Vec<_>>();

        let mut rng = StdRng::from_seed(RNG_SEED);
        segments.shuffle(&mut rng);

        hopr_async_runtime::prelude::spawn(futures::stream::iter(segments).map(Ok).forward(r_sink));

        let actual = seq_stream
            .try_collect::<Vec<_>>()
            .timeout(Duration::from_secs(5))
            .await??;

        assert_eq!(actual, expected);
        Ok(())
    }

    #[test_log::test(async_std::test)]
    pub async fn reassembler_and_sequencer_test_missing_segment() -> anyhow::Result<()> {
        let expected = (1u32..=10)
            .map(|frame_id| Frame {
                frame_id,
                data: hopr_crypto_random::random_bytes::<100>().into(),
            })
            .collect::<Vec<_>>();

        let (r_sink, seq_stream) = frame_reconstructor(Duration::from_millis(50));

        let mut segments = expected
            .iter()
            .cloned()
            .flat_map(|f| f.segment(22).unwrap())
            .filter(|s| s.frame_id != 4 || s.seq_idx != 1)
            .collect::<Vec<_>>();

        let mut rng = StdRng::from_seed(RNG_SEED);
        segments.shuffle(&mut rng);

        hopr_async_runtime::prelude::spawn(futures::stream::iter(segments).map(Ok).forward(r_sink));

        let actual = seq_stream.collect::<Vec<_>>().timeout(Duration::from_secs(5)).await?;

        assert_eq!(actual.len(), expected.len());
        for i in 0..expected.len() {
            if i != 3 {
                assert!(matches!(&actual[i], Ok(frame) if expected[i].eq(frame)));
            } else {
                assert!(matches!(actual[i], Err(SessionError::FrameDiscarded(4))))
            }
        }
        Ok(())
    }
}
