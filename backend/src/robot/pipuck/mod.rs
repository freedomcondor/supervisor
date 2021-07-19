use futures::FutureExt;
use uuid::Uuid;
use std::{future::Future, pin::Pin, task::{Context, Poll}};
use tokio::{sync::mpsc, task::JoinHandle};
use crate::network::fernbedienung;

mod task;

pub use task::{
    Action, Error, Receiver, Request, Sender, State
};

pub struct PiPuck(JoinHandle<Uuid>);

impl PiPuck {
    pub fn new(device: fernbedienung::Device) -> (Uuid, Sender, Self) {
        let uuid = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(task::new(uuid, rx, device));
        (uuid, tx, Self(handle))
    }
}

impl Future for PiPuck {
    type Output = Result<Uuid, tokio::task::JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().0.poll_unpin(cx)
    }
}
