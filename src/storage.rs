use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr::{addr_of, addr_of_mut, NonNull},
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
    task::Poll,
};
use std::task::Waker;

use crate::{Command, Error};

#[derive(Debug)]
pub struct FrameState(AtomicUsize);

impl FrameState {
    const NONE: usize = 0;
    const CREATED: usize = 1;
    const SENDABLE: usize = 2;
    const SENDING: usize = 3;
    const RX_BUSY: usize = 5;
    const RX_DONE: usize = 6;
    const RX_PROCESSING: usize = 7;
}

#[derive(Debug)]
pub struct PduFrame {
    /// Data length.
    len: usize,

    // TODO: Un-pub
    pub index: u8,
}

pub struct PduStorage<const N: usize, const DATA: usize> {
    pub frames: UnsafeCell<MaybeUninit<[FrameElement<DATA>; N]>>,
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
    pub frames: NonNull<FrameElement<0>>,
    pub len: usize,
    frame_data_len: usize,
    idx: AtomicU8,
    _lifetime: PhantomData<&'a ()>,
}

// TODO: Move this to `PduLoop` - it's for the outer wrapper
unsafe impl<'a> Sync for PduStorageRef<'a> {}

impl<'a> PduStorageRef<'a> {
    // TODO: CreatedFrame struct to encapsulate functions
    pub fn alloc_frame(&self, command: Command, data_length: u16) -> Result<FrameBox<'a>, Error> {
        let data_length = usize::from(data_length);

        if data_length > self.frame_data_len {
            return Err(Error::DataTooLong);
        }

        let idx_u8 = self.idx.fetch_add(1, Ordering::AcqRel);

        let idx = usize::from(idx_u8) % self.len;

        let frame = unsafe { NonNull::new_unchecked(self.frames.as_ptr().add(idx)) };
        let frame = unsafe { FrameElement::claim_created(frame) }?;

        // Initialise frame
        unsafe {
            addr_of_mut!((*frame.as_ptr()).frame).write(PduFrame {
                // TODO: Command, etc
                len: data_length,
                index: idx_u8,
            });

            let buf_ptr = addr_of_mut!((*frame.as_ptr()).buffer);
            buf_ptr.write_bytes(0x00, data_length);
        }

        Ok(FrameBox {
            frame,
            waker: None,
            _lifetime: PhantomData,
        })
    }

    // TODO: Wrap in ReceivingFrame to constrain methods
    pub fn get_receiving(&self, idx: u8) -> Option<FrameBox<'a>> {
        let idx = usize::from(idx);

        if idx >= self.len {
            return None;
        }

        log::trace!("Receiving frame {idx}");

        let frame = unsafe { NonNull::new_unchecked(self.frames.as_ptr().add(idx)) };
        let frame = unsafe { FrameElement::claim_receiving(frame)? };

        Some(FrameBox {
            frame,
            waker: None,
            _lifetime: PhantomData,
        })
    }
}

// impl<'a> PduStorageRef<'a> {
//     unsafe fn swap_state(
//         this: NonNull<FrameElement<0>>,
//         from: usize,
//         to: usize,
//     ) -> Result<NonNull<FrameElement<0>>, usize> {
//         let fptr = this.as_ptr();

//         (&*addr_of_mut!((*fptr).status)).0.compare_exchange(
//             from,
//             to,
//             Ordering::AcqRel,
//             Ordering::Relaxed,
//         )?;

//         // If we got here, it's ours.
//         Ok(this)
//     }

//     pub unsafe fn claim_created(&self) -> Result<FrameBox<'a>, Error> {
//         let idx = self.idx.fetch_add(1, Ordering::Relaxed);

//         let idx = (idx as usize) % self.len;

//         let frame = NonNull::new_unchecked(self.frames.as_ptr().add(idx));

//         let frame = Self::swap_state(frame, FrameState::NONE, FrameState::CREATED)
//             .map_err(|_| Error::Anything)?;

//         // Initialise frame
//         unsafe {
//             addr_of_mut!((*frame.as_ptr()).frame).write(PduFrame {
//                 // todo
//             });
//             let buf_ptr = addr_of_mut!((*frame.as_ptr()).buffer);
//             buf_ptr.write_bytes(0x00, self.frame_data_len);
//         }

