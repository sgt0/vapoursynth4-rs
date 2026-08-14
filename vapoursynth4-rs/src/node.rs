/*
 This Source Code Form is subject to the terms of the Mozilla Public
 License, v. 2.0. If a copy of the MPL was not distributed with this
 file, You can obtain one at http://mozilla.org/MPL/2.0/.
*/

mod dependency;
mod filter;
pub(crate) mod internal;

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use crate::{
    AudioInfo, VideoInfo,
    api::Api,
    core::Core,
    ffi,
    frame::{AudioFrame, Frame, FrameContext, VideoFrame, internal::FrameFromPtr},
    node::internal::FilterExtern,
};

pub use dependency::*;
pub use filter::*;

pub trait Node: Sized + Send + Sync + crate::_private::Sealed {
    type FrameType: Frame;

    fn api(&self) -> Api;

    #[must_use]
    fn as_ptr(&self) -> *mut ffi::VSNode;

    #[must_use]
    fn get_frame_filter(&self, n: i32, ctx: &mut FrameContext) -> Self::FrameType;

    fn set_linear_filter(&mut self) -> i32 {
        unsafe { (self.api().setLinearFilter)(self.as_ptr()) }
    }

    fn set_cache_mode(&mut self, mode: CacheMode) {
        unsafe {
            (self.api().setCacheMode)(self.as_ptr(), mode);
        }
    }

    fn set_cache_options(&mut self, fixed_size: i32, max_size: i32, max_history_size: i32) {
        unsafe {
            (self.api().setCacheOptions)(self.as_ptr(), fixed_size, max_size, max_history_size);
        }
    }

    /// # Errors
    ///
    /// Return the internal error message if the frame is not ready.
    fn get_frame(&self, n: i32) -> Result<Self::FrameType, CString> {
        let mut buf = vec![0; 1024];
        let ptr = unsafe { (self.api().getFrame)(n, self.as_ptr(), buf.as_mut_ptr(), 1024) };

        if ptr.is_null() {
            let mut buf = std::mem::ManuallyDrop::new(buf);
            Err(unsafe { CStr::from_ptr(buf.as_mut_ptr()).into() })
        } else {
            unsafe { Ok(Self::FrameType::from_ptr(ptr, self.api())) }
        }
    }

    /// Requests the asynchronous generation of a frame.
    ///
    /// Returns a [Future] that resolves to the requested frame, or the error
    /// message that was thrown when rendering the frame.
    fn get_frame_async(
        &self,
        n: i32,
    ) -> impl Future<Output = Result<Self::FrameType, CString>> + Send {
        let (sender, receiver) = async_channel::bounded(1);
        let payload = Box::new(FrameDonePayload {
            sender,
            api: self.api(),
        });
        unsafe {
            (self.api().getFrameAsync)(
                n,
                self.as_ptr(),
                frame_done_callback::<Self::FrameType>,
                Box::into_raw(payload).cast(),
            );
        }

        async move {
            receiver
                .recv()
                .await
                .unwrap_or_else(|_| Err(c"async frame request was cancelled".into()))
        }
    }
}

/// Contains what [`frame_done_callback`] needs.
struct FrameDonePayload<F: Frame> {
    sender: async_channel::Sender<Result<F, CString>>,
    api: Api,
}

