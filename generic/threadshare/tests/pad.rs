// Copyright (C) 2019-2020 François Laignel <fengalin@free.fr>
// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
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

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;

use glib::GBoxed;
use glib::{glib_object_impl, glib_object_subclass};

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst::{gst_debug, gst_error_msg, gst_info, gst_log};

use lazy_static::lazy_static;

use std::boxed::Box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{
    Context, PadSink, PadSinkRef, PadSrc, PadSrcRef, PadSrcWeak, Task, TaskState,
};

const DEFAULT_CONTEXT: &str = "";
const THROTTLING_DURATION: u32 = 2;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare pad test");
    });
}

// Src

static SRC_PROPERTIES: [glib::subclass::Property; 1] =
    [glib::subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    })];

#[derive(Clone, Debug, Default)]
struct Settings {
    context: String,
}

lazy_static! {
    static ref SRC_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-element-src-test",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Test Src Element"),
    );
}

#[derive(Clone, Debug)]
struct PadSrcTestHandler;

impl PadSrcTestHandler {
    async fn push_item(
        pad: &PadSrcRef<'_>,
        item: Item,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(SRC_CAT, obj: pad.gst_pad(), "Handling {:?}", item);

        match item {
            Item::Event(event) => {
                pad.push_event(event).await;

                Ok(gst::FlowSuccess::Ok)
            }
            Item::Buffer(buffer) => pad.push(buffer).await,
            Item::BufferList(list) => pad.push_list(list).await,
        }
    }
}

impl PadSrcHandler for PadSrcTestHandler {
    type ElementImpl = ElementSrcTest;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        elem_src_test: &ElementSrcTest,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                elem_src_test.task.flush_start().unwrap();
                true
            }
            EventView::Qos(..) | EventView::Reconfigure(..) | EventView::Latency(..) => true,
            EventView::FlushStop(..) => {
                elem_src_test.task.flush_stop().unwrap();
                true
            }
            _ => false,
        };

        if ret {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        ret
    }
}

#[derive(Debug)]
struct ElementSrcTestTask {
    element: gst::Element,
    src_pad: PadSrcWeak,
    receiver: mpsc::Receiver<Item>,
}

impl ElementSrcTestTask {
    fn new(element: &gst::Element, src_pad: &PadSrc, receiver: mpsc::Receiver<Item>) -> Self {
        ElementSrcTestTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            receiver,
        }
    }
}

impl ElementSrcTestTask {
    fn flush(&mut self) {
        // Purge the channel
        while let Ok(Some(_item)) = self.receiver.try_next() {}
    }
}

impl TaskImpl for ElementSrcTestTask {
    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = self.receiver.next().await;

            let item = match item {
                Some(item) => item,
                None => {
                    gst_log!(SRC_CAT, obj: &self.element, "SrcPad channel aborted");
                    return Err(gst::FlowError::Eos);
                }
            };

            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");
            let res = PadSrcTestHandler::push_item(&pad, item).await;
            match res {
                Ok(_) => gst_log!(SRC_CAT, obj: &self.element, "Successfully pushed item"),
                Err(gst::FlowError::Flushing) => {
                    gst_debug!(SRC_CAT, obj: &self.element, "Flushing")
                }
                Err(err) => panic!("Got error {}", err),
            }

            res.map(drop)
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(SRC_CAT, obj: &self.element, "Stopping task");
            self.flush();
            gst_log!(SRC_CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(SRC_CAT, obj: &self.element, "Starting task flush");
            self.flush();
            gst_log!(SRC_CAT, obj: &self.element, "Task flush started");
            Ok(())
        }
        .boxed()
    }
}

#[derive(Debug)]
struct ElementSrcTest {
    src_pad: PadSrc,
    task: Task,
    sender: StdMutex<Option<mpsc::Sender<Item>>>,
    settings: StdMutex<Settings>,
}

