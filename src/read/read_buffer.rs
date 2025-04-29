use std::alloc::Layout;
use std::fmt::{Debug, Formatter};
use std::io;
use std::mem::MaybeUninit;
use std::num::NonZero;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

// heap based so that moving is faster,
// since this is stored in a growable hashmap
pub(super) struct ReadBuffer {
    // The buffer.
    ptr: NonNull<u8>,
    capacity: NonZero<usize>,
    // The current seek offset into `buf`, must always be <= `filled`.
    pos: usize,
    // How many bytes have been filled must always be <= `capacity`
    filled: usize,
    // How many bytes have been initialized must always be `filled` >= but <= `capacity`
    initialized: usize
}

impl Unpin for ReadBuffer {}
unsafe impl Send for ReadBuffer {}
unsafe impl Sync for ReadBuffer {}


impl Debug for ReadBuffer {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f
            .debug_struct("ReadBuffer")
            .field("buffer", &self.bytes())
            .finish_non_exhaustive()
    }
}

// memory management
impl ReadBuffer {
    pub fn new(capacity: NonZero<usize>) -> io::Result<Self> {
        let ptr = 'buf: {
            if let Ok(layout) = Layout::array::<u8>(capacity.get()) {
                // Safety:
                // layout size is non-zero
                // and layout fits in isize bytes
                if let Some(ptr) = NonNull::new(unsafe { std::alloc::alloc(layout) }) {
                    break 'buf ptr
                }
            }
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "failed to allocate read buffer"))
        };

        Ok(Self {
            ptr,
            capacity,
            pos: 0,
            filled: 0,
            initialized: 0,
        })
    }
}

impl Drop for ReadBuffer {
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(
                self.ptr.as_ptr(),
                Layout::from_size_align_unchecked(self.capacity.get(), align_of::<u8>())
            )
        }
    }
}


// Read ops
impl ReadBuffer {
    fn raw_bytes(&mut self) -> &mut [MaybeUninit<u8>] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                self.capacity.get()
            )
        }
    }
    
    fn bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.ptr.as_ptr().add(self.pos),
                self.filled.unchecked_sub(self.pos)
            )
        }
    }
    
    pub fn fill_buffer<R: AsyncRead>(&mut self, cx: &mut Context<'_>, reader: Pin<&mut R>) -> Poll<io::Result<Option<usize>>> {

        let mut buffer = unsafe {
            // initialized buf not filled
            std::hint::assert_unchecked(self.filled <= self.initialized);
            std::hint::assert_unchecked(self.initialized <= self.capacity.get());

            let init = self.initialized;
            let fill = self.filled;
            
            let mut buff = ReadBuf::uninit(self.raw_bytes());
            buff.assume_init(init);
            buff.set_filled(fill);
            buff
        };
        
        if buffer.remaining() == 0 { 
            return Poll::Ready(Ok(None))
        }
        
        let res = R::poll_read(
            reader,
            cx,
            &mut buffer
        );
        
        match res {
            Poll::Ready(Ok(())) => {
                let filled = buffer.filled().len();
                let init = buffer.initialized().len();

                let old_filled = self.filled;

                self.filled = filled;
                self.initialized = init;
                
                Poll::Ready(Ok(Some(filled - old_filled)))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending
        }
    }

    pub fn read(&mut self, buffer: &mut ReadBuf) {
        let bytes_buffer = unsafe { buffer.unfilled_mut() };
        let bytes = self.bytes();

        let len = std::cmp::min(bytes_buffer.len(), bytes.len());

        if len == 0 {
            return
        }

        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                bytes_buffer.as_mut_ptr().cast::<u8>(),
                len
            );

            buffer.assume_init(len);
            let filled_len = buffer.filled().len();
            buffer.set_filled(filled_len + len);
            
            debug_assert_eq!(buffer.filled()[filled_len..], bytes[..len]);
        }
        
        self.pos += len;
        
        unsafe { std::hint::assert_unchecked(self.pos <= self.filled) }
        if self.pos == self.filled {
            self.pos = 0;
            self.filled = 0;
        }
    }
}



