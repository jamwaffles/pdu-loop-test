#![feature(const_maybe_uninit_zeroed)]

mod storage;

use storage::{FrameState, PduStorage, PduStorageRef};

struct PduLoop<'a> {
    storage: PduStorageRef<'a>,
}

unsafe impl<'a> Sync for PduLoop<'a> {}

impl<'a> PduLoop<'a> {
    fn new(storage: PduStorageRef<'a>) -> Self {
        Self { storage }
    }
}

#[cfg(feature = "ignore")]
#[test]
fn test_it() {
    use core::{task::Poll, time::Duration};
    use smoltcp::wire::EthernetAddress;
    use std::thread;

    static STORAGE: PduStorage<16, 128> = PduStorage::<16, 128>::new();
    static PDU_LOOP: PduLoop = PduLoop::new(STORAGE.as_ref());

    // Test the whole TX/RX loop with multiple threads
    #[test]
    fn parallel() {
        // Comment out to make this test work with miri
        // env_logger::try_init().ok();

        let (s, mut r) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        thread::Builder::new()
            .name("TX task".to_string())
            .spawn(move || {
                smol::block_on(async move {
                    let mut packet_buf = [0u8; 1536];

                    log::info!("Spawn TX task");

                    core::future::poll_fn::<(), _>(move |ctx| {
                        log::info!("Poll fn");

                        PDU_LOOP
                            .send_frames_blocking(ctx.waker(), |frame, data| {
                                let packet = frame
                                    .write_ethernet_packet(&mut packet_buf, data)
                                    .expect("Write Ethernet frame");

                                s.send(packet.to_vec()).unwrap();

                                // Simulate packet send delay
                                smol::Timer::after(Duration::from_millis(1));

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

        thread::Builder::new()
            .name("RX task".to_string())
            .spawn(move || {
                smol::block_on(async move {
                    log::info!("Spawn RX task");

                    while let Some(ethernet_frame) = r.recv().await {
                        // Munge fake sent frame into a fake received frame
                        let ethernet_frame = {
                            let mut frame = EthernetFrame::new_checked(ethernet_frame).unwrap();
                            frame.set_src_addr(EthernetAddress([
                                0x12, 0x10, 0x10, 0x10, 0x10, 0x10,
                            ]));
                            frame.into_inner()
                        };

                        log::info!("Received packet");

                        PDU_LOOP.pdu_rx(&ethernet_frame).expect("RX");
                    }
                })
            })
            .unwrap();

        smol::block_on(async move {
            for i in 0..64 {
                let data = [0xaa, 0xbb, 0xcc, 0xdd, i];

                log::info!("Send PDU {i}");

                let (result, _wkc) = PDU_LOOP
                    .pdu_tx_readwrite(
                        Command::Fpwr {
                            address: 0x1000,
                            register: 0x0980,
                        },
                        &data,
                    )
                    .await
                    .unwrap();

                assert_eq!(result, &data);
            }
        });
    }
}
