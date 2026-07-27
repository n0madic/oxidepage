// UI Events (https://w3c.github.io/uievents/) plus the Pointer Events and
// Input Events members automation depends on.
//
// The whole family hangs off `UIEvent : Event`, and the generator chains
// prototypes for the full depth (`Event` <- `UIEvent` <- `MouseEvent` <-
// `PointerEvent`), so `new PointerEvent(...) instanceof MouseEvent` holds.
//
// `view` is typed `any` rather than `Window?`: the code generator does not
// return interface-typed attributes, and `Document.defaultView` already takes
// the same escape. It is the Window object or null, nothing else.
//
// Init dictionaries are declared so the generator classifies them (a dictionary
// is a passthrough type — the glue hands the imp a raw value and the imp reads
// the members), *not* because the generator understands dictionary
// inheritance: `MouseEventInit : EventInit` is honoured by
// `imp::ui_event::parse_ui_init` and its callers, which read the inherited
// members themselves.

dictionary UIEventInit : EventInit {
  any view = null;
  long detail = 0;
};

interface UIEvent : Event {
  constructor(DOMString type, optional UIEventInit eventInitDict = {});
  readonly attribute any view;
  readonly attribute long detail;
  // Legacy, and still what `document.createEvent("UIEvent")` users call.
  undefined initUIEvent(DOMString type, optional boolean bubbles = false,
                        optional boolean cancelable = false,
                        optional any view = null, optional long detail = 0);
};

dictionary EventModifierInit : UIEventInit {
  boolean ctrlKey = false;
  boolean shiftKey = false;
  boolean altKey = false;
  boolean metaKey = false;
};

dictionary MouseEventInit : EventModifierInit {
  double screenX = 0;
  double screenY = 0;
  double clientX = 0;
  double clientY = 0;
  short button = 0;
  unsigned short buttons = 0;
  EventTarget? relatedTarget = null;
};

interface MouseEvent : UIEvent {
  constructor(DOMString type, optional MouseEventInit eventInitDict = {});
  readonly attribute double screenX;
  readonly attribute double screenY;
  readonly attribute double clientX;
  readonly attribute double clientY;
  readonly attribute double pageX;
  readonly attribute double pageY;
  readonly attribute double offsetX;
  readonly attribute double offsetY;
  readonly attribute double x;
  readonly attribute double y;
  readonly attribute boolean ctrlKey;
  readonly attribute boolean shiftKey;
  readonly attribute boolean altKey;
  readonly attribute boolean metaKey;
  readonly attribute short button;
  readonly attribute unsigned short buttons;
  readonly attribute EventTarget? relatedTarget;
  boolean getModifierState(DOMString keyArg);
};

dictionary WheelEventInit : MouseEventInit {
  double deltaX = 0;
  double deltaY = 0;
  double deltaZ = 0;
  unsigned long deltaMode = 0;
};

interface WheelEvent : MouseEvent {
  constructor(DOMString type, optional WheelEventInit eventInitDict = {});
  const unsigned long DOM_DELTA_PIXEL = 0;
  const unsigned long DOM_DELTA_LINE = 1;
  const unsigned long DOM_DELTA_PAGE = 2;
  readonly attribute double deltaX;
  readonly attribute double deltaY;
  readonly attribute double deltaZ;
  readonly attribute unsigned long deltaMode;
};

dictionary PointerEventInit : MouseEventInit {
  long pointerId = 0;
  double width = 1;
  double height = 1;
  float pressure = 0;
  DOMString pointerType = "";
  boolean isPrimary = false;
};

interface PointerEvent : MouseEvent {
  constructor(DOMString type, optional PointerEventInit eventInitDict = {});
  readonly attribute long pointerId;
  readonly attribute double width;
  readonly attribute double height;
  readonly attribute double pressure;
  readonly attribute DOMString pointerType;
  readonly attribute boolean isPrimary;
};

dictionary KeyboardEventInit : EventModifierInit {
  DOMString key = "";
  DOMString code = "";
  unsigned long location = 0;
  boolean repeat = false;
  boolean isComposing = false;
  unsigned long charCode = 0;
  unsigned long keyCode = 0;
};

interface KeyboardEvent : UIEvent {
  constructor(DOMString type, optional KeyboardEventInit eventInitDict = {});
  const unsigned long DOM_KEY_LOCATION_STANDARD = 0;
  const unsigned long DOM_KEY_LOCATION_LEFT = 1;
  const unsigned long DOM_KEY_LOCATION_RIGHT = 2;
  const unsigned long DOM_KEY_LOCATION_NUMPAD = 3;
  readonly attribute DOMString key;
  readonly attribute DOMString code;
  readonly attribute unsigned long location;
  readonly attribute boolean ctrlKey;
  readonly attribute boolean shiftKey;
  readonly attribute boolean altKey;
  readonly attribute boolean metaKey;
  readonly attribute boolean repeat;
  readonly attribute boolean isComposing;
  // Legacy but universally read: jQuery, every hotkey library.
  readonly attribute unsigned long charCode;
  readonly attribute unsigned long keyCode;
  readonly attribute unsigned long which;
  boolean getModifierState(DOMString keyArg);
};

dictionary FocusEventInit : UIEventInit {
  EventTarget? relatedTarget = null;
};

interface FocusEvent : UIEvent {
  constructor(DOMString type, optional FocusEventInit eventInitDict = {});
  readonly attribute EventTarget? relatedTarget;
};

dictionary InputEventInit : UIEventInit {
  DOMString? data = null;
  boolean isComposing = false;
  DOMString inputType = "";
};

interface InputEvent : UIEvent {
  constructor(DOMString type, optional InputEventInit eventInitDict = {});
  readonly attribute DOMString? data;
  readonly attribute boolean isComposing;
  readonly attribute DOMString inputType;
};

dictionary CompositionEventInit : UIEventInit {
  DOMString data = "";
};

// The interface exists and is constructible — test tooling builds one — but the
// engine never generates a composition: there is no IME headless. That is a
// data carrier, not a stub that lies (P6).
interface CompositionEvent : UIEvent {
  constructor(DOMString type, optional CompositionEventInit eventInitDict = {});
  readonly attribute DOMString data;
};
