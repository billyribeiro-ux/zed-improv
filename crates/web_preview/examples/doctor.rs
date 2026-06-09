//! Looking Glass doctor: runs the REAL `web_preview` pipeline (the exact crate code Zed runs —
//! `chrome::launch`, `cdp::CdpClient`, `screencast`, `source_map`) end to end against a live dev
//! server and prints stage-by-stage evidence. Use it to pinpoint which stage fails on a given
//! machine without building all of Zed's UI.
//!
//! ```sh
//! cargo run -p web_preview --example doctor                       # against http://localhost:5173
//! WEB_PREVIEW_URL=http://localhost:5176 cargo run -p web_preview --example doctor
//! WEB_PREVIEW_CHROME=/path/to/chrome DOCTOR_SCALE=2 cargo run -p web_preview --example doctor
//! ```
//!
//! Artifacts (the decoded first frame, etc.) are written to a temp dir printed at the end.

use anyhow::{Context as _, Result, anyhow};
use futures::{FutureExt as _, StreamExt as _};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use web_preview::cdp::CdpClient;
use web_preview::source_map::{Framework, Resolution};
use web_preview::{chrome, dev_server, screencast, source_map};

struct Doctor {
    results: Vec<(String, bool, String)>,
}

impl Doctor {
    fn report(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        println!(
            "{}  {}{}",
            if ok { "PASS" } else { "FAIL" },
            name,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        );
        self.results.push((name.to_string(), ok, detail));
    }

    fn finish(&self, artifacts: &std::path::Path) -> ! {
        let failed = self.results.iter().filter(|(_, ok, _)| !ok).count();
        println!(
            "\n=== {}/{} checks passed · artifacts in {} ===",
            self.results.len() - failed,
            self.results.len(),
            artifacts.display()
        );
        if failed > 0 {
            println!("\nFailed stages:");
            for (name, ok, detail) in &self.results {
                if !ok {
                    println!("  · {name} — {detail}");
                }
            }
        }
        std::process::exit(if failed > 0 { 1 } else { 0 });
    }
}

