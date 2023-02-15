use core::future::Future;
use core::task::Poll;
use core::task::Waker;
use std::ptr::NonNull;

struct PtrThing {
    foo: u8,
}

struct MyThing {
    ptr: NonNull<PtrThing>,
}

struct MyFut {
    waker: Option<Waker>,
    thing: MyThing,
}

impl Future for MyFut {
    type Output = Result<u8, ()>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Self::Output> {
        println!("Poll fut");

        if let Some(w) = self.waker.replace(cx.waker().clone()) {
            w.wake();
        }

        Poll::Pending
    }
}

struct Client {
    ptr_things: [PtrThing; 2],
}

impl Client {
    async fn do_something(&self) -> Result<u8, ()> {
        let ptr_thing = self.ptr_things.get(0).unwrap();

        let thing = MyThing {
            ptr: unsafe { NonNull::new_unchecked(ptr_thing as *const _ as *mut _) },
        };

        let fut = MyFut { waker: None, thing };

        fut.await?;

        Ok(0xffu8)
    }
}

fn main() {
    println!("Test");

    let client = Client {
        ptr_things: [PtrThing { foo: 0x01u8 }, PtrThing { foo: 0x02u8 }],
    };

    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(client.do_something()).unwrap();
}
