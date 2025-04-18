/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, Mutex};
use std::thread;

use compositing_traits::{WebrenderExternalImageRegistry, WebrenderImageHandlerType};
use fnv::FnvHashMap;
use ipc_channel::ipc::{IpcSender, channel};
use log::{trace, warn};
use webrender_api::ExternalImageId;

/// GL player threading API entry point that lives in the
/// constellation.
use crate::{GLPlayerMsg, GLPlayerMsgForward};

/// A GLPlayerThread manages the life cycle and message demultiplexing of
/// a set of video players with GL render.
pub struct GLPlayerThread {
    /// Map of live players.
    players: FnvHashMap<u64, IpcSender<GLPlayerMsgForward>>,
    /// List of registered webrender external images.
    /// We use it to get an unique ID for new players.
    external_images: Arc<Mutex<WebrenderExternalImageRegistry>>,
}

impl GLPlayerThread {
    pub fn new(external_images: Arc<Mutex<WebrenderExternalImageRegistry>>) -> Self {
        GLPlayerThread {
            players: Default::default(),
            external_images,
        }
    }

    pub fn start(
        external_images: Arc<Mutex<WebrenderExternalImageRegistry>>,
    ) -> IpcSender<GLPlayerMsg> {
        let (sender, receiver) = channel().unwrap();
        thread::Builder::new()
            .name("GLPlayer".to_owned())
            .spawn(move || {
                let mut renderer = GLPlayerThread::new(external_images);
                loop {
                    let msg = receiver.recv().unwrap();
                    let exit = renderer.handle_msg(msg);
                    if exit {
                        return;
                    }
                }
            })
            .expect("Thread spawning failed");

        sender
    }

    /// Handles a generic GLPlayerMsg message
    #[inline]
    fn handle_msg(&mut self, msg: GLPlayerMsg) -> bool {
        trace!("processing {:?}", msg);
        match msg {
            GLPlayerMsg::RegisterPlayer(sender) => {
                let id = self
                    .external_images
                    .lock()
                    .unwrap()
                    .next_id(WebrenderImageHandlerType::Media)
                    .0;
                self.players.insert(id, sender.clone());
                sender.send(GLPlayerMsgForward::PlayerId(id)).unwrap();
            },
            GLPlayerMsg::UnregisterPlayer(id) => {
                self.external_images
                    .lock()
                    .unwrap()
                    .remove(&ExternalImageId(id));
                if self.players.remove(&id).is_none() {
                    warn!("Tried to remove an unknown player");
                }
            },
            GLPlayerMsg::Lock(id, handler_sender) => {
                if let Some(sender) = self.players.get(&id) {
                    sender.send(GLPlayerMsgForward::Lock(handler_sender)).ok();
                }
            },
            GLPlayerMsg::Unlock(id) => {
                if let Some(sender) = self.players.get(&id) {
                    sender.send(GLPlayerMsgForward::Unlock()).ok();
                }
            },
            GLPlayerMsg::Exit => return true,
        }

        false
    }
}
