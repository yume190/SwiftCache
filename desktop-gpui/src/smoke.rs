mod native;
use crate::app::Workspace;
use gpui_kit::component::WindowExt;
use gpui_kit::*;
use serde_json::{Value, json};

fn point_click(window: &mut Window, position: Point<Pixels>, cx: &mut App) {
    window.dispatch_event(
        MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }
        .to_platform_input(),
        cx,
    );
    window.dispatch_event(
        MouseDownEvent {
            position,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Modifiers::default(),
            first_mouse: false,
        }
        .to_platform_input(),
        cx,
    );
    window.dispatch_event(
        MouseUpEvent {
            position,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Modifiers::default(),
        }
        .to_platform_input(),
        cx,
    );
}
fn key(window: &mut Window, text: &str, cx: &mut App) {
    let keystroke = Keystroke::parse_with_os(text, cx.platform_info().os).unwrap();
    window.dispatch_event(
        KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        }
        .to_platform_input(),
        cx,
    );
    window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
}

pub fn start(handle: AnyWindowHandle, workspace: Entity<Workspace>, cx: &mut App) {
    let Some(dir) = std::env::var_os("SWIFTCACHE_UI_SMOKE") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    cx.spawn(async move |cx| {
        let mut last = 0;
        loop {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            let command = std::fs::read(dir.join("ui-command.json")).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            let Some(command) = command else { continue };
            let seq = command["seq"].as_u64().unwrap_or(0);
            if seq <= last { continue }
            last = seq;
            let result = handle.update(cx, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
                native::accessibility();
                let mut native_error = None;
                match command["op"].as_str().unwrap_or("") {
                    "point" => point_click(window, point(px(command["x"].as_f64().unwrap() as f32), px(command["y"].as_f64().unwrap() as f32)), cx),
                    "key" => key(window, command["key"].as_str().unwrap(), cx),
                    "input" => { cx.write_to_clipboard(ClipboardItem::new_string(command["text"].as_str().unwrap().to_owned())); key(window,"cmd-v",cx); },
                    "scroll" => {
                        window.dispatch_event(ScrollEvent { position: point(px(command["x"].as_f64().unwrap_or(900.) as f32), px(command["y"].as_f64().unwrap_or(600.) as f32)), delta: ScrollDelta::Pixels(point(px(0.), px(command["delta"].as_f64().unwrap() as f32))), ..Default::default() }.to_platform_input(), cx);
                    }
                    "quit" => cx.quit(),
                    "resize" => window.resize(size(px(1599.),px(999.))),
                    "press" => { native_error = native::press(command["label"].as_str().unwrap()).err(); },
                    "save-panel" => { native_error = native::save(command["path"].as_str().unwrap()).err(); },
                    "state" => {},
                    other => panic!("unsupported smoke operation: {other}"),
                }
                window.refresh();
                window.draw(cx).clear(cx);
                window.on_next_frame(|_, _| {});
                let dialog=window.has_active_dialog(cx);
                let ws = workspace.read(cx);
                json!({"seq":seq,"page":format!("{:?}",ws.active_page),"selected":ws.selected_id,"status":ws.status,"busy":ws.busy,"error":ws.error,"notice":ws.notice,"dirty":ws.service_dirty(cx),"new":ws.service.is_new,"dialog":dialog,"draft":ws.current_config(cx),"records":crate::records::smoke_snapshot(&ws.records),"cache":crate::cache::smoke_snapshot(&ws.cache),"a11y":window.debug_a11y_tree_json(),"visible":window.is_visible(),"native_error":native_error})
            });
            let payload = result.unwrap_or_else(|e| json!({"seq":seq,"error":e.to_string()}));
            std::fs::write(dir.join("ui-result.json"),serde_json::to_vec_pretty(&payload).unwrap()).unwrap();
        }
    }).detach();
}
