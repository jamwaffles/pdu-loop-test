use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr::{addr_of_mut, NonNull},
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};
use std::ptr::addr_of;

#[derive(Debug)]
enum Error {
    Anything,
}

#[derive(Debug)]
pub struct FrameState(AtomicUsize);

impl FrameState {
    const NONE: usize = 0;
    const CREATED: usize = 1;
}

#[derive(Debug)]
pub struct PduFrame {
    //
}

pub struct PduStorage<const N: usize, const DATA: usize> {
    frames: UnsafeCell<MaybeUninit<[FrameElement<DATA>; N]>>,
    // data: UnsafeCell<[[u8; DATA]; N]>,
    // frame_states: UnsafeCell<[FrameState; N]>,
}

unsafe impl<const N: usize, const DATA: usize> Sync for PduStorage<N, DATA> {}

impl<const N: usize, const DATA: usize> PduStorage<N, DATA> {
    pub const fn new() -> Self {
        let frames = UnsafeCell::new(unsafe { MaybeUninit::zeroed().assume_init() });

        Self { frames }
    }

    pub const fn as_ref<'a>(&'a self) -> PduStorageRef<'a> {
        PduStorageRef {
            frames: unsafe { NonNull::new_unchecked(self.frames.get().cast()) },
            len: N,
            frame_data_len: DATA,
            idx: AtomicU8::new(0),
            _lifetime: PhantomData,
        }
    }
}

pub struct PduStorageRef<'a> {
    frames: NonNull<FrameElement<0>>,
    len: usize,
    frame_data_len: usize,
    idx: AtomicU8,
    _lifetime: PhantomData<&'a ()>,
}

// TODO: Move this to `PduLoop` - it's for the outer wrapper
unsafe impl<'a> Sync for PduStorageRef<'a> {}

impl<'a> PduStorageRef<'a> {
    unsafe fn swap_state(
        this: NonNull<FrameElement<0>>,
        from: usize,
        to: usize,
    ) -> Result<NonNull<FrameElement<0>>, usize> {
        let fptr = this.as_ptr();

        (&*addr_of_mut!((*fptr).status)).0.compare_exchange(
            from,
            to,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )?;

        // If we got here, it's ours.
        Ok(this)
    }

    // TODO: Return CreatedFrame wrapper struct so we can only call mark_sendable in this state
    unsafe fn claim_created(&self) -> Result<FrameBox<'a>, Error> {
        let idx = self.idx.fetch_add(1, Ordering::Relaxed);

        let idx = (idx as usize) % self.len;

        let frame = NonNull::new_unchecked(self.frames.as_ptr().add(idx));

        let frame = Self::swap_state(frame, FrameState::NONE, FrameState::CREATED)
            .map_err(|_| Error::Anything)?;

        // Initialise frame
        unsafe {
            addr_of_mut!((*frame.as_ptr()).frame).write(PduFrame {
                // todo
            });
            let buf_ptr = addr_of_mut!((*frame.as_ptr()).buffer);
            buf_ptr.write_bytes(0x00, self.frame_data_len);
        }

        Ok(FrameBox {
            frame,
            buf_len: self.frame_data_len,
            _lifetime: PhantomData,
        })
    }
}

/// An individual frame state, PDU header config, and data buffer.
#[derive(Debug)]
#[repr(C)]
pub struct FrameElement<const N: usize> {
    frame: PduFrame,
    status: FrameState,
    buffer: [u8; N],
}

impl<const N: usize> FrameElement<N> {
    unsafe fn buf_ptr(this: NonNull<FrameElement<N>>) -> NonNull<u8> {
        let buf_ptr: *mut [u8; N] = unsafe { addr_of_mut!((*this.as_ptr()).buffer) };
        let buf_ptr: *mut u8 = buf_ptr.cast();
        NonNull::new_unchecked(buf_ptr)
    }
}

// Used to store a FrameElement with erased const generics
// TODO: Create wrapper types so we can confine method calls to only certain states
#[derive(Debug)]
pub struct FrameBox<'a> {
    frame: NonNull<FrameElement<0>>,
    buf_len: usize,
    _lifetime: PhantomData<&'a mut FrameElement<0>>,
}

impl<'a> FrameBox<'a> {
    unsafe fn frame(&self) -> &PduFrame {
        unsafe { &*addr_of!((*self.frame.as_ptr()).frame) }
    }

    unsafe fn frame_mut(&self) -> &mut PduFrame {
        unsafe { &mut *addr_of_mut!((*self.frame.as_ptr()).frame) }
    }

    unsafe fn frame_and_buf(&self) -> (&PduFrame, &[u8]) {
        let buf_ptr = unsafe { addr_of!((*self.frame.as_ptr()).buffer).cast::<u8>() };
        let buf = unsafe { core::slice::from_raw_parts(buf_ptr, self.buf_len) };
        let frame = unsafe { &*addr_of!((*self.frame.as_ptr()).frame) };
        (frame, buf)
    }

    unsafe fn frame_and_buf_mut(&mut self) -> (&mut PduFrame, &mut [u8]) {
        let buf_ptr = unsafe { addr_of_mut!((*self.frame.as_ptr()).buffer).cast::<u8>() };
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr, self.buf_len) };
        let frame = unsafe { &mut *addr_of_mut!((*self.frame.as_ptr()).frame) };

        (frame, buf)
    }

    unsafe fn buf_mut(&mut self) -> &mut [u8] {
        let ptr = FrameElement::<0>::buf_ptr(self.frame);
        core::slice::from_raw_parts_mut(ptr.as_ptr(), self.buf_len)
    }
}

mod tests {
    use super::*;

    #[test]
    fn claim_created() {
        let _ = env_logger::builder().is_test(true).try_init();

        const NUM_FRAMES: usize = 16;
        let storage: PduStorage<NUM_FRAMES, 128> = PduStorage::new();
        let s = storage.as_ref();

        // let s = STORAGE.as_ref();

        std::thread::scope(|scope| {
            let handles = (0..(NUM_FRAMES * 2))
                .map(|_| {
                    scope.spawn(|| {
                        let frame = unsafe { s.claim_created() };
                        log::debug!("Get frame: {:?}", frame);
                    })
                })
                .collect::<Vec<_>>();

            handles
                .into_iter()
                .for_each(|handle| log::debug!("{:?}", handle.join()));
        });
    }
}
