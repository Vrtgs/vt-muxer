use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::net::Ipv6Addr;
use std::str::FromStr;
use bytemuck::{Pod, Zeroable};
use crate::integers::NetworkOrderU16;

#[derive(Copy, Clone, Pod, Zeroable, Ord, PartialOrd, Eq, PartialEq)]
#[repr(C, packed)]
pub struct IpAddr {
    octets: [u8; 16]
}

impl IpAddr {
    pub const fn to_std(self) -> std::net::IpAddr {
        Ipv6Addr::from_bits(u128::from_be_bytes(self.octets)).to_canonical()
    }

    pub const fn from_std(ip: std::net::IpAddr) -> Self {
        let v6 = match ip {
            std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped(),
            std::net::IpAddr::V6(v6) => v6,
        };

        Self { octets: v6.octets() }
    }
}


#[derive(Copy, Clone, Pod, Zeroable, Ord, PartialOrd, Eq, PartialEq)]
#[repr(C, packed)]
pub struct SocketAddr {
    ip: IpAddr,
    port: NetworkOrderU16
}

impl SocketAddr {
    pub const fn to_std(self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(
            self.ip.to_std(),
            self.port.get()
        )
    }

    pub const fn from_std(sock: std::net::SocketAddr) -> Self {
        Self {
            ip: IpAddr::from_std(sock.ip()),
            port: NetworkOrderU16::new(sock.port())
        }
    }

    pub const fn ip(&self) -> IpAddr {
        self.ip
    }

    pub const fn port(&self) -> u16 {
        self.port.get()
    }
}

macro_rules! impl_display_debug {
    ($($type:ty),+) => {$(
        impl Display for $type {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                Display::fmt(&self.to_std(), f)
            }
        }
        
        impl Debug for $type {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                Debug::fmt(&self.to_std(), f)
            }
        }
    )+};
}

impl_display_debug!(IpAddr, SocketAddr);

macro_rules! impl_byte_hash {
    ($($type:ty),+) => {$(
        impl Hash for $type {
            fn hash<H: Hasher>(&self, state: &mut H) {
                state.write(bytemuck::bytes_of(self))
            }
        
            fn hash_slice<H: Hasher>(data: &[Self], state: &mut H)
            where
                Self: Sized,
            {
                state.write(bytemuck::cast_slice(data))
            }
        }
    )+};
}

impl_byte_hash!(IpAddr, SocketAddr);

macro_rules! impl_from_str {
    ($($ty:ident),+) => {$(
        impl FromStr for $ty {
            type Err = <std::net::$ty as FromStr>::Err;
            
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.parse().map(Self::from_std)
            }
        }
    )*};
}

impl_from_str!(IpAddr, SocketAddr);