/// Callback invoked by the core when an asynchronously requested frame is
/// ready.
///
/// # Safety
///
/// `user_data` must own a `Box<FrameDonePayload<F>>`, and this must run at most
/// once for it. `F` must be the frame type produced by the node the request was
/// made on.
unsafe extern "system-unwind" fn frame_done_callback<F: Frame>(
    user_data: *mut c_void,
    f: *const ffi::VSFrame,
    _n: c_int,
    _node: *mut ffi::VSNode,
    error_msg: *const c_char,
) {
    let payload = unsafe { Box::from_raw(user_data.cast::<FrameDonePayload<F>>()) };

    let result = if f.is_null() {
        let msg = if error_msg.is_null() {
            CString::default()
        } else {
            unsafe { CStr::from_ptr(error_msg).into() }
        };
        Err(msg)
    } else {
        Ok(unsafe { F::from_ptr(f, payload.api) })
    };

    // The receiver may already be gone if the future was dropped. In that case
    // the frame is freed as `result` is dropped here.
    let _ = payload.sender.try_send(result);
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct VideoNode {
    handle: *const ffi::VSNode,
    api: Api,
}

impl crate::_private::Sealed for VideoNode {}
unsafe impl Send for VideoNode {}
unsafe impl Sync for VideoNode {}

impl Node for VideoNode {
    type FrameType = VideoFrame;

    #[inline]
    fn api(&self) -> Api {
        self.api
    }

    #[inline]
    fn as_ptr(&self) -> *mut ffi::VSNode {
        self.handle.cast_mut()
    }

    fn get_frame_filter(&self, n: i32, ctx: &mut FrameContext) -> Self::FrameType {
        unsafe {
            VideoFrame::from_ptr(
                (self.api.getFrameFilter)(n, self.as_ptr(), ctx.as_ptr()),
                self.api,
            )
        }
    }
}

impl VideoNode {
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a [`ffi::VSNode`] that represents a video node.
    #[must_use]
    pub unsafe fn from_ptr(ptr: *mut ffi::VSNode, api: Api) -> Self {
        Self { handle: ptr, api }
    }

    #[must_use]
    pub fn info(&self) -> &VideoInfo {
        // SAFETY: `vi` is valid if the node is a video node
        unsafe { &*(self.api.getVideoInfo)(self.as_ptr()) }
    }

    /// # Panics
    ///
    /// Panics if the the dependency index is larger than [`i32::MAX`].
    pub fn new<F: Filter>(
        name: &str,
        info: &VideoInfo,
        filter: F,
        dependencies: &[ffi::VSFilterDependency],
        core: impl AsRef<Core>,
    ) -> Option<Self> {
        let filter = Box::new(filter);
        let name = CString::new(name).ok()?;
        let core = core.as_ref();
        let ptr = unsafe {
            (core.api().createVideoFilter2)(
                name.as_ptr(),
                info,
                F::filter_get_frame,
                Some(F::filter_free),
                F::FILTER_MODE,
                dependencies.as_ptr(),
                dependencies.len().try_into().unwrap(),
                Box::into_raw(filter).cast(),
                core.as_ptr(),
            )
        };
        ptr.is_null()
            .then_some(unsafe { Self::from_ptr(ptr, core.api()) })
    }
}

impl Clone for VideoNode {
    fn clone(&self) -> Self {
        unsafe { Self::from_ptr((self.api.addNodeRef)(self.as_ptr()), self.api) }
    }
}

impl Drop for VideoNode {
    fn drop(&mut self) {
        unsafe { (self.api.freeNode)(self.as_ptr()) }
    }
}

#[derive(PartialEq, Eq, Hash, Debug)]
pub struct AudioNode {
    handle: *const ffi::VSNode,
    api: Api,
}

impl crate::_private::Sealed for AudioNode {}
unsafe impl Send for AudioNode {}
unsafe impl Sync for AudioNode {}

impl Node for AudioNode {
    type FrameType = AudioFrame;

    #[inline]
    fn api(&self) -> Api {
        self.api
    }

    #[inline]
    fn as_ptr(&self) -> *mut ffi::VSNode {
        self.handle.cast_mut()
    }

    fn get_frame_filter(&self, n: i32, ctx: &mut FrameContext) -> Self::FrameType {
        unsafe {
            AudioFrame::from_ptr(
                (self.api.getFrameFilter)(n, self.as_ptr(), ctx.as_ptr()),
                self.api,
            )
        }
    }
}

impl AudioNode {
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a [`ffi::VSNode`] that represents an audio node.
    #[must_use]
    pub unsafe fn from_ptr(ptr: *mut ffi::VSNode, api: Api) -> Self {
        Self { handle: ptr, api }
    }

    #[must_use]
    pub fn info(&self) -> &AudioInfo {
        // SAFETY: `ai` is valid if the node is an audio node
        unsafe { &*(self.api.getAudioInfo)(self.as_ptr()) }
    }

    /// # Panics
    ///
    /// Panics if the the dependency index is larger than [`i32::MAX`].
    pub fn new<F: Filter>(
        name: &str,
        info: &AudioInfo,
        filter: F,
        dependencies: &[ffi::VSFilterDependency],
        core: impl AsRef<Core>,
    ) -> Option<Self> {
        let filter = Box::new(filter);
        let name = CString::new(name).ok()?;
        let core = core.as_ref();
        let ptr = unsafe {
            (core.api().createAudioFilter2)(
                name.as_ptr(),
                info,
                F::filter_get_frame,
                Some(F::filter_free),
                F::FILTER_MODE,
                dependencies.as_ptr(),
                dependencies.len().try_into().unwrap(),
                Box::into_raw(filter).cast(),
                core.as_ptr(),
            )
        };
        ptr.is_null()
            .then_some(unsafe { Self::from_ptr(ptr, core.api()) })
    }
}

impl Clone for AudioNode {
    fn clone(&self) -> Self {
        unsafe { Self::from_ptr((self.api.addNodeRef)(self.as_ptr()), self.api) }
    }
}

impl Drop for AudioNode {
    fn drop(&mut self) {
        unsafe { (self.api.freeNode)(self.as_ptr()) }
    }
}

pub type FilterMode = ffi::VSFilterMode;
pub type CacheMode = ffi::VSCacheMode;
