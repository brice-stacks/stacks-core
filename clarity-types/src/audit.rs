// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Diagnostic hooks for auditing type-checker behaviors that are known to be
//! unsound, but that cannot be changed without a consensus change.
//!
//! This module only exists with the `type-audit` cargo feature. Nothing is
//! recorded until [`enable`] is called, so a binary built with the feature
//! behaves exactly like one built without it until a tool opts in.
//!
//! The recorded events are consumed by `stacks-inspect audit-contract-types`,
//! which re-typechecks every contract in a chainstate to find out whether the
//! behaviors actually show up on-chain.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use stacks_common::types::StacksEpochId;

use crate::representations::Span;
use crate::types::TypeSignature;

/// A type-checker behavior worth reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeAuditEvent {
    /// `least_supertype` unified two tuple types with different key sets.
    ///
    /// The unification only checks that every key of the first operand exists
    /// in the second one, so `{a: uint}` and `{a: uint, b: uint}` unify to
    /// `{a: uint}` while the second operand still produces a runtime value with
    /// the extra key. See stx-labs/clarity-wasm#858.
    TupleSupertypeKeyMismatch {
        a: TypeSignature,
        b: TypeSignature,
        result: TypeSignature,
    },
    /// The result type computed for a `fold` does not account for the type of
    /// the initial value, which is what the fold returns on an empty sequence.
    ///
    /// `unified` is what the result type would be if the initial value were
    /// taken into account, or `None` if the two cannot be unified at all.
    FoldInitialTypeMismatch {
        initial: TypeSignature,
        result: TypeSignature,
        unified: Option<TypeSignature>,
        span: Span,
    },
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static EVENTS: Mutex<Vec<TypeAuditEvent>> = Mutex::new(Vec::new());

/// Start recording events. Events are kept until [`drain`] is called.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Stop recording events and discard the ones not drained yet.
pub fn disable() {
    ENABLED.store(false, Ordering::Relaxed);
    drain();
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Take every event recorded since the last call.
pub fn drain() -> Vec<TypeAuditEvent> {
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *events)
}

/// Record an event. The closure is only evaluated when recording is enabled.
pub fn record(event: impl FnOnce() -> TypeAuditEvent) {
    if !is_enabled() {
        return;
    }
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    events.push(event());
}

/// Called by the `fold` type-checkers once the result type is computed.
///
/// A fold over an empty sequence returns its initial value, so the result type
/// must admit it. Records an event when unifying the two would change the
/// result type, or fail.
pub fn check_fold_result(
    epoch: &StacksEpochId,
    initial: &TypeSignature,
    result: &TypeSignature,
    span: &Span,
) {
    if !is_enabled() {
        return;
    }
    let unified = TypeSignature::least_supertype(epoch, result, initial).ok();
    if unified.as_ref() == Some(result) {
        return;
    }
    record(|| TypeAuditEvent::FoldInitialTypeMismatch {
        initial: initial.clone(),
        result: result.clone(),
        unified,
        span: span.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClarityName;
    use crate::types::TupleTypeSignature;

    fn tuple(fields: &[(&str, TypeSignature)]) -> TypeSignature {
        TupleTypeSignature::try_from(
            fields
                .iter()
                .map(|(name, ty)| (ClarityName::try_from(name.to_string()).unwrap(), ty.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .into()
    }

    // The sink is global, so a single test exercises every hook: the events it
    // looks for are specific enough not to be confused with another test's.
    #[test]
    fn hooks_record_the_two_known_behaviors() {
        let narrow = tuple(&[("id", TypeSignature::UIntType)]);
        let wide = tuple(&[
            ("id", TypeSignature::UIntType),
            ("extra", TypeSignature::UIntType),
        ]);

        // Nothing is recorded while disabled.
        let _ = TypeSignature::least_supertype(&StacksEpochId::Epoch21, &narrow, &wide);
        assert!(drain().is_empty());

        enable();
        for epoch in [StacksEpochId::Epoch20, StacksEpochId::Epoch21] {
            // Narrow first: accepted, and recorded.
            let result = TypeSignature::least_supertype(&epoch, &narrow, &wide).unwrap();
            assert_eq!(result, narrow);
            let expected = TypeAuditEvent::TupleSupertypeKeyMismatch {
                a: narrow.clone(),
                b: wide.clone(),
                result: narrow.clone(),
            };
            assert!(drain().contains(&expected), "{epoch:?}");

            // Wide first: rejected, so nothing to record.
            assert!(TypeSignature::least_supertype(&epoch, &wide, &narrow).is_err());
            assert!(
                !drain()
                    .iter()
                    .any(|e| matches!(e, TypeAuditEvent::TupleSupertypeKeyMismatch { .. })),
                "{epoch:?}"
            );

            // Same keys: nothing to record.
            TypeSignature::least_supertype(&epoch, &wide, &wide).unwrap();
            assert!(
                !drain()
                    .iter()
                    .any(|e| matches!(e, TypeAuditEvent::TupleSupertypeKeyMismatch { .. })),
                "{epoch:?}"
            );
        }

        // `(fold f l (err u2))` where `f` returns `(ok uint)`.
        let initial =
            TypeSignature::new_response(TypeSignature::NoType, TypeSignature::UIntType).unwrap();
        let result =
            TypeSignature::new_response(TypeSignature::UIntType, TypeSignature::NoType).unwrap();
        let unified =
            TypeSignature::new_response(TypeSignature::UIntType, TypeSignature::UIntType).unwrap();
        check_fold_result(&StacksEpochId::Epoch21, &initial, &result, &Span::ZERO);
        let expected = TypeAuditEvent::FoldInitialTypeMismatch {
            initial: initial.clone(),
            result: result.clone(),
            unified: Some(unified),
            span: Span::ZERO,
        };
        assert!(drain().contains(&expected));

        // Initial `none` folded into `(optional uint)`: already coherent.
        let initial = TypeSignature::new_option(TypeSignature::NoType).unwrap();
        let result = TypeSignature::new_option(TypeSignature::UIntType).unwrap();
        check_fold_result(&StacksEpochId::Epoch21, &initial, &result, &Span::ZERO);
        assert!(
            !drain()
                .iter()
                .any(|e| matches!(e, TypeAuditEvent::FoldInitialTypeMismatch { .. }))
        );

        // Incompatible initial value: recorded without a unified type.
        let result = TypeSignature::list_of(TypeSignature::UIntType, 5).unwrap();
        check_fold_result(
            &StacksEpochId::Epoch21,
            &TypeSignature::BoolType,
            &result,
            &Span::ZERO,
        );
        let expected = TypeAuditEvent::FoldInitialTypeMismatch {
            initial: TypeSignature::BoolType,
            result,
            unified: None,
            span: Span::ZERO,
        };
        assert!(drain().contains(&expected));

        disable();
    }
}
