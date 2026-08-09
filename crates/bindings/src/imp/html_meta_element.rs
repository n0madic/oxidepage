//! `HTMLMetaElement`: plain attribute reflection.
//!
//! Nothing here changes behaviour — the engine reads `<meta charset>` and
//! `<meta name=viewport>` from the parsed attributes, not from these members.
//! They exist because script reads them: `document.querySelector('meta').name`
//! is how a page (and WPT's own `testharness.js`, for `<meta name=timeout>`)
//! asks what a `<meta>` declares, and an absent member answers `undefined`,
//! which reads as "no such metadata" rather than as "unimplemented".

use crate::imp::reflect::string_reflector;

string_reflector!(name, set_name, "name");
// `httpEquiv` reflects the hyphenated attribute — the one place the IDL name
// and the content attribute name differ on this interface.
string_reflector!(http_equiv, set_http_equiv, "http-equiv");
string_reflector!(content, set_content, "content");
string_reflector!(media, set_media, "media");
