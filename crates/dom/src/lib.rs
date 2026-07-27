//! The arena DOM: single source of truth for document state
//! (design doc §5.2).
//!
//! - [`tree::DomTree`]: generational-arena tree with spec mutation
//!   algorithms; every mutation runs through one code path that updates the
//!   tree, queues [`observer`] records, and sets dirty flags.
//! - [`parser`]: suspendable streaming HTML parsing (html5ever) writing
//!   directly into the arena via the [`sink`] `TreeSink`.
//! - [`serialize`]: `innerHTML`/`outerHTML` via html5ever's serializer.
//! - [`decode`]: byte-stream decoding with spec encoding sniffing.
//! - [`event`]: native capture/target/bubble dispatch skeleton.
//! - [`dump`]: html5lib tree dump, for conformance tests and debugging.

pub mod arena;
pub mod custom_element;
pub mod decode;
pub mod dump;
pub mod event;
pub mod form;
pub mod node;
pub mod observer;
pub mod parser;
pub mod select;
pub mod serialize;
pub mod shadow;
pub mod sink;
pub mod stylo;
pub mod stylo_data;
pub mod tree;

pub use custom_element::{CustomElementReaction, CustomElementState, is_valid_custom_element_name};
pub use event::{AddEventListenerOptions, Event, EventPhase, ListenerCallback, ListenerId};
pub use form::{ClickActivation, FormState, SelectionDirection, input_type};
pub use node::{DocumentData, ElementData, Node, NodeData, NodeFlags, NodeKind};
pub use observer::{
    MutationObserverId, MutationRecord, MutationRecordType, ObserveInit, ObserveOptions,
};
pub use parser::{ParseOptions, ParseSignal, Parser, parse_document, parse_fragment};
pub use select::{CompiledSelectorList, parse_selector_list};
pub use shadow::ShadowMode;
pub use sink::ParsedDocument;
pub use tree::{DomTree, StyleUpdate};

// Re-export the vocabulary types the DOM API surface speaks.
pub use html5ever::interface::QuirksMode;
pub use html5ever::tendril::StrTendril;
pub use html5ever::{Attribute, LocalName, Namespace, Prefix, QualName};
pub use oxidepage_base::{DomException, DomExceptionKind, NodeId};
