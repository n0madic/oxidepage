//! The `Emulation` and `Security` domains.
//!
//! Small, and mostly a question of what to *refuse*. Every override here that
//! the engine cannot actually perform answers an error rather than `{}` — a
//! test that sets a timezone and then asserts on a formatted date must fail at
//! the setter, not silently compare against the wrong zone.

use std::sync::Arc;

use oxidepage_engine::page_api::Viewport;
use serde::Deserialize;
use serde_json::json;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Emulation.setDeviceMetricsOverride" => set_device_metrics(connection, request),
        "Emulation.clearDeviceMetricsOverride" => clear_device_metrics(connection, request),
        "Emulation.setUserAgentOverride" => set_user_agent_override(connection, request),
        "Emulation.setEmulatedMedia" => set_emulated_media(connection, request),
        "Emulation.setFocusEmulationEnabled" => set_focus_emulation(connection, request),
        // Puppeteer sends this on every `setViewport`. Turning touch *off* is
        // the state anyway; turning it on is refused with the touch events.
        "Emulation.setTouchEmulationEnabled" => set_touch_emulation(connection, request),
        "Emulation.setScriptExecutionDisabled" => set_script_disabled(connection, request),
        "Emulation.setLocaleOverride" => set_locale_override(connection, request),
        // Deliberate refusals, each because the capability does not exist:
        // pretending otherwise is what P6 forbids.
        "Emulation.setTimezoneOverride" => Err(ProtocolError::server(
            "Emulation.setTimezoneOverride is not implemented: QuickJS-NG has no ICU, so there \
             is no Intl and Date follows the process timezone",
        )),
        "Emulation.setGeolocationOverride" => Err(ProtocolError::server(
            "Emulation.setGeolocationOverride is not implemented: there is no Geolocation API \
             to override",
        )),
        "Emulation.setEmitTouchEventsForMouse" => Err(ProtocolError::server(
            "Emulation.setEmitTouchEventsForMouse is not implemented: there are no touch events",
        )),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

pub fn dispatch_security(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Security.enable" | "Security.disable" => Ok(json!({})),
        "Security.setIgnoreCertificateErrors" => set_ignore_certificate_errors(connection, request),
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceMetricsParams {
    width: u32,
    height: u32,
    device_scale_factor: f64,
    /// Accepted and ignored — there is no mobile emulation to switch on, and
    /// the member is required by the protocol so refusing it would refuse the
    /// whole command.
    #[serde(default)]
    mobile: bool,
}

fn set_device_metrics(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: DeviceMetricsParams = request.parse()?;
    let _ = params.mobile;

    // CDP's 0 means "do not override this dimension". A zero-area viewport
    // would lay out nothing at all, so it must not reach the engine.
    if params.width == 0 || params.height == 0 {
        return Ok(json!({}));
    }
    // The scale factor is also `devicePixelRatio` as the page sees it, which is
    // what makes `deviceScaleFactor: 2` produce a 2x screenshot *and* a page
    // that picks its 2x images.
    let dpr = if params.device_scale_factor > 0.0 {
        params.device_scale_factor as f32
    } else {
        1.0
    };
    session.page.set_viewport(Viewport {
        width: params.width as f32,
        height: params.height as f32,
        dpr,
    })?;
    Ok(json!({}))
}

fn clear_device_metrics(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    // There is no "the size before the override" to restore — the engine holds
    // one viewport, not a stack — so this returns to the library default, which
    // is what a driver's `setViewport(null)` means in practice.
    session.page.set_viewport(Viewport::default())?;
    Ok(json!({}))
}

/// `Emulation.setLocaleOverride` — `navigator.language`/`languages` **and**
/// the `Accept-Language` header (ADR-0034 D6).
///
/// Both halves or nothing, which is what makes this implementable where
/// `setUserAgentOverride` is not: moving only the script side would leave every
/// request advertising the old locale, and a page that renders in the wrong
/// language while `navigator.language` insists otherwise is worse than a loud
/// refusal.
///
/// An absent `locale` clears the override in Chrome; there is no "the locale
/// before the override" to restore here, so it is refused rather than silently
/// doing nothing.
fn set_locale_override(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        #[serde(default)]
        locale: Option<String>,
    }
    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    let Some(locale) = params.locale.filter(|l| !l.is_empty()) else {
        return Err(ProtocolError::invalid_params(
            "Emulation.setLocaleOverride requires a locale: there is no prior locale to restore",
        ));
    };
    session
        .page
        .set_languages(vec![locale])?
        .map_err(ProtocolError::server)?;
    Ok(json!({}))
}

