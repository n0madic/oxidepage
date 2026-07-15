//! `HTMLOptGroupElement`. A disabled optgroup disables its options, which the
//! DOM's "actually disabled" walk already accounts for.

use crate::imp::reflect::{bool_reflector, string_reflector};

bool_reflector!(disabled, set_disabled, "disabled");
string_reflector!(label, set_label, "label");
