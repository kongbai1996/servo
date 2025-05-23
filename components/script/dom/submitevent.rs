/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::rust::HandleObject;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::codegen::Bindings::SubmitEventBinding;
use crate::dom::bindings::codegen::Bindings::SubmitEventBinding::SubmitEventMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::reflect_dom_object_with_proto;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::Event;
use crate::dom::htmlelement::HTMLElement;
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

#[dom_struct]
#[allow(non_snake_case)]
pub(crate) struct SubmitEvent {
    event: Event,
    submitter: Option<DomRoot<HTMLElement>>,
}

impl SubmitEvent {
    fn new_inherited(submitter: Option<DomRoot<HTMLElement>>) -> SubmitEvent {
        SubmitEvent {
            event: Event::new_inherited(),
            submitter,
        }
    }

    pub(crate) fn new(
        window: &Window,
        type_: Atom,
        bubbles: bool,
        cancelable: bool,
        submitter: Option<DomRoot<HTMLElement>>,
        can_gc: CanGc,
    ) -> DomRoot<SubmitEvent> {
        Self::new_with_proto(window, None, type_, bubbles, cancelable, submitter, can_gc)
    }

    fn new_with_proto(
        window: &Window,
        proto: Option<HandleObject>,
        type_: Atom,
        bubbles: bool,
        cancelable: bool,
        submitter: Option<DomRoot<HTMLElement>>,
        can_gc: CanGc,
    ) -> DomRoot<SubmitEvent> {
        let ev = reflect_dom_object_with_proto(
            Box::new(SubmitEvent::new_inherited(submitter)),
            window,
            proto,
            can_gc,
        );
        {
            let event = ev.upcast::<Event>();
            event.init_event(type_, bubbles, cancelable);
        }
        ev
    }
}

impl SubmitEventMethods<crate::DomTypeHolder> for SubmitEvent {
    /// <https://html.spec.whatwg.org/multipage/#submitevent>
    fn Constructor(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
        type_: DOMString,
        init: &SubmitEventBinding::SubmitEventInit,
    ) -> DomRoot<SubmitEvent> {
        SubmitEvent::new_with_proto(
            window,
            proto,
            Atom::from(type_),
            init.parent.bubbles,
            init.parent.cancelable,
            init.submitter.as_ref().map(|s| DomRoot::from_ref(&**s)),
            can_gc,
        )
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.event.IsTrusted()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-submitevent-submitter>
    fn GetSubmitter(&self) -> Option<DomRoot<HTMLElement>> {
        self.submitter.as_ref().map(|s| DomRoot::from_ref(&**s))
    }
}
