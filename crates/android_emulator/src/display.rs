use std::sync::Arc;

use gpui::{
    App, Bounds, Context, DevicePixels, EventEmitter, FocusHandle, Focusable,
    ImageSource, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, RenderImage, Styled, Task, WeakEntity, Window,
    div, img,
};
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use ui::{Color, Label, LabelCommon, LabelSize, prelude::*};
use util::ResultExt;

use crate::emulator_client::{EmulatorClient, RgbaFrame};

pub enum DisplayEvent {}

impl EventEmitter<DisplayEvent> for EmulatorDisplay {}

enum EmulatorInputEvent {
    Mouse { x: i32, y: i32, buttons: i32 },
    Key { key: String },
}

pub struct EmulatorDisplay {
    focus_handle: FocusHandle,
    current_frame: Option<Arc<RenderImage>>,
    /// Native device dimensions derived from the most recent frame.
    device_width: i32,
    device_height: i32,
    /// Bounds of the rendered image element, captured each render cycle so
    /// that mouse events can be translated into device-pixel coordinates.
    rendered_bounds: Option<Bounds<Pixels>>,
    /// Whether the primary mouse button is currently held down.
    mouse_down: bool,
    /// Last sent mouse event to avoid flooding duplicate packets.
    last_mouse_event: Option<(i32, i32, i32)>,
    input_sender: smol::channel::Sender<EmulatorInputEvent>,
    _frame_task: Task<()>,
    _stream_task: Task<Result<(), gpui_tokio::JoinError>>,
    _input_task: Task<Result<(), gpui_tokio::JoinError>>,
}

impl EmulatorDisplay {
    pub fn new(grpc_port: u16, cx: &mut Context<Self>) -> Self {
        let (frame_tx, frame_rx) = smol::channel::bounded::<RgbaFrame>(2);
        let (input_sender, input_receiver) = smol::channel::bounded::<EmulatorInputEvent>(256);

        // Run the gRPC streaming call on the tokio runtime — tonic requires it.
        let stream_task = gpui_tokio::Tokio::spawn(cx, async move {
            let connect_result = EmulatorClient::connect_with_retry(
                grpc_port,
                30,
                std::time::Duration::from_secs(1),
            )
            .await;

            match connect_result {
                Ok(client) => {
                    client.stream_frames(frame_tx).await.log_err();
                }
                Err(error) => {
                    log::error!(
                        "android_emulator: failed to connect on port {}: {}",
                        grpc_port,
                        error
                    );
                }
            }
        });

        let input_task = gpui_tokio::Tokio::spawn(cx, async move {
            let mut client = EmulatorClient::connect_with_retry(
                grpc_port,
                30,
                std::time::Duration::from_secs(1),
            )
            .await
            .log_err();

            while let Ok(input_event) = input_receiver.recv().await {
                if client.is_none() {
                    client = EmulatorClient::connect(grpc_port).await.log_err();
                }

                let Some(active_client) = client.as_ref() else {
                    continue;
                };

                let send_result = match input_event {
                    EmulatorInputEvent::Mouse { x, y, buttons } => {
                        active_client.send_mouse(x, y, buttons).await
                    }
                    EmulatorInputEvent::Key { key } => active_client.send_key(key).await,
                };

                if let Err(error) = send_result {
                    log::error!(
                        "android_emulator: failed to send input on port {}: {}",
                        grpc_port,
                        error
                    );
                    client = None;
                }
            }
        });

        // Read decoded frames off the smol channel and trigger re-renders.
        let frame_task = cx.spawn(async move |this: WeakEntity<EmulatorDisplay>, cx| {
            while let Ok(rgba_frame) = frame_rx.recv().await {
                let Some(render_image) = rgba_to_render_image(rgba_frame) else {
                    continue;
                };
                let image = Arc::new(render_image);
                this.update(cx, |display, cx| {
                    display.current_frame = Some(image);
                    cx.notify();
                })
                .log_err();
            }
        });

        Self {
            focus_handle: cx.focus_handle(),
            current_frame: None,
            device_width: 0,
            device_height: 0,
            rendered_bounds: None,
            mouse_down: false,
            last_mouse_event: None,
            input_sender,
            _frame_task: frame_task,
            _stream_task: stream_task,
            _input_task: input_task,
        }
    }

    fn send_mouse_at(
        &mut self,
        position: gpui::Point<Pixels>,
        buttons: i32,
        _cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.rendered_bounds else {
            return;
        };
        if self.device_width == 0 || self.device_height == 0 {
            return;
        }

        let (device_x, device_y) = self.panel_to_device(position, bounds);
        let mouse_event = (device_x, device_y, buttons);
        if self.last_mouse_event == Some(mouse_event) {
            return;
        }

        self.last_mouse_event = Some(mouse_event);
        self.input_sender
            .try_send(EmulatorInputEvent::Mouse {
                x: device_x,
                y: device_y,
                buttons,
            })
            .log_err();
    }

    fn send_key_input(&self, key: String, _cx: &mut Context<Self>) {
        self.input_sender
            .try_send(EmulatorInputEvent::Key { key })
            .log_err();
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(key) = key_for_emulator(&event.keystroke) else {
            return;
        };

        self.send_key_input(key, cx);
        cx.stop_propagation();
    }

    /// Translates a point in panel-logical-pixel space into device-pixel
    /// coordinates by scaling proportionally to the known device resolution.
    fn panel_to_device(
        &self,
        position: gpui::Point<Pixels>,
        container_bounds: Bounds<Pixels>,
    ) -> (i32, i32) {
        let bounds = self.contained_image_bounds(container_bounds);
        if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
            return (0, 0);
        }

