use crate::delphi::DynArraySetLength;
use std::{
    ops::{Deref, DerefMut},
    ptr, slice,
};

#[repr(transparent)]
pub struct DelphiList<T, const P: usize>(pub *mut T);

impl<T, const P: usize> Default for DelphiList<T, P> {
    fn default() -> Self {
        Self(ptr::null_mut())
    }
}

impl<T, const P: usize> Deref for DelphiList<T, P> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        if self.0.is_null() {
            &[]
        } else {
            unsafe { slice::from_raw_parts(self.0, self.0.cast::<usize>().sub(1).read()) }
        }
    }
}

impl<T, const P: usize> DerefMut for DelphiList<T, P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if self.0.is_null() {
            &mut []
        } else {
            unsafe { slice::from_raw_parts_mut(self.0, self.0.cast::<usize>().sub(1).read()) }
        }
    }
}

impl<'a, T, const P: usize> IntoIterator for &'a DelphiList<T, P> {
    type IntoIter = slice::Iter<'a, T>;
    type Item = &'a T;

    fn into_iter(self) -> Self::IntoIter {
        self.deref().into_iter()
    }
}

impl<T, const P: usize> DelphiList<T, P> {
    pub fn alloc(&mut self, len: usize) {
        unsafe {
            DynArraySetLength(&mut self.0, P as _, 1, len);
        }
    }
}

unsafe impl<T, const P: usize> Sync for DelphiList<T, P> {}
