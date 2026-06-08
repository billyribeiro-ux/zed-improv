//! Stream the page into a Zed panel via CDP `Page.startScreencast`, and translate panel-local mouse
//! coordinates back to page coordinates for input forwarding.
//!
//! CDP delivers each frame as a base64-encoded JPEG/PNG plus metadata describing the visible page
//! region. We decode to a BGRA [`gpui::RenderImage`] (the format GPUI's `img` element expects),
//! following the same decode-and-channel-swap path as `crates/gpui/src/elements/img.rs`.

use crate::cdp::CdpClient;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use gpui::RenderImage;
use image::Frame;
use serde::Deserialize;
use serde_json::{Value, json};
use smallvec::SmallVec;
use std::sync::Arc;

/// Metadata CDP sends alongside each screencast frame, describing how the captured bitmap maps onto
/// the page. Used to translate clicks from panel space back to page CSS pixels.
#[derive(Debug, Clone, Deserialize)]
pub struct FrameMetadata {
    #[serde(rename = "offsetTop")]
    pub offset_top: f32,
    #[serde(rename = "pageScaleFactor")]
    pub page_scale_factor: f32,
    #[serde(rename = "deviceWidth")]
    pub device_width: f32,
    #[serde(rename = "deviceHeight")]
    pub device_height: f32,
    #[serde(rename = "scrollOffsetX")]
    pub scroll_offset_x: f32,
    #[serde(rename = "scrollOffsetY")]
    pub scroll_offset_y: f32,
}

/// A decoded screencast frame ready to render, plus the metadata needed for coordinate mapping.
pub struct DecodedFrame {
    pub image: Arc<RenderImage>,
    pub metadata: FrameMetadata,
}

/// Begin streaming frames. The caller subscribes to `Page.screencastFrame` and feeds each event's
/// params to [`decode_frame`], then acks via [`ack`].
pub async fn start(cdp: &CdpClient) -> Result<()> {
    cdp.send(
        "Page.startScreencast",
        json!({
            "format": "jpeg",
            "quality": 80,
            "everyNthFrame": 1,
        }),
    )
    .await
    .context("Page.startScreencast")?;
    Ok(())
}

/// Acknowledge a received frame so Chrome sends the next one. Must be called for every frame.
pub async fn ack(cdp: &CdpClient, session_id: i64) -> Result<()> {
    cdp.send(
        "Page.screencastFrameAck",
        json!({ "sessionId": session_id }),
    )
    .await
    .context("Page.screencastFrameAck")?;
    Ok(())
}

/// The `sessionId` carried by a `Page.screencastFrame` event (needed to ack it).
pub fn session_id(params: &Value) -> Option<i64> {
    params.get("sessionId").and_then(Value::as_i64)
}

/// Decode a `Page.screencastFrame` event's params into a renderable frame.
pub fn decode_frame(params: &Value) -> Result<DecodedFrame> {
    let data = params
        .get("data")
        .and_then(Value::as_str)
        .context("screencast frame missing data")?;
    let metadata: FrameMetadata = serde_json::from_value(
        params
            .get("metadata")
            .cloned()
            .context("screencast frame missing metadata")?,
    )
    .context("parsing screencast frame metadata")?;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("decoding screencast frame base64")?;

    let mut rgba = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
        .context("decoding screencast jpeg")?
        .into_rgba8();

    // GPUI's RenderImage expects BGRA; swap red/blue in place (matches gpui/src/elements/img.rs).
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let frames = SmallVec::from_elem(Frame::new(rgba), 1);
    Ok(DecodedFrame {
        image: Arc::new(RenderImage::new(frames)),
        metadata,
    })
}

/// Translate a point within the rendered image (image pixels, top-left origin) to page CSS
/// coordinates for `Input.dispatchMouseEvent`. `image_width`/`image_height` are the dimensions of
/// the *contained* (letterboxed) image rect, not the whole pane.
///
/// Returns `None` when the frame can't be mapped meaningfully (image or device dimensions are zero,
/// e.g. a transient post-navigation frame), so the caller skips dispatch rather than sending a
/// click to the page origin.
pub fn image_to_page_coords(
    metadata: &FrameMetadata,
    image_x: f32,
    image_y: f32,
    image_width: f32,
    image_height: f32,
) -> Option<(f32, f32)> {
    if image_width <= 0.0
        || image_height <= 0.0
        || metadata.device_width <= 0.0
        || metadata.device_height <= 0.0
    {
        return None;
    }

    // The captured bitmap covers the device viewport (device_width x device_height) scaled to fit
    // the image rect. Map the click into device space, then divide out the page scale factor and add
    // the scroll offset to land on the page's CSS-pixel coordinate.
    let device_x = image_x / image_width * metadata.device_width;
    let device_y = image_y / image_height * metadata.device_height;

    let scale = if metadata.page_scale_factor != 0.0 {
        metadata.page_scale_factor
    } else {
        1.0
    };

    // `offset_top` is the captured region's top inset within the device viewport; subtract it before
    // dividing out the scale so clicks land correctly when the page is scrolled. Clamp at 0 so an
    // out-of-viewport coordinate never dispatches as a negative page position.
    let page_x = (device_x / scale + metadata.scroll_offset_x).max(0.0);
    let page_y = ((device_y - metadata.offset_top) / scale + metadata.scroll_offset_y).max(0.0);
    Some((page_x, page_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> FrameMetadata {
        FrameMetadata {
            offset_top: 0.0,
            page_scale_factor: 1.0,
            device_width: 1000.0,
            device_height: 800.0,
            scroll_offset_x: 0.0,
            scroll_offset_y: 0.0,
        }
    }

    #[test]
    fn maps_center_click_to_device_center_at_unit_scale() {
        // A click at the center of a 500x400 image rect maps to the center of a 1000x800 page.
        assert_eq!(
            image_to_page_coords(&metadata(), 250.0, 200.0, 500.0, 400.0),
            Some((500.0, 400.0))
        );
    }

    #[test]
    fn applies_scroll_offset() {
        let mut metadata = metadata();
        metadata.scroll_offset_y = 300.0;
        assert_eq!(
            image_to_page_coords(&metadata, 0.0, 0.0, 500.0, 400.0),
            Some((0.0, 300.0))
        );
    }

    #[test]
    fn divides_out_page_scale_factor() {
        let mut metadata = metadata();
        metadata.page_scale_factor = 2.0;
        // Bottom-right of the rect maps to device (1000,800), then /2 for the page-scale zoom.
        assert_eq!(
            image_to_page_coords(&metadata, 500.0, 400.0, 500.0, 400.0),
            Some((500.0, 400.0))
        );
    }

    #[test]
    fn zero_sized_image_returns_none() {
        assert_eq!(
            image_to_page_coords(&metadata(), 10.0, 10.0, 0.0, 0.0),
            None
        );
    }

    #[test]
    fn zero_device_dimensions_return_none() {
        let mut metadata = metadata();
        metadata.device_width = 0.0;
        assert_eq!(
            image_to_page_coords(&metadata, 10.0, 10.0, 500.0, 400.0),
            None
        );
    }

    #[test]
    fn out_of_viewport_offset_clamps_to_zero() {
        // A large offset_top that would push page_y negative is clamped to 0, never dispatched negative.
        let mut metadata = metadata();
        metadata.offset_top = 1000.0;
        let (_, page_y) = image_to_page_coords(&metadata, 0.0, 0.0, 500.0, 400.0).expect("maps");
        assert_eq!(page_y, 0.0);
    }
}
