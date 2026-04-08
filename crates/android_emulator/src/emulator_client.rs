#![allow(dead_code)]

use anyhow::{Context as _, Result};
use prost::Message;
use tokio_stream::StreamExt as _;
use tonic::codec::ProstCodec;
use tonic::transport::Channel;

// ---------------------------------------------------------------------------
// Protobuf message types (hand-written, matching emulator_controller.proto)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct ImageFormat {
    /// ImgFormat enum: 0 = PNG, 1 = RGBA8888, 2 = RGB888
    #[prost(int32, tag = "1")]
    pub format: i32,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(uint32, tag = "5")]
    pub display: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Image {
    #[prost(message, optional, tag = "1")]
    pub format: Option<ImageFormat>,
    /// Deprecated top-level width/height, still populated by some builds.
    #[prost(uint32, tag = "2")]
    pub width: u32,
    #[prost(uint32, tag = "3")]
    pub height: u32,
    /// Raw pixel bytes in the format specified by `ImageFormat::format`.
    #[prost(bytes = "vec", tag = "4")]
    pub image: Vec<u8>,
    #[prost(uint32, tag = "5")]
    pub seq: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Touch {
    #[prost(int32, tag = "1")]
    pub x: i32,
    #[prost(int32, tag = "2")]
    pub y: i32,
    #[prost(int32, tag = "3")]
    pub identifier: i32,
    /// Non-zero while the finger is down; 0 to release.
    #[prost(int32, tag = "4")]
    pub pressure: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TouchEvent {
    #[prost(message, repeated, tag = "1")]
    pub touches: Vec<Touch>,
    #[prost(int32, tag = "2")]
    pub display: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct MouseEvent {
    #[prost(int32, tag = "1")]
    pub x: i32,
    #[prost(int32, tag = "2")]
    pub y: i32,
    /// 0 = no button, 1 = left button pressed.
    #[prost(int32, tag = "3")]
    pub buttons: i32,
    #[prost(int32, tag = "4")]
    pub display: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyboardEvent {
    /// KeyCodeType: 0=Usb, 1=Evdev, 2=XKB, 3=Win, 4=Mac
    #[prost(int32, tag = "1")]
    pub code_type: i32,
    /// KeyEventType: 0=keydown, 1=keyup, 2=keypress
    #[prost(int32, tag = "2")]
    pub event_type: i32,
    #[prost(int32, tag = "3")]
    pub key_code: i32,
    /// W3C key string, e.g. "GoHome", "GoBack", "a".
    #[prost(string, tag = "4")]
    pub key: String,
    /// UTF-8 text to inject.
    #[prost(string, tag = "5")]
    pub text: String,
}

/// prost-encoded `google.protobuf.Empty` — zero bytes on the wire.
#[derive(Clone, PartialEq, Message)]
pub struct Empty {}

// ---------------------------------------------------------------------------
// gRPC method paths
// ---------------------------------------------------------------------------

const STREAM_SCREENSHOT_PATH: &str =
    "/android.emulation.control.EmulatorController/streamScreenshot";
const SEND_TOUCH_PATH: &str = "/android.emulation.control.EmulatorController/sendTouch";
const SEND_MOUSE_PATH: &str = "/android.emulation.control.EmulatorController/sendMouse";
const SEND_KEY_PATH: &str = "/android.emulation.control.EmulatorController/sendKey";
const MAX_SCREENSHOT_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const STREAM_TARGET_WIDTH: u32 = 720;
const STREAM_TARGET_HEIGHT: u32 = 1280;

// ---------------------------------------------------------------------------
// RgbaFrame — decoded frame handed to the display layer
// ---------------------------------------------------------------------------

/// A single frame received from the emulator in RGBA8888 format.
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    /// Raw pixel bytes: width × height × 4, RGBA order, top-left origin.
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// EmulatorClient
// ---------------------------------------------------------------------------

/// Thin wrapper around a raw tonic `Channel` that speaks the
/// `EmulatorController` gRPC service.
///
/// All methods are `async` and must be driven by the tokio runtime.
/// Use `gpui_tokio::Tokio::spawn` to bridge into GPUI.
pub struct EmulatorClient {
    channel: Channel,
}

impl EmulatorClient {
    /// Opens a connection to the emulator's gRPC server at
    /// `http://127.0.0.1:<grpc_port>`.
    pub async fn connect(grpc_port: u16) -> Result<Self> {
        let endpoint = format!("http://127.0.0.1:{}", grpc_port);
        let channel = Channel::from_shared(endpoint)
            .context("invalid gRPC endpoint")?
            .connect()
            .await
            .with_context(|| {
                format!(
                    "failed to connect to emulator gRPC server on port {}",
                    grpc_port
                )
            })?;

        Ok(Self { channel })
    }

    /// Tries to connect to the emulator's gRPC server with retries.
    pub async fn connect_with_retry(
        grpc_port: u16,
        max_attempts: usize,
        retry_delay: std::time::Duration,
    ) -> Result<Self> {
        let endpoint = format!("http://127.0.0.1:{}", grpc_port);
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=max_attempts {
            let connect_result = Channel::from_shared(endpoint.clone())
                .context("invalid gRPC endpoint")?
                .connect()
                .await;

            match connect_result {
                Ok(channel) => return Ok(Self { channel }),
                Err(error) => {
                    last_error = Some(
                        anyhow::Error::new(error).context(format!(
                            "failed to connect to emulator gRPC server on port {} (attempt {}/{})",
                            grpc_port, attempt, max_attempts
                        )),
                    );
                }
            }

            if attempt < max_attempts {
                tokio::time::sleep(retry_delay).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "failed to connect to emulator gRPC server on port {} after {} attempts",
                grpc_port,
                max_attempts
            )
        }))
    }

    /// Calls `streamScreenshot` and forwards each decoded `RgbaFrame` to
    /// `sender`.  Returns when the stream ends or `sender` is closed.
    pub async fn stream_frames(&self, sender: smol::channel::Sender<RgbaFrame>) -> Result<()> {
        let request_body = ImageFormat {
            format: 1, // RGBA8888
            // Request a smaller stream size to reduce decode/render load.
            width: STREAM_TARGET_WIDTH,
            height: STREAM_TARGET_HEIGHT,
            display: 0,
        };

        let mut grpc = tonic::client::Grpc::new(self.channel.clone())
            .max_decoding_message_size(MAX_SCREENSHOT_MESSAGE_BYTES);
        grpc.ready()
            .await
            .context("gRPC channel not ready for streamScreenshot")?;

        let path = http::uri::PathAndQuery::from_static(STREAM_SCREENSHOT_PATH);
        let codec: ProstCodec<ImageFormat, Image> = ProstCodec::default();
        let mut stream = grpc
            .server_streaming(tonic::Request::new(request_body), path, codec)
            .await
            .context("streamScreenshot RPC failed")?
            .into_inner();

        while let Some(result) = stream.next().await {
            let image = result.context("error reading screenshot frame")?;

            let (width, height) = frame_dimensions(&image);
            if width == 0 || height == 0 || image.image.is_empty() {
                continue;
            }

            let frame = RgbaFrame {
                width,
                height,
                data: image.image,
            };

            if sender.send(frame).await.is_err() {
                // The display entity was dropped; stop streaming.
                break;
            }
        }

        Ok(())
    }

    /// Sends a touch event.  `pressure > 0` means finger down; `pressure = 0`
    /// lifts the finger.
    pub async fn send_touch(
        &self,
        x: i32,
        y: i32,
        identifier: i32,
        pressure: i32,
    ) -> Result<()> {
        let event = TouchEvent {
            touches: vec![Touch {
                x,
                y,
                identifier,
                pressure,
            }],
            display: 0,
        };

        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        grpc.ready()
            .await
            .context("gRPC channel not ready for sendTouch")?;

        let path = http::uri::PathAndQuery::from_static(SEND_TOUCH_PATH);
        let codec: ProstCodec<TouchEvent, Empty> = ProstCodec::default();
        grpc.unary(tonic::Request::new(event), path, codec)
            .await
            .context("sendTouch RPC failed")?;

        Ok(())
    }

    /// Sends a mouse event.  `buttons = 1` means the left button is pressed.
    pub async fn send_mouse(&self, x: i32, y: i32, buttons: i32) -> Result<()> {
        let event = MouseEvent {
            x,
            y,
            buttons,
            display: 0,
        };

        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        grpc.ready()
            .await
            .context("gRPC channel not ready for sendMouse")?;

        let path = http::uri::PathAndQuery::from_static(SEND_MOUSE_PATH);
        let codec: ProstCodec<MouseEvent, Empty> = ProstCodec::default();
        grpc.unary(tonic::Request::new(event), path, codec)
            .await
            .context("sendMouse RPC failed")?;

        Ok(())
    }

    /// Sends a key press using a W3C key string (e.g. `"GoHome"`, `"GoBack"`).
    pub async fn send_key(&self, key: impl Into<String>) -> Result<()> {
        let event = KeyboardEvent {
            event_type: 2, // keypress
            key: key.into(),
            ..Default::default()
        };

        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        grpc.ready()
            .await
            .context("gRPC channel not ready for sendKey")?;

        let path = http::uri::PathAndQuery::from_static(SEND_KEY_PATH);
        let codec: ProstCodec<KeyboardEvent, Empty> = ProstCodec::default();
        grpc.unary(tonic::Request::new(event), path, codec)
            .await
            .context("sendKey RPC failed")?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extracts `(width, height)` from an `Image`, preferring the nested
/// `ImageFormat` fields and falling back to the deprecated top-level fields.
fn frame_dimensions(image: &Image) -> (u32, u32) {
    if let Some(fmt) = &image.format {
        if fmt.width > 0 && fmt.height > 0 {
            return (fmt.width, fmt.height);
        }
    }
    (image.width, image.height)
}