//         Ok(FrameBox {
//             frame,
//             buf_len: self.frame_data_len,
//             _lifetime: PhantomData,
//         })
//     }
// }

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

    unsafe fn set_state(this: NonNull<FrameElement<N>>, state: usize) {
        // TODO: not every state?

        let fptr = this.as_ptr();

        (&*addr_of_mut!((*fptr).status))
            .0
            .store(state, Ordering::Release);
    }

    unsafe fn swap_state(
        this: NonNull<FrameElement<N>>,
        from: usize,
        to: usize,
    ) -> Result<NonNull<FrameElement<N>>, usize> {
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

    /// Attempt to clame a frame element as CREATED. Succeeds if the selected
    /// FrameElement is currently in the NONE state.
    pub unsafe fn claim_created(
        this: NonNull<FrameElement<N>>,
    ) -> Result<NonNull<FrameElement<N>>, Error> {
        Self::swap_state(this, FrameState::NONE, FrameState::CREATED).map_err(|e| {
            log::error!(
                "Failed to claim frame: status is {:?}, expected {:?}",
                e,
                FrameState::NONE
            );

            Error::SwapState
        })
    }

    pub unsafe fn claim_sending(
        this: NonNull<FrameElement<N>>,
    ) -> Option<NonNull<FrameElement<N>>> {
        Self::swap_state(this, FrameState::SENDABLE, FrameState::SENDING).ok()
    }

    pub unsafe fn claim_receiving(
        this: NonNull<FrameElement<N>>,
    ) -> Option<NonNull<FrameElement<N>>> {
        Self::swap_state(this, FrameState::SENDING, FrameState::RX_BUSY).ok()
    }
}

// Used to store a FrameElement with erased const generics
// TODO: Create wrapper types so we can confine method calls to only certain states
#[derive(Debug)]
pub struct FrameBox<'a> {
    pub frame: NonNull<FrameElement<0>>,
    pub waker: Option<Waker>,
    pub _lifetime: PhantomData<&'a mut FrameElement<0>>,
}

// TODO: Un-pub all
impl<'a> FrameBox<'a> {
    pub unsafe fn frame(&self) -> &PduFrame {
        unsafe { &*addr_of!((*self.frame.as_ptr()).frame) }
    }

    pub unsafe fn frame_mut(&self) -> &mut PduFrame {
        unsafe { &mut *addr_of_mut!((*self.frame.as_ptr()).frame) }
    }

    unsafe fn buf_len(&self) -> usize {
        self.frame().len
    }

    pub unsafe fn frame_and_buf(&self) -> (&PduFrame, &[u8]) {
        let buf_ptr = unsafe { addr_of!((*self.frame.as_ptr()).buffer).cast::<u8>() };
        let buf = unsafe { core::slice::from_raw_parts(buf_ptr, self.buf_len()) };
        let frame = unsafe { &*addr_of!((*self.frame.as_ptr()).frame) };
        (frame, buf)
    }

    pub unsafe fn frame_and_buf_mut(&mut self) -> (&mut PduFrame, &mut [u8]) {
        let buf_ptr = unsafe { addr_of_mut!((*self.frame.as_ptr()).buffer).cast::<u8>() };
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr, self.buf_len()) };
        let frame = unsafe { &mut *addr_of_mut!((*self.frame.as_ptr()).frame) };

        (frame, buf)
    }

    pub unsafe fn buf_mut(&mut self) -> &mut [u8] {
        let ptr = FrameElement::<0>::buf_ptr(self.frame);
        core::slice::from_raw_parts_mut(ptr.as_ptr(), self.buf_len())
    }

    // TODO: Move to CreatedFrame
    pub fn mark_sendable(self) -> ReceiveFrameFut<'a> {
        unsafe {
            FrameElement::set_state(self.frame, FrameState::SENDABLE);
        }
        ReceiveFrameFut {
            frame: Some(self),
            count: 0,
        }
    }

    // TODO: Move to SendableFrame
    pub fn mark_sent(self) {
        log::trace!("Mark sent");

        unsafe {
            FrameElement::set_state(self.frame, FrameState::SENDING);
        }
    }

    // TODO: Move to ReceivingFrame
    pub fn mark_received(mut self) -> Result<(), Error> {
        // let (frame, buf) = unsafe { self.frame_and_buf_mut() };
        let index = unsafe { self.frame() }.index;

        log::trace!("Mark received {:?}", self.waker);

        let waker = self.waker.take().ok_or_else(|| {
            log::error!(
                "Attempted to wake frame #{} with no waker, possibly caused by timeout",
                index
            );

            Error::InvalidFrameState
        })?;

        waker.wake();

        unsafe {
            FrameElement::set_state(self.frame, FrameState::RX_DONE);
        }
        Ok(())
    }
}