/// `Emulation.setUserAgentOverride` / `Network.setUserAgentOverride`.
///
/// **Refused, deliberately.** The identity a page reports is fixed when the
/// page is built: `NavigatorData` has no interior mutability, and the
/// `User-Agent` header is baked into `RequestDefaults` at the same moment, so
/// there is no seam to change it through afterwards. Half-doing it — updating
/// `navigator.userAgent` but not the header, or the reverse — is worse than
/// not doing it: a test would see a page that claims one identity to script and
/// another to the server.
///
/// What *does* work is setting the identity when the page is created
/// (`NewPageOptions::navigator`), which an embedder using `oxidepage-engine`
/// directly can do. CDP has no parameter for that on `Target.createTarget`.
pub fn set_user_agent_override(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    connection.require_session(request)?;
    Err(ProtocolError::server(
        "setUserAgentOverride is not implemented: a page's navigator identity and its \
         User-Agent header are fixed when the page is created",
    ))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct EmulatedMediaParams {
    #[serde(default)]
    media: Option<String>,
    #[serde(default)]
    features: Vec<MediaFeature>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaFeature {
    name: String,
    value: String,
}

fn set_emulated_media(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    connection.require_session(request)?;
    let params: EmulatedMediaParams = request.parse()?;

    // `screen` and `""` both mean "no override", which is the state anyway.
    if let Some(media) = params.media.as_deref()
        && !media.is_empty()
        && media != "screen"
    {
        return Err(ProtocolError::server(format!(
            "Emulation.setEmulatedMedia({media}) is not implemented: @media print is a \
             documented non-goal (ADR-0026)"
        )));
    }
    // A feature set to the value the engine *already reports* is a no-op, and
    // accepting it is not a lie — the same rule the `screen`/`""` media case
    // above follows, and the one ADR-0030 D9 states for every refusal in this
    // domain. Playwright sends `prefers-color-scheme=light` while creating
    // **every** page, so refusing it makes `newPage()` throw and nothing about
    // Playwright works at all.
    //
    // Deliberately narrow: only each feature's **default**, which is the state
    // the engine is already in and the only one it can be in — there is no
    // dark mode, no motion reduction and no forced-colors mode to switch to, so
    // any *other* value is still refused rather than silently ignored.
    //
    // One caveat, recorded in ADR-0033's limits rather than hidden: stylo
    // reports `prefers-reduced-motion` and `forced-colors` as *not matching* in
    // `matchMedia`, so a driver that sets one of these and then asserts the
    // query holds will disagree with the page. That is a pre-existing gap in
    // media-feature support, not something accepting the no-op introduces.
    for feature in &params.features {
        let honest = matches!(
            (feature.name.as_str(), feature.value.as_str()),
            ("prefers-color-scheme", "light")
                | ("prefers-reduced-motion", "no-preference")
                | ("forced-colors", "none")
                | ("prefers-contrast", "no-preference")
        );
        if !honest {
            return Err(ProtocolError::server(format!(
                "Emulation.setEmulatedMedia cannot override the media feature {}={}",
                feature.name, feature.value
            )));
        }
    }
    Ok(json!({}))
}

fn set_focus_emulation(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    connection.require_session(request)?;
    // Trivially true and worth answering: a headless page is always focused
    // (`document.hasFocus()` says so), so the emulation Playwright asks for is
    // already the behavior.
    Ok(json!({}))
}

fn set_script_disabled(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        value: bool,
    }
    connection.require_session(request)?;
    let params: Params = request.parse()?;
    if params.value {
        return Err(ProtocolError::server(
            "Emulation.setScriptExecutionDisabled(true) is not implemented: there is no switch \
             to stop the realm from running script",
        ));
    }
    Ok(json!({}))
}

fn set_ignore_certificate_errors(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        ignore: bool,
    }
    let _ = connection;
    let params: Params = request.parse()?;
    if params.ignore {
        // The TLS verifier is baked into the shared hyper client at browser
        // construction (`rustls` with bundled roots), so there is no per-page
        // or even per-browser switch after the fact. Answering `{}` would let a
        // driver believe a self-signed origin will now load.
        return Err(ProtocolError::server(
            "Security.setIgnoreCertificateErrors(true) is not implemented: the TLS verifier is \
             fixed at browser construction",
        ));
    }
    Ok(json!({}))
}

fn set_touch_emulation(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        enabled: bool,
    }
    connection.require_session(request)?;
    let params: Params = request.parse()?;
    if params.enabled {
        return Err(ProtocolError::server(
            "Emulation.setTouchEmulationEnabled(true) is not implemented: there are no touch \
             events to emulate",
        ));
    }
    Ok(json!({}))
}
