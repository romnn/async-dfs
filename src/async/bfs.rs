use super::{Node, NodeStream, StreamQueue};

use futures::stream::{FuturesOrdered, Stream, StreamExt};
use futures::FutureExt;
use pin_project::pin_project;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

#[allow(clippy::module_name_repetitions)]
#[derive(Default)]
#[pin_project]
/// Asynchronous breadth-first stream for types implementing the [`Node`] trait.
///
/// ### Example
/// ```
/// use futures::StreamExt;
/// use par_dfs::r#async::{Node, Bfs, NodeStream};
///
/// #[derive(PartialEq, Eq, Hash, Clone, Debug)]
/// struct WordNode(String);
///
/// #[async_trait::async_trait]
/// impl Node for WordNode {
///     type Error = std::convert::Infallible;
///
///     async fn children(
///         self: std::sync::Arc<Self>,
///         _depth: usize
///     ) -> Result<NodeStream<Self, Self::Error>, Self::Error> {
///         let len = self.0.len();
///         let nodes: Vec<String> = if len > 1 {
///             let mid = len/2;
///             vec![self.0[..mid].into(), self.0[mid..].into()]
///         } else {
///             assert!(len == 1);
///             vec![self.0.clone()]
///         };
///         let nodes = nodes.into_iter()
///             .map(Self)
///             .map(Result::Ok);
///         let stream = futures::stream::iter(nodes);
///         Ok(Box::pin(stream.boxed()))
///     }
/// }
///
/// let result = tokio_test::block_on(async {
///     let word = "Hello World";
///     let root = WordNode(word.into());
///     let limit = (word.len() as f32).log2().ceil() as usize;
///     let bfs = Bfs::<WordNode>::new(root, limit, true);
///     let output = bfs
///         .collect::<Vec<_>>()
///         .await
///         .into_iter()
///         .collect::<Result<Vec<_>, _>>()
///         .unwrap();
///     output[output.len()-word.len()..]
///         .into_iter().map(|s| s.0.as_str()).collect::<String>()
/// });
/// assert_eq!(result, "Hello World");
/// ```
///
/// [`Node`]: trait@crate::async::Node
pub struct Bfs<N>
where
    N: Node,
{
    #[pin]
    current_stream: Option<(usize, NodeStream<N, N::Error>)>,
    child_streams_futs: StreamQueue<N, N::Error>,
    max_depth: Option<usize>,
    allow_circles: bool,
    visited: HashSet<N>,
}

impl<N> Bfs<N>
where
    N: Node + Send + Unpin + Clone + 'static,
    N::Error: Send + 'static,
{
    #[inline]
    /// Creates a new [`Bfs`] stream.
    ///
    /// The BFS will be performed from the `root` node up to depth `max_depth`.
    ///
    /// When `allow_circles`, visited nodes will not be tracked, which can lead to cycles.
    ///
    /// [`Bfs`]: struct@crate::async::Bfs
    pub fn new<R, D>(root: R, max_depth: D, allow_circles: bool) -> Self
    where
        R: Into<N>,
        D: Into<Option<usize>>,
    {
        let root = root.into();
        let max_depth = max_depth.into();
        let mut child_streams_futs: StreamQueue<N, N::Error> = FuturesOrdered::new();
        let depth = 1;
        let child_stream_fut = Arc::new(root.clone())
            .children(depth)
            .map(move |stream| (depth, stream));
        child_streams_futs.push_back(Box::pin(child_stream_fut));

        Self {
            current_stream: None,
            child_streams_futs,
            max_depth,
            visited: HashSet::from_iter([root]),
            allow_circles,
        }
    }
}