pub struct ReceiveFrameFut<'sto> {
    frame: Option<FrameBox<'sto>>,
    count: usize,
}

impl<'sto> Future for ReceiveFrameFut<'sto> {
    // type Output = Result<ReceivedFrame<'sto>, Error>;
    type Output = Result<(), Error>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Self::Output> {
        // log::trace!("Poll fut");

        self.count += 1;

        println!("Poll fut {:?} times", self.count);

        let mut rxin = match self.frame.take() {
            Some(r) => r,
            None => return Poll::Ready(Err(Error::NoFrame)),
        };

        if let Some(w) = rxin.waker.replace(cx.waker().clone()) {
            println!("Have waker");
        } else {
            println!("Set waker");
        }

        self.frame = Some(rxin);

        if self.count > 10 {
            return Poll::Ready(Ok(()));
        } else {
            return Poll::Pending;
        }

        // let mut rxin = match self.frame.take() {
        //     Some(r) => r,
        //     None => return Poll::Ready(Err(Error::NoFrame)),
        // };

        // let swappy = unsafe {
        //     FrameElement::swap_state(rxin.frame, FrameState::RX_DONE, FrameState::RX_PROCESSING)
        // };
        // let was = match swappy {
        //     Ok(fe) => {
        //         log::trace!("Frame future is ready");
        //         return Poll::Ready(Ok(ReceivedFrame { frame: rxin }));
        //     }
        //     Err(e) => e,
        // };

        // // These are the states from the time we start sending until the response
        // // is received. If we observe any of these, it's fine, and we should keep
        // // waiting.
        // let okay = &[
        //     FrameState::SENDABLE,
        //     FrameState::SENDING,
        //     // FrameState::WAIT_RX,
        //     // FrameState::RX_BUSY,
        //     FrameState::RX_DONE,
        // ];

        // if okay.iter().any(|s| s == &was) {
        //     // TODO: touching the waker here would be unsound!
        //     //
        //     // This is because if the sender ever touches this
        //     //

        //     // let fm = unsafe { rxin.frame_mut() };

        //     log::trace!("Replace waker {:?}", rxin.waker);

        //     // if let Some(w) = rxin.waker.replace(cx.waker().clone()) {
        //     //     // w.wake();
        //     // }
        //     self.frame = Some(rxin);
        //     return Poll::Pending;
        // }

        // // any OTHER observed values of `was` indicates that this future has
        // // lived longer than it should have.
        // //
        // // We have had a bad day.
        // Poll::Ready(Err(Error::InvalidFrameState))
    }
}

#[derive(Debug)]
pub struct ReceivedFrame<'sto> {
    frame: FrameBox<'sto>,
}

impl<'sto> Drop for ReceivedFrame<'sto> {
    fn drop(&mut self) {
        unsafe { FrameElement::set_state(self.frame.frame, FrameState::NONE) }
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

        std::thread::scope(|scope| {
            let handles = (0..(NUM_FRAMES * 2))
                .map(|_| {
                    scope.spawn(|| {
                        let frame = unsafe { s.alloc_frame(Command::Whatever, 128) };
                        log::debug!("Get frame: {:?}", frame);
                    })
                })
                .collect::<Vec<_>>();

            handles
                .into_iter()
                .for_each(|handle| log::debug!("{:?}", handle.join()));
        });
    }

    #[test]
    fn no_spare_frames() {
        let _ = env_logger::builder().is_test(true).try_init();

        const NUM_FRAMES: usize = 16;

        let storage: PduStorage<NUM_FRAMES, 128> = PduStorage::new();
        let s = storage.as_ref();

        for _ in 0..NUM_FRAMES {
            assert!(unsafe { s.alloc_frame(Command::Whatever, 128) }.is_ok());
        }

        assert!(unsafe { s.alloc_frame(Command::Whatever, 128) }.is_err());
    }

    #[test]
    fn too_long() {
        let _ = env_logger::builder().is_test(true).try_init();

        const NUM_FRAMES: usize = 16;

        let storage: PduStorage<NUM_FRAMES, 128> = PduStorage::new();
        let s = storage.as_ref();

        assert!(unsafe { s.alloc_frame(Command::Whatever, 129) }.is_err());
    }
}