impl ElementSrcTest {
    fn try_push(&self, item: Item) -> Result<(), Item> {
        let state = self.task.lock_state();
        if *state != TaskState::Started && *state != TaskState::Paused {
            gst_debug!(SRC_CAT, "ElementSrcTest rejecting item due to pad state");

            return Err(item);
        }

        match self.sender.lock().unwrap().as_mut() {
            Some(sender) => sender
                .try_send(item)
                .map_err(mpsc::TrySendError::into_inner),
            None => Err(item),
        }
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();
        let context = Context::acquire(&settings.context, THROTTLING_DURATION).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to acquire Context: {}", err]
            )
        })?;

        let (sender, receiver) = mpsc::channel(1);
        *self.sender.lock().unwrap() = Some(sender);

        self.task
            .prepare(
                ElementSrcTestTask::new(element, &self.src_pad, receiver),
                context,
            )
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::Failed,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) {
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        *self.sender.lock().unwrap() = None;
        self.task.unprepare().unwrap();

        gst_debug!(SRC_CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &gst::Element) {
        gst_debug!(SRC_CAT, obj: element, "Stopping");
        self.task.stop().unwrap();
        gst_debug!(SRC_CAT, obj: element, "Stopped");
    }

    fn start(&self, element: &gst::Element) {
        gst_debug!(SRC_CAT, obj: element, "Starting");
        self.task.start().unwrap();
        gst_debug!(SRC_CAT, obj: element, "Started");
    }

    fn pause(&self, element: &gst::Element) {
        gst_debug!(SRC_CAT, obj: element, "Pausing");
        self.task.pause().unwrap();
        gst_debug!(SRC_CAT, obj: element, "Paused");
    }
}

