//! SDK-owned C ABI trampolines for string inputs, output slots and deallocation.
//!
//! Raw validity and allocation ownership remain the ABI caller's contract.
//! Plugin bodies use Rust strings and an exclusive, lifetime-bound output
//! capability instead of manipulating pointers themselves.
use crate::{StbString, StepResult, StepResultTag};

pub struct Output<'a> {
    slot: Option<&'a mut StbString>,
    written: bool,
}

impl<'a> Output<'a> {
    /// # Safety
    /// A non-null `slot` must be aligned, writable and exclusively borrowed
    /// for `'a`. The host reclaims a written string through this plugin's
    /// `free_string` callback after the handler returns.
    pub unsafe fn from_raw(slot: *mut StbString) -> Self {
        Self {
            slot: unsafe { slot.as_mut() },
            written: false,
        }
    }

    pub fn is_available(&self) -> bool {
        self.slot.is_some()
    }

    /// Allocate only when a slot exists. Repeated writes reclaim only outputs
    /// this view itself produced, never a value initially supplied by the host.
    pub fn write(&mut self, text: String) -> bool {
        self.replace(Some(text))
    }

    pub fn clear(&mut self) -> bool {
        self.replace(None)
    }

    fn replace(&mut self, text: Option<String>) -> bool {
        let Some(slot) = self.slot.as_deref_mut() else {
            return false;
        };
        if self.written {
            free_string(*slot);
        }
        *slot = text
            .map(StbString::from_string)
            .unwrap_or_else(StbString::empty);
        self.written = true;
        true
    }
}

/// ABI deallocation callback for strings produced by this SDK instance.
///
/// Install this as `FreeStringFn` in the plugin's registrars. The ABI caller
/// must pass ownership of a live `StbString::from_*` boxed-slice allocation
/// exactly once. Never use it for a borrowed input or another producer's string.
pub extern "C" fn free_string(value: StbString) {
    if value.is_empty() || value.ptr.is_null() {
        return;
    }
    // SAFETY: the ABI caller transfers the original boxed-slice allocation
    // exactly once; construction and this callback use the same allocator.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            value.ptr as *mut u8,
            value.len,
        )));
    }
}

/// ABI cleanup callback for a step result produced by this SDK instance.
/// Its live union arm and string allocation must follow the `StepResult` ABI.
pub extern "C" fn free_step_result(result: StepResult) {
    // SAFETY: the producer initializes the union arm indicated by its tag.
    let text = unsafe {
        match result.tag {
            StepResultTag::Pending => result.pending_payload().progress,
            StepResultTag::Done => result.done_payload().result,
            StepResultTag::Err => result.err_payload().message,
        }
    };
    free_string(text);
}

/// An owned copy of a plugin-produced step result, without ABI pointers.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Pending(String),
    Done(String),
    Err(String),
}

/// # Safety
/// The result must have a valid live union arm and transfer ownership of a
/// string produced by this SDK instance exactly once.
pub unsafe fn take_step_result(result: StepResult) -> Step {
    // SAFETY: the producer's tag identifies its initialized payload.
    let copy = unsafe {
        match result.tag {
            StepResultTag::Pending => {
                Step::Pending(result.pending_payload().progress.to_string_lossy())
            }
            StepResultTag::Done => Step::Done(result.done_payload().result.to_string_lossy()),
            StepResultTag::Err => Step::Err(result.err_payload().message.to_string_lossy()),
        }
    };
    free_step_result(result);
    copy
}

/// Copy and reclaim a result returned by a plugin's tool callback. Use only
/// with a live, owned result allocated by this SDK instance, not host memory.
#[macro_export]
macro_rules! take_step_result {
    ($result:expr) => {{
        // SAFETY: this is the result boundary of a plugin-owned tool callback.
        unsafe { $crate::callbacks::take_step_result($result) }
    }};
}

