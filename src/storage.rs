use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr::{addr_of, addr_of_mut, NonNull},
    sync::atomic::{AtomicU8, Ordering},
    task::Poll,
};
use std::{ops::Deref, task::Waker};

use spin::RwLock;

use crate::{Command, Error};

#[atomic_enum::atomic_enum]
#[derive(PartialEq, Default)]
pub enum FrameState {
    // SAFETY: Because we create a bunch of `Frame`s with `MaybeUninit::zeroed`, the `None` state
    // MUST be equal to zero. All other fields in `Frame` are overridden in `replace`, so there
    // should be no UB there.
    #[default]
    None = 0,
    Created = 1,
    Sendable = 2,
    Sending = 3,
    RxBusy = 5,
    RxDone = 6,
    RxProcessing = 7,
}

#[derive(Debug)]
pub struct PduFrame {
    /// Data length.
    len: usize,

    // TODO: Un-pub
    pub index: u8,

    pub waker: spin::RwLock<Option<Waker>>,
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

impl<'a> PduStorageRef<'a> {
    pub fn alloc_frame(
        &self,
        command: Command,
        data_length: u16,
    ) -> Result<CreatedFrame<'a>, Error> {
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
                waker: RwLock::new(None),
            });

            let buf_ptr = addr_of_mut!((*frame.as_ptr()).buffer);
            buf_ptr.write_bytes(0x00, data_length);
        }

        Ok(CreatedFrame {
            inner: FrameBox {
                frame,
                _lifetime: PhantomData,
            },
        })
    }

    /// Updates state from SENDING -> RX_BUSY
    pub fn get_receiving(&self, idx: u8) -> Option<ReceivingFrame<'a>> {
        let idx = usize::from(idx);

        if idx >= self.len {
            return None;
        }

        log::trace!("Receiving frame {idx}");

        let frame = unsafe { NonNull::new_unchecked(self.frames.as_ptr().add(idx)) };
        let frame = unsafe { FrameElement::claim_receiving(frame)? };

        Some(ReceivingFrame {
            inner: FrameBox {
                frame,
                _lifetime: PhantomData,
            },
        })
    }
}

/// An individual frame state, PDU header config, and data buffer.
#[derive(Debug)]
#[repr(C)]
pub struct FrameElement<const N: usize> {
    frame: PduFrame,
    status: AtomicFrameState,
    buffer: [u8; N],
}

impl<const N: usize> FrameElement<N> {
    unsafe fn buf_ptr(this: NonNull<FrameElement<N>>) -> NonNull<u8> {
        let buf_ptr: *mut [u8; N] = unsafe { addr_of_mut!((*this.as_ptr()).buffer) };
        let buf_ptr: *mut u8 = buf_ptr.cast();
        NonNull::new_unchecked(buf_ptr)
    }

    unsafe fn set_state(this: NonNull<FrameElement<N>>, state: FrameState) {
        let fptr = this.as_ptr();

        (&*addr_of_mut!((*fptr).status)).store(state, Ordering::Release);
    }

    unsafe fn swap_state(
        this: NonNull<FrameElement<N>>,
        from: FrameState,
        to: FrameState,
    ) -> Result<NonNull<FrameElement<N>>, FrameState> {
        let fptr = this.as_ptr();

        (&*addr_of_mut!((*fptr).status)).compare_exchange(
            from,
            to,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )?;

        // If we got here, it's ours.
        Ok(this)
    }

    /// Attempt to clame a frame element as CREATED. Succeeds if the selected FrameElement is
    /// currently in the NONE state.
    pub unsafe fn claim_created(
        this: NonNull<FrameElement<N>>,
    ) -> Result<NonNull<FrameElement<N>>, Error> {
        Self::swap_state(this, FrameState::None, FrameState::Created).map_err(|e| {
            log::error!(
                "Failed to claim frame: status is {:?}, expected {:?}",
                e,
                FrameState::None
            );

            Error::SwapState
        })
    }

    pub unsafe fn claim_sending(
        this: NonNull<FrameElement<N>>,
    ) -> Option<NonNull<FrameElement<N>>> {
        Self::swap_state(this, FrameState::Sendable, FrameState::Sending).ok()
    }

    pub unsafe fn claim_receiving(
        this: NonNull<FrameElement<N>>,
    ) -> Option<NonNull<FrameElement<N>>> {
        Self::swap_state(this, FrameState::Sending, FrameState::RxBusy).ok()
    }
}

// Used to store a FrameElement with erased const generics
#[derive(Debug)]
pub struct FrameBox<'a> {
    pub frame: NonNull<FrameElement<0>>,
    pub _lifetime: PhantomData<&'a mut FrameElement<0>>,
}

// TODO: Un-pub all
impl<'a> FrameBox<'a> {
    pub unsafe fn replace_waker(&self, waker: Waker) {
        (&*addr_of!((*self.frame.as_ptr()).frame.waker))
            .try_write()
            .expect("Contention replace_waker")
            .replace(waker);
    }

    pub unsafe fn take_waker(&self) -> Option<Waker> {
        (&*addr_of!((*self.frame.as_ptr()).frame.waker))
            .try_write()
            .expect("Contention take_waker")
            .take()
    }

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

    pub unsafe fn buf(&self) -> &[u8] {
        let ptr = FrameElement::<0>::buf_ptr(self.frame);
        core::slice::from_raw_parts(ptr.as_ptr(), self.buf_len())
    }

