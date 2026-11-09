use std::any::Any;
use std::ffi::{CStr, c_void};
use std::fmt;
use std::sync::Arc;

use crate::ffi;

/// A pointer passed to SQLite with a type name.
#[derive(Clone)]
pub struct SqlPointer {
    value: PointerValue,
    name: &'static CStr,
}

#[derive(Clone)]
enum PointerValue {
    Shared {
        value: Arc<dyn Any + Send + Sync>,
        release: unsafe extern "C" fn(*mut c_void),
    },
    Raw(*mut c_void),
}

impl SqlPointer {
    pub(crate) fn from_arc<T: Send + Sync + 'static>(value: Arc<T>, name: &'static CStr) -> Self {
        unsafe extern "C" fn release<T>(pointer: *mut c_void) {
            // This is the strong reference transferred by `into_sqlite`.
            unsafe { drop(Arc::from_raw(pointer.cast::<T>())) };
        }

        Self {
            value: PointerValue::Shared {
                value,
                release: release::<T>,
            },
            name,
        }
    }

    pub(crate) fn from_raw(pointer: *mut c_void, name: &'static CStr) -> Self {
        Self {
            value: PointerValue::Raw(pointer),
            name,
        }
    }

    fn as_ptr(&self) -> *const c_void {
        match &self.value {
            PointerValue::Shared { value, .. } => Arc::as_ptr(value) as *const () as *const c_void,
            PointerValue::Raw(pointer) => *pointer,
        }
    }

    pub(crate) fn into_sqlite(self) -> (*mut c_void, &'static CStr, ffi::sqlite3_destructor_type) {
        match self.value {
            PointerValue::Shared { value, release } => {
                let pointer = Arc::into_raw(value) as *const () as *mut c_void;
                (pointer, self.name, Some(release))
            }
            PointerValue::Raw(pointer) => (pointer, self.name, None),
        }
    }
}

impl fmt::Debug for SqlPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlPointer")
            .field("pointer", &self.as_ptr())
            .field("name", &self.name)
            .finish()
    }
}

impl PartialEq for SqlPointer {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.as_ptr() == other.as_ptr()
    }
}