impl ObjectSubclass for ElementSrcTest {
    const NAME: &'static str = "TsElementSrcTest";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = glib::subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut glib::subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing Test Src Element",
            "Generic",
            "Src Element for Pad Src Test",
            "François Laignel <fengalin@free.fr>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&SRC_PROPERTIES);
    }

    fn new_with_class(klass: &glib::subclass::simple::ClassStruct<Self>) -> Self {
        ElementSrcTest {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.get_pad_template("src").unwrap(), Some("src")),
                PadSrcTestHandler,
            ),
            task: Task::default(),
            sender: StdMutex::new(None),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for ElementSrcTest {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &SRC_PROPERTIES[id];

        match *prop {
            glib::subclass::Property("context", ..) => {
                let context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());

                self.settings.lock().unwrap().context = context;
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for ElementSrcTest {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_log!(SRC_CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause(element);
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                self.stop(element);
            }
            gst::StateChange::PausedToPlaying => {
                self.start(element);
            }
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn send_event(&self, _element: &gst::Element, event: gst::Event) -> bool {
        match event.view() {
            EventView::FlushStart(..) => {
                self.task.flush_start().unwrap();
            }
            EventView::FlushStop(..) => {
                self.task.flush_stop().unwrap();
            }
            _ => (),
        }

        if !event.is_serialized() {
            self.src_pad.gst_pad().push_event(event)
        } else {
            self.try_push(Item::Event(event)).is_ok()
        }
    }
}

// Sink

#[derive(Debug)]
enum Item {
    Buffer(gst::Buffer),
    BufferList(gst::BufferList),
    Event(gst::Event),
}

#[derive(Clone, Debug, GBoxed)]
#[gboxed(type_name = "TsTestItemSender")]
struct ItemSender {
    sender: mpsc::Sender<Item>,
}

static SINK_PROPERTIES: [glib::subclass::Property; 1] =
    [glib::subclass::Property("sender", |name| {
        glib::ParamSpec::boxed(
            name,
            "Sender",
            "Channel sender to forward the incoming items to",
            ItemSender::get_type(),
            glib::ParamFlags::WRITABLE,
        )
    })];

#[derive(Clone, Debug, Default)]
struct PadSinkTestHandler;

impl PadSinkHandler for PadSinkTestHandler {
    type ElementImpl = ElementSinkTest;

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        _elem_sink_test: &ElementSinkTest,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let element = element.clone();
        async move {
            let elem_sink_test = ElementSinkTest::from_instance(&element);
            elem_sink_test
                .forward_item(&element, Item::Buffer(buffer))
                .await
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        _pad: &PadSinkRef,
        _elem_sink_test: &ElementSinkTest,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let element = element.clone();
        async move {
            let elem_sink_test = ElementSinkTest::from_instance(&element);
            elem_sink_test
                .forward_item(&element, Item::BufferList(list))
                .await
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        elem_sink_test: &ElementSinkTest,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Handling non-serialized {:?}", event);

        match event.view() {
            EventView::FlushStart(..) => {
                elem_sink_test.stop(&element);
                true
            }
            _ => false,
        }
    }

    fn sink_event_serialized(
        &self,
        pad: &PadSinkRef,
        _elem_sink_test: &ElementSinkTest,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling serialized {:?}", event);

        let element = element.clone();
        async move {
            let elem_sink_test = ElementSinkTest::from_instance(&element);

            if let EventView::FlushStop(..) = event.view() {
                elem_sink_test.start(&element);
            }

            elem_sink_test
                .forward_item(&element, Item::Event(event))
                .await
                .is_ok()
        }
        .boxed()
    }
}

#[derive(Debug)]
struct ElementSinkTest {
    sink_pad: PadSink,
    flushing: AtomicBool,
    sender: FutMutex<Option<mpsc::Sender<Item>>>,
}

impl ElementSinkTest {
    async fn forward_item(
        &self,
        element: &gst::Element,
        item: Item,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if !self.flushing.load(Ordering::SeqCst) {
            gst_debug!(SINK_CAT, obj: element, "Fowarding {:?}", item);
            self.sender
                .lock()
                .await
                .as_mut()
                .expect("Item Sender not set")
                .send(item)
                .await
                .map(|_| gst::FlowSuccess::Ok)
                .map_err(|_| gst::FlowError::Error)
        } else {
            gst_debug!(
                SINK_CAT,
                obj: element,
                "Not fowarding {:?} due to flushing",
                item
            );
            Err(gst::FlowError::Flushing)
        }
    }

    fn start(&self, element: &gst::Element) {
        gst_debug!(SINK_CAT, obj: element, "Starting");
        self.flushing.store(false, Ordering::SeqCst);
        gst_debug!(SINK_CAT, obj: element, "Started");
    }

    fn stop(&self, element: &gst::Element) {
        gst_debug!(SINK_CAT, obj: element, "Stopping");
        self.flushing.store(true, Ordering::SeqCst);
        gst_debug!(SINK_CAT, obj: element, "Stopped");
    }
}

impl ElementSinkTest {
    fn push_flush_start(&self, element: &gst::Element) {
        gst_debug!(SINK_CAT, obj: element, "Pushing FlushStart");
        self.sink_pad
            .gst_pad()
            .push_event(gst::Event::new_flush_start().build());
        gst_debug!(SINK_CAT, obj: element, "FlushStart pushed");
    }

    fn push_flush_stop(&self, element: &gst::Element) {
        gst_debug!(SINK_CAT, obj: element, "Pushing FlushStop");
        self.sink_pad
            .gst_pad()
            .push_event(gst::Event::new_flush_stop(true).build());
        gst_debug!(SINK_CAT, obj: element, "FlushStop pushed");
    }
}

lazy_static! {
    static ref SINK_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-element-sink-test",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Test Sink Element"),
    );
}

impl ObjectSubclass for ElementSinkTest {
    const NAME: &'static str = "TsElementSinkTest";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = glib::subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut glib::subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing Test Sink Element",
            "Generic",
            "Sink Element for Pad Test",
            "François Laignel <fengalin@free.fr>",
        );

        let caps = gst::Caps::new_any();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&SINK_PROPERTIES);
    }

    fn new_with_class(klass: &glib::subclass::simple::ClassStruct<Self>) -> Self {
        ElementSinkTest {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.get_pad_template("sink").unwrap(), Some("sink")),
                PadSinkTestHandler,
            ),
            flushing: AtomicBool::new(true),
            sender: FutMutex::new(None),
        }
    }
}

impl ObjectImpl for ElementSinkTest {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &SINK_PROPERTIES[id];

