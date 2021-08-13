/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Utilities for tracing JS-managed values.
//!
//! The lifetime of DOM objects is managed by the SpiderMonkey Garbage
//! Collector. A rooted DOM object implementing the interface `Foo` is traced
//! as follows:
//!
//! 1. The GC calls `_trace` defined in `FooBinding` during the marking
//!    phase. (This happens through `JSClass.trace` for non-proxy bindings, and
//!    through `ProxyTraps.trace` otherwise.)
//! 2. `_trace` calls `Foo::trace()` (an implementation of `JSTraceable`).
//!    This is typically derived via a `#[dom_struct]`
//!    (implies `#[derive(JSTraceable)]`) annotation.
//!    Non-JS-managed types have an empty inline `trace()` method,
//!    achieved via `unsafe_no_jsmanaged_fields!` or similar.
//! 3. For all fields, `Foo::trace()`
//!    calls `trace()` on the field.
//!    For example, for fields of type `Dom<T>`, `Dom<T>::trace()` calls
//!    `trace_reflector()`.
//! 4. `trace_reflector()` calls `Dom::TraceEdge()` with a
//!    pointer to the `JSObject` for the reflector. This notifies the GC, which
//!    will add the object to the graph, and will trace that object as well.
//! 5. When the GC finishes tracing, it [`finalizes`](../index.html#destruction)
//!    any reflectors that were not reachable.
//!
//! The `unsafe_no_jsmanaged_fields!()` macro adds an empty implementation of
//! `JSTraceable` to a datatype.

use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::{DomObject, Reflector};
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::{DOMString, USVString};
use crate::dom::bindings::utils::WindowProxyHandler;
use crate::dom::gpubuffer::GPUBufferState;
use crate::dom::gpucanvascontext::WebGPUContextId;
use crate::dom::gpucommandencoder::GPUCommandEncoderState;
use crate::dom::htmlimageelement::SourceSet;
use crate::dom::htmlmediaelement::{HTMLMediaElementFetchContext, MediaFrameRenderer};
use crate::dom::identityhub::Identities;
use crate::script_runtime::{ContextForRequestInterrupt, StreamConsumer};
use crate::script_thread::IncompleteParserContexts;
use crate::task::TaskBox;
use app_units::Au;
use canvas_traits::canvas::{
    CanvasGradientStop, CanvasId, LinearGradientStyle, RadialGradientStyle,
};
use canvas_traits::canvas::{
    CompositionOrBlending, Direction, LineCapStyle, LineJoinStyle, RepetitionStyle, TextAlign,
    TextBaseline,
};
use canvas_traits::webgl::WebGLVertexArrayId;
use canvas_traits::webgl::{
    ActiveAttribInfo, ActiveUniformBlockInfo, ActiveUniformInfo, GlType, TexDataType, TexFormat,
};
use canvas_traits::webgl::{GLLimits, WebGLQueryId, WebGLSamplerId};
use canvas_traits::webgl::{WebGLBufferId, WebGLChan, WebGLContextId, WebGLError};
use canvas_traits::webgl::{WebGLFramebufferId, WebGLMsgSender, WebGLPipeline, WebGLProgramId};
use canvas_traits::webgl::{WebGLReceiver, WebGLRenderbufferId, WebGLSLVersion, WebGLSender};
use canvas_traits::webgl::{WebGLShaderId, WebGLSyncId, WebGLTextureId, WebGLVersion};
use content_security_policy::CspList;
use crossbeam_channel::{Receiver, Sender};
use cssparser::RGBA;
use devtools_traits::{CSSError, TimelineMarkerType, WorkerId};
use embedder_traits::{EventLoopWaker, MediaMetadata};
use encoding_rs::{Decoder, Encoding};
use euclid::default::{Point2D, Rect, Rotation3D, Transform2D};
use euclid::Length as EuclidLength;
use html5ever::buffer_queue::BufferQueue;
use html5ever::{LocalName, Namespace, Prefix, QualName};
use http::header::HeaderMap;
use hyper::Method;
use hyper::StatusCode;
use indexmap::IndexMap;
use ipc_channel::ipc::{IpcReceiver, IpcSender};
use js::glue::{CallObjectTracer, CallScriptTracer, CallStringTracer, CallValueTracer};
use js::jsapi::{
    GCTraceKindToAscii, Heap, JSObject, JSScript, JSString, JSTracer, JobQueue, TraceKind,
};
use js::jsval::JSVal;
use js::rust::{GCMethods, Handle, Runtime};
use js::typedarray::TypedArray;
use js::typedarray::TypedArrayElement;
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use media::WindowGLContext;
use metrics::{InteractiveMetrics, InteractiveWindow};
use mime::Mime;
use msg::constellation_msg::{
    BlobId, BroadcastChannelRouterId, BrowsingContextId, HistoryStateId, MessagePortId,
    MessagePortRouterId, PipelineId, TopLevelBrowsingContextId,
};
use msg::constellation_msg::{ServiceWorkerId, ServiceWorkerRegistrationId};
use net_traits::filemanager_thread::RelativePos;
use net_traits::image::base::{Image, ImageMetadata};
use net_traits::image_cache::{ImageCache, PendingImageId};
use net_traits::request::{CredentialsMode, ParserMetadata, Referrer, Request, RequestBuilder};
use net_traits::response::HttpsState;
use net_traits::response::{Response, ResponseBody};
use net_traits::storage_thread::StorageType;
use net_traits::{Metadata, NetworkError, ReferrerPolicy, ResourceFetchTiming, ResourceThreads};
use parking_lot::{Mutex as ParkMutex, RwLock};
use profile_traits::mem::ProfilerChan as MemProfilerChan;
use profile_traits::time::ProfilerChan as TimeProfilerChan;
use script_layout_interface::message::PendingRestyle;
use script_layout_interface::rpc::LayoutRPC;
use script_layout_interface::StyleAndOpaqueLayoutData;
use script_traits::serializable::BlobImpl;
use script_traits::transferable::MessagePortImpl;
use script_traits::{
    DocumentActivity, DrawAPaintImageResult, FocusSequenceNumber, MediaSessionActionType,
    ScriptToConstellationChan, TimerEventId, TimerSource, UntrustedNodeAddress, WebrenderIpcSender,
    WindowSizeData, WindowSizeType,
};
use selectors::matching::ElementSelectorFlags;
use serde::{Deserialize, Serialize};
use servo_arc::Arc as ServoArc;
use servo_atoms::Atom;
use servo_media::audio::analyser_node::AnalysisEngine;
use servo_media::audio::buffer_source_node::AudioBuffer;
use servo_media::audio::context::AudioContext;
use servo_media::audio::graph::NodeId;
use servo_media::audio::panner_node::{DistanceModel, PanningModel};
use servo_media::audio::param::ParamType;
use servo_media::player::audio::AudioRenderer;
use servo_media::player::video::VideoFrame;
use servo_media::player::Player;
use servo_media::streams::registry::MediaStreamId;
use servo_media::streams::MediaStreamType;
use servo_media::webrtc::WebRtcController;
use servo_url::{ImmutableOrigin, MutableOrigin, ServoUrl};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hash};

