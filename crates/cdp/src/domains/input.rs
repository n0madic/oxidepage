//! The `Input` domain: trusted mouse, wheel, keyboard and text insertion.
//!
//! A thin mapping onto the engine's own input synthesis (ADR-0023): everything
//! interesting — the `mouseover`/`mouseenter` chain, the focus transfer, the
//! `beforeinput` → mutate → `input` sequence, activation behavior — already
//! happens below `page`. What lives here is the vocabulary translation, and two
//! decisions worth naming:
//!
//! * **The key table is the single source of `code` and `keyCode`.**
//!   `windowsVirtualKeyCode` is accepted and not used to override it: honouring
//!   a driver's number would let `KeyboardEvent.code` and `.keyCode` disagree
//!   about which physical key was pressed, which no real keyboard can do.
//! * **`text` is the driver's to decide.** `keyDown` passes the `text` it was
//!   given (absent means "let the table decide"), `rawKeyDown` and `keyUp` pass
//!   none. That is what makes `rawKeyDown` — which Puppeteer sends for
//!   `Backspace`, `Tab` and the arrows — type nothing while still running the
//!   key's own default action.

use std::sync::Arc;

use oxidepage_engine::page_api::{
    KeyEvent, KeyEventKind, Modifiers, MouseEventKind, MouseInput, WheelInput, key_for_code,
};
use serde::Deserialize;
use serde_json::json;

use crate::error::{CommandResult, ProtocolError};
use crate::message::Request;
use crate::session::Connection;

pub fn dispatch(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    match request.method.as_str() {
        "Input.dispatchMouseEvent" => dispatch_mouse(connection, request),
        "Input.dispatchKeyEvent" => dispatch_key(connection, request),
        "Input.insertText" => insert_text(connection, request),
        // No touch events, no `DataTransfer`, and therefore nothing to
        // withhold: `method_not_found` lets a driver feature-detect and fall
        // back, which is exactly what Puppeteer's `elementHandle.scrollIntoView`
        // does when it catches a protocol error. See ADR-0031 D4.
        _ => Err(ProtocolError::method_not_found(&request.method)),
    }
}

/// CDP's modifier bitmask, verified against Puppeteer's `#modifierBit`.
///
/// Not the same order as any DOM constant, and not derivable — it is a wire
/// contract, so it is transcribed and named rather than computed.
fn modifiers_from(bits: i64) -> Modifiers {
    Modifiers {
        alt: bits & 1 != 0,
        ctrl: bits & 2 != 0,
        meta: bits & 4 != 0,
        shift: bits & 8 != 0,
    }
}

