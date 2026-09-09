use super::*;
use std::time::Instant;

const SAMPLES: usize = 60;
const BIN_SECONDS: u64 = 5;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TrendTarget {
    generation: u64,
    key: String,
    uid: Option<String>,
}

#[derive(Default)]
pub(crate) struct ContainerHistory {
    target: Option<TrendTarget>,
    samples: VecDeque<(Instant, Option<(i64, i64)>)>,
}

impl ContainerHistory {
    fn select(&mut self, target: Option<TrendTarget>) {
        if self.target != target {
            self.target = target;
            self.samples.clear();
        }
    }

    fn record(&mut self, value: Option<(i64, i64)>, now: Instant) {
        if self.target.is_none() {
            return;
        }
        while self.samples.front().is_some_and(|(time, _)| {
            now.saturating_duration_since(*time).as_secs() >= SAMPLES as u64 * BIN_SECONDS
        }) {
            self.samples.pop_front();
        }
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now, value));
    }

    fn bars(&self, now: Instant, cpu: bool) -> [Option<u64>; SAMPLES] {
        let mut bars = [None; SAMPLES];
        for (time, value) in &self.samples {
            let age = now.saturating_duration_since(*time).as_secs() / BIN_SECONDS;
            if age < SAMPLES as u64 {
                bars[SAMPLES - 1 - age as usize] = value
                    .map(|(c, m)| if cpu { c } else { m })
                    .and_then(|v| u64::try_from(v).ok());
            }
        }
        bars
    }
}

impl App {
    fn container_trend_target(&self) -> Option<TrendTarget> {
        if self.mode != Mode::Containers
            && !(self.mode == Mode::Command && self.palette_return == Mode::Containers)
        {
            return None;
        }
        let (ns, pod) = self.container_pod.as_ref()?;
        let container = self.container_list.get(self.container_state.selected()?)?;
        let pod_key = format!("{ns}/{pod}");
        let obj = self.store.get(&pod_key)?;
        Some(TrendTarget {
            generation: self.generation,
            key: format!("{pod_key}/{container}"),
            uid: obj.metadata.uid.clone(),
        })
    }

    pub(super) fn sync_container_history(&mut self) {
        self.container_history.select(self.container_trend_target());
    }

    pub(super) fn record_container_history(&mut self, failed: bool) {
        self.sync_container_history();
        let value = self.container_history.target.as_ref().and_then(|target| {
            if failed {
                None
            } else {
                self.container_metrics.get(&target.key).copied()
            }
        });
        self.container_history.record(value, Instant::now());
    }

    pub(crate) fn container_trend_bars(&self, cpu: bool) -> [Option<u64>; SAMPLES] {
        if self.container_history.target != self.container_trend_target() {
            return [None; SAMPLES];
        }
        self.container_history.bars(Instant::now(), cpu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_bounded_and_missing_values_are_not_zero() {
        let mut history = ContainerHistory::default();
        history.select(Some(TrendTarget {
            generation: 1,
            key: "ns/pod/app".into(),
            uid: Some("a".into()),
        }));
        let start = Instant::now();
        for i in 0..100 {
            history.record(Some((i, i * 2)), start + Duration::from_secs(i as u64 * 5));
        }
        assert_eq!(history.samples.len(), 60);
        let now = start + Duration::from_secs(500);
        history.record(None, now);
        let bars = history.bars(now, true);
        assert_eq!(bars[59], None);
        assert_eq!(bars[58], Some(99));
        history.record(Some((0, 0)), now + Duration::from_secs(5));
        assert_eq!(
            history.bars(now + Duration::from_secs(5), true)[59],
            Some(0)
        );
        assert_eq!(
            history.bars(now + Duration::from_secs(400), true),
            [None; 60]
        );
        history.select(Some(TrendTarget {
            generation: 1,
            key: "ns/pod/app".into(),
            uid: Some("b".into()),
        }));
        assert!(history.samples.is_empty());
    }
}