impl<N> Stream for Bfs<N>
where
    N: Node + Send + Clone + Unpin + 'static,
    N::Error: Send + 'static,
{
    type Item = Result<N, N::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // println!("------- poll");
        // println!("has current stream: {:?}", this.current_stream.is_some());

        loop {
            let mut current_stream = this.current_stream.as_mut().as_pin_mut();
            let next_item = match current_stream.as_deref_mut() {
                Some((depth, stream)) => {
                    let next_item = stream.as_mut().poll_next(cx);
                    Some(next_item.map(|node| (depth, node)))
                }
                None => None,
            };

            // println!("next item: {:?}", next_item);
            match next_item {
                // stream item is ready but failure success
                Some(Poll::Ready((_, Some(Err(err))))) => {
                    return Poll::Ready(Some(Err(err)));
                }
                // stream item is ready and success
                Some(Poll::Ready((depth, Some(Ok(node))))) => {
                    if *this.allow_circles || !this.visited.contains(&node) {
                        if !*this.allow_circles {
                            this.visited.insert(node.clone());
                        }

                        if let Some(max_depth) = this.max_depth {
                            if depth >= max_depth {
                                return Poll::Ready(Some(Ok(node)));
                            }
                        }

                        // add child stream future to be polled
                        let arc_node = Arc::new(node.clone());
                        let next_depth = *depth + 1;
                        let child_stream_fut = arc_node
                            .children(next_depth)
                            .map(move |stream| (next_depth, stream));
                        this.child_streams_futs
                            .push_back(Box::pin(child_stream_fut));

                        return Poll::Ready(Some(Ok(node)));
                    }
                }
                // stream item is pending
                Some(Poll::Pending) => {
                    return Poll::Pending;
                }
                // no current stream or completed
                Some(Poll::Ready((_, None))) | None => {
                    // proceed to poll the next stream
                }
            }

            // poll the next stream
            // println!("child stream futs: {:?}", this.child_streams_futs.len());
            match this.child_streams_futs.poll_next_unpin(cx) {
                Poll::Ready(Some((depth, stream))) => {
                    // println!(
                    //     "child stream fut depth {} completed: {:?}",
                    //     depth,
                    //     stream.is_ok()
                    // );
                    let stream = match stream {
                        Ok(stream) => stream.boxed(),
                        Err(err) => futures::stream::iter([Err(err)]).boxed(),
                    };
                    this.current_stream.set(Some((depth, Box::pin(stream))));
                }
                // when there are no more child stream futures,
                // we are done
                Poll::Ready(None) => {
                    // println!("no more child streams");
                    return Poll::Ready(None);
                }
                // still waiting for the next stream
                Poll::Pending => {
                    // println!("child stream is still pending");
                    return Poll::Pending;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Bfs;
    use anyhow::Result;

    macro_rules! depths {
        ($stream:ident) => {{
            $stream
                // collect the entire stream
                .collect::<Vec<_>>()
                .await
                .into_iter()
                // fail on first error
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                // get depth
                .map(|item| item.0)
                .collect::<Vec<_>>()
        }};
    }

    macro_rules! test_depths_unordered {
        ($name:ident: $values:expr) => {
            paste::item! {
                #[tokio::test(flavor = "multi_thread")]
                async fn [< test_ $name _ unordered >] () -> Result<()> {
                    use tokio::time::{sleep, Duration};
                    use std::cmp::Ordering;
                    use futures::StreamExt;
                    let (iter, expected_depths) = $values;
                    let iter = iter
                        .map(|node| async move {
                            sleep(Duration::from_millis(100)).await;
                            node
                        })
                        .buffer_unordered(8);
                    let depths = depths!(iter);
                    assert!(crate::utils::test::is_monotonic(&depths, Ordering::Greater));
                    crate::utils::test::assert_eq_sorted!(depths, expected_depths);
                    Ok(())
                }
            }
        };
    }

    macro_rules! test_depths_ordered {
        ($name:ident: $values:expr) => {
            paste::item! {
                #[tokio::test(flavor = "multi_thread")]
                async fn [< test_ $name _ ordered >] () -> Result<()> {
                    use tokio::time::{sleep, Duration};
                    use std::cmp::Ordering;
                    use futures::StreamExt;
                    let (iter, expected_depths) = $values;
                    let iter = iter
                        .map(|node| async move {
                            sleep(Duration::from_millis(100)).await;
                            node
                        })
                        .buffered(8);
                    let depths = depths!(iter);
                    assert!(crate::utils::test::is_monotonic(&depths, Ordering::Greater));
                    similar_asserts::assert_eq!(depths, expected_depths);
                    Ok(())
                }
            }
        };
    }

    macro_rules! test_depths {
        ($name:ident: $values:expr, $($macro:ident,)*) => {
            $(
                $macro!($name: $values);
            )*
        }
    }

    test_depths!(
        bfs:
        (
            Bfs::<crate::utils::test::Node>::new(0, 3, true),
            [1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3]
        ),
        test_depths_ordered,
        test_depths_unordered,
    );

    test_depths!(
        bfs_no_circles:
        (
            Bfs::<crate::utils::test::Node>::new(0, 3, false),
            [1, 2, 3]
        ),
        test_depths_ordered,
        test_depths_unordered,
    );
}