/// A trait to allow tracing (only) DOM objects.
pub(crate) use js::gc::Traceable as JSTraceable;
use js::glue::{CallScriptTracer, CallStringTracer, CallValueTracer};
use js::jsapi::{GCTraceKindToAscii, Heap, JSScript, JSString, JSTracer, TraceKind};
use js::jsval::JSVal;
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
pub(crate) use script_bindings::trace::*;

use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomObject;
use crate::dom::htmlimageelement::SourceSet;
use crate::dom::htmlmediaelement::HTMLMediaElementFetchContext;
use crate::dom::windowproxy::WindowProxyHandler;
use crate::script_runtime::StreamConsumer;
use crate::script_thread::IncompleteParserContexts;
use crate::task::TaskBox;

unsafe impl<T: CustomTraceable> CustomTraceable for DomRefCell<T> {
    unsafe fn trace(&self, trc: *mut JSTracer) {
        (*self).borrow().trace(trc)
    }
}

/// Wrapper type for nop traceble
///
/// SAFETY: Inner type must not impl JSTraceable
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(crown, crown::trace_in_no_trace_lint::must_not_have_traceable)]
pub(crate) struct NoTrace<T>(pub(crate) T);

impl<T: Display> Display for NoTrace<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> From<T> for NoTrace<T> {
    fn from(item: T) -> Self {
        Self(item)
    }
}

#[allow(unsafe_code)]
unsafe impl<T> JSTraceable for NoTrace<T> {
    #[inline]
    unsafe fn trace(&self, _: *mut ::js::jsapi::JSTracer) {}
}

impl<T: MallocSizeOf> MallocSizeOf for NoTrace<T> {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops)
    }
}

/// HashMap wrapper, that has non-jsmanaged keys
///
/// Not all methods are reexposed, but you can access inner type via .0
#[cfg_attr(crown, crown::trace_in_no_trace_lint::must_not_have_traceable(0))]
#[derive(Clone, Debug)]
pub(crate) struct HashMapTracedValues<K, V, S = RandomState>(pub(crate) HashMap<K, V, S>);

impl<K, V, S: Default> Default for HashMapTracedValues<K, V, S> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<K, V> HashMapTracedValues<K, V, RandomState> {
    /// Wrapper for HashMap::new()
    #[inline]
    #[must_use]
    pub(crate) fn new() -> HashMapTracedValues<K, V, RandomState> {
        Self(HashMap::new())
    }
}