        match *prop {
            glib::subclass::Property("sender", ..) => {
                let ItemSender { sender } = value
                    .get::<&ItemSender>()
                    .expect("type checked upstream")
                    .expect("ItemSender not found")
                    .clone();
                *futures::executor::block_on(self.sender.lock()) = Some(sender);
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.sink_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for ElementSinkTest {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_log!(SINK_CAT, obj: element, "Changing state {:?}", transition);

        if let gst::StateChange::PausedToReady = transition {
            self.stop(element);
        }

        let success = self.parent_change_state(element, transition)?;

        if let gst::StateChange::ReadyToPaused = transition {
            self.start(element);
        }

        Ok(success)
    }
}

fn setup(
    context_name: &str,
    mut middle_element_1: Option<gst::Element>,
    mut middle_element_2: Option<gst::Element>,
) -> (
    gst::Pipeline,
    gst::Element,
    gst::Element,
    mpsc::Receiver<Item>,
) {
    init();

    let pipeline = gst::Pipeline::new(None);

    // Src
    let src_element = glib::Object::new(ElementSrcTest::get_type(), &[])
        .unwrap()
        .downcast::<gst::Element>()
        .unwrap();
    src_element.set_property("context", &context_name).unwrap();
    pipeline.add(&src_element).unwrap();

    let mut last_element = src_element.clone();

    if let Some(middle_element) = middle_element_1.take() {
        pipeline.add(&middle_element).unwrap();
        last_element.link(&middle_element).unwrap();
        last_element = middle_element;
    }

    if let Some(middle_element) = middle_element_2.take() {
        // Don't link the 2 middle elements: this is used for ts-proxy
        pipeline.add(&middle_element).unwrap();
        last_element = middle_element;
    }

    // Sink
    let sink_element = glib::Object::new(ElementSinkTest::get_type(), &[])
        .unwrap()
        .downcast::<gst::Element>()
        .unwrap();
    pipeline.add(&sink_element).unwrap();
    last_element.link(&sink_element).unwrap();

    let (sender, receiver) = mpsc::channel::<Item>(10);
    sink_element
        .set_property("sender", &ItemSender { sender })
        .unwrap();

    (pipeline, src_element, sink_element, receiver)
}

fn nominal_scenario(
    scenario_name: &str,
    pipeline: gst::Pipeline,
    src_element: gst::Element,
    mut receiver: mpsc::Receiver<Item>,
) {
    let elem_src_test = ElementSrcTest::from_instance(&src_element);

    pipeline.set_state(gst::State::Playing).unwrap();

    // Initial events
    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_stream_start(scenario_name)
                .group_id(gst::GroupId::next())
                .build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::StreamStart(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Buffer
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![1, 2, 3, 4])))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    // BufferList
    let mut list = gst::BufferList::new();
    list.get_mut()
        .unwrap()
        .add(gst::Buffer::from_slice(vec![1, 2, 3, 4]));
    elem_src_test.try_push(Item::BufferList(list)).unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::BufferList(_) => (),
        other => panic!("Unexpected item {:?}", other),
    }

    // Pause the Pad task
    pipeline.set_state(gst::State::Paused).unwrap();

    // Item accepted, but not processed before switching to Playing again
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![5, 6, 7])))
        .unwrap();

    // Nothing forwarded
    receiver.try_next().unwrap_err();

    // Switch back the Pad task to Started
    pipeline.set_state(gst::State::Playing).unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![5, 6, 7].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    // Flush
    src_element.send_event(gst::Event::new_flush_start().build());
    src_element.send_event(gst::Event::new_flush_stop(true).build());

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::FlushStop(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Buffer
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![8, 9])))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![8, 9].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    // EOS
    elem_src_test
        .try_push(Item::Event(gst::Event::new_eos().build()))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Eos(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    pipeline.set_state(gst::State::Ready).unwrap();

    // Receiver was dropped when stopping => can't send anymore
    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_stream_start(&format!("{}_past_stop", scenario_name))
                .group_id(gst::GroupId::next())
                .build(),
        ))
        .unwrap_err();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn src_sink_nominal() {
    let name = "src_sink_nominal";

    let (pipeline, src_element, _sink_element, receiver) = setup(&name, None, None);

    nominal_scenario(&name, pipeline, src_element, receiver);
}

#[test]
fn src_tsqueue_sink_nominal() {
    init();

    let name = "src_tsqueue_sink";

    let ts_queue = gst::ElementFactory::make("ts-queue", Some("ts-queue")).unwrap();
    ts_queue
        .set_property("context", &format!("{}_queue", name))
        .unwrap();
    ts_queue
        .set_property("context-wait", &THROTTLING_DURATION)
        .unwrap();

    let (pipeline, src_element, _sink_element, receiver) = setup(name, Some(ts_queue), None);

    nominal_scenario(&name, pipeline, src_element, receiver);
}

#[test]
fn src_queue_sink_nominal() {
    init();

    let name = "src_queue_sink";

    let queue = gst::ElementFactory::make("queue", Some("queue")).unwrap();
    let (pipeline, src_element, _sink_element, receiver) = setup(name, Some(queue), None);

    nominal_scenario(&name, pipeline, src_element, receiver);
}

