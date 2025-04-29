use std::cmp::Ordering;
use std::mem::MaybeUninit;
use bytemuck::Zeroable;
use crate::thin_addr::SocketAddr;
use crate::integers::NetworkOrderU16;
use crate::constructor::Construct;

pub const PACKET_SIZE: u16 = u16::MAX;

#[derive(Zeroable)]
#[repr(C)]
pub(super) struct PacketBuffer {
    socket_addr: SocketAddr,
    len: NetworkOrderU16,
    data: [MaybeUninit<u8>; PACKET_SIZE as usize]
}

unsafe impl Construct for PacketBuffer {
    type Data = SocketAddr;
    
    unsafe fn init(this: *mut Self, socket: SocketAddr) {
        unsafe {
            std::ptr::write(&raw mut (*this).socket_addr, socket);
            std::ptr::write(&raw mut (*this).len, const { NetworkOrderU16::new(0) });
        }
    }
}

const LEN_FIELD_CUTOFF: u32 = (size_of::<SocketAddr>() + 1) as u32;

impl PacketBuffer {
    pub fn addr(&self) -> SocketAddr {
        self.socket_addr
    }

    pub fn packet_size(&self) -> u16 {
        self.len.get()
    }

    pub fn is_full(&self) -> bool {
        self.packet_size() == PACKET_SIZE
    }
    
    pub fn must_flush_for(&self, written: u32) -> bool {
        match written.cmp(&LEN_FIELD_CUTOFF) {
            Ordering::Less => self.is_full(),
            Ordering::Equal => {
                let [_, second] = self.len.0;
                second == u8::MAX
            },
            Ordering::Greater => true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.packet_size() == 0
    }

    pub fn clear(&mut self) {
        self.len = NetworkOrderU16::new(0)
    }
    
    pub fn push(&mut self, data: &[u8], written: u32) -> u16 {
        const {
            assert!(u32::MAX > size_of::<Self>() as u32)
        }
        
        let max_len = match written.cmp(&LEN_FIELD_CUTOFF) {
            // the length field has not been sent out yet, buffer more data
            Ordering::Less => {
                let spare_capacity = self.len.get();
                PACKET_SIZE - spare_capacity
            },
            
            // the most significant byte has been sent
            // we can still buffer just that little bit more data
            Ordering::Equal => {
                let [_, unsent_len] = self.len.0;
                u16::from(u8::MAX - unsent_len)
            },
            
            Ordering::Greater => return 0
        };

        let len = match data.len() > max_len.into() {
            true => max_len,
            false => data.len() as u16
        };
        
        unsafe {
            let last_len = self.len.get();
            self.len = NetworkOrderU16::new(len.unchecked_add(last_len));
            
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.data.as_mut_ptr().cast::<u8>().add(last_len.into()),
                len.into()
            )
        }        
        
        len
    }
    
    pub fn bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                size_of::<SocketAddr>()
                    + size_of::<NetworkOrderU16>()
                    + usize::from(self.len.get())
            )
        }
    }

    pub fn bytes_from(&self, written: u32) -> &[u8] {
        &self.bytes()[written as usize..]
    }
}

#[cfg(test)]
mod tests {
    use crate::constructor::ConstructExt;
    use super::*;

    const DUMMY_ADDR: SocketAddr = {
        SocketAddr::from_std(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::new(127, 0, 0, 1),
            8080
        )))
    };
    
    #[test]
    fn test_packet_buffer_initialization() {
        let buffer = PacketBuffer::box_new(DUMMY_ADDR);

        assert_eq!(buffer.socket_addr, DUMMY_ADDR);
        assert_eq!(buffer.len.get(), 0);
    }

    #[test]
    fn test_boxed_construction() {
        let boxed_buffer = PacketBuffer::box_new(DUMMY_ADDR);

        assert_eq!(boxed_buffer.addr(), DUMMY_ADDR);
        assert_eq!(boxed_buffer.len, 0);
    }

    #[test]
    fn test_push_data() {
        let mut buffer = PacketBuffer::box_new(DUMMY_ADDR);

        let data = [1, 2, 3, 4, 5];
        let written = 0; // Nothing written yet

        let pushed = buffer.push(&data, written);
        assert_eq!(pushed, 5);
        assert_eq!(buffer.len.get(), 5);

        // Get the full bytes including socket, length and data
        let bytes = buffer.bytes();

        // so our data should start after that
        let socket_size = size_of::<SocketAddr>();
        let len_size = size_of::<NetworkOrderU16>();
        let data_offset = socket_size + len_size;

        // Verify the data was copied correctly
        assert_eq!(&bytes[data_offset..data_offset + 5], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_push_multiple_times() {
        let mut buffer = PacketBuffer::box_new(DUMMY_ADDR);

        // First push
        let data1 = [1, 2, 3];
        let pushed1 = buffer.push(&data1, 0);
        assert_eq!(pushed1, 3);
        assert_eq!(buffer.len.get(), 3);

        // Second push
        let data2 = [4, 5];
        let pushed2 = buffer.push(&data2, 0);
        assert_eq!(pushed2, 2);
        assert_eq!(buffer.len.get(), 5);

        // Get the full bytes
        let bytes = buffer.bytes();
        let socket_size = size_of::<SocketAddr>();
        let len_size = size_of::<NetworkOrderU16>();
        let data_offset = socket_size + len_size;

        // Verify all data was copied correctly
        assert_eq!(&bytes[data_offset..data_offset + 5], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_current_bytes() {
        let mut buffer = PacketBuffer::box_new(DUMMY_ADDR);

        let data = [1, 2, 3, 4, 5];
        buffer.push(&data, 0);

        // Simulate having written 3 bytes already
        let remaining = buffer.bytes_from(3);

        // Should only have the bytes we haven't "written" yet
        assert_eq!(remaining.len(), buffer.bytes().len() - 3);
    }

    #[test]
    fn test_push_limit_with_written() {
        let addr = SocketAddr::from_std(([127, 0, 0, 1], 8080).into());
        let mut buffer = PacketBuffer::box_new(addr);

        // Push some initial data
        let data1 = [1, 2, 3, 4, 5];
        buffer.push(&data1, 0);

        // Simulate that socket + 1 bytes have been written
        let socket_size = size_of::<SocketAddr>();
        let written = (socket_size + 1) as u32;

        // Try to push more data
        let data2 = [6, 7, 8, 9, 10];
        let pushed = buffer.push(&data2, written);

        // We can still buffer a limited amount
        assert!(pushed > 0);

        // But if we've written more than that, we can't push more data
        let pushed_after = buffer.push(&data2, written + pushed as u32);
        assert_eq!(pushed_after, 0);
    }
}