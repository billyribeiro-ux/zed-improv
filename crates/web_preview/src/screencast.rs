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
use util::ResultExt as _;

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

/// Set the page's layout viewport (CSS px) and device scale, so the page lays out at the size the
/// user sees rather than Chrome's headless 800×600 default. Awaited, so callers can gate the first
/// frame on it landing.
pub async fn set_viewport(
    cdp: &CdpClient,
    css_width: u32,
    css_height: u32,
    device_scale: f32,
) -> Result<()> {
    cdp.send(
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": css_width.max(1),
            "height": css_height.max(1),
            "deviceScaleFactor": device_scale.max(1.0),
            "mobile": false,
        }),
    )
    .await
    .context("Emulation.setDeviceMetricsOverride")?;
    Ok(())
}

/// Begin streaming frames sized to the device-pixel dimensions of the viewport (so the streamed
/// image is sharp on the panel, not an upscaled 800×600). The caller subscribes to
/// `Page.screencastFrame` BEFORE calling this (a static page emits exactly one frame, at screencast
/// start; subscribing after drops it and the preview stays blank forever), decodes via
/// [`decode_frame`], and acks via [`ack_no_reply`].
pub async fn start(cdp: &CdpClient, quality: u8, max_width: u32, max_height: u32) -> Result<()> {
    cdp.send(
        "Page.startScreencast",
        json!({
            "format": "jpeg",
            "quality": quality,
            // Always 1: with everyNthFrame > 1 Chrome skips the *only* frame a static page ever
            // produces and the preview is permanently blank (verified against Chrome 149). Frame
            // throttling is done client-side in the frame loop instead.
            "everyNthFrame": 1,
            "maxWidth": max_width.max(1),
            "maxHeight": max_height.max(1),
        }),
    )
    .await
    .context("Page.startScreencast")?;
    Ok(())
}

/// (Re)start the screencast with new size bounds (after a viewport change). Best-effort.
///
/// We deliberately do NOT `Page.stopScreencast` first: `Page.startScreencast` re-keys an existing
/// screencast in place, and a stop-then-start has a window where, if this task is cancelled between
/// the two awaits (e.g. a rapid resize replaces it), the stream is left STOPPED with nothing to
/// restart it. A single idempotent start has no such gap.
pub async fn restart(cdp: &CdpClient, quality: u8, max_width: u32, max_height: u32) {
    start(cdp, quality, max_width, max_height).await.log_err();
}

/// Acknowledge a received frame so Chrome sends the next one. Fire-and-forget: the ack has no
/// meaningful result, so we don't register (and then have to reap) a pending-response slot per frame.
pub fn ack_no_reply(cdp: &CdpClient, session_id: i64) {
    cdp.send_no_reply(
        "Page.screencastFrameAck",
        json!({ "sessionId": session_id }),
    );
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
    // the image rect. Map the click into device space, then divide out the page scale factor.
    let device_x = image_x / image_width * metadata.device_width;
    let device_y = image_y / image_height * metadata.device_height;

    let scale = if metadata.page_scale_factor != 0.0 {
        metadata.page_scale_factor
    } else {
        1.0
    };

    // Return VIEWPORT (CSS-px) coordinates — NOT document coordinates. `Input.dispatchMouseEvent`
    // (click/scroll/move) expects viewport coords and applies scroll itself; adding the scroll offset
    // here made every click/scroll wrong by `scrollY` once the page was scrolled (verified against
    // real Chrome). `offset_top` (CSS px) accounts for a partially-captured top region. Callers that
    // need document coords (e.g. `DOM.getNodeForLocation`) add `scroll_offset_*` themselves.
    let page_x = (device_x / scale).max(0.0);
    let page_y = (device_y / scale - metadata.offset_top).max(0.0);
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
    fn returns_viewport_coords_ignoring_scroll() {
        // Result is VIEWPORT coordinates: scroll offset is NOT added (Input.dispatchMouseEvent
        // applies scroll itself; adding it here makes clicks wrong once the page is scrolled).
        let mut metadata = metadata();
        metadata.scroll_offset_y = 300.0;
        assert_eq!(
            image_to_page_coords(&metadata, 0.0, 0.0, 500.0, 400.0),
            Some((0.0, 0.0))
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

    #[test]
    fn offset_top_subtracted_in_css_px_under_zoom() {
        // offset_top is CSS px; device_y must be converted to CSS px BEFORE subtracting it.
        // device viewport 1000x800, scale 2, offset_top 100 (CSS px), click at bottom-right of rect.
        let mut metadata = metadata();
        metadata.page_scale_factor = 2.0;
        metadata.offset_top = 100.0;
        // click at image (500,400) of a 500x400 rect → device (1000,800) → /2 = (500,400) CSS,
        // then page_y = 400 - 100 = 300; page_x = 500.
        let (x, y) = image_to_page_coords(&metadata, 500.0, 400.0, 500.0, 400.0).expect("maps");
        assert_eq!((x, y), (500.0, 300.0));
    }
}
