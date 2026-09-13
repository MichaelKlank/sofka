use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn popup_text(app: &mut App, width: u16, height: u16, title: &str) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let title_row = (0..height)
        .rev()
        .find(|&y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains(title)
        })
        .expect("popup title is visible");
    let top = (0..=title_row)
        .rev()
        .find(|&y| (0..width).any(|x| buffer[(x, y)].symbol() == "╭"))
        .unwrap();
    let left = (0..width)
        .rev()
        .find(|&x| buffer[(x, top)].symbol() == "╭")
        .unwrap();
    let right = (left..width)
        .find(|&x| buffer[(x, top)].symbol() == "╮")
        .unwrap();
    let bottom = (top + 1..height)
        .find(|&y| buffer[(left, y)].symbol() == "╰")
        .unwrap();
    if matches!(title, "Confirm" | "Input") {
        assert!(left > 0 && right < width - 1);
        assert!(top > 0 && bottom < height - 1);
        assert!(usize::from(bottom - top + 1) <= usize::from(height) * 70 / 100);
    }
    (top + 1..bottom)
        .flat_map(|y| (left + 1..right).map(move |x| (x, y)))
        .map(|pos| buffer[pos].symbol())
        .collect()
}

fn compact(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != '▌')
        .collect()
}

#[tokio::test]
async fn confirmation_popup_wraps_the_full_plugin_label_and_controls() {
    let (mut app, _rx) = app_with_pod();
    app.cluster.context = "admin@home-kubernetes-with-a-long-context-name".into();
    app.plugins = vec![crate::config::Plugin {
        palette: Some("benchmark-test".into()),
        name: "HTTP benchmark with a long description 日本語".into(),
        command: "true".into(),
        dangerous: true,
        ..Default::default()
    }];
    plugin_command(&mut app, "benchmark-test");
    assert_eq!(app.mode, Mode::Confirm);
    for (width, height) in [(160, 30), (60, 24), (32, 28), (100, 12)] {
        let text = compact(&popup_text(&mut app, width, height, "Confirm"));
        assert!(
            text.contains(&compact(&app.confirm_label)),
            "{width}x{height}: {text}"
        );
        assert!(text.contains("y:confirm"), "{text}");
        assert!(text.contains("esc:cancel"), "{text}");
        assert!(app.pending.is_none());
    }
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn input_popup_wraps_the_label_value_cursor_and_controls() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    let name = "production-region-with-a-long-context-name-日本語";
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec![name.into()],
    });
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    for c in "-renamed".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    for (width, height) in [(100, 24), (48, 24), (32, 28)] {
        let text = compact(&popup_text(&mut app, width, height, "Input"));
        assert!(text.contains(&compact(&app.prompt_label)), "{text}");
        assert!(
            text.contains(&format!("{}█", compact(&app.prompt_input))),
            "{text}"
        );
        assert!(text.contains("enter:apply"), "{text}");
        assert!(text.contains("esc:cancel"), "{text}");
    }
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Contexts);
}

#[tokio::test]
async fn context_popup_wraps_rows_and_keeps_selection_after_resize() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    let names = (0..12)
        .map(|i| format!("region-{i:02}-{}-日本語-tail", "abcdefghij".repeat(5)))
        .collect::<Vec<_>>();
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: names.clone(),
    });
    app.handle_key(press(KeyCode::Home)).unwrap();
    for _ in 0..8 {
        app.handle_key(press(KeyCode::Down)).unwrap();
    }
    let selected = app.ctx_state.selected().unwrap();
    for width in [100, 48, 32, 100] {
        let text = compact(&popup_text(&mut app, width, 24, "Contexts"));
        assert!(
            text.contains(&compact(&names[selected])),
            "width {width}: {text}"
        );
        assert_eq!(app.ctx_state.selected(), Some(selected));
    }
}

#[tokio::test]
async fn command_popup_wraps_long_context_suggestions() {
    let (mut app, _rx) = test_app();
    let name = format!("gke-{}-tail", "production-region-".repeat(5));
    app.all_contexts = vec![name.clone()];
    for c in ":pods @gke".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    for width in [120, 80] {
        let text = compact(&popup_text(&mut app, width, 24, "commands"));
        assert!(text.contains(&compact(&name)), "width {width}: {text}");
        assert!(text.contains("ctx"));
    }
}

#[tokio::test]
async fn confirmation_popup_pages_long_text_with_controls_visible() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![crate::config::Plugin {
        palette: Some("benchmark-test".into()),
        name: format!("first-line {} final-line", "long-description ".repeat(80)),
        command: "true".into(),
        dangerous: true,
        ..Default::default()
    }];
    plugin_command(&mut app, "benchmark-test");
    let first = compact(&popup_text(&mut app, 60, 24, "Confirm"));
    assert!(first.contains("first-line"));
    assert!(app.popup_max_scroll > 0);
    let mut seen = first;
    while app.popup_scroll < app.popup_max_scroll {
        let previous = app.popup_scroll;
        app.handle_key(press(KeyCode::PageDown)).unwrap();
        assert!(app.popup_scroll > previous);
        assert_eq!(app.mode, Mode::Confirm);
        let text = compact(&popup_text(&mut app, 60, 24, "Confirm"));
        assert!(text.contains("y:confirm"));
        assert!(text.contains("esc:cancel"));
        seen.push_str(&text);
    }
    assert!(seen.contains("final-line"));
    assert!(app.pending.is_none());
    while app.popup_scroll > 0 {
        app.handle_key(press(KeyCode::PageUp)).unwrap();
    }
    assert!(compact(&popup_text(&mut app, 60, 24, "Confirm")).contains("first-line"));
    app.handle_key(press(KeyCode::Esc)).unwrap();
    plugin_command(&mut app, "benchmark-test");
    assert_eq!(app.popup_scroll, 0);
}

#[tokio::test]
async fn input_popup_follows_the_cursor_after_scrolling() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["test".into()],
    });
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    for _ in 0..200 {
        app.handle_key(press(KeyCode::Char('a'))).unwrap();
    }
    let text = compact(&popup_text(&mut app, 40, 16, "Input"));
    assert!(text.contains('█'));
    assert!(text.contains("enter:apply"));
    assert_eq!(app.popup_scroll, app.popup_max_scroll);
    assert!(app.popup_scroll > 0);
    app.handle_key(press(KeyCode::PageUp)).unwrap();
    assert!(app.popup_scroll < app.popup_max_scroll);
    app.handle_key(press(KeyCode::Char('z'))).unwrap();
    let text = compact(&popup_text(&mut app, 40, 16, "Input"));
    assert!(text.contains("z█"));
    assert_eq!(app.popup_scroll, app.popup_max_scroll);
    popup_text(&mut app, 120, 40, "Input");
    assert_eq!(app.popup_scroll, 0);
}
