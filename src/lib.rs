#![feature(const_maybe_uninit_zeroed)]
#![allow(unused)]

mod storage;

use core::{ptr::NonNull, task::Waker};
use std::{cell::RefCell, marker::PhantomData};

use nom::error::context;
use smoltcp::wire::EthernetFrame;
use spin::RwLock;
use storage::{FrameBox, FrameElement, FrameState, PduStorage, PduStorageRef};

// DELETEME
#[derive(Debug)]
pub enum Command {
    Whatever,
}

#[derive(Debug, PartialEq)]
pub enum Error {
    DataTooLong,
    SwapState,
    NoFrame,
    InvalidFrameState,
    SendFailed,
}

struct PduLoop<'a> {
    storage: PduStorageRef<'a>,
    tx_waker: spin::RwLock<Option<Waker>>,
}

unsafe impl<'a> Sync for PduLoop<'a> {}

impl<'a> PduLoop<'a> {
    const fn new(storage: PduStorageRef<'a>) -> Self {
        Self {
            storage,
            tx_waker: RwLock::new(None),
        }
    }

    /// Tell the packet sender there is data ready to send.
    fn wake_sender(&self) {
        if let Some(waker) = self.tx_waker.read().as_ref() {
            waker.wake_by_ref()
        }
    }

    pub fn send_frames_blocking<F>(&self, waker: &Waker, mut send: F) -> Result<(), Error>
    where
        F: FnMut(&FrameBox<'_>) -> Result<(), ()>,
    {
        for idx in 0..self.storage.len {
            let frame = unsafe { NonNull::new_unchecked(self.storage.frames.as_ptr().add(idx)) };

            let sending = if let Some(frame) = unsafe { FrameElement::claim_sending(frame) } {
                // TODO: Wrap in `SendableFrame`
                FrameBox {
                    frame,
                    _lifetime: PhantomData,
                }
            } else {
                continue;
            };

            match send(&sending) {
                Ok(_) => {
                    sending.mark_sent();
                }
                Err(_) => {
                    return Err(Error::SendFailed);
                }
            }
        }

        // TODO: A less garbage way to concurrently read/write the waker
        if self.tx_waker.read().is_none() {
            self.tx_waker.write().replace(waker.clone());
        }

        Ok(())
    }

    pub async fn pdu_broadcast_zeros(
        &self,
        register: u16,
        payload_length: u16,
    ) -> Result<(), Error> {
        let mut frame = unsafe { self.storage.alloc_frame(Command::Whatever, payload_length) }?;

        // Buffer is initialised with zeroes

        let frame = frame.mark_sendable();

        self.wake_sender();

        let res = frame.await?;

        // dbg!(res);

        Ok(())
    }

    pub fn pdu_rx(&self, ethernet_frame: &[u8]) -> Result<(), Error> {
        // let raw_packet = EthernetFrame::new_checked(ethernet_frame).expect("Ethernet");

        // Look for EtherCAT packets whilst ignoring broadcast packets sent from self.
        // As per <https://github.com/OpenEtherCATsociety/SOEM/issues/585#issuecomment-1013688786>,
        // the first slave will set the second bit of the MSB of the MAC address. This means if we
        // send e.g. 10:10:10:10:10:10, we receive 12:10:10:10:10:10 which is useful for this
        // filtering.
        // if raw_packet.ethertype() != ETHERCAT_ETHERTYPE || raw_packet.src_addr() == MASTER_ADDR {
        //     return Ok(());
        // }

        // let i = raw_packet.payload();
        let i = ethernet_frame;

        // let (i, header) = context("header", FrameHeader::parse)(i)?;

        // Only take as much as the header says we should
        // let (_rest, i) = take(header.payload_len())(i)?;
        // let (i, command_code) = map_res(u8, CommandCode::try_from)(i)?;
        // let (i, index) = u8(i)?;

        // HACK: Pretend an EtherCAT frame is just the packet index for this prototype
        let index = i[0];

        let mut rxin_frame = self
            .storage
            .get_receiving(index)
            .expect("todo(ajm) should fix this/data race says what?");

        // TODO
        // if frame.pdu.index != index {
        //     rxin_frame.reset_readable();
        //     return Err(Error::Pdu(PduError::Validation(
        //         PduValidationError::IndexMismatch {
        //             sent: frame.pdu.index,
        //             sent,
        //             received: index,
        //         },
        //     )));
        // }

        // TODO
        // if command.code() != frame.pdu().command().code() {
        //     let received = frame.pdu().command();
        //     rxin_frame.reset_readable();
        // }

        // let (frame, frame_data) = unsafe { rxin_frame.frame_and_buf_mut() };

        // TODO: Set frame data

        // Wakes frame future, sets RX_DONE
        rxin_frame.mark_received();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{storage::PduStorage, PduLoop};
    use core::{task::Poll, time::Duration};
    use smoltcp::wire::{EthernetAddress, EthernetFrame};
    use std::thread;
    use tokio::runtime::Handle;

    static STORAGE: PduStorage<16, 128> = PduStorage::<16, 128>::new();
    static PDU_LOOP: PduLoop = PduLoop::new(STORAGE.as_ref());

    #[tokio::test]
    async fn broadcast_zeros() {
        // Comment out to make this test work with miri
        env_logger::try_init().ok();

        // let storage = PduStorage::<16, 128>::new();
        // let pdu_loop = PduLoop::new(storage.as_ref());

        let rt = tokio::runtime::Runtime::new().unwrap();

        let (s, mut r) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        let handle = Handle::current();

        let tx_handle = thread::Builder::new()
            .name("TX task".to_string())
            .spawn(move || {
                handle.block_on(async move {
                    let mut packet_buf = [0u8; 1536];

                    log::info!("Spawn TX task");

                    core::future::poll_fn::<(), _>(move |ctx| {
                        log::info!("Poll fn");

                        PDU_LOOP
                            .send_frames_blocking(ctx.waker(), |frame| {
                                // let packet = frame
                                //     .write_ethernet_packet(&mut packet_buf, data)
                                //     .expect("Write Ethernet frame");

                                let packet = [unsafe { frame.frame() }.index; 16];

                                s.send(packet.to_vec()).unwrap();

                                log::info!("Sent packet");

                                Ok(())
                            })
                            .unwrap();

                        Poll::Pending
                    })
                    .await
                })
            })
            .unwrap();

        let handle = Handle::current();

        let rx_handle = thread::Builder::new()
            .name("RX task".to_string())
            .spawn(move || {
                handle.block_on(async move {
                    log::info!("Spawn RX task");

                    while let Some(ethernet_frame) = r.recv().await {
                        // TODO
                        // // Munge fake sent frame into a fake received frame
                        // let ethernet_frame = {
                        //     let mut frame = EthernetFrame::new_checked(ethernet_frame).unwrap();
                        //     frame.set_src_addr(EthernetAddress([
                        //         0x12, 0x10, 0x10, 0x10, 0x10, 0x10,
                        //     ]));
                        //     frame.into_inner()
                        // };

                        log::info!("Received packet {:?}", ethernet_frame);

                        PDU_LOOP.pdu_rx(&ethernet_frame).expect("RX");
                    }
                })
            })
            .unwrap();

        PDU_LOOP.pdu_broadcast_zeros(0x1234, 16).await.unwrap();
    }
}
