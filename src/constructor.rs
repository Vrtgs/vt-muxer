/// # Safety
/// 
/// must implement `Construct::init` properly
pub(super) unsafe trait Construct {
    type Data;

    /// `this` parameter has no initialization guarantees
    /// After method call the object pointed to by the `this` pointer
    /// is assumed to be completely initialized
    unsafe fn init(this: *mut Self, data: Self::Data);
}

pub(super) trait ConstructExt: Construct + Sized { 
    fn box_new(data: Self::Data) -> Box<Self>;
}

impl<T: Construct> ConstructExt for T { 
    #[inline]
    fn box_new(data: Self::Data) -> Box<Self> {
        let mut this = Box::new_uninit();
        unsafe {
            T::init(this.as_mut_ptr(), data);
            this.assume_init()
        }
    }
}