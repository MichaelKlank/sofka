use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
pub(crate) struct Title {
    current: Option<String>,
}

impl Title {
    pub(crate) fn update(&mut self, next: Option<String>) {
        self.update_with(next, set);
    }

    fn update_with(&mut self, next: Option<String>, write: impl FnOnce(Option<&str>)) {
        if self.current != next {
            write(next.as_deref());
            self.current = next;
        }
    }
}

pub(crate) fn set(title: Option<&str>) {
    if let Some(title) = title {
        ACTIVE.store(true, Ordering::Relaxed);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(title));
    } else {
        clear();
    }
}

pub(crate) fn clear() {
    if ACTIVE.swap(false, Ordering::Relaxed) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_only_changes_and_clears_on_disable() {
        let mut title = Title::default();
        let mut writes = Vec::new();
        for next in [
            None,
            Some("sofka: dev/all"),
            Some("sofka: dev/all"),
            Some("sofka: prod/default"),
            None,
            None,
        ] {
            title.update_with(next.map(str::to_owned), |value| {
                writes.push(value.map(str::to_owned))
            });
        }
        assert_eq!(
            writes,
            [
                Some("sofka: dev/all".into()),
                Some("sofka: prod/default".into()),
                None
            ]
        );
    }
}
