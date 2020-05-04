use std::{
    cell::RefCell,
    process::Command,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use slog::Logger;

#[cfg(feature = "udev")]
use smithay::backend::session::{auto::AutoSession, Session};
use smithay::{
    backend::input::{
        self, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
        PointerButtonEvent, PointerMotionAbsoluteEvent, PointerMotionEvent,
    },
    reexports::wayland_server::protocol::wl_pointer,
    wayland::{
        seat::{keysyms as xkb, AxisFrame, KeyboardHandle, Keysym, ModifiersState, PointerHandle},
        SERIAL_COUNTER as SCOUNTER,
    },
};

use crate::shell::MyWindowMap;

pub struct AnvilInputHandler {
    log: Logger,
    pointer: PointerHandle,
    keyboard: KeyboardHandle,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    screen_size: (u32, u32),
    #[cfg(feature = "udev")]
    session: Option<AutoSession>,
    running: Arc<AtomicBool>,
}

pub struct InputInitData {
    pub pointer: PointerHandle,
    pub keyboard: KeyboardHandle,
    pub window_map: Rc<RefCell<MyWindowMap>>,
    pub screen_size: (u32, u32),
    pub running: Arc<AtomicBool>,
    pub pointer_location: Rc<RefCell<(f64, f64)>>,
}

impl AnvilInputHandler {
    pub fn new(log: Logger, data: InputInitData) -> AnvilInputHandler {
        AnvilInputHandler {
            log,
            pointer: data.pointer,
            keyboard: data.keyboard,
            window_map: data.window_map,
            screen_size: data.screen_size,
            running: data.running,
            pointer_location: data.pointer_location,
            #[cfg(feature = "udev")]
            session: None,
        }
    }

    #[cfg(feature = "udev")]
    pub fn new_with_session(log: Logger, data: InputInitData, session: AutoSession) -> AnvilInputHandler {
        AnvilInputHandler {
            log,
            pointer: data.pointer,
            keyboard: data.keyboard,
            window_map: data.window_map,
            screen_size: data.screen_size,
            running: data.running,
            pointer_location: data.pointer_location,
            session: Some(session),
        }
    }
}

impl AnvilInputHandler {
    pub fn process_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard_key::<B>(event),
            InputEvent::PointerMotion { event, .. } => self.on_pointer_move::<B>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => self.on_pointer_move_absolute::<B>(event),
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<B>(event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<B>(event),
            _ => {
                // other events are not handled in anvil (yet)
            }
        }
    }

    fn on_keyboard_key<B: InputBackend>(&mut self, evt: B::KeyboardKeyEvent) {
        let keycode = evt.key_code();
        let state = evt.state();
        debug!(self.log, "key"; "keycode" => keycode, "state" => format!("{:?}", state));
        let serial = SCOUNTER.next_serial();
        let log = &self.log;
        let time = Event::time(&evt);
        let mut action = KeyAction::None;
        self.keyboard
            .input(keycode, state, serial, time, |modifiers, keysym| {
                debug!(log, "keysym";
                    "state" => format!("{:?}", state),
                    "mods" => format!("{:?}", modifiers),
                    "keysym" => ::xkbcommon::xkb::keysym_get_name(keysym)
                );
                action = process_keyboard_shortcut(*modifiers, keysym);
                // forward to client only if action == KeyAction::Forward
                // both for pressed and released, to avoid inconsistencies
                if let KeyAction::Forward = action {
                    true
                } else {
                    false
                }
            });
        if let KeyState::Released = state {
            // only process special actions on key press, not release
            return;
        }
        match action {
            KeyAction::Quit => {
                info!(self.log, "Quitting.");
                self.running.store(false, Ordering::SeqCst);
            }
            #[cfg(feature = "udev")]
            KeyAction::VtSwitch(vt) => {
                if let Some(ref mut session) = self.session {
                    info!(log, "Trying to switch to vt {}", vt);
                    if let Err(err) = session.change_vt(vt) {
                        error!(log, "Error switching to vt {}: {}", vt, err);
                    }
                }
            }
            KeyAction::Run(cmd) => {
                info!(self.log, "Starting program"; "cmd" => cmd.clone());
                if let Err(e) = Command::new(&cmd).spawn() {
                    error!(log,
                        "Failed to start program";
                        "cmd" => cmd,
                        "err" => format!("{:?}", e)
                    );
                }
            }
            _ => (),
        }
    }

    fn on_pointer_move<B: InputBackend>(&mut self, evt: B::PointerMotionEvent) {
        let (x, y) = (evt.delta_x(), evt.delta_y());
        let serial = SCOUNTER.next_serial();
        let mut location = self.pointer_location.borrow_mut();
        location.0 += x as f64;
        location.1 += y as f64;
        // clamp to screen limits
        // this event is never generated by winit so self.screen_size is relevant
        location.0 = (location.0).max(0.0).min(self.screen_size.0 as f64);
        location.1 = (location.1).max(0.0).min(self.screen_size.1 as f64);
        let under = self
            .window_map
            .borrow()
            .get_surface_under((location.0, location.1));
        self.pointer.motion(*location, under, serial, evt.time());
    }

    fn on_pointer_move_absolute<B: InputBackend>(&mut self, evt: B::PointerMotionAbsoluteEvent) {
        // different cases depending on the context:
        let (x, y) = {
            #[cfg(feature = "udev")]
            {
                if self.session.is_some() {
                    // we are started on a tty
                    let (ux, uy) = evt.position_transformed(self.screen_size);
                    (ux as f64, uy as f64)
                } else {
                    // we are started in winit
                    evt.position()
                }
            }
            #[cfg(not(feature = "udev"))]
            {
                evt.position()
            }
        };
        *self.pointer_location.borrow_mut() = (x, y);
        let serial = SCOUNTER.next_serial();
        let under = self.window_map.borrow().get_surface_under((x as f64, y as f64));
        self.pointer.motion((x, y), under, serial, evt.time());
    }

    fn on_pointer_button<B: InputBackend>(&mut self, evt: B::PointerButtonEvent) {
        let serial = SCOUNTER.next_serial();
        let button = match evt.button() {
            input::MouseButton::Left => 0x110,
            input::MouseButton::Right => 0x111,
            input::MouseButton::Middle => 0x112,
            input::MouseButton::Other(b) => b as u32,
        };
        let state = match evt.state() {
            input::MouseButtonState::Pressed => {
                // change the keyboard focus unless the pointer is grabbed
                if !self.pointer.is_grabbed() {
                    let under = self
                        .window_map
                        .borrow_mut()
                        .get_surface_and_bring_to_top(*self.pointer_location.borrow());
                    self.keyboard
                        .set_focus(under.as_ref().map(|&(ref s, _)| s), serial);
                }
                wl_pointer::ButtonState::Pressed
            }
            input::MouseButtonState::Released => wl_pointer::ButtonState::Released,
        };
        self.pointer.button(button, state, serial, evt.time());
    }

    fn on_pointer_axis<B: InputBackend>(&mut self, evt: B::PointerAxisEvent) {
        let source = match evt.source() {
            input::AxisSource::Continuous => wl_pointer::AxisSource::Continuous,
            input::AxisSource::Finger => wl_pointer::AxisSource::Finger,
            input::AxisSource::Wheel | input::AxisSource::WheelTilt => wl_pointer::AxisSource::Wheel,
        };
        let horizontal_amount = evt
            .amount(input::Axis::Horizontal)
            .unwrap_or_else(|| evt.amount_discrete(input::Axis::Horizontal).unwrap() * 3.0);
        let vertical_amount = evt
            .amount(input::Axis::Vertical)
            .unwrap_or_else(|| evt.amount_discrete(input::Axis::Vertical).unwrap() * 3.0);
        let horizontal_amount_discrete = evt.amount_discrete(input::Axis::Horizontal);
        let vertical_amount_discrete = evt.amount_discrete(input::Axis::Vertical);

        {
            let mut frame = AxisFrame::new(evt.time()).source(source);
            if horizontal_amount != 0.0 {
                frame = frame.value(wl_pointer::Axis::HorizontalScroll, horizontal_amount);
                if let Some(discrete) = horizontal_amount_discrete {
                    frame = frame.discrete(wl_pointer::Axis::HorizontalScroll, discrete as i32);
                }
            } else if source == wl_pointer::AxisSource::Finger {
                frame = frame.stop(wl_pointer::Axis::HorizontalScroll);
            }
            if vertical_amount != 0.0 {
                frame = frame.value(wl_pointer::Axis::VerticalScroll, vertical_amount);
                if let Some(discrete) = vertical_amount_discrete {
                    frame = frame.discrete(wl_pointer::Axis::VerticalScroll, discrete as i32);
                }
            } else if source == wl_pointer::AxisSource::Finger {
                frame = frame.stop(wl_pointer::Axis::VerticalScroll);
            }
            self.pointer.axis(frame);
        }
    }
}

/// Possible results of a keyboard action
enum KeyAction {
    /// Quit the compositor
    Quit,
    /// Trigger a vt-switch
    VtSwitch(i32),
    /// run a command
    Run(String),
    /// Forward the key to the client
    Forward,
    /// Do nothing more
    None,
}

fn process_keyboard_shortcut(modifiers: ModifiersState, keysym: Keysym) -> KeyAction {
    if modifiers.ctrl && modifiers.alt && keysym == xkb::KEY_BackSpace
        || modifiers.logo && keysym == xkb::KEY_q
    {
        // ctrl+alt+backspace = quit
        // logo + q = quit
        KeyAction::Quit
    } else if keysym >= xkb::KEY_XF86Switch_VT_1 && keysym <= xkb::KEY_XF86Switch_VT_12 {
        // VTSwicth
        KeyAction::VtSwitch((keysym - xkb::KEY_XF86Switch_VT_1 + 1) as i32)
    } else if modifiers.logo && keysym == xkb::KEY_Return {
        // run terminal
        KeyAction::Run("weston-terminal".into())
    } else {
        KeyAction::Forward
    }
}
