use std::mem::MaybeUninit;
use bytemuck::{Pod, Zeroable};
use crate::thin_addr::SocketAddr;
use crate::integers::NetworkOrderU16;

#[derive(Copy, Clone, Pod, Zeroable)]
#[repr(C, packed)]
pub(crate) struct MuxFrameHeader {
    socket: SocketAddr,
    len: NetworkOrderU16
}

impl MuxFrameHeader {
    pub(crate) const fn empty(socket: SocketAddr) -> Self {
        Self {
            socket,
            len: NetworkOrderU16::new(0)
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.socket
    }

    pub fn len(&self) -> u16 {
        self.len.get()
    }

    pub fn uninit_bytes(this: &mut MaybeUninit<Self>) -> &mut [MaybeUninit<u8>; size_of::<Self>()] {
        unsafe { &mut *(this.as_mut_ptr() as *mut [MaybeUninit<u8>; size_of::<Self>()]) }
    }
}