/// Define a command or prompt callback with a `&str` input and `&mut Output`.
/// The four-argument form also supports runtime-action trampolines. Both
/// forms copy the input inside the ABI boundary and contain handler panics.
#[macro_export]
macro_rules! string_handler {
    ($(#[$attr:meta])* $vis:vis fn $name:ident($input:ident, $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name(raw: $crate::StbStringRef, slot: *mut $crate::StbString,
            $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: the host keeps its input buffer and exclusive output
                // slot valid until this ABI invocation returns.
                let text = unsafe { raw.as_str() }.to_owned();
                let $input = text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
    ($(#[$attr:meta])* $vis:vis fn $name:ident($action:ident, $input:ident, $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name($action: u32, raw: $crate::StbStringRef, slot: *mut $crate::StbString,
            $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: as in the three-argument ABI trampoline above.
                let text = unsafe { raw.as_str() }.to_owned();
                let $input = text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
}

/// Define a tool execute callback whose call id is an ordinary borrowed str.
/// Owned parameters still follow the host's `free_params` allocation contract.
#[macro_export]
macro_rules! execute_handler {
    (fn $name:ident($id:ident, $params:ident, $free:ident) $body:block) => {
        extern "C" fn $name(
            raw: $crate::StbStringRef,
            $params: $crate::StbString,
            $free: Option<$crate::FreeStringFn>,
        ) -> $crate::StepHandle {
            $crate::guard_or(std::ptr::null_mut(), || {
                // SAFETY: host input is borrowed for this ABI call only.
                let text = unsafe { raw.as_str() }.to_owned();
                let $id = text.as_str();
                $body
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StbStringRef;

    string_handler! { fn echo(input, output, _data) {
        i32::from(!output.write(input.to_owned()))
    } }
    string_handler! { fn replace_twice(input, output, _data) {
        output.write(input.to_owned());
        output.write("second".into());
        0
    } }
    string_handler! { fn panicking(_input, output, _data) {
        output.write("valid output before panic".into());
        panic!("callback failed");
    } }
    string_handler! { fn action(action, input, output, _data) {
        output.write(format!("{action}:{input}"));
        0
    } }

    #[test]
    fn missing_output_is_reported_without_a_pointer_write() {
        assert_eq!(
            echo(
                StbStringRef::from_str("text"),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            1
        );
    }

    #[test]
    fn strings_are_copied_and_repeated_output_writes_are_reclaimed() {
        let mut output = StbString::empty();
        assert_eq!(
            echo(
                StbStringRef::from_str("你好"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(output.to_string_lossy(), "你好");
        free_string(output);
        output = StbString::empty();
        assert_eq!(
            replace_twice(
                StbStringRef::from_str("first"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(output.to_string_lossy(), "second");
        free_string(output);
    }

    #[test]
    fn panics_leave_a_valid_reclaimable_output() {
        let mut output = StbString::empty();
        assert_eq!(
            panicking(StbStringRef::empty(), &mut output, std::ptr::null_mut()),
            1
        );
        assert_eq!(output.to_string_lossy(), "valid output before panic");
        free_string(output);
    }

    #[test]
    fn runtime_action_signature_is_preserved() {
        let mut output = StbString::empty();
        assert_eq!(
            action(
                17,
                StbStringRef::from_str("{}"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(output.to_string_lossy(), "17:{}");
        free_string(output);
    }

    #[test]
    fn writing_does_not_reclaim_an_initial_host_owned_value() {
        let initial = StbString::from_string("host-owned".into());
        let mut output = initial;
        assert_eq!(
            echo(
                StbStringRef::from_str("plugin-owned"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(initial.to_string_lossy(), "host-owned");
        assert_eq!(output.to_string_lossy(), "plugin-owned");
        free_string(initial);
        free_string(output);
    }

    #[test]
    fn every_step_result_variant_can_be_reclaimed() {
        free_step_result(StepResult::pending(StbString::from_string(
            "progress".into(),
        )));
        free_step_result(StepResult::done(StbString::from_string("done".into())));
        free_step_result(StepResult::err(StbString::from_string("error".into())));
        free_string(StbString::empty());
        assert_eq!(
            take_step_result!(StepResult::err(StbString::from_string("cancelled".into()))),
            Step::Err("cancelled".into())
        );
    }
}
