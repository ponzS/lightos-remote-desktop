//! Keep absolute mouse events on a virtual display independent of ambient mouse motion.
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use core_graphics::{
    display::CGDisplay,
    event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use display_control::{DisplayMode, DisplayOutput};
use enigo::{Button, Direction};
use objc2_app_kit::NSEvent;

use super::virtual_display;

#[derive(Default)]
struct ButtonState {
    held: bool,
    count: i64,
    last_press: Option<(Instant, CGPoint)>,
}

pub(super) struct VirtualPointer {
    id: u32,
    normalized: (f64, f64),
    buttons: [ButtonState; 3],
}

impl VirtualPointer {
    pub(super) fn new(id: u32) -> Self {
        Self {
            id,
            normalized: (0.5, 0.5),
            buttons: Default::default(),
        }
    }

    pub(super) fn move_to(&mut self, output: &DisplayOutput, x: i32, y: i32) -> Result<()> {
        let logical_width = f64::from(output.width) / output.scale;
        let logical_height = f64::from(output.height) / output.scale;
        self.normalized = (f64::from(x) / logical_width, f64::from(y) / logical_height);
        let mut movement = (CGEventType::MouseMoved, CGMouseButton::Left);
        for (button, event) in [
            (CGMouseButton::Left, CGEventType::LeftMouseDragged),
            (CGMouseButton::Right, CGEventType::RightMouseDragged),
            (CGMouseButton::Center, CGEventType::OtherMouseDragged),
        ] {
            if self.buttons[button as usize].held {
                movement = (event, button);
                break;
            }
        }
        post_mouse(movement.0, movement.1, self.position()?, 0)
    }

    pub(super) fn button(&mut self, button: Button, direction: Direction) -> Result<()> {
        let (button, down, up) = match button {
            Button::Left => (
                CGMouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
            ),
            Button::Right => (
                CGMouseButton::Right,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
            ),
            Button::Middle => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
            ),
            _ => bail!("unsupported macOS virtual display mouse button"),
        };
        let position = self.position()?;
        let state = &mut self.buttons[button as usize];
        if direction == Direction::Press {
            let now = Instant::now();
            let count = match state.last_press {
                Some((time, last))
                    if now.duration_since(time).as_secs_f64() <= NSEvent::doubleClickInterval()
                        && (position.x - last.x).abs().max((position.y - last.y).abs()) <= 4.0 =>
                {
                    state.count.saturating_add(1)
                }
                _ => 1,
            };
            post_mouse(down, button, position, count)?;
            state.last_press = Some((now, position));
            state.count = count;
            state.held = true;
        } else if state.held {
            post_mouse(up, button, position, state.count)?;
            state.held = false;
        }
        Ok(())
    }

    fn position(&self) -> Result<CGPoint> {
        let mode = virtual_display::mode(self.id).context("macOS virtual display disconnected")?;
        let display = CGDisplay::new(self.id);
        ensure!(
            display.is_active() && virtual_display::is_extended(self.id),
            "macOS virtual display unavailable"
        );
        Ok(absolute_position(
            display.bounds().origin,
            mode,
            self.normalized,
        ))
    }
}

fn absolute_position(origin: CGPoint, mode: DisplayMode, (x, y): (f64, f64)) -> CGPoint {
    let width = f64::from(mode.width) / mode.scale;
    let height = f64::from(mode.height) / mode.scale;
    CGPoint::new(
        origin.x + (x * width).clamp(0.0, width - 1.0),
        origin.y + (y * height).clamp(0.0, height - 1.0),
    )
}

fn post_mouse(
    event: CGEventType,
    button: CGMouseButton,
    position: CGPoint,
    click_count: i64,
) -> Result<()> {
    let event = mouse_event(event, button, position, click_count)?;
    // Post the absolute point to the session. HID may treat it as relative
    // movement and apply acceleration before delivering it to the window.
    event.post(CGEventTapLocation::Session);
    Ok(())
}

fn mouse_event(
    event: CGEventType,
    button: CGMouseButton,
    position: CGPoint,
    click_count: i64,
) -> Result<CGEvent> {
    let state = CGEventSourceStateID::CombinedSessionState;
    let source =
        CGEventSource::new(state).map_err(|_| anyhow!("create macOS mouse event source"))?;
    let event = CGEvent::new_mouse_event(source, event, position, button)
        .map_err(|_| anyhow!("create macOS mouse event"))?;
    event.set_flags(CGEventFlags::from_bits_retain(unsafe {
        CGEventSourceFlagsState(state)
    }));
    event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button as i64);
    event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count);
    event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, 0);
    event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, 0);
    Ok(event)
}

unsafe extern "C" {
    fn CGEventSourceFlagsState(state: CGEventSourceStateID) -> u64;
}
