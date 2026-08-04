//! The custom-element registry data held by [`WorldState`](crate::WorldState).
//!
//! The DOM crate stores only per-element state and reaction intents (it cannot
//! depend on JS). The *definitions* — constructors, lifecycle callbacks — live
//! here, behind `Persistent` [`JsValue`]s. The registry survives a page's
//! lifetime and is cleared on navigation.

use std::collections::HashMap;
use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_js::{JsScope, JsValue};

/// One `customElements.define(name, ctor, options)` definition.
pub(crate) struct CustomElementDefinition {
    pub name: String,
    /// The class/constructor (`Persistent`).
    pub constructor: JsValue,
    /// `constructor.prototype`.
    #[allow(dead_code)]
    pub prototype: JsValue,
    /// `constructor.observedAttributes`, lower-cased verbatim from the getter.
    pub observed_attributes: Vec<String>,
    pub connected: Option<JsValue>,
    pub disconnected: Option<JsValue>,
    pub attribute_changed: Option<JsValue>,
}

impl CustomElementDefinition {
    #[must_use]
    pub fn observes(&self, attr: &str) -> bool {
        self.observed_attributes.iter().any(|a| a == attr)
    }
}

/// The realm's custom-element registry.
#[derive(Default)]
pub(crate) struct CustomElementRegistry {
    /// Definitions in `define` order.
    pub definitions: Vec<Rc<CustomElementDefinition>>,
    /// Pending `whenDefined(name)` promises: `name -> (promise, resolve)`.
    pub when_defined: HashMap<String, (JsValue, JsValue)>,
    /// Elements currently being upgraded/created. The base `HTMLElement`
    /// constructor pops the top entry to bind itself to that pre-created node.
    /// A single stack (rather than per-definition) is a simplification valid
    /// for autonomous custom elements; see the ADR.
    pub construction_stack: Vec<NodeId>,
}

impl CustomElementRegistry {
    /// The definition registered for `name`, if any.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<Rc<CustomElementDefinition>> {
        self.definitions
            .iter()
            .find(|d| d.name == name)
            .map(Rc::clone)
    }

    /// The definition whose constructor is strictly equal to `ctor` (reverse
    /// lookup for `HTMLElement()`/`getName`).
    #[must_use]
    pub fn by_constructor(
        &self,
        scope: &dyn JsScope,
        ctor: &JsValue,
    ) -> Option<Rc<CustomElementDefinition>> {
        self.definitions
            .iter()
            .find(|d| scope.strict_equals(&d.constructor, ctor))
            .map(Rc::clone)
    }

    /// Clears all state for a new navigation.
    pub fn clear(&mut self) {
        self.definitions.clear();
        self.when_defined.clear();
        self.construction_stack.clear();
    }
}