#[test]
fn src_tsproxy_sink_nominal() {
    init();

    let name = "src_tsproxy_sink";

    let ts_proxy_sink = gst::ElementFactory::make("ts-proxysink", Some("ts-proxysink")).unwrap();
    ts_proxy_sink
        .set_property("proxy-context", &format!("{}_proxy_context", name))
        .unwrap();

    let ts_proxy_src = gst::ElementFactory::make("ts-proxysrc", Some("ts-proxysrc")).unwrap();
    ts_proxy_src
        .set_property("proxy-context", &format!("{}_proxy_context", name))
        .unwrap();
    ts_proxy_src
        .set_property("context", &format!("{}_context", name))
        .unwrap();
    ts_proxy_src
        .set_property("context-wait", &THROTTLING_DURATION)
        .unwrap();

    let (pipeline, src_element, _sink_element, receiver) =
        setup(name, Some(ts_proxy_sink), Some(ts_proxy_src));

    nominal_scenario(&name, pipeline, src_element, receiver);
}

#[test]
fn start_pause_start() {
    init();

    let scenario_name = "start_pause_start";

    let (pipeline, src_element, _sink_element, mut receiver) = setup(&scenario_name, None, None);

    let elem_src_test = ElementSrcTest::from_instance(&src_element);

    pipeline.set_state(gst::State::Playing).unwrap();

    // Initial events
    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_stream_start(scenario_name)
                .group_id(gst::GroupId::next())
                .build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::StreamStart(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Buffer
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![1, 2, 3, 4])))
        .unwrap();

    pipeline.set_state(gst::State::Paused).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![5, 6, 7])))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![5, 6, 7].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    // Nothing else forwarded
    receiver.try_next().unwrap_err();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn start_stop_start() {
    init();

    let scenario_name = "start_stop_start";

    let (pipeline, src_element, _sink_element, mut receiver) = setup(&scenario_name, None, None);

    let elem_src_test = ElementSrcTest::from_instance(&src_element);

    pipeline.set_state(gst::State::Playing).unwrap();

    // Initial events
    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_stream_start(&format!("{}-after_stop", scenario_name))
                .group_id(gst::GroupId::next())
                .build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::StreamStart(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Buffer
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![1, 2, 3, 4])))
        .unwrap();

    pipeline.set_state(gst::State::Ready).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    // Initial events again
    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_stream_start(scenario_name)
                .group_id(gst::GroupId::next())
                .build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(_buffer) => {
            gst_info!(
                SRC_CAT,
                "{}: initial buffer went through, don't expect any pending item to be dropped",
                scenario_name
            );

            match futures::executor::block_on(receiver.next()).unwrap() {
                Item::Event(event) => match event.view() {
                    EventView::StreamStart(_) => (),
                    other => panic!("Unexpected event {:?}", other),
                },
                other => panic!("Unexpected item {:?}", other),
            }
        }
        Item::Event(event) => match event.view() {
            EventView::StreamStart(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![5, 6, 7])))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![5, 6, 7].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn start_flush() {
    init();

    let scenario_name = "start_flush";

    let (pipeline, src_element, sink_element, mut receiver) = setup(&scenario_name, None, None);

    let elem_src_test = ElementSrcTest::from_instance(&src_element);

    pipeline.set_state(gst::State::Playing).unwrap();

    // Initial events
    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_stream_start(&format!("{}-after_stop", scenario_name))
                .group_id(gst::GroupId::next())
                .build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::StreamStart(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Buffer
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![1, 2, 3, 4])))
        .unwrap();

    let elem_sink_test = ElementSinkTest::from_instance(&sink_element);

    elem_sink_test.push_flush_start(&sink_element);

    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![5, 6, 7])))
        .unwrap_err();

    elem_sink_test.push_flush_stop(&sink_element);

    elem_src_test
        .try_push(Item::Event(
            gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
        ))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        Item::Buffer(buffer) => {
            // In some cases, the first Buffer might be processed before FlushStart
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());

            match futures::executor::block_on(receiver.next()).unwrap() {
                Item::Event(event) => match event.view() {
                    EventView::Segment(_) => (),
                    other => panic!("Unexpected event {:?}", other),
                },
                other => panic!("Unexpected item {:?}", other),
            }
        }
        other => panic!("Unexpected item {:?}", other),
    }

    // Post flush buffer
    elem_src_test
        .try_push(Item::Buffer(gst::Buffer::from_slice(vec![8, 9])))
        .unwrap();

    match futures::executor::block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![8, 9].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    pipeline.set_state(gst::State::Null).unwrap();
}
