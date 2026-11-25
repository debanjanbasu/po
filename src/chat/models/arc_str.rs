//! Shared autosurgeon `with` support for `Arc<str>` fields — used by every
//! chat model that stores reference-counted strings.

use std::sync::Arc;

use automerge::ObjId;
use autosurgeon::{Hydrate, HydrateError, Prop, ReadDoc, Reconciler};

pub fn hydrate<D: ReadDoc>(doc: &D, obj: &ObjId, prop: Prop<'_>) -> Result<Arc<str>, HydrateError> {
    String::hydrate(doc, obj, prop).map(|s| Arc::from(s.as_str()))
}

pub fn reconcile<R: Reconciler>(value: &Arc<str>, mut reconciler: R) -> Result<(), R::Error> {
    reconciler.str(value.as_ref())
}