async fn with_timeout<T>(
    executor: &gpui::BackgroundExecutor,
    duration: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T> {
    futures::select! {
        result = future.fuse() => Ok(result),
        _ = executor.timer(duration).fuse() => Err(anyhow!("timed out after {duration:?}")),
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let app = gpui_platform::headless();
    app.run(|cx| {
        gpui_tokio::init(cx);
        cx.spawn(async move |cx| {
            let artifacts = std::env::temp_dir().join("web-preview-doctor");
            std::fs::create_dir_all(&artifacts).ok();
            let mut doctor = Doctor { results: Vec::new() };
            if let Err(error) = run(&mut doctor, &artifacts, cx).await {
                doctor.report("doctor run aborted", false, format!("{error:#}"));
            }
            doctor.finish(&artifacts);
        })
        .detach();
    });
}

async fn run(
    doctor: &mut Doctor,
    artifacts: &std::path::Path,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let url = env_or("WEB_PREVIEW_URL", "http://localhost:5173");
    let scale: f32 = env_or("DOCTOR_SCALE", "2").parse().unwrap_or(2.0);
    let (vp_w, vp_h) = (1280u32, 800u32);
    let executor = cx.background_executor().clone();
    let http_client: Arc<dyn http_client::HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());

    println!("Looking Glass doctor — url={url} scale={scale} viewport={vp_w}x{vp_h}\n");

    // Stage 1: dev server reachability (real dev_server module).
    let reachable = dev_server::is_reachable(&url, http_client.clone()).await;
    doctor.report("dev server reachable", reachable, url.clone());
    if !reachable {
        dev_server::wait_until_ready(&url, http_client.clone(), executor.clone(), Duration::from_secs(15))
            .await
            .context("dev server never became reachable — start it and re-run")?;
    }

    // Stage 2: locate Chrome (real chrome module; WEB_PREVIEW_CHROME overrides).
    let chrome_override = std::env::var("WEB_PREVIEW_CHROME").ok();
    let chrome_path = chrome::locate_chrome(chrome_override.as_deref())?;
    doctor.report("chrome located", true, chrome_path.display().to_string());

    // Stage 3: launch Chrome with the production flags (incl. --force-device-scale-factor).
    let port = chrome::free_port()?;
    let user_data_dir = std::env::temp_dir().join(format!("web-preview-doctor-chrome-{port}"));
    let process = chrome::launch(
        chrome_path,
        &url,
        port,
        user_data_dir,
        scale,
        http_client.clone(),
        executor.clone(),
    )
    .await
    .context("launching chrome")?;
    doctor.report("chrome launched + CDP endpoints discovered", true, format!("port {port}"));

    // Stage 4: CDP connect over the browser endpoint (real CdpClient via gpui_tokio).
    let cdp = CdpClient::connect(process.ws_url.clone(), cx)
        .await
        .context("connecting CDP")?;
    doctor.report("CDP websocket connected (browser endpoint)", true, "");

    // Stage 5: auto-attach to the page target (exact establish_session sequence).
    cdp.send(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
    )
    .await
    .context("Target.setAutoAttach")?;
    for _ in 0..50 {
        if cdp.page_session().is_some() {
            break;
        }
        executor.timer(Duration::from_millis(100)).await;
    }
    doctor.report(
        "page target attached (flatten session)",
        cdp.page_session().is_some(),
        cdp.page_session().unwrap_or_default(),
    );
    anyhow::ensure!(cdp.page_session().is_some(), "no page target attached");

    // Stage 6: enable domains.
    for domain in ["Page", "DOM", "CSS", "Overlay", "Runtime"] {
        cdp.send(&format!("{domain}.enable"), Value::Null)
            .await
            .with_context(|| format!("enabling {domain}"))?;
    }
    doctor.report("Page/DOM/CSS/Overlay/Runtime enabled", true, "");

    // Stage 7: framework detection (real source_map probe, same retry loop as the panel).
    let mut framework = Framework::Unknown;
    for attempt in 0..10 {
        framework = source_map::detect_framework(&cdp).await;
        if framework != Framework::Unknown {
            break;
        }
        if attempt < 9 {
            executor.timer(Duration::from_millis(300)).await;
        }
    }
    doctor.report(
        "framework detected",
        framework != Framework::Unknown,
        framework.label(),
    );

    // Stage 8: viewport override, then subscribe BEFORE starting the screencast (the fixed order),
    // then start.
    screencast::set_viewport(&cdp, vp_w, vp_h, scale).await?;
    let mut frames = cdp.subscribe("Page.screencastFrame");
    let mut loads = cdp.subscribe("Page.loadEventFired");
    let (cap_w, cap_h) = (
        (vp_w as f32 * scale.max(1.0)).round() as u32,
        (vp_h as f32 * scale.max(1.0)).round() as u32,
    );
    screencast::start(&cdp, 80, cap_w, cap_h).await?;
    doctor.report("viewport set + screencast started", true, format!("capture {cap_w}x{cap_h}"));

    // Stage 9: first frame (a static page sends exactly one settled frame — this catches the
    // dead-preview bug where the subscription raced the screencast start).
    let first = with_timeout(&executor, Duration::from_secs(10), frames.next()).await;
    let mut params = match first {
        Ok(Some(params)) => params,
        Ok(None) => anyhow::bail!("frame stream closed before first frame"),
        Err(_) => {
            doctor.report("first screencast frame", false, "no frame within 10s — preview would be blank");
            anyhow::bail!("no first frame");
        }
    };
    if let Some(session_id) = screencast::session_id(&params) {
        screencast::ack_no_reply(&cdp, session_id);
    }
    let first_size = screencast::decode_frame(&params)
        .map(|frame| {
            let size = frame.image.size(0);
            format!("{}x{}", size.width.0, size.height.0)
        })
        .unwrap_or_else(|_| "undecodable".into());
    doctor.report("first screencast frame received", true, first_size);

    // Stage 10: drain to the SETTLED frame (the viewport-override resize can emit transient
    // partial-size captures while the surface is still resizing; the panel always shows the
    // latest frame, so that's what must be full physical resolution) and decode with the REAL
    // decoder (JPEG -> BGRA RenderImage).
    let mut quiet = 0u32;
    while quiet < 7 {
        match with_timeout(&executor, Duration::from_millis(100), frames.next()).await {
            Ok(Some(newer)) => {
                if let Some(session_id) = screencast::session_id(&newer) {
                    screencast::ack_no_reply(&cdp, session_id);
                }
                params = newer;
                quiet = 0;
            }
            _ => quiet += 1,
        }
    }
    let decoded = screencast::decode_frame(&params).context("decoding settled frame")?;
    let size = decoded.image.size(0);
    let sharp = size.width.0 as u32 == cap_w && size.height.0 as u32 == cap_h;
    doctor.report(
        "settled frame decodes at physical (retina) resolution",
        sharp,
        format!(
            "decoded {}x{} (expected {cap_w}x{cap_h}); metadata CSS {}x{}",
            size.width.0, size.height.0, decoded.metadata.device_width, decoded.metadata.device_height
        ),
    );
    if let Some(data) = params.get("data").and_then(Value::as_str) {
        use base64::Engine as _;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
            std::fs::write(artifacts.join("settled-frame.jpg"), bytes).ok();
        }
    }

    // Stage 11: find representative content elements to probe (a bare page-center hit can land on
    // the framework's empty mount div, which legitimately has no source metadata).
    let candidates: Vec<(f32, f32)> = cdp
        .send(
            "Runtime.evaluate",
            json!({
                "expression": r#"
                    (() => {
                        const points = [];
                        for (const sel of ['button', 'a[href]', 'h1', 'h2', 'p', 'li', 'span', 'main *']) {
                            const el = Array.from(document.querySelectorAll(sel)).find((el) => {
                                const r = el.getBoundingClientRect();
                                return r.width > 4 && r.height > 4 && r.x >= 0 && r.y >= 0
                                    && r.x < innerWidth && r.y < innerHeight;
                            });
                            if (el) {
                                const r = el.getBoundingClientRect();
                                points.push([r.x + r.width / 2, r.y + r.height / 2]);
                            }
                        }
                        points.push([innerWidth / 2, innerHeight / 2]);
                        return points;
                    })()
                "#,
                "returnByValue": true,
            }),
        )
        .await?
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_array)
        .map(|points| {
            points
                .iter()
                .filter_map(|point| {
                    let x = point.get(0).and_then(Value::as_f64)? as f32;
                    let y = point.get(1).and_then(Value::as_f64)? as f32;
                    Some((x, y))
                })
                .collect()
        })
        .unwrap_or_default();
    doctor.report(
        "content elements located for pick probe",
        !candidates.is_empty(),
        format!("{} probe points", candidates.len()),
    );

    // Stage 12: pick pipeline (getNodeForLocation -> getDocument/push nodeId -> resolveNode ->
    // real source_map::resolve) at each probe point until one resolves to source.
    let scroll = (
        decoded.metadata.scroll_offset_x,
        decoded.metadata.scroll_offset_y,
    );
    cdp.send("DOM.getDocument", json!({ "depth": 0 })).await?;
    let mut hit_test_ok = false;
    let mut node_id_ok = false;
    let mut css_rules: usize = 0;
    let mut best_resolution: Option<Resolution> = None;
    for (probe_x, probe_y) in &candidates {
        let Ok(located) = cdp
            .send(
                "DOM.getNodeForLocation",
                json!({
                    "x": (probe_x + scroll.0) as i64,
                    "y": (probe_y + scroll.1) as i64,
                    "includeUserAgentShadowDOM": false,
                }),
            )
            .await
        else {
            continue;
        };
        let Some(backend_node_id) = located.get("backendNodeId").and_then(Value::as_i64) else {
            continue;
        };
        hit_test_ok = true;

        if let Ok(pushed) = cdp
            .send(
                "DOM.pushNodesByBackendIdsToFrontend",
                json!({ "backendNodeIds": [backend_node_id] }),
            )
            .await
        {
            if let Some(node_id) = pushed
                .get("nodeIds")
                .and_then(Value::as_array)
                .and_then(|ids| ids.first())
                .and_then(Value::as_i64)
                .filter(|id| *id != 0)
            {
                node_id_ok = true;
                if css_rules == 0 {
                    if let Ok(matched) = cdp
                        .send("CSS.getMatchedStylesForNode", json!({ "nodeId": node_id }))
                        .await
                    {
                        css_rules = matched
                            .get("matchedCSSRules")
                            .and_then(Value::as_array)
                            .map(Vec::len)
                            .unwrap_or(0);
                    }
                }
            }
        }

        let Ok(resolved) = cdp
            .send("DOM.resolveNode", json!({ "backendNodeId": backend_node_id }))
            .await
        else {
            continue;
        };
        let Some(object_id) = resolved
            .get("object")
            .and_then(|object| object.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if let Ok(resolution) = source_map::resolve(&cdp, framework, &object_id).await {
            let is_source = matches!(resolution, Resolution::Source(_));
            best_resolution = Some(resolution);
            if is_source {
                break;
            }
        }
    }
    doctor.report("element hit-test at probe points", hit_test_ok, "");
    doctor.report("frontend nodeId for CSS panel", node_id_ok, "");
    doctor.report("CSS panel rules load", css_rules > 0, format!("{css_rules} matched rules"));
    match &best_resolution {
        Some(Resolution::Source(location)) => doctor.report(
            "pick resolves to source",
            true,
            format!("{}:{}:{}", location.file, location.line, location.column),
        ),
        Some(Resolution::SelectorOnly { selector, hint }) => {
            // With an unknown framework this is the designed degradation, not a failure.
            let ok = framework == Framework::Unknown;
            doctor.report(
                "pick resolves to source",
                ok,
                format!(
                    "selector-only ({selector}); {}",
                    hint.clone()
                        .unwrap_or_else(|| "no source metadata on probed elements".into())
                ),
            );
        }
        None => doctor.report("pick resolves to source", false, "no probe point resolved"),
    }

    // Stage 12b: forward a click (exact forward_click event sequence) at the first probe point.
    if let Some((click_x, click_y)) = candidates.first() {
        cdp.send_no_reply(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": click_x, "y": click_y, "button": "none", "buttons": 0 }),
        );
        for (event_type, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
            cdp.send(
                "Input.dispatchMouseEvent",
                json!({
                    "type": event_type, "x": click_x, "y": click_y,
                    "button": "left", "buttons": buttons, "clickCount": 1,
                }),
            )
            .await
            .context("dispatching click")?;
        }
        doctor.report(
            "click dispatched to page",
            true,
            format!("at CSS ({click_x:.0}, {click_y:.0})"),
        );
    }

    // Stage 13: reload survival — loadEventFired arrives, re-init brings frames back.
    cdp.send("Page.reload", json!({ "ignoreCache": false })).await?;
    let load = with_timeout(&executor, Duration::from_secs(15), loads.next()).await;
    doctor.report("loadEventFired after reload", load.is_ok(), "");
    screencast::set_viewport(&cdp, vp_w, vp_h, scale).await?;
    screencast::start(&cdp, 80, cap_w, cap_h).await?;
    let post_reload = with_timeout(&executor, Duration::from_secs(10), frames.next()).await;
    let got_frame = matches!(&post_reload, Ok(Some(_)));
    doctor.report("frames resume after reload re-init", got_frame, "");
    if let Ok(Some(params)) = post_reload {
        if let Some(session_id) = screencast::session_id(&params) {
            screencast::ack_no_reply(&cdp, session_id);
        }
        if let Some(data) = params.get("data").and_then(Value::as_str) {
            use base64::Engine as _;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                std::fs::write(artifacts.join("post-reload-frame.jpg"), bytes).ok();
            }
        }
    }

    // Keep Chrome alive until here; dropping kills it.
    drop(process);
    Ok(())
}