impl<K, V, S> HashMapTracedValues<K, V, S> {
    #[inline]
    pub(crate) fn iter(&self) -> std::collections::hash_map::Iter<'_, K, V> {
        self.0.iter()
    }

    #[inline]
    pub(crate) fn drain(&mut self) -> std::collections::hash_map::Drain<'_, K, V> {
        self.0.drain()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K, V, S> HashMapTracedValues<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    #[inline]
    pub(crate) fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.0.insert(k, v)
    }

    #[inline]
    pub(crate) fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.0.get(k)
    }

    #[inline]
    pub(crate) fn get_mut<Q: Hash + Eq + ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.0.get_mut(k)
    }

    #[inline]
    pub(crate) fn contains_key<Q: Hash + Eq + ?Sized>(&self, k: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
    {
        self.0.contains_key(k)
    }

    #[inline]
    pub(crate) fn remove<Q: Hash + Eq + ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.0.remove(k)
    }

    #[inline]
    pub(crate) fn entry(&mut self, key: K) -> std::collections::hash_map::Entry<'_, K, V> {
        self.0.entry(key)
    }
}

impl<K, V, S> MallocSizeOf for HashMapTracedValues<K, V, S>
where
    K: Eq + Hash + MallocSizeOf,
    V: MallocSizeOf,
    S: BuildHasher,
{
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.0.size_of(ops)
    }
}

#[allow(unsafe_code)]
unsafe impl<K, V: JSTraceable, S> JSTraceable for HashMapTracedValues<K, V, S> {
    #[inline]
    unsafe fn trace(&self, trc: *mut ::js::jsapi::JSTracer) {
        for v in self.0.values() {
            v.trace(trc);
        }
    }
}

unsafe_no_jsmanaged_fields!(Box<dyn TaskBox>);

unsafe_no_jsmanaged_fields!(IncompleteParserContexts);

#[allow(dead_code)]
/// Trace a `JSScript`.
pub(crate) fn trace_script(tracer: *mut JSTracer, description: &str, script: &Heap<*mut JSScript>) {
    unsafe {
        trace!("tracing {}", description);
        CallScriptTracer(
            tracer,
            script.ptr.get() as *mut _,
            GCTraceKindToAscii(TraceKind::Script),
        );
    }
}

#[allow(dead_code)]
/// Trace a `JSVal`.
pub(crate) fn trace_jsval(tracer: *mut JSTracer, description: &str, val: &Heap<JSVal>) {
    unsafe {
        if !val.get().is_markable() {
            return;
        }

        trace!("tracing value {}", description);
        CallValueTracer(
            tracer,
            val.ptr.get() as *mut _,
            GCTraceKindToAscii(val.get().trace_kind()),
        );
    }
}

#[allow(dead_code)]
/// Trace a `JSString`.
pub(crate) fn trace_string(tracer: *mut JSTracer, description: &str, s: &Heap<*mut JSString>) {
    unsafe {
        trace!("tracing {}", description);
        CallStringTracer(
            tracer,
            s.ptr.get() as *mut _,
            GCTraceKindToAscii(TraceKind::String),
        );
    }
}

unsafe impl<T: JSTraceable> JSTraceable for DomRefCell<T> {
    unsafe fn trace(&self, trc: *mut JSTracer) {
        (*self).borrow().trace(trc)
    }
}

unsafe_no_jsmanaged_fields!(TrustedPromise);
unsafe_no_jsmanaged_fields!(PropertyDeclarationBlock);
unsafe_no_jsmanaged_fields!(Font);
// These three are interdependent, if you plan to put jsmanaged data
// in one of these make sure it is propagated properly to containing structs
unsafe_no_jsmanaged_fields!(DocumentActivity, WindowSizeData, WindowSizeType);
unsafe_no_jsmanaged_fields!(
    BrowsingContextId,
    HistoryStateId,
    PipelineId,
    TopLevelBrowsingContextId
);
unsafe_no_jsmanaged_fields!(TimerEventId, TimerSource);
unsafe_no_jsmanaged_fields!(TimelineMarkerType);
unsafe_no_jsmanaged_fields!(WorkerId);
unsafe_no_jsmanaged_fields!(FocusSequenceNumber);
unsafe_no_jsmanaged_fields!(BufferQueue, QuirksMode, StrTendril);
unsafe_no_jsmanaged_fields!(Runtime);
unsafe_no_jsmanaged_fields!(ContextForRequestInterrupt);
unsafe_no_jsmanaged_fields!(HeaderMap, Method);
unsafe_no_jsmanaged_fields!(WindowProxyHandler);
unsafe_no_jsmanaged_fields!(SourceSet);
unsafe_no_jsmanaged_fields!(HTMLMediaElementFetchContext);
unsafe_no_jsmanaged_fields!(StreamConsumer);

unsafe impl<T: DomObject> JSTraceable for Trusted<T> {
    #[inline]
    unsafe fn trace(&self, _: *mut JSTracer) {
        // Do nothing
    }
}
