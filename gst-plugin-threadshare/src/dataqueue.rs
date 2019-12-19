// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use futures::channel::oneshot;
use futures::lock::Mutex;

use gst;
use gst::gst_debug;
use gst::prelude::*;

use lazy_static::lazy_static;

use std::collections::VecDeque;
use std::sync::Arc;
use std::{u32, u64};

lazy_static! {
    static ref DATA_QUEUE_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-dataqueue",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing queue"),
    );
}

#[derive(Debug)]
pub enum DataQueueItem {
    Buffer(gst::Buffer),
    BufferList(gst::BufferList),
    Event(gst::Event),
}

impl DataQueueItem {
    fn size(&self) -> (u32, u32) {
        match *self {
            DataQueueItem::Buffer(ref buffer) => (1, buffer.get_size() as u32),
            DataQueueItem::BufferList(ref list) => (
                list.len() as u32,
                list.iter().map(|b| b.get_size() as u32).sum::<u32>(),
            ),
            DataQueueItem::Event(_) => (0, 0),
        }
    }

    fn timestamp(&self) -> Option<u64> {
        match *self {
            DataQueueItem::Buffer(ref buffer) => buffer.get_dts_or_pts().0,
            DataQueueItem::BufferList(ref list) => {
                list.iter().filter_map(|b| b.get_dts_or_pts().0).next()
            }
            DataQueueItem::Event(_) => None,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum DataQueueState {
    Paused,
    Started,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct DataQueue(Arc<Mutex<DataQueueInner>>);

#[derive(Debug)]
struct DataQueueInner {
    element: gst::Element,
    src_pad: gst::Pad,

    state: DataQueueState,
    queue: VecDeque<DataQueueItem>,

    cur_size_buffers: u32,
    cur_size_bytes: u32,
    max_size_buffers: Option<u32>,
    max_size_bytes: Option<u32>,
    max_size_time: Option<u64>,

    wake_sender: Option<oneshot::Sender<()>>,
}

impl DataQueueInner {
    fn wake(&mut self) {
        self.wake_sender.take();
    }
}

impl DataQueue {
    pub fn new(
        element: &gst::Element,
        src_pad: &gst::Pad,
        max_size_buffers: Option<u32>,
        max_size_bytes: Option<u32>,
        max_size_time: Option<u64>,
    ) -> DataQueue {
        DataQueue(Arc::new(Mutex::new(DataQueueInner {
            element: element.clone(),
            src_pad: src_pad.clone(),
            state: DataQueueState::Stopped,
            queue: VecDeque::new(),
            cur_size_buffers: 0,
            cur_size_bytes: 0,
            max_size_buffers,
            max_size_bytes,
            max_size_time,
            wake_sender: None,
        })))
    }

    pub async fn start(&self) {
        let mut inner = self.0.lock().await;
        if inner.state == DataQueueState::Started {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already Started");
            return;
        }
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Starting data queue");
        inner.state = DataQueueState::Started;
        inner.wake();
    }

    pub async fn pause(&self) {
        let mut inner = self.0.lock().await;
        if inner.state == DataQueueState::Paused {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already Paused");
            return;
        }
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Pausing data queue");
        assert_eq!(DataQueueState::Started, inner.state);
        inner.state = DataQueueState::Paused;
    }

    pub async fn stop(&self) {
        let mut inner = self.0.lock().await;
        if inner.state == DataQueueState::Stopped {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already Stopped");
            return;
        }
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Stopping data queue");
        inner.state = DataQueueState::Stopped;
        inner.wake();
    }

    pub async fn clear(&self) {
        let mut inner = self.0.lock().await;

        assert_eq!(inner.state, DataQueueState::Paused);
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Clearing data queue");

        let src_pad = inner.src_pad.clone();
        for item in inner.queue.drain(..) {
            if let DataQueueItem::Event(event) = item {
                if event.is_sticky()
                    && event.get_type() != gst::EventType::Segment
                    && event.get_type() != gst::EventType::Eos
                {
                    let _ = src_pad.store_sticky_event(&event);
                }
            }
        }

        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue cleared");
    }

    pub async fn push(&self, item: DataQueueItem) -> Result<(), DataQueueItem> {
        let mut inner = self.0.lock().await;

        if inner.state != DataQueueState::Started {
            gst_debug!(
                DATA_QUEUE_CAT,
                obj: &inner.element,
                "Rejecting item {:?} in state {:?}",
                item,
                inner.state
            );
            return Err(item);
        }

        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Pushing item {:?}", item);

        let (count, bytes) = item.size();
        let queue_ts = inner.queue.iter().filter_map(|i| i.timestamp()).next();
        let ts = item.timestamp();

        if let Some(max) = inner.max_size_buffers {
            if max <= inner.cur_size_buffers {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Queue is full (buffers): {} <= {}", max, inner.cur_size_buffers);
                return Err(item);
            }
        }

        if let Some(max) = inner.max_size_bytes {
            if max <= inner.cur_size_bytes {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Queue is full (bytes): {} <= {}", max, inner.cur_size_bytes);
                return Err(item);
            }
        }

        // FIXME: Use running time
        if let (Some(max), Some(queue_ts), Some(ts)) = (inner.max_size_time, queue_ts, ts) {
            let level = if queue_ts > ts {
                queue_ts - ts
            } else {
                ts - queue_ts
            };

            if max <= level {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Queue is full (time): {} <= {}", max, level);
                return Err(item);
            }
        }

        inner.queue.push_back(item);
        inner.cur_size_buffers += count;
        inner.cur_size_bytes += bytes;

        inner.wake();

        Ok(())
    }

    // Implementing `next` as an `async fn` instead of a `Stream` because of the `async` `Mutex`
    // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/merge_requests/204#note_322774
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> Option<DataQueueItem> {
        loop {
            let wake_receiver = {
                let mut inner = self.0.lock().await;

                match inner.state {
                    DataQueueState::Started => match inner.queue.pop_front() {
                        None => {
                            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue is empty");
                            let (sender, receiver) = oneshot::channel();
                            inner.wake_sender = Some(sender);

                            receiver
                        }
                        Some(item) => {
                            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Popped item {:?}", item);

                            let (count, bytes) = item.size();
                            inner.cur_size_buffers -= count;
                            inner.cur_size_bytes -= bytes;

                            return Some(item);
                        }
                    },
                    DataQueueState::Paused => {
                        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue Paused");
                        let (sender, receiver) = oneshot::channel();
                        inner.wake_sender = Some(sender);

                        receiver
                    }
                    DataQueueState::Stopped => {
                        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue Stopped");
                        return None;
                    }
                }
            };

            let _ = wake_receiver.await;
        }
    }
}