/// CDP's `MouseButton` name → `MouseEvent.button`.
///
/// `"none"` maps to **0**, not −1: `MouseEvent.button` reads 0 on a plain
/// `mousemove` in every browser, and the engine's own wheel synthesis already
/// builds `button: 0`.
fn button_from(name: Option<&str>) -> Result<i16, ProtocolError> {
    match name.unwrap_or("none") {
        "none" | "left" => Ok(0),
        "middle" => Ok(1),
        "right" => Ok(2),
        "back" => Ok(3),
        "forward" => Ok(4),
        other => Err(ProtocolError::invalid_params(format!(
            "Unknown mouse button: {other}"
        ))),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MouseParams {
    r#type: String,
    x: f64,
    y: f64,
    #[serde(default)]
    modifiers: Option<i64>,
    #[serde(default)]
    button: Option<String>,
    /// CDP's mask *is* `MouseEvent.buttons`, so it passes straight through.
    #[serde(default)]
    buttons: Option<u16>,
    #[serde(default)]
    click_count: Option<i32>,
    #[serde(default)]
    delta_x: Option<f64>,
    #[serde(default)]
    delta_y: Option<f64>,
    // Declared so the ignore is visible rather than silently absorbed by
    // serde's unknown-field tolerance (the same convention `Runtime.evaluate`
    // follows for `contextId`).
    #[serde(default)]
    pointer_type: Option<String>,
    #[serde(default)]
    timestamp: Option<f64>,
    #[serde(default)]
    force: Option<f64>,
    #[serde(default)]
    tilt_x: Option<f64>,
    #[serde(default)]
    tilt_y: Option<f64>,
    #[serde(default)]
    twist: Option<i64>,
}

fn dispatch_mouse(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: MouseParams = request.parse()?;
    let _ = (
        &params.pointer_type,
        params.timestamp,
        params.force,
        params.tilt_x,
        params.tilt_y,
        params.twist,
    );

    let modifiers = modifiers_from(params.modifiers.unwrap_or(0));
    let (x, y) = (params.x as f32, params.y as f32);

    if params.r#type == "mouseWheel" {
        session.page.dispatch_wheel(WheelInput {
            x,
            y,
            delta_x: params.delta_x.unwrap_or(0.0),
            delta_y: params.delta_y.unwrap_or(0.0),
            modifiers,
        })?;
        return Ok(json!({}));
    }

    let kind = match params.r#type.as_str() {
        "mousePressed" => MouseEventKind::Down,
        "mouseReleased" => MouseEventKind::Up,
        "mouseMoved" => MouseEventKind::Move,
        // Not silently treated as a move: a driver that sent a type we do not
        // implement must learn that, not watch its press vanish.
        other => {
            return Err(ProtocolError::invalid_params(format!(
                "Unknown mouse event type: {other}"
            )));
        }
    };
    let default_clicks = i32::from(kind != MouseEventKind::Move);
    session.page.dispatch_mouse(MouseInput {
        kind,
        x,
        y,
        button: button_from(params.button.as_deref())?,
        buttons: params.buttons.unwrap_or(0),
        modifiers,
        click_count: params.click_count.unwrap_or(default_clicks),
    })?;
    Ok(json!({}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyParams {
    r#type: String,
    #[serde(default)]
    modifiers: Option<i64>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    auto_repeat: Option<bool>,
    #[serde(default)]
    location: Option<u32>,
    /// Accepted and **not** used to override the key table's `keyCode` — see
    /// this module's header.
    #[serde(default)]
    windows_virtual_key_code: Option<i64>,
    #[serde(default)]
    native_virtual_key_code: Option<i64>,
    #[serde(default)]
    unmodified_text: Option<String>,
    #[serde(default)]
    is_keypad: Option<bool>,
    /// macOS editing commands (`selectAll`, `moveToEndOfLine`, …). Declared,
    /// ignored, and named as a deliberate limit in ADR-0031.
    #[serde(default)]
    commands: Option<Vec<String>>,
}

fn dispatch_key(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    let session = connection.require_session(request)?;
    let params: KeyParams = request.parse()?;
    let _ = (
        params.windows_virtual_key_code,
        params.native_virtual_key_code,
        &params.unmodified_text,
        params.is_keypad,
        &params.commands,
    );

    let modifiers = modifiers_from(params.modifiers.unwrap_or(0));

    // `char` is not a key press at all — it is the text a key produced, which
    // is the engine's `insert_text`.
    if params.r#type == "char" {
        let Some(text) = params.text else {
            return Err(ProtocolError::invalid_params(
                "Input.dispatchKeyEvent of type 'char' requires text",
            ));
        };
        session.page.insert_text(text)?;
        return Ok(json!({}));
    }

    let (kind, text) = match params.r#type.as_str() {
        "keyDown" => (KeyEventKind::Down, params.text.clone()),
        // The key table already yields no text for exactly the keys Puppeteer
        // sends `rawKeyDown` for (`Backspace`, `Tab`, the arrows), so letting it
        // decide is identical in effect *and* leaves their default actions
        // intact — which passing `Some("")` would not.
        "rawKeyDown" => (KeyEventKind::Down, None),
        "keyUp" => (KeyEventKind::Up, None),
        other => {
            return Err(ProtocolError::invalid_params(format!(
                "Unknown key event type: {other}"
            )));
        }
    };

    // A driver may name the key, the physical code, or both. Neither is not
    // enough to synthesize anything honest.
    let code = params.code.clone();
    let key = match params.key {
        Some(key) => key,
        None => {
            let named = code.as_deref().unwrap_or_default();
            key_for_code(named, modifiers.shift).ok_or_else(|| {
                ProtocolError::invalid_params(format!(
                    "Input.dispatchKeyEvent needs a key, or a code this keyboard has: {named:?}"
                ))
            })?
        }
    };

    session.page.dispatch_key(KeyEvent {
        kind,
        key,
        modifiers,
        repeat: params.auto_repeat.unwrap_or(false),
        text,
        // The driver is authoritative about which *physical* key this is, and
        // it is the only source for the members the US-layout table stores once
        // (`ShiftLeft`/`ShiftRight` share a `key`). Unlike
        // `windowsVirtualKeyCode`, honouring it cannot make `code` and
        // `keyCode` disagree — they are different axes.
        code,
        location: params.location.unwrap_or(0),
    })?;
    Ok(json!({}))
}

fn insert_text(connection: &Arc<Connection>, request: &Request) -> CommandResult {
    #[derive(Deserialize)]
    struct Params {
        text: String,
    }

    let session = connection.require_session(request)?;
    let params: Params = request.parse()?;
    session.page.insert_text(params.text)?;
    Ok(json!({}))
}