        // Pixels / Pixels = f32 (GPUI implements Div for Pixels)
        let relative_x = (position.x - bounds.origin.x).max(Pixels::ZERO);
        let relative_y = (position.y - bounds.origin.y).max(Pixels::ZERO);

        let x_ratio = (relative_x / bounds.size.width).clamp(0.0, 1.0);
        let y_ratio = (relative_y / bounds.size.height).clamp(0.0, 1.0);

        let max_x = (self.device_width - 1).max(0) as f32;
        let max_y = (self.device_height - 1).max(0) as f32;

        (
            (x_ratio * max_x).round() as i32,
            (y_ratio * max_y).round() as i32,
        )
    }

    fn contained_image_bounds(&self, container_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        if self.device_width <= 0 || self.device_height <= 0 {
            return container_bounds;
        }

        let container_height = container_bounds.size.height / px(1.);
        let container_width = container_bounds.size.width / px(1.);
        if container_width <= 0.0 || container_height <= 0.0 {
            return container_bounds;
        }

        let device_aspect = self.device_width as f32 / self.device_height as f32;
        let container_aspect = container_width / container_height;

        if container_aspect > device_aspect {
            let height = container_bounds.size.height;
            let width = px((height / px(1.)) * device_aspect);
            let x_offset = px((container_width - (width / px(1.))) * 0.5);
            Bounds::new(
                gpui::point(container_bounds.origin.x + x_offset, container_bounds.origin.y),
                gpui::size(width, height),
            )
        } else {
            let width = container_bounds.size.width;
            let height = px((width / px(1.)) / device_aspect);
            let y_offset = px((container_height - (height / px(1.))) * 0.5);
            Bounds::new(
                gpui::point(container_bounds.origin.x, container_bounds.origin.y + y_offset),
                gpui::size(width, height),
            )
        }
    }
}

impl Focusable for EmulatorDisplay {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EmulatorDisplay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Sync device dimensions from the latest frame so coordinate translation
        // stays accurate even after device rotation.
        if let Some(frame) = &self.current_frame {
            let size: gpui::Size<DevicePixels> = frame.size(0);
            self.device_width = size.width.0;
            self.device_height = size.height.0;
        }

        let max_display_width = if self.device_width > 0 {
            px(self.device_width as f32 / window.scale_factor())
        } else {
            px(480.)
        };
        let max_display_height = if self.device_height > 0 {
            px(self.device_height as f32 / window.scale_factor())
        } else {
            px(960.)
        };

        let content: gpui::AnyElement = if let Some(frame) = &self.current_frame {
            div()
                .w_full()
                .h_full()
                .max_w(max_display_width)
                .max_h(max_display_height)
                .child(
                    img(ImageSource::Render(frame.clone()))
                        .w_full()
                        .h_full()
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .child(
                    Label::new("Connecting to emulator…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        };
        let display = cx.entity().downgrade();

        div()
            .id("emulator-display")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(gpui::black())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.handle_key_down(event, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    this.mouse_down = true;
                    this.send_mouse_at(event.position, 1, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.mouse_down {
                    this.send_mouse_at(event.position, 1, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.mouse_down = false;
                    this.send_mouse_at(event.position, 0, cx);
                }),
            )
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_children_prepainted(move |children_bounds, _window, cx| {
                        let Some(bounds) = children_bounds.first().cloned() else {
                            return;
                        };
                        display
                            .update(cx, |display, _cx| {
                                display.rendered_bounds = Some(bounds);
                            })
                            .log_err();
                    })
                    .child(content),
            )
    }
}

fn key_for_emulator(keystroke: &gpui::Keystroke) -> Option<String> {
    if keystroke.modifiers.control || keystroke.modifiers.platform || keystroke.modifiers.function {
        return None;
    }

    if let Some(key_char) = keystroke.key_char.as_ref() {
        if key_char.chars().count() == 1 {
            return Some(key_char.clone());
        }
    }

    match keystroke.key.as_str() {
        "enter" => Some("Enter".to_string()),
        "tab" => Some("Tab".to_string()),
        "backspace" => Some("Backspace".to_string()),
        "delete" => Some("Delete".to_string()),
        "escape" | "esc" => Some("Escape".to_string()),
        "left" => Some("ArrowLeft".to_string()),
        "right" => Some("ArrowRight".to_string()),
        "up" => Some("ArrowUp".to_string()),
        "down" => Some("ArrowDown".to_string()),
        "home" => Some("Home".to_string()),
        "end" => Some("End".to_string()),
        "pageup" => Some("PageUp".to_string()),
        "pagedown" => Some("PageDown".to_string()),
        "space" => Some(" ".to_string()),
        key if key.len() == 1 => Some(key.to_string()),
        key if key.starts_with('f')
            && key[1..].chars().all(|character| character.is_ascii_digit()) =>
        {
            Some(key.to_ascii_uppercase())
        }
        _ => None,
    }
}

/// Converts an `RgbaFrame` (RGBA8888, top-left origin) into a GPUI
/// `RenderImage`. GPUI's renderer expects BGRA byte order so R and B
/// channels are swapped.
fn rgba_to_render_image(frame: RgbaFrame) -> Option<RenderImage> {
    let mut buffer: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(frame.width, frame.height, frame.data)?;

    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let animation_frame = Frame::new(buffer);
    Some(RenderImage::new(SmallVec::from_elem(animation_frame, 1)))
}