    pub unsafe fn buf_mut(&mut self) -> &mut [u8] {
        let ptr = FrameElement::<0>::buf_ptr(self.frame);
        core::slice::from_raw_parts_mut(ptr.as_ptr(), self.buf_len())
    }
}

#[derive(Debug)]
pub struct CreatedFrame<'a> {
    inner: FrameBox<'a>,
}

impl<'a> CreatedFrame<'a> {
    pub fn mark_sendable(self) -> ReceiveFrameFut<'a> {
        unsafe {
            FrameElement::set_state(self.inner.frame, FrameState::Sendable);
        }
        ReceiveFrameFut {
            frame: Some(self.inner),
            count: 0,
        }
    }
}

#[derive(Debug)]
pub struct SendableFrame<'a> {
    inner: FrameBox<'a>,
}

impl<'a> SendableFrame<'a> {
    pub fn new(inner: FrameBox<'a>) -> Self {
        Self { inner }
    }

    pub fn mark_sent(self) {
        log::trace!("Mark sent");

        unsafe {
            FrameElement::set_state(self.inner.frame, FrameState::Sending);
        }
    }

    // TODO: Generate frame with nom, etc
    pub fn write_ethernet_packet<'buf>(&self, buf: &'buf mut [u8]) -> Result<&'buf [u8], Error> {
        // HACK
        const LEN: usize = 16;

        if buf.len() < LEN {
            return Err(Error::BufferTooShort);
        }

        // Fill with some garbage data
        let packet = [unsafe { self.inner.frame() }.index; LEN];

        let chunk = &mut buf[0..LEN];

        chunk.copy_from_slice(&packet);

        Ok(chunk)
    }
}

#[derive(Debug)]
pub struct ReceivingFrame<'a> {
    inner: FrameBox<'a>,
}

impl<'a> ReceivingFrame<'a> {
    pub fn mark_received(mut self) -> Result<(), Error> {
        let frame = unsafe { self.inner.frame() };

        log::trace!("Frame and buf mark_received");

        let idx = frame.index;

        log::trace!("Mark received, waker is {:?}", frame.waker);

        let waker = unsafe { self.inner.take_waker() }.ok_or_else(|| {
            log::error!(
                "Attempted to wake frame #{} with no waker, possibly caused by timeout",
                frame.index
            );

            Error::InvalidFrameState
        })?;

        unsafe {
            FrameElement::set_state(self.inner.frame, FrameState::RxDone);
        }

        waker.wake();

        Ok(())
    }

    pub fn buf_mut(&mut self) -> &mut [u8] {
        unsafe { self.inner.buf_mut() }
    }

    pub fn reset_readable(self) {
        unsafe { FrameElement::set_state(self.inner.frame, FrameState::None) }
    }
}

pub struct ReceiveFrameFut<'sto> {
    frame: Option<FrameBox<'sto>>,
    count: usize,
}

impl<'sto> Future for ReceiveFrameFut<'sto> {
    type Output = Result<ReceivedFrame<'sto>, Error>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Self::Output> {
        self.count += 1;

        log::debug!("Poll fut {:?} times", self.count);

        let rxin = match self.frame.take() {
            Some(r) => r,
            None => return Poll::Ready(Err(Error::NoFrame)),
        };

        log::trace!("Take");

        let swappy = unsafe {
            FrameElement::swap_state(rxin.frame, FrameState::RxDone, FrameState::RxProcessing)
        };

        log::trace!("Swappy");

        let was = match swappy {
            Ok(_frame_element) => {
                log::trace!("Frame future is ready");
                return Poll::Ready(Ok(ReceivedFrame { inner: rxin }));
            }
            Err(e) => e,
        };

        log::trace!("Was {:?}", was);

        match was {
            FrameState::Sendable | FrameState::Sending => {
                unsafe { rxin.replace_waker(cx.waker().clone()) };

                self.frame = Some(rxin);

                Poll::Pending
            }
            _ => Poll::Ready(Err(Error::InvalidFrameState)),
        }
    }
}

#[derive(Debug)]
pub struct ReceivedFrame<'sto> {
    inner: FrameBox<'sto>,
}

impl<'sto> ReceivedFrame<'sto> {
    fn len(&self) -> usize {
        // TODO
        //  let len: usize = self.frame().pdu.flags.len().into();
        // debug_assert!(len <= self.fb.buf_len);

        // HACK
        let len = 16;

        len
    }

    // TODO: Add wkc(), etc
}

impl<'sto> Drop for ReceivedFrame<'sto> {
    fn drop(&mut self) {
        unsafe { FrameElement::set_state(self.inner.frame, FrameState::None) }
    }
}

impl<'sto> Deref for ReceivedFrame<'sto> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { self.inner.buf() }
    }
}

mod tests {
    use super::*;

    #[test]
    fn no_spare_frames() {
        let _ = env_logger::builder().is_test(true).try_init();

        const NUM_FRAMES: usize = 16;

        let storage: PduStorage<NUM_FRAMES, 128> = PduStorage::new();
        let s = storage.as_ref();

        for _ in 0..NUM_FRAMES {
            assert!(s.alloc_frame(Command::Whatever, 128).is_ok());
        }

        assert!(s.alloc_frame(Command::Whatever, 128).is_err());
    }

    #[test]
    fn too_long() {
        let _ = env_logger::builder().is_test(true).try_init();

        const NUM_FRAMES: usize = 16;

        let storage: PduStorage<NUM_FRAMES, 128> = PduStorage::new();
        let s = storage.as_ref();

        assert!(s.alloc_frame(Command::Whatever, 129).is_err());
    }
}