#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::task::{ready, Waker};
    use super::*;

    struct BuffReader<R> {
        buffer: ReadBuffer,
        reader: R,
    }

    impl<R: AsyncRead + Unpin> BuffReader<R> {
        fn new(reader: R, capacity: NonZero<usize>) -> io::Result<Self> {
            let buffer = ReadBuffer::new(capacity)?;
            Ok(Self { buffer, reader })
        }
    }


    impl<R: AsyncRead + Unpin> AsyncRead for BuffReader<R> {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            let this = Pin::into_inner(self);


            while buf.remaining() != 0 {
                let read = ready!(ReadBuffer::fill_buffer(&mut this.buffer, cx, Pin::new(&mut this.reader)))?;
                ReadBuffer::read(&mut this.buffer, buf);
                if read.is_none_or(|read| read == 0) { 
                    break
                }
            }
            
            Poll::Ready(Ok(()))
        }
    }
    
    macro_rules! get_nonzero {
        ($expr: expr) => {
            const { NonZero::new($expr).unwrap() }
        };
    }

    #[test]
    fn test_new_buffer() {
        let capacity = get_nonzero!(1024);
        let buffer = ReadBuffer::new(capacity).unwrap();

        assert_eq!(buffer.capacity, capacity);
        assert_eq!(buffer.pos, 0);
        assert_eq!(buffer.filled, 0);
    }

    #[test]
    fn test_capacity() {
        let capacity = get_nonzero!(2048);
        let buffer = ReadBuffer::new(capacity).unwrap();

        assert_eq!(buffer.capacity, capacity);
    }

    /// A simple mock reader that simulates reads from a VecDeque
    /// and always returns Ready
    struct MockReader {
        data: VecDeque<u8>,
    }

    impl MockReader {
        fn new(data: Vec<u8>) -> Self {
            Self {
                data: VecDeque::from(data),
            }
        }
    }

    impl AsyncRead for MockReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let available = self.data.len();
            if available == 0 {
                return Poll::Ready(Ok(()));
            }

            let can_read = std::cmp::min(available, buf.remaining());
            if can_read == 0 {
                return Poll::Ready(Ok(()));
            }

            for _ in 0..can_read {
                if let Some(byte) = self.data.pop_front() {
                    buf.put_slice(&[byte]);
                }
            }

            Poll::Ready(Ok(()))
        }
    }

    macro_rules! get_nonzero {
        ($expr: expr) => {
            NonZero::new($expr).unwrap()
        };
    }

    #[test]
    fn test_buff_reader_simple_read() {
        // Create test data
        let test_data = b"Hello, world!".to_vec();
        let mock_reader = MockReader::new(test_data.clone());

        // Create BuffReader with a buffer smaller than the test data
        let buffer_size = NonZero::new(test_data.len()).unwrap();
        let mut buff_reader = BuffReader::new(mock_reader, buffer_size).unwrap();

        // Create an output buffer to read into
        let mut output = vec![0; test_data.len()];
        let mut read_buf = ReadBuf::new(&mut output);

        // Create a simple context
        let mut cx = Context::from_waker(Waker::noop());

        // First read should fill the buffer and then read from it
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf),
            Poll::Ready(Ok(()))
        ));

        // Should have read all the data
        assert_eq!(read_buf.filled(), test_data.as_slice());
    }

    #[test]
    fn test_buff_reader_partial_reads() {
        // Create test data
        let test_data = b"Hello, world!".to_vec();
        let mock_reader = MockReader::new(test_data.clone());

        // Create BuffReader with a buffer size equal to the test data
        let buffer_size = get_nonzero!(test_data.len());
        let mut buff_reader = BuffReader::new(mock_reader, buffer_size).unwrap();

        let mut cx = Context::from_waker(Waker::noop());

        // Read first 5 bytes
        let mut output1 = vec![0; 5];
        let mut read_buf1 = ReadBuf::new(&mut output1);
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf1),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf1.filled(), b"Hello");

        // Read next 8 bytes
        let mut output2 = vec![0; 8];
        let mut read_buf2 = ReadBuf::new(&mut output2);
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf2),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf2.filled(), b", world!");

        // Should be at end of buffer now
        let mut output3 = vec![0; 10];
        let mut read_buf3 = ReadBuf::new(&mut output3);
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf3),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf3.filled().len(), 0);
    }

    #[test]
    fn test_buff_reader_multiple_fills() {
        // Create test data larger than our buffer
        let test_data = b"This is a longer string that will require multiple buffer fills".to_vec();
        let mock_reader = MockReader::new(test_data.clone());

        // Create a small buffer to force multiple fills
        let buffer_size = get_nonzero!(16);
        let mut buff_reader = BuffReader::new(mock_reader, buffer_size).unwrap();

        // Create an output buffer large enough for all the data
        let mut output = vec![0; test_data.len()];
        let mut read_buf = ReadBuf::new(&mut output);

        let mut cx = Context::from_waker(Waker::noop());

        // Reading should complete in a single call since our MockReader always returns Ready
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf),
            Poll::Ready(Ok(()))
        ));

        // Should have read all the data
        assert_eq!(read_buf.filled(), test_data.as_slice());
    }

    #[test]
    fn test_buff_reader_empty_read() {
        // Create an empty mock reader
        let mock_reader = MockReader::new(vec![]);

        let buffer_size = get_nonzero!(16);
        let mut buff_reader = BuffReader::new(mock_reader, buffer_size).unwrap();

        let mut cx = Context::from_waker(Waker::noop());

        let mut output = vec![0; 10];
        let mut read_buf = ReadBuf::new(&mut output);

        // Should be ready with no data
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf.filled().len(), 0);
    }

    #[test]
    fn test_buff_reader_exact_size() {
        // Create test data that exactly matches the buffer size
        let buffer_size = get_nonzero!(8);
        let test_data = b"12345678".to_vec();
        let mock_reader = MockReader::new(test_data.clone());

        let mut buff_reader = BuffReader::new(mock_reader, buffer_size).unwrap();

        let mut cx = Context::from_waker(Waker::noop());

        // The output buffer is smaller than the test data
        let mut output = vec![0; 4];
        let mut read_buf = ReadBuf::new(&mut output);

        // First read should get first 4 bytes
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf.filled(), b"1234");

        // The second read should get the remaining 4 bytes
        let mut output2 = vec![0; 4];
        let mut read_buf2 = ReadBuf::new(&mut output2);
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf2),
            Poll::Ready(Ok(()))
        ));
        
        assert_eq!(read_buf2.filled(), b"5678");
    }

    #[test]
    fn test_buff_reader_zero_size_read() {
        // Create a reader with data but try to read 0 bytes
        let test_data = b"Hello".to_vec();
        let mock_reader = MockReader::new(test_data);

        let buffer_size = get_nonzero!(16);
        let mut buff_reader = BuffReader::new(mock_reader, buffer_size).unwrap();

        let mut cx = Context::from_waker(Waker::noop());

        // Create an empty output buffer (0 capacity)
        let mut output = vec![0; 0];
        let mut read_buf = ReadBuf::new(&mut output);

        // Should be ready but read 0 bytes
        assert!(matches!(
            Pin::new(&mut buff_reader).poll_read(&mut cx, &mut read_buf),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf.filled().len(), 0);
    }
}