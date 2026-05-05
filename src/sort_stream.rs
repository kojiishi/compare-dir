use std::collections::HashMap;
use std::sync::mpsc;

/// Sorts an out-of-order stream of events from a producer.
///
/// Spawns a thread to run the `producer` with a new channel.
/// The producer writes events to its sender.
/// `sort_stream` receives these events, buffers them if they are out of order
/// based on `get_index`, and forwards them to `tx` in order.
///
/// Events where `get_index` returns `None` are forwarded immediately.
pub(crate) fn sort_stream<'scope, T>(
    producer: impl FnOnce(mpsc::Sender<T>) -> anyhow::Result<()> + Send + 'scope,
    tx: mpsc::Sender<T>,
    get_index: impl Fn(&T) -> Option<usize> + Send + 'scope,
) -> anyhow::Result<()>
where
    T: Send + Sync + 'static,
{
    let (tx_unordered, rx_unordered) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            if let Err(e) = producer(tx_unordered) {
                log::error!("Error during unordered production: {:?}", e);
            }
        });

        let mut buffer = HashMap::new();
        let mut next_index = 0;
        for event in rx_unordered {
            if let Some(index) = get_index(&event) {
                if index == next_index {
                    tx.send(event)?;
                    next_index += 1;
                    while let Some(buffered) = buffer.remove(&next_index) {
                        tx.send(buffered)?;
                        next_index += 1;
                    }
                } else {
                    buffer.insert(index, event);
                }
            } else {
                tx.send(event)?;
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Clone)]
    enum TestEvent {
        Start,
        Result(usize, String),
        End,
    }

    #[test]
    fn test_sort_stream_ordered() -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        sort_stream(
            |tx| {
                tx.send(TestEvent::Start)?;
                tx.send(TestEvent::Result(1, "one".to_string()))?;
                tx.send(TestEvent::Result(0, "zero".to_string()))?;
                tx.send(TestEvent::Result(2, "two".to_string()))?;
                tx.send(TestEvent::End)?;
                Ok(())
            },
            tx,
            |event| match event {
                TestEvent::Result(i, _) => Some(*i),
                _ => None,
            },
        )?;
        let mut final_results = Vec::new();
        while let Ok(event) = rx.recv() {
            final_results.push(event);
        }
        assert_eq!(
            final_results,
            vec![
                TestEvent::Start,
                TestEvent::Result(0, "zero".to_string()),
                TestEvent::Result(1, "one".to_string()),
                TestEvent::Result(2, "two".to_string()),
                TestEvent::End,
            ]
        );
        Ok(())
    }

    #[test]
    fn test_sort_stream_producer_stops_early() -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        sort_stream(
            |tx| {
                tx.send(TestEvent::Start)?;
                tx.send(TestEvent::Result(0, "zero".to_string()))?;
                anyhow::bail!("Producer error!");
            },
            tx,
            |event| match event {
                TestEvent::Result(i, _) => Some(*i),
                _ => None,
            },
        )?;
        let mut final_results = Vec::new();
        while let Ok(event) = rx.recv() {
            final_results.push(event);
        }
        assert_eq!(
            final_results,
            vec![TestEvent::Start, TestEvent::Result(0, "zero".to_string()),]
        );
        Ok(())
    }
}
