use std::cmp::Ordering;
use std::fmt::{Debug, Display, Formatter};
use bytemuck::{Pod, Zeroable};

#[derive(Copy, Clone, Pod, Zeroable, Eq, PartialEq)]
#[repr(transparent)]
pub struct NetworkOrderU16(pub [u8; 2]);


impl Display for NetworkOrderU16 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.get(), f)
    }
}

impl Debug for NetworkOrderU16 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkOrderU16")
            .field("bytes", &self.0)
            .field("value", &self.get())
            .finish()
    }
}

impl NetworkOrderU16 {
    #[inline(always)]
    pub const fn new(x: u16) -> Self {
        Self(x.to_be_bytes())
    }

    #[inline(always)]
    pub const fn get(self) -> u16 {
        u16::from_be_bytes(self.0)
    }
}

impl PartialOrd for NetworkOrderU16 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Self::cmp(other, other))
    }
}

impl Ord for NetworkOrderU16 {
    fn cmp(&self, other: &Self) -> Ordering {
        Ord::cmp(&self.get(), &other.get())
    }
}

impl PartialEq<u16> for NetworkOrderU16 {
    fn eq(&self, &other: &u16) -> bool {
        self.get() == other
    }

    fn ne(&self, &other: &u16) -> bool {
        self.get() != other
    }
}

impl PartialOrd<u16> for NetworkOrderU16 {
    fn partial_cmp(&self, other: &u16) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.get(), other)
    }

    fn lt(&self, &other: &u16) -> bool {
        self.get() < other
    }


    fn le(&self, &other: &u16) -> bool {
        self.get() <= other
    }

    fn gt(&self, &other: &u16) -> bool {
        self.get() > other
    }


    fn ge(&self, &other: &u16) -> bool {
        self.get() >= other
    }